//! Pure-Rust, read-only HDF5 reader limited to what S-102 products use.
//!
//! Supported: superblock v0-v3; object header v1/v2; old-style (symbol
//! table) and new-style (compact or dense) groups; compact and dense
//! attributes; integer, IEEE float, fixed and variable-length string,
//! compound, enumeration, array datatypes; compact, contiguous and chunked
//! layouts with v1 B-tree, single, implicit, fixed-array and v2 B-tree chunk
//! indexes; deflate, shuffle and Fletcher-32 filters. Anything else
//! (shared messages, committed datatypes, external/virtual storage, szip,
//! n-bit, scale-offset, extensible arrays, huge heap objects, filtered heaps,
//! soft/external link targets) is refused with an error.
mod btree;
mod data;
pub(crate) mod datatype;
mod heap;
pub mod io;
mod object;

use anyhow::{bail, ensure, Context, Result};
pub use datatype::{Dt, Member};
use io::{Cur, ReadAt};
use object::{Header, LinkTarget};
use std::sync::{Arc, OnceLock};

pub struct Ctx {
    img: Arc<dyn ReadAt>,
    base: u64,
    pub so: usize,
    pub sl: usize,
    heaps: heap::HeapCache,
}
impl Ctx {
    /// Bytes at a file-relative address.
    pub fn read(&self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let abs = self
            .base
            .checked_add(addr)
            .context("HDF5 address overflow")?;
        let mut buf = vec![0u8; len];
        self.img.read_at(abs, &mut buf)?;
        Ok(buf)
    }
    pub fn len_from(&self, addr: u64) -> Result<usize> {
        let abs = self
            .base
            .checked_add(addr)
            .context("HDF5 address overflow")?;
        ensure!(abs < self.img.len(), "HDF5 address beyond file end");
        Ok(usize::try_from(self.img.len() - abs).unwrap_or(usize::MAX))
    }
    pub fn eof(&self) -> u64 {
        self.img.len().saturating_sub(self.base)
    }
    /// Bytes of one variable-length element (`length, collection, index`).
    pub fn vlen(&self, raw: &[u8], unit: usize) -> Result<Vec<u8>> {
        let mut c = Cur::new(raw);
        let len = c.u32()? as usize;
        let coll = c.addr(self.so)?;
        let index = c.u32()?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let coll = coll.context("Variable-length data without heap collection")?;
        let heap = self.heaps.get(self, coll)?;
        let obj = heap.object(index)?;
        let bytes = len
            .checked_mul(unit)
            .context("Variable-length size overflow")?;
        ensure!(obj.len() >= bytes, "HDF5 variable-length object truncated");
        Ok(obj[..bytes].to_vec())
    }
    /// Declared byte length of a variable-length element, without the heap.
    pub fn vlen_len(&self, raw: &[u8]) -> Result<u64> {
        Ok(u64::from(Cur::new(raw).u32()?))
    }
}

#[derive(Clone)]
pub struct File {
    ctx: Arc<Ctx>,
    root: Group,
}
impl File {
    pub fn open(img: Arc<dyn ReadAt>) -> Result<Self> {
        let len = img.len();
        let mut base = None;
        let mut at = 0u64;
        while at + 8 <= len {
            let mut sig = [0u8; 8];
            img.read_at(at, &mut sig)?;
            if &sig == b"\x89HDF\r\n\x1a\n" {
                base = Some(at);
                break;
            }
            at = if at == 0 { 512 } else { at * 2 };
        }
        let base = base.context("Not an HDF5 file (no superblock signature)")?;
        let mut head = [0u8; 9];
        img.read_at(base, &mut head)?;
        let version = head[8];
        let (so, sl, root_addr);
        match version {
            0 | 1 => {
                let mut fixed = vec![0u8; 24];
                img.read_at(base, &mut fixed)?;
                so = usize::from(fixed[13]);
                sl = usize::from(fixed[14]);
                ensure!(
                    matches!(so, 2 | 4 | 8) && matches!(sl, 2 | 4 | 8),
                    "Unsupported HDF5 offset/length sizes"
                );
                let extra = if version == 1 { 4 } else { 0 };
                let total = 24 + extra + 4 * so + (2 * so + 24);
                let mut raw = vec![0u8; total];
                img.read_at(base, &mut raw)?;
                let mut c = Cur::new(&raw);
                c.skip(24 + extra + 4 * so)?;
                c.addr(so)?; // link name offset
                root_addr = c.addr(so)?.context("HDF5 root group address undefined")?;
            }
            2 | 3 => {
                let mut fixed = vec![0u8; 12];
                img.read_at(base, &mut fixed)?;
                so = usize::from(fixed[9]);
                sl = usize::from(fixed[10]);
                ensure!(
                    matches!(so, 2 | 4 | 8) && matches!(sl, 2 | 4 | 8),
                    "Unsupported HDF5 offset/length sizes"
                );
                let total = 12 + 4 * so + 4;
                let mut raw = vec![0u8; total];
                img.read_at(base, &mut raw)?;
                io::verify_checksum(&raw, "superblock")?;
                let mut c = Cur::new(&raw);
                c.skip(12)?;
                c.addr(so)?; // base address (signature offset is used)
                c.addr(so)?; // superblock extension (no feature this reader needs)
                c.addr(so)?; // end of file
                root_addr = c.addr(so)?.context("HDF5 root group address undefined")?;
            }
            _ => bail!("Unsupported HDF5 superblock version {version}"),
        }
        let ctx = Arc::new(Ctx {
            img,
            base,
            so,
            sl,
            heaps: Default::default(),
        });
        let root = Group::open(&ctx, root_addr, 0)?;
        Ok(Self { ctx, root })
    }
    pub fn root(&self) -> &Group {
        &self.root
    }
    pub fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
    }
}

#[derive(Clone)]
pub struct Group {
    ctx: Arc<Ctx>,
    header: Arc<Header>,
    links: Arc<Vec<(String, LinkTarget)>>,
    attrs: Arc<OnceLock<std::result::Result<Vec<Attribute>, String>>>,
    depth: usize,
}

fn is_group(h: &Header) -> bool {
    h.find(object::MSG_SYMBOL_TABLE).is_some()
        || h.find(object::MSG_LINK_INFO).is_some()
        || h.find(object::MSG_LINK).is_some()
}

impl Group {
    fn open(ctx: &Arc<Ctx>, addr: u64, depth: usize) -> Result<Self> {
        ensure!(depth < 64, "HDF5 group nesting too deep");
        let header = Header::read(ctx, addr)?;
        ensure!(
            is_group(&header) && header.find(object::MSG_LAYOUT).is_none(),
            "HDF5 object is not a group"
        );
        let links = read_links(ctx, &header)?;
        Ok(Self {
            ctx: ctx.clone(),
            header: Arc::new(header),
            links: Arc::new(links),
            attrs: Arc::new(OnceLock::new()),
            depth,
        })
    }
    pub fn member_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.links.iter().map(|(n, _)| n.clone()).collect();
        names.sort();
        names
    }
    fn resolve(&self, path: &str) -> Result<(u64, Group)> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        ensure!(!parts.is_empty(), "Empty HDF5 path");
        let mut group = self.clone();
        for (i, part) in parts.iter().enumerate() {
            let target = group
                .links
                .iter()
                .find(|(n, _)| n == part)
                .map(|(_, t)| t.clone())
                .with_context(|| format!("HDF5 object '{path}' does not exist"))?;
            let addr = match target {
                LinkTarget::Hard(a) => a,
                LinkTarget::Soft(to) => {
                    bail!("HDF5 soft links are not supported ('{path}' -> '{to}')")
                }
                LinkTarget::External => bail!("HDF5 external links are not supported ('{path}')"),
            };
            if i + 1 == parts.len() {
                return Ok((addr, group));
            }
            group = Group::open(&self.ctx, addr, group.depth + 1)?;
        }
        unreachable!()
    }
    pub fn link_exists(&self, path: &str) -> bool {
        self.resolve(path).is_ok()
    }
    pub fn group(&self, path: &str) -> Result<Group> {
        let (addr, parent) = self.resolve(path)?;
        Group::open(&self.ctx, addr, parent.depth + 1)
            .with_context(|| format!("Cannot open HDF5 group '{path}'"))
    }
    pub fn dataset(&self, path: &str) -> Result<Dataset> {
        let (addr, _) = self.resolve(path)?;
        Dataset::open(&self.ctx, addr).with_context(|| format!("Cannot open HDF5 dataset '{path}'"))
    }
    fn attributes(&self) -> Result<&Vec<Attribute>> {
        self.attrs
            .get_or_init(|| read_attributes(&self.ctx, &self.header).map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
    pub fn attr_names(&self) -> Result<Vec<String>> {
        Ok(self.attributes()?.iter().map(|a| a.name.clone()).collect())
    }
    pub fn attr(&self, name: &str) -> Result<Attribute> {
        self.attributes()?
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .with_context(|| format!("HDF5 attribute '{name}' does not exist"))
    }
}

fn read_links(ctx: &Arc<Ctx>, h: &Header) -> Result<Vec<(String, LinkTarget)>> {
    let mut out = Vec::new();
    if let Some(m) = h.find(object::MSG_SYMBOL_TABLE) {
        let mut c = Cur::new(&m.data);
        let btree_addr = c.addr(ctx.so)?.context("Undefined group B-tree")?;
        let heap_addr = c.addr(ctx.so)?.context("Undefined group heap")?;
        let heap = heap::LocalHeap::read(ctx, heap_addr)?;
        btree::walk_v1(ctx, btree_addr, 0, ctx.sl, &mut |_, snod| {
            let head = ctx.read(snod, 8)?;
            let mut c = Cur::new(&head);
            c.signature(b"SNOD")?;
            ensure!(c.u8()? == 1, "Unsupported HDF5 symbol node version");
            c.skip(1)?;
            let n = usize::from(c.u16()?);
            let entry = 2 * ctx.so + 24;
            let raw = ctx.read(snod + 8, n * entry)?;
            let mut c = Cur::new(&raw);
            for _ in 0..n {
                let name_off = c.uint(ctx.so)?;
                let addr = c.addr(ctx.so)?.context("Undefined symbol address")?;
                c.skip(24)?;
                out.push((heap.name(name_off)?, LinkTarget::Hard(addr)));
                ensure!(out.len() <= 1_000_000, "HDF5 group exceeds link limit");
            }
            Ok(())
        })?;
        return Ok(out);
    }
    for m in h.all(object::MSG_LINK) {
        out.push(object::parse_link(&m.data, ctx.so)?);
    }
    if let Some(m) = h.find(object::MSG_LINK_INFO) {
        let mut c = Cur::new(&m.data);
        ensure!(c.u8()? == 0, "Unsupported HDF5 link info version");
        let flags = c.u8()?;
        if flags & 1 != 0 {
            c.skip(8)?;
        }
        if let Some(fheap) = c.addr(ctx.so)? {
            let name_index = c.addr(ctx.so)?.context("Dense links without name index")?;
            let fh = heap::FractalHeap::read(ctx, fheap)?;
            let (kind, records) = btree::records_v2(ctx, name_index)?;
            btree::expect_kind(kind, &[5], "link name index")?;
            for r in records {
                ensure!(r.len() > 4, "Short link name record");
                let obj = fh.object(ctx, &r[4..])?;
                out.push(object::parse_link(&obj, ctx.so)?);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for (n, _) in &out {
        ensure!(seen.insert(n.clone()), "Duplicate HDF5 link name {n}");
    }
    Ok(out)
}

fn read_attributes(ctx: &Arc<Ctx>, h: &Header) -> Result<Vec<Attribute>> {
    let mut out = Vec::new();
    for m in h.all(object::MSG_ATTRIBUTE) {
        out.push(Attribute::new(
            ctx,
            object::parse_attribute(&m.data, ctx.sl)?,
        )?);
    }
    if let Some(m) = h.find(object::MSG_ATTRIBUTE_INFO) {
        let mut c = Cur::new(&m.data);
        ensure!(c.u8()? == 0, "Unsupported HDF5 attribute info version");
        let flags = c.u8()?;
        if flags & 1 != 0 {
            c.skip(2)?;
        }
        if let Some(fheap) = c.addr(ctx.so)? {
            let name_index = c
                .addr(ctx.so)?
                .context("Dense attributes without name index")?;
            let fh = heap::FractalHeap::read(ctx, fheap)?;
            let (kind, records) = btree::records_v2(ctx, name_index)?;
            btree::expect_kind(kind, &[8], "attribute name index")?;
            for r in records {
                ensure!(r.len() == fh.id_len + 9, "Unexpected attribute record size");
                let obj = fh.object(ctx, &r[..fh.id_len])?;
                out.push(Attribute::new(ctx, object::parse_attribute(&obj, ctx.sl)?)?);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for a in &out {
        ensure!(
            seen.insert(a.name.clone()),
            "Duplicate HDF5 attribute {}",
            a.name
        );
    }
    Ok(out)
}

/// Element bytes plus their datatype, ready for typed decoding.
pub struct Elements {
    pub dt: Dt,
    pub esize: usize,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct Attribute {
    ctx: Arc<Ctx>,
    pub name: String,
    pub dt: Dt,
    pub shape: Option<Vec<u64>>,
    data: Arc<Vec<u8>>,
}
impl Attribute {
    fn new(ctx: &Arc<Ctx>, m: object::AttributeMessage) -> Result<Self> {
        let n = m.space.count()?;
        let esize = m.dt.size(ctx.so);
        let need = n
            .checked_mul(esize as u64)
            .context("Attribute size overflow")?;
        ensure!(
            m.data.len() as u64 >= need,
            "HDF5 attribute '{}' data truncated",
            m.name
        );
        Ok(Self {
            ctx: ctx.clone(),
            name: m.name,
            dt: m.dt,
            shape: m.space.dims,
            data: Arc::new(m.data[..need as usize].to_vec()),
        })
    }
    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }
    pub fn elements(&self) -> Elements {
        Elements {
            esize: self.dt.size(self.ctx.so),
            dt: self.dt.clone(),
            data: self.data.to_vec(),
        }
    }
}

#[derive(Clone)]
pub struct Dataset {
    ctx: Arc<Ctx>,
    pub dt: Dt,
    pub shape: Option<Vec<u64>>,
    storage: Arc<data::Storage>,
}
impl std::fmt::Debug for Dataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PureDataset")
            .field("shape", &self.shape)
            .field("storage", &self.storage)
            .finish()
    }
}
impl Dataset {
    fn open(ctx: &Arc<Ctx>, addr: u64) -> Result<Self> {
        let h = Header::read(ctx, addr)?;
        let dt_msg = h
            .find(object::MSG_DATATYPE)
            .context("Dataset without datatype")?;
        ensure!(
            h.find(object::MSG_LAYOUT).is_some(),
            "HDF5 object is not a dataset"
        );
        let dt = datatype::parse(&mut Cur::new(&dt_msg.data), 0)?;
        let space = datatype::parse_space(
            &mut Cur::new(
                &h.find(object::MSG_DATASPACE)
                    .context("Dataset without dataspace")?
                    .data,
            ),
            ctx.sl,
        )?;
        let esize = dt.size(ctx.so);
        ensure!(esize > 0, "Zero-size HDF5 datatype");
        let fill = object::parse_fill(&h)?;
        let dims = space.dims.clone().unwrap_or_default();
        let storage = data::Storage::new(ctx, &h, dims, esize, fill)?;
        Ok(Self {
            ctx: ctx.clone(),
            dt,
            shape: space.dims,
            storage: Arc::new(storage),
        })
    }
    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }
    /// Raw elements of the hyperslab (whole dataset when `start` is None).
    pub fn elements(&self, sel: Option<(&[u64], &[u64])>) -> Result<Elements> {
        let esize = self.dt.size(self.ctx.so);
        let data = match (&self.shape, sel) {
            (None, _) => Vec::new(),
            (Some(dims), None) => {
                let zeros = vec![0; dims.len()];
                self.storage.read(&self.ctx, &zeros, dims)?
            }
            (Some(_), Some((start, count))) => self.storage.read(&self.ctx, start, count)?,
        };
        Ok(Elements {
            dt: self.dt.clone(),
            esize,
            data,
        })
    }
}
