//! Version 1 B-trees (group symbol nodes, chunk index) and version 2 B-trees
//! (dense link/attribute name indexes, chunk index records).
use super::io::{limit_enc_size, verify_checksum, Cur};
use super::Ctx;
use anyhow::{bail, ensure, Context, Result};

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 4_000_000;

/// Walk a v1 B-tree of `node_type`; `key_size` bytes per key. Calls `leaf`
/// with (left key bytes, child address) for every level-0 child in order.
pub fn walk_v1(
    ctx: &Ctx,
    addr: u64,
    node_type: u8,
    key_size: usize,
    leaf: &mut dyn FnMut(&[u8], u64) -> Result<()>,
) -> Result<()> {
    let mut visited = 0usize;
    walk_v1_node(ctx, addr, node_type, key_size, leaf, 0, &mut visited)
}
fn walk_v1_node(
    ctx: &Ctx,
    addr: u64,
    node_type: u8,
    key_size: usize,
    leaf: &mut dyn FnMut(&[u8], u64) -> Result<()>,
    depth: usize,
    visited: &mut usize,
) -> Result<()> {
    ensure!(depth < MAX_DEPTH, "HDF5 v1 B-tree too deep");
    let head_len = 8 + 2 * ctx.so;
    let head = ctx.read(addr, head_len)?;
    let mut c = Cur::new(&head);
    c.signature(b"TREE")?;
    ensure!(c.u8()? == node_type, "HDF5 v1 B-tree node type mismatch");
    let level = c.u8()?;
    let entries = usize::from(c.u16()?);
    *visited += entries;
    ensure!(
        *visited <= MAX_ENTRIES,
        "HDF5 v1 B-tree exceeds entry limit"
    );
    let body_len = entries * (key_size + ctx.so) + key_size;
    let body = ctx.read(addr + head_len as u64, body_len)?;
    let mut c = Cur::new(&body);
    for _ in 0..entries {
        let key = c.bytes(key_size)?;
        let child = c.addr(ctx.so)?.context("Undefined HDF5 B-tree child")?;
        if level == 0 {
            leaf(key, child)?;
        } else {
            walk_v1_node(ctx, child, node_type, key_size, leaf, depth + 1, visited)?;
        }
    }
    Ok(())
}

/// Every record of a v2 B-tree, in tree order.
pub fn records_v2(ctx: &Ctx, addr: u64) -> Result<(u8, Vec<Vec<u8>>)> {
    let len = 4 + 1 + 1 + 4 + 2 + 2 + 1 + 1 + ctx.so + 2 + ctx.sl + 4;
    let raw = ctx.read(addr, len)?;
    verify_checksum(&raw, "v2 B-tree header")?;
    let mut c = Cur::new(&raw);
    c.signature(b"BTHD")?;
    ensure!(c.u8()? == 0, "Unsupported HDF5 v2 B-tree version");
    let kind = c.u8()?;
    let node_size = c.u32()? as usize;
    let rec_size = usize::from(c.u16()?);
    let depth = usize::from(c.u16()?);
    c.skip(2)?;
    let root = c.addr(ctx.so)?;
    let root_records = usize::from(c.u16()?);
    let total = c.uint(ctx.sl)?;
    ensure!(
        rec_size > 0
            && node_size > 10 + rec_size
            && depth < MAX_DEPTH
            && total <= MAX_ENTRIES as u64,
        "Invalid HDF5 v2 B-tree header"
    );
    let mut out = Vec::new();
    let Some(root) = root else {
        return Ok((kind, out));
    };
    // Node capacities per depth (H5B2__hdr_init).
    let max_leaf = (node_size - 10) / rec_size;
    let max_nrec_size = limit_enc_size(max_leaf as u64);
    let mut cum = vec![max_leaf as u64];
    let mut cum_size = vec![0usize];
    for d in 1..=depth {
        let p = ctx.so + max_nrec_size + if d > 1 { cum_size[d - 1] } else { 0 };
        ensure!(node_size > 10 + p, "Invalid HDF5 v2 B-tree node size");
        let max = ((node_size - 10 - p) / (rec_size + p)) as u64;
        let c_max = (max + 1)
            .checked_mul(cum[d - 1])
            .and_then(|v| v.checked_add(max))
            .unwrap_or(u64::MAX);
        cum.push(c_max);
        cum_size.push(limit_enc_size(c_max));
    }
    walk_v2(
        ctx,
        root,
        root_records,
        depth,
        kind,
        rec_size,
        max_nrec_size,
        &cum_size,
        &mut out,
    )?;
    ensure!(
        out.len() as u64 == total,
        "HDF5 v2 B-tree record count mismatch"
    );
    Ok((kind, out))
}
#[allow(clippy::too_many_arguments)]
fn walk_v2(
    ctx: &Ctx,
    addr: u64,
    nrec: usize,
    depth: usize,
    kind: u8,
    rec_size: usize,
    max_nrec_size: usize,
    cum_size: &[usize],
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    ensure!(
        out.len() + nrec <= MAX_ENTRIES,
        "HDF5 v2 B-tree exceeds entry limit"
    );
    if depth == 0 {
        let len = 6 + nrec * rec_size + 4;
        let raw = ctx.read(addr, len)?;
        verify_checksum(&raw, "v2 B-tree leaf")?;
        let mut c = Cur::new(&raw);
        c.signature(b"BTLF")?;
        ensure!(
            c.u8()? == 0 && c.u8()? == kind,
            "HDF5 v2 B-tree leaf mismatch"
        );
        for _ in 0..nrec {
            out.push(c.bytes(rec_size)?.to_vec());
        }
        return Ok(());
    }
    let ptr = ctx.so + max_nrec_size + if depth > 1 { cum_size[depth - 1] } else { 0 };
    let len = 6 + nrec * rec_size + (nrec + 1) * ptr + 4;
    let raw = ctx.read(addr, len)?;
    verify_checksum(&raw, "v2 B-tree internal node")?;
    let mut c = Cur::new(&raw);
    c.signature(b"BTIN")?;
    ensure!(
        c.u8()? == 0 && c.u8()? == kind,
        "HDF5 v2 B-tree node mismatch"
    );
    let mut records = Vec::with_capacity(nrec);
    for _ in 0..nrec {
        records.push(c.bytes(rec_size)?.to_vec());
    }
    let mut children = Vec::with_capacity(nrec + 1);
    for _ in 0..=nrec {
        let child = c.addr(ctx.so)?.context("Undefined v2 B-tree child")?;
        let n = c.uint(max_nrec_size)? as usize;
        if depth > 1 {
            c.uint(cum_size[depth - 1])?;
        }
        children.push((child, n));
    }
    for (i, (child, n)) in children.into_iter().enumerate() {
        walk_v2(
            ctx,
            child,
            n,
            depth - 1,
            kind,
            rec_size,
            max_nrec_size,
            cum_size,
            out,
        )?;
        if i < records.len() {
            out.push(std::mem::take(&mut records[i]));
        }
    }
    Ok(())
}

/// Unused-kind guard for callers that expect a specific record type.
pub fn expect_kind(kind: u8, expected: &[u8], what: &str) -> Result<()> {
    if !expected.contains(&kind) {
        bail!("Unexpected HDF5 v2 B-tree type {kind} for {what}");
    }
    Ok(())
}
