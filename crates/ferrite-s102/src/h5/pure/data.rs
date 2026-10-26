//! Raw dataset storage: compact, contiguous and chunked layouts, chunk indexes
//! (v1 B-tree, single chunk, implicit, fixed array, v2 B-tree) and the
//! deflate / shuffle / Fletcher-32 filters. Everything else fails closed.
use super::btree;
use super::io::{le, verify_checksum, Cur};
use super::object::{Filter, Header, MSG_EXTERNAL, MSG_FILTERS, MSG_LAYOUT};
use super::Ctx;
use anyhow::{bail, ensure, Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};

/// Largest selection or chunk this reader materializes at once.
const MAX_BYTES: u64 = 1 << 31;

#[derive(Debug, Clone, Copy)]
pub struct ChunkLoc {
    pub addr: u64,
    pub size: u64,
    pub mask: u32,
}

#[derive(Debug)]
enum Index {
    BtreeV1(u64),
    Single(ChunkLoc),
    Implicit(u64),
    FixedArray(u64),
    BtreeV2(u64),
    Empty,
}

#[derive(Debug)]
enum Layout {
    Compact(Vec<u8>),
    Contiguous(Option<u64>),
    Chunked {
        chunk: Vec<u64>,
        index: Index,
        filter_edges: bool,
    },
}

pub struct Storage {
    layout: Layout,
    filters: Vec<Filter>,
    esize: usize,
    dims: Vec<u64>,
    fill: Option<Vec<u8>>,
    chunks: OnceLock<std::result::Result<ChunkMap, String>>,
    cache: Mutex<Vec<DecodedChunk>>,
}
/// Chunk grid coordinates (in chunks) to storage.
type ChunkMap = HashMap<Vec<u64>, ChunkLoc>;
/// A recently decoded chunk (grid coordinates, bytes).
type DecodedChunk = (Vec<u64>, Arc<Vec<u8>>);
impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("layout", &self.layout)
            .field("dims", &self.dims)
            .finish()
    }
}

fn product(v: &[u64]) -> Result<u64> {
    v.iter().try_fold(1u64, |a, b| {
        a.checked_mul(*b).context("HDF5 extent overflow")
    })
}

impl Storage {
    pub fn new(
        ctx: &Ctx,
        header: &Header,
        dims: Vec<u64>,
        esize: usize,
        fill: Option<Vec<u8>>,
    ) -> Result<Self> {
        ensure!(
            header.find(MSG_EXTERNAL).is_none(),
            "External HDF5 storage is not supported"
        );
        let filters = match header.find(MSG_FILTERS) {
            Some(m) => super::object::parse_filters(&m.data)?,
            None => Vec::new(),
        };
        for f in &filters {
            ensure!(
                matches!(f.id, 1..=3),
                "HDF5 filter {} is not supported by the pure reader",
                f.id
            );
        }
        if let Some(fill) = &fill {
            ensure!(
                fill.len() == esize,
                "HDF5 fill value size differs from element size"
            );
        }
        let m = header.find(MSG_LAYOUT).context("Dataset without layout")?;
        let layout = parse_layout(ctx, &m.data, dims.len(), esize)?;
        if !filters.is_empty() {
            ensure!(
                matches!(layout, Layout::Chunked { .. }),
                "HDF5 filters require chunked layout"
            );
        }
        Ok(Self {
            layout,
            filters,
            esize,
            dims,
            fill,
            chunks: OnceLock::new(),
            cache: Mutex::new(Vec::new()),
        })
    }

    fn fill_into(&self, out: &mut [u8]) {
        match &self.fill {
            Some(f) => out
                .chunks_exact_mut(self.esize)
                .for_each(|e| e.copy_from_slice(f)),
            None => out.fill(0),
        }
    }

    /// Raw file-order bytes of the hyperslab `start..start+count`, row-major.
    pub fn read(&self, ctx: &Ctx, start: &[u64], count: &[u64]) -> Result<Vec<u8>> {
        let rank = self.dims.len();
        ensure!(
            start.len() == rank && count.len() == rank,
            "Selection rank mismatch"
        );
        for i in 0..rank {
            ensure!(
                start[i]
                    .checked_add(count[i])
                    .is_some_and(|e| e <= self.dims[i]),
                "Selection outside HDF5 dataset extent"
            );
        }
        let n = product(count)?;
        let bytes = n
            .checked_mul(self.esize as u64)
            .filter(|b| *b <= MAX_BYTES)
            .context("HDF5 selection exceeds reader budget")?;
        let mut out = vec![0u8; bytes as usize];
        if n == 0 {
            return Ok(out);
        }
        match &self.layout {
            Layout::Compact(raw) => {
                let total = product(&self.dims)? as usize * self.esize;
                ensure!(raw.len() >= total, "Truncated HDF5 compact data");
                copy_box(
                    raw,
                    &self.dims,
                    &vec![0; rank],
                    start,
                    count,
                    self.esize,
                    &mut out,
                    count,
                )?;
            }
            Layout::Contiguous(None) => self.fill_into(&mut out),
            Layout::Contiguous(Some(addr)) => {
                let total = product(&self.dims)?
                    .checked_mul(self.esize as u64)
                    .context("HDF5 extent overflow")?;
                ensure!(
                    addr.checked_add(total).is_some_and(|e| e <= ctx.eof()),
                    "HDF5 contiguous data beyond file end"
                );
                self.read_contiguous(ctx, *addr, start, count, &mut out)?;
            }
            Layout::Chunked { chunk, .. } => {
                let chunk = chunk.clone();
                self.read_chunked(ctx, &chunk, start, count, &mut out)?;
            }
        }
        Ok(out)
    }

    fn read_contiguous(
        &self,
        ctx: &Ctx,
        addr: u64,
        start: &[u64],
        count: &[u64],
        out: &mut [u8],
    ) -> Result<()> {
        let rank = self.dims.len();
        if rank == 0 {
            let raw = ctx.read(addr, self.esize)?;
            out.copy_from_slice(&raw);
            return Ok(());
        }
        let run = count[rank - 1] as usize * self.esize;
        let mut idx = vec![0u64; rank - 1];
        let mut o = 0;
        loop {
            let mut lin = 0u64;
            for d in 0..rank {
                let i = if d < rank - 1 {
                    start[d] + idx[d]
                } else {
                    start[d]
                };
                lin = lin * self.dims[d] + i;
            }
            let raw = ctx.read(addr + lin * self.esize as u64, run)?;
            out[o..o + run].copy_from_slice(&raw);
            o += run;
            if !advance(&mut idx, &count[..rank - 1]) {
                break;
            }
        }
        Ok(())
    }

    fn chunk_map(&self, ctx: &Ctx, chunk: &[u64]) -> Result<&HashMap<Vec<u64>, ChunkLoc>> {
        let built = self.chunks.get_or_init(|| {
            self.build_chunk_map(ctx, chunk)
                .map_err(|e| format!("{e:#}"))
        });
        built.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn build_chunk_map(&self, ctx: &Ctx, chunk: &[u64]) -> Result<HashMap<Vec<u64>, ChunkLoc>> {
        let Layout::Chunked { index, .. } = &self.layout else {
            unreachable!()
        };
        let rank = self.dims.len();
        let grid: Vec<u64> = self
            .dims
            .iter()
            .zip(chunk)
            .map(|(d, c)| d.div_ceil(*c))
            .collect();
        let nchunks = product(&grid)?;
        ensure!(
            nchunks <= 16_000_000,
            "HDF5 chunk grid exceeds reader budget"
        );
        let full = product(chunk)? * self.esize as u64;
        let mut map = HashMap::new();
        let filtered = !self.filters.is_empty();
        match index {
            Index::Empty => {}
            Index::Single(loc) => {
                map.insert(vec![0; rank], *loc);
            }
            Index::Implicit(addr) => {
                ensure!(!filtered, "Implicit chunk index with filters");
                for (i, key) in grid_keys(&grid).enumerate() {
                    map.insert(
                        key,
                        ChunkLoc {
                            addr: addr + i as u64 * full,
                            size: full,
                            mask: 0,
                        },
                    );
                }
            }
            Index::BtreeV1(addr) => {
                let key_size = 8 + 8 * (rank + 1);
                btree::walk_v1(ctx, *addr, 1, key_size, &mut |key, child| {
                    let mut c = Cur::new(key);
                    let size = u64::from(c.u32()?);
                    let mask = c.u32()?;
                    let mut offs = Vec::with_capacity(rank);
                    for size in &chunk[..rank] {
                        let o = le(c.bytes(8)?);
                        ensure!(o.is_multiple_of(*size), "Unaligned HDF5 chunk offset");
                        offs.push(o / size);
                    }
                    map.insert(
                        offs,
                        ChunkLoc {
                            addr: child,
                            size,
                            mask,
                        },
                    );
                    Ok(())
                })?;
            }
            Index::FixedArray(addr) => {
                fixed_array(ctx, *addr, &grid, filtered, full, &mut map)?;
            }
            Index::BtreeV2(addr) => {
                let (kind, records) = btree::records_v2(ctx, *addr)?;
                btree::expect_kind(kind, if filtered { &[11] } else { &[10] }, "chunk index")?;
                let size_len = chunk_size_len(full);
                for r in records {
                    let mut c = Cur::new(&r);
                    let a = c.addr(ctx.so)?.context("Undefined chunk address")?;
                    let (size, mask) = if filtered {
                        (c.uint(size_len)?, c.u32()?)
                    } else {
                        (full, 0)
                    };
                    let mut offs = Vec::with_capacity(rank);
                    for _ in 0..rank {
                        offs.push(le(c.bytes(8)?));
                    }
                    map.insert(
                        offs,
                        ChunkLoc {
                            addr: a,
                            size,
                            mask,
                        },
                    );
                }
            }
        }
        for key in map.keys() {
            ensure!(
                key.len() == rank && key.iter().zip(&grid).all(|(k, g)| k < g),
                "HDF5 chunk outside dataset grid"
            );
        }
        Ok(map)
    }

    fn chunk_bytes(&self, ctx: &Ctx, chunk: &[u64], key: &[u64]) -> Result<Option<Arc<Vec<u8>>>> {
        if let Some((_, data)) = self.cache.lock().unwrap().iter().find(|(k, _)| k == key) {
            return Ok(Some(data.clone()));
        }
        let Some(loc) = self.chunk_map(ctx, chunk)?.get(key).copied() else {
            return Ok(None);
        };
        let full = product(chunk)? * self.esize as u64;
        ensure!(loc.size <= MAX_BYTES, "HDF5 chunk exceeds reader budget");
        let mut data = ctx.read(loc.addr, loc.size as usize)?;
        let Layout::Chunked { filter_edges, .. } = &self.layout else {
            unreachable!()
        };
        let edge = key
            .iter()
            .zip(chunk)
            .zip(&self.dims)
            .any(|((k, c), d)| (k + 1) * c > *d);
        if !(edge && !filter_edges) {
            for (i, f) in self.filters.iter().enumerate().rev() {
                if loc.mask & (1 << i) != 0 {
                    continue;
                }
                data = apply_filter(f, data, full)?;
            }
        }
        ensure!(
            data.len() as u64 == full,
            "HDF5 chunk decodes to {} bytes, expected {full}",
            data.len()
        );
        let data = Arc::new(data);
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= 16 {
            cache.remove(0);
        }
        cache.push((key.to_vec(), data.clone()));
        Ok(Some(data))
    }

    fn read_chunked(
        &self,
        ctx: &Ctx,
        chunk: &[u64],
        start: &[u64],
        count: &[u64],
        out: &mut [u8],
    ) -> Result<()> {
        let rank = self.dims.len();
        let first: Vec<u64> = (0..rank).map(|d| start[d] / chunk[d]).collect();
        let last: Vec<u64> = (0..rank)
            .map(|d| (start[d] + count[d] - 1) / chunk[d])
            .collect();
        let span: Vec<u64> = (0..rank).map(|d| last[d] - first[d] + 1).collect();
        let mut idx = vec![0u64; rank];
        let mut filled = None::<Vec<u8>>;
        loop {
            let key: Vec<u64> = (0..rank).map(|d| first[d] + idx[d]).collect();
            let origin: Vec<u64> = (0..rank).map(|d| key[d] * chunk[d]).collect();
            // Intersection of the chunk with the selection.
            let lo: Vec<u64> = (0..rank).map(|d| origin[d].max(start[d])).collect();
            let hi: Vec<u64> = (0..rank)
                .map(|d| (origin[d] + chunk[d]).min(start[d] + count[d]))
                .collect();
            let data = match self.chunk_bytes(ctx, chunk, &key)? {
                Some(d) => d,
                None => {
                    let f = filled.get_or_insert_with(|| {
                        let mut v = vec![0u8; (product(chunk).unwrap_or(0) as usize) * self.esize];
                        self.fill_into(&mut v);
                        v
                    });
                    Arc::new(f.clone())
                }
            };
            // Copy rows of the intersection.
            let in_start: Vec<u64> = (0..rank).map(|d| lo[d] - origin[d]).collect();
            let sub: Vec<u64> = (0..rank).map(|d| hi[d] - lo[d]).collect();
            let out_start: Vec<u64> = (0..rank).map(|d| lo[d] - start[d]).collect();
            copy_box(
                &data, chunk, &out_start, &in_start, &sub, self.esize, out, count,
            )?;
            if !advance(&mut idx, &span) {
                break;
            }
        }
        Ok(())
    }
}

/// Copy box `sub` from `src` (extent `src_dims`, at `src_start`) to `out`
/// (extent `out_dims`, at `out_start`).
#[allow(clippy::too_many_arguments)]
fn copy_box(
    src: &[u8],
    src_dims: &[u64],
    out_start: &[u64],
    src_start: &[u64],
    sub: &[u64],
    esize: usize,
    out: &mut [u8],
    out_dims: &[u64],
) -> Result<()> {
    let rank = src_dims.len();
    if rank == 0 {
        out[..esize].copy_from_slice(&src[..esize]);
        return Ok(());
    }
    let run = sub[rank - 1] as usize * esize;
    let mut idx = vec![0u64; rank - 1];
    loop {
        let mut s = 0u64;
        let mut o = 0u64;
        for d in 0..rank {
            let i = if d < rank - 1 { idx[d] } else { 0 };
            s = s * src_dims[d] + src_start[d] + i;
            o = o * out_dims[d] + out_start[d] + i;
        }
        let s = s as usize * esize;
        let o = o as usize * esize;
        ensure!(
            s + run <= src.len() && o + run <= out.len(),
            "HDF5 copy out of range"
        );
        out[o..o + run].copy_from_slice(&src[s..s + run]);
        if !advance(&mut idx, &sub[..rank - 1]) {
            break;
        }
    }
    Ok(())
}

/// Row-major odometer; false once every index wrapped.
fn advance(idx: &mut [u64], extent: &[u64]) -> bool {
    for d in (0..idx.len()).rev() {
        idx[d] += 1;
        if idx[d] < extent[d] {
            return true;
        }
        idx[d] = 0;
    }
    false
}

fn grid_keys(grid: &[u64]) -> impl Iterator<Item = Vec<u64>> + '_ {
    let total = grid.iter().product::<u64>();
    (0..total).map(move |mut i| {
        let mut key = vec![0; grid.len()];
        for d in (0..grid.len()).rev() {
            key[d] = i % grid[d];
            i /= grid[d];
        }
        key
    })
}

fn chunk_size_len(full: u64) -> usize {
    (1 + (super::io::log2_floor(full) as usize + 8) / 8).min(8)
}

fn parse_layout(ctx: &Ctx, data: &[u8], rank: usize, esize: usize) -> Result<Layout> {
    let mut c = Cur::new(data);
    let version = c.u8()?;
    match version {
        1 | 2 => {
            let ndims = usize::from(c.u8()?);
            let class = c.u8()?;
            c.skip(5)?;
            let addr = if class != 0 { c.addr(ctx.so)? } else { None };
            let mut dims = Vec::with_capacity(ndims);
            for _ in 0..ndims {
                dims.push(u64::from(c.u32()?));
            }
            match class {
                0 => {
                    let size = c.u32()? as usize;
                    Ok(Layout::Compact(c.bytes(size)?.to_vec()))
                }
                1 => Ok(Layout::Contiguous(addr)),
                2 => {
                    c.u32()?;
                    ensure!(ndims == rank + 1, "HDF5 chunk rank mismatch");
                    Ok(Layout::Chunked {
                        chunk: dims[..rank].to_vec(),
                        index: addr.map_or(Index::Empty, Index::BtreeV1),
                        filter_edges: true,
                    })
                }
                _ => bail!("Unknown HDF5 layout class {class}"),
            }
        }
        3 | 4 => {
            let class = c.u8()?;
            match class {
                0 => {
                    let size = usize::from(c.u16()?);
                    Ok(Layout::Compact(c.bytes(size)?.to_vec()))
                }
                1 => {
                    let addr = c.addr(ctx.so)?;
                    c.uint(ctx.sl)?;
                    Ok(Layout::Contiguous(addr))
                }
                2 if version == 3 => {
                    let ndims = usize::from(c.u8()?);
                    ensure!(ndims == rank + 1, "HDF5 chunk rank mismatch");
                    let addr = c.addr(ctx.so)?;
                    let mut dims = Vec::with_capacity(ndims);
                    for _ in 0..ndims {
                        dims.push(u64::from(c.u32()?));
                    }
                    ensure!(
                        dims[rank] as usize == esize,
                        "HDF5 chunk element size mismatch"
                    );
                    check_chunk(&dims[..rank])?;
                    Ok(Layout::Chunked {
                        chunk: dims[..rank].to_vec(),
                        index: addr.map_or(Index::Empty, Index::BtreeV1),
                        filter_edges: true,
                    })
                }
                2 => {
                    let flags = c.u8()?;
                    let ndims = usize::from(c.u8()?);
                    ensure!(ndims == rank + 1, "HDF5 chunk rank mismatch");
                    let width = usize::from(c.u8()?);
                    let mut dims = Vec::with_capacity(ndims);
                    for _ in 0..ndims {
                        dims.push(c.uint(width)?);
                    }
                    ensure!(
                        dims[rank] as usize == esize,
                        "HDF5 chunk element size mismatch"
                    );
                    check_chunk(&dims[..rank])?;
                    let kind = c.u8()?;
                    let mut single = None;
                    match kind {
                        1 => {
                            if flags & 0x02 != 0 {
                                single = Some((c.uint(ctx.sl)?, c.u32()?));
                            }
                        }
                        2 => {}
                        3 => {
                            c.u8()?;
                        }
                        4 => bail!("HDF5 extensible-array chunk index is not supported"),
                        5 => {
                            c.skip(6)?;
                        }
                        _ => bail!("Unknown HDF5 chunk index type {kind}"),
                    }
                    let addr = c.addr(ctx.so)?;
                    let chunk = dims[..rank].to_vec();
                    let full = product(&chunk)? * esize as u64;
                    let index = match (kind, addr) {
                        (_, None) => Index::Empty,
                        (1, Some(a)) => {
                            let (size, mask) = single.unwrap_or((full, 0));
                            Index::Single(ChunkLoc {
                                addr: a,
                                size,
                                mask,
                            })
                        }
                        (2, Some(a)) => Index::Implicit(a),
                        (3, Some(a)) => Index::FixedArray(a),
                        (5, Some(a)) => Index::BtreeV2(a),
                        _ => unreachable!(),
                    };
                    Ok(Layout::Chunked {
                        chunk,
                        index,
                        filter_edges: flags & 0x01 == 0,
                    })
                }
                3 => bail!("HDF5 virtual datasets are not supported"),
                _ => bail!("Unknown HDF5 layout class {class}"),
            }
        }
        _ => bail!("Unsupported HDF5 layout version {version}"),
    }
}

fn check_chunk(dims: &[u64]) -> Result<()> {
    ensure!(dims.iter().all(|d| *d > 0), "Zero HDF5 chunk dimension");
    Ok(())
}

fn fixed_array(
    ctx: &Ctx,
    addr: u64,
    grid: &[u64],
    filtered: bool,
    full: u64,
    map: &mut HashMap<Vec<u64>, ChunkLoc>,
) -> Result<()> {
    let hlen = 4 + 1 + 1 + 1 + 1 + ctx.sl + ctx.so + 4;
    let raw = ctx.read(addr, hlen)?;
    verify_checksum(&raw, "fixed array header")?;
    let mut c = Cur::new(&raw);
    c.signature(b"FAHD")?;
    ensure!(c.u8()? == 0, "Unsupported fixed array version");
    let client = c.u8()?;
    let entry = usize::from(c.u8()?);
    let page_bits = c.u8()?;
    let n = c.uint(ctx.sl)?;
    let Some(dblock) = c.addr(ctx.so)? else {
        return Ok(());
    };
    ensure!(
        client == u8::from(filtered) && n == product(grid)? && n <= 16_000_000,
        "HDF5 fixed array does not match chunk grid"
    );
    let size_len = if filtered {
        ensure!(entry > ctx.so + 4, "Invalid filtered fixed array entry");
        entry - ctx.so - 4
    } else {
        ensure!(entry == ctx.so, "Invalid fixed array entry");
        0
    };
    let page = 1u64 << page_bits.min(32);
    let prefix = 4 + 1 + 1 + ctx.so;
    let mut elements: Vec<u8> = Vec::new();
    if n > page {
        let npages = n.div_ceil(page);
        let bitmap = npages.div_ceil(8) as usize;
        let head = ctx.read(dblock, prefix + bitmap + 4)?;
        verify_checksum(&head, "fixed array data block")?;
        ensure!(head.starts_with(b"FADB"), "Missing FADB signature");
        let bits = head[prefix..prefix + bitmap].to_vec();
        let mut at = dblock + (prefix + bitmap + 4) as u64;
        for p in 0..npages {
            let count = page.min(n - p * page) as usize;
            let len = count * entry;
            if bits[(p / 8) as usize] & (0x80 >> (p % 8)) != 0 {
                let raw = ctx.read(at, len + 4)?;
                verify_checksum(&raw, "fixed array page")?;
                elements.extend_from_slice(&raw[..len]);
            } else {
                elements.extend(std::iter::repeat_n(0xff, len));
            }
            at += (page as usize * entry + 4) as u64;
        }
    } else {
        let len = prefix + n as usize * entry + 4;
        let raw = ctx.read(dblock, len)?;
        verify_checksum(&raw, "fixed array data block")?;
        ensure!(raw.starts_with(b"FADB"), "Missing FADB signature");
        elements.extend_from_slice(&raw[prefix..len - 4]);
    }
    for (i, key) in grid_keys(grid).enumerate() {
        let mut c = Cur::new(&elements[i * entry..(i + 1) * entry]);
        let Some(a) = c.addr(ctx.so)? else { continue };
        let (size, mask) = if filtered {
            (c.uint(size_len)?, c.u32()?)
        } else {
            (full, 0)
        };
        map.insert(
            key,
            ChunkLoc {
                addr: a,
                size,
                mask,
            },
        );
    }
    Ok(())
}

fn apply_filter(f: &Filter, data: Vec<u8>, full: u64) -> Result<Vec<u8>> {
    match f.id {
        1 => {
            let mut out = Vec::with_capacity(full as usize);
            flate2::read::ZlibDecoder::new(&data[..])
                .take(full + 1)
                .read_to_end(&mut out)
                .context("HDF5 deflate chunk is corrupt")?;
            ensure!(
                out.len() as u64 <= full,
                "HDF5 deflate chunk exceeds chunk size"
            );
            Ok(out)
        }
        2 => {
            let es = if f.has_client {
                f.client[0] as usize
            } else {
                1
            };
            if es <= 1 || data.len() < es {
                return Ok(data);
            }
            let n = data.len() / es;
            let mut out = vec![0u8; data.len()];
            for b in 0..es {
                for i in 0..n {
                    out[i * es + b] = data[b * n + i];
                }
            }
            let tail = n * es;
            out[tail..].copy_from_slice(&data[tail..]);
            Ok(out)
        }
        3 => {
            ensure!(data.len() >= 4, "Truncated Fletcher-32 chunk");
            let (body, sum) = data.split_at(data.len() - 4);
            let stored = u32::from_le_bytes(sum.try_into()?);
            let computed = super::io::fletcher32(body);
            ensure!(
                stored == computed || stored == computed.swap_bytes(),
                "HDF5 Fletcher-32 checksum mismatch"
            );
            Ok(body.to_vec())
        }
        id => bail!("HDF5 filter {id} is not supported"),
    }
}
