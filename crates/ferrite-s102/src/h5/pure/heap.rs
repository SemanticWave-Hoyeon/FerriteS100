//! Local heaps (old group names), global heap collections (variable-length
//! data) and fractal heaps (dense links and attributes).
use super::io::{le, limit_enc_size, log2_floor, verify_checksum, Cur};
use super::Ctx;
use anyhow::{bail, ensure, Context, Result};
use std::{collections::HashMap, sync::Arc};

/// Old-style group local heap: only the data segment is needed.
pub struct LocalHeap {
    data: Vec<u8>,
}
impl LocalHeap {
    pub fn read(ctx: &Ctx, addr: u64) -> Result<Self> {
        let head = ctx.read(addr, 8 + 2 * ctx.sl + ctx.so)?;
        let mut c = Cur::new(&head);
        c.signature(b"HEAP")?;
        ensure!(c.u8()? == 0, "Unsupported HDF5 local heap version");
        c.skip(3)?;
        let size = c.uint(ctx.sl)?;
        c.uint(ctx.sl)?;
        let data_addr = c.addr(ctx.so)?.context("Local heap without data segment")?;
        ensure!(size <= 64 * 1024 * 1024, "HDF5 local heap exceeds limit");
        Ok(Self {
            data: ctx.read(data_addr, size as usize)?,
        })
    }
    pub fn name(&self, offset: u64) -> Result<String> {
        let rest = self
            .data
            .get(offset as usize..)
            .context("HDF5 local heap name offset out of range")?;
        let n = rest
            .iter()
            .position(|b| *b == 0)
            .context("Unterminated HDF5 local heap name")?;
        Ok(String::from_utf8(rest[..n].to_vec())?)
    }
}

/// One global heap collection: object index -> byte range.
pub struct GlobalHeap {
    data: Vec<u8>,
    objects: HashMap<u16, (usize, usize)>,
}
impl GlobalHeap {
    pub fn read(ctx: &Ctx, addr: u64) -> Result<Self> {
        let head = ctx.read(addr, 8 + ctx.sl)?;
        let mut c = Cur::new(&head);
        c.signature(b"GCOL")?;
        ensure!(c.u8()? == 1, "Unsupported HDF5 global heap version");
        c.skip(3)?;
        let size = c.uint(ctx.sl)?;
        ensure!(
            size >= (8 + ctx.sl) as u64 && size <= 256 * 1024 * 1024,
            "Invalid HDF5 global heap collection size"
        );
        let data = ctx.read(addr, size as usize)?;
        let mut objects = HashMap::new();
        let mut p = 8 + ctx.sl;
        while p + 8 + ctx.sl <= data.len() {
            let mut c = Cur::new(&data[p..]);
            let index = c.u16()?;
            c.skip(6)?;
            let len = c.uint(ctx.sl)? as usize;
            if index == 0 {
                break;
            }
            let start = p + 8 + ctx.sl;
            let end = start.checked_add(len).context("Global heap overflow")?;
            ensure!(
                end <= data.len(),
                "HDF5 global heap object exceeds collection"
            );
            objects.insert(index, (start, len));
            p = start + len.div_ceil(8) * 8;
        }
        Ok(Self { data, objects })
    }
    pub fn object(&self, index: u32) -> Result<&[u8]> {
        let index = u16::try_from(index).context("Global heap index out of range")?;
        let (start, len) = *self
            .objects
            .get(&index)
            .with_context(|| format!("Missing HDF5 global heap object {index}"))?;
        Ok(&self.data[start..start + len])
    }
}

/// Fractal heap reader for managed and tiny objects (no I/O filters, no huge objects).
pub struct FractalHeap {
    pub id_len: usize,
    max_heap_bits: u16,
    table_width: u64,
    start_block: u64,
    max_direct: u64,
    root: Option<u64>,
    root_rows: u64,
    checksummed_direct: bool,
    max_managed: u64,
}
impl FractalHeap {
    pub fn read(ctx: &Ctx, addr: u64) -> Result<Self> {
        let so = ctx.so;
        let sl = ctx.sl;
        let fixed =
            4 + 1 + 2 + 2 + 1 + 4 + sl + so + sl + so + 8 * sl + 2 + sl + sl + 2 + 2 + so + 2;
        let head = ctx.read(addr, fixed)?;
        let mut c = Cur::new(&head);
        c.signature(b"FRHP")?;
        ensure!(c.u8()? == 0, "Unsupported HDF5 fractal heap version");
        let id_len = usize::from(c.u16()?);
        let filter_len = c.u16()?;
        ensure!(
            filter_len == 0,
            "Filtered HDF5 fractal heaps are not supported"
        );
        let flags = c.u8()?;
        let max_managed = u64::from(c.u32()?);
        c.uint(sl)?; // next huge id
        c.addr(so)?; // huge btree
        c.uint(sl)?;
        c.addr(so)?;
        for _ in 0..8 {
            c.uint(sl)?;
        }
        let table_width = u64::from(c.u16()?);
        let start_block = c.uint(sl)?;
        let max_direct = c.uint(sl)?;
        let max_heap_bits = c.u16()?;
        c.u16()?; // starting rows
        let root = c.addr(so)?;
        let root_rows = u64::from(c.u16()?);
        let all = ctx.read(addr, fixed + 4)?;
        verify_checksum(&all, "fractal heap header")?;
        ensure!(
            table_width.is_power_of_two()
                && start_block.is_power_of_two()
                && max_direct.is_power_of_two()
                && max_direct >= start_block
                && (1..=64).contains(&max_heap_bits),
            "Invalid HDF5 fractal heap doubling table"
        );
        Ok(Self {
            id_len,
            max_heap_bits,
            table_width,
            start_block,
            max_direct,
            root,
            root_rows,
            checksummed_direct: flags & 0x02 != 0,
            max_managed,
        })
    }
    fn offset_size(&self) -> usize {
        usize::from(self.max_heap_bits).div_ceil(8)
    }
    fn length_size(&self) -> usize {
        limit_enc_size(self.max_direct.min(self.max_managed))
    }
    fn row_block(&self, row: u64) -> u64 {
        if row == 0 {
            self.start_block
        } else {
            self.start_block << (row - 1)
        }
    }
    fn max_direct_rows(&self) -> u64 {
        u64::from(log2_floor(self.max_direct) - log2_floor(self.start_block)) + 2
    }
    /// Object bytes for one heap ID.
    pub fn object(&self, ctx: &Ctx, id: &[u8]) -> Result<Vec<u8>> {
        ensure!(!id.is_empty(), "Empty HDF5 heap ID");
        let kind = (id[0] >> 4) & 0x03;
        ensure!(id[0] >> 6 == 0, "Unsupported HDF5 heap ID version");
        match kind {
            0 => {
                let os = self.offset_size();
                let ls = self.length_size();
                ensure!(id.len() >= 1 + os + ls, "Short HDF5 managed heap ID");
                let offset = le(&id[1..1 + os]);
                let len = le(&id[1 + os..1 + os + ls]);
                ensure!(
                    len <= self.max_managed,
                    "HDF5 heap object exceeds managed limit"
                );
                self.managed(ctx, offset, len)
            }
            2 => {
                let len = usize::from(id[0] & 0x0f) + 1;
                ensure!(id.len() > len, "Short HDF5 tiny heap ID");
                Ok(id[1..1 + len].to_vec())
            }
            _ => bail!("Huge HDF5 fractal heap objects are not supported"),
        }
    }
    fn managed(&self, ctx: &Ctx, offset: u64, len: u64) -> Result<Vec<u8>> {
        let root = self.root.context("Empty HDF5 fractal heap")?;
        if self.root_rows == 0 {
            return self.direct(ctx, root, 0, self.start_block, offset, len);
        }
        let mut block = root;
        let mut block_offset = 0u64;
        let mut rows = self.root_rows;
        for _ in 0..64 {
            let (child, child_offset, size, direct) =
                self.indirect_child(ctx, block, block_offset, rows, offset)?;
            if direct {
                return self.direct(ctx, child, child_offset, size, offset, len);
            }
            // An indirect child covering `size` bytes has rows r with
            // start*width*2^(r-1) == size.
            let per_row0 = self.start_block * self.table_width;
            ensure!(size >= per_row0, "Invalid HDF5 indirect block size");
            rows = u64::from(log2_floor(size / per_row0)) + 1;
            block = child;
            block_offset = child_offset;
        }
        bail!("HDF5 fractal heap too deep")
    }
    fn indirect_child(
        &self,
        ctx: &Ctx,
        addr: u64,
        block_offset: u64,
        rows: u64,
        target: u64,
    ) -> Result<(u64, u64, u64, bool)> {
        let os = self.offset_size();
        let direct_rows = rows.min(self.max_direct_rows());
        let indirect_rows = rows - direct_rows;
        let entries = direct_rows * self.table_width + indirect_rows * self.table_width;
        ensure!(entries <= 1 << 20, "HDF5 indirect block too large");
        let size = 5 + ctx.so + os + entries as usize * ctx.so + 4;
        let raw = ctx.read(addr, size)?;
        verify_checksum(&raw, "fractal heap indirect block")?;
        let mut c = Cur::new(&raw);
        c.signature(b"FHIB")?;
        ensure!(
            c.u8()? == 0,
            "Unsupported fractal heap indirect block version"
        );
        c.addr(ctx.so)?;
        ensure!(
            c.uint(os)? == block_offset,
            "Fractal heap block offset mismatch"
        );
        let mut row_start = block_offset;
        for row in 0..rows {
            let bsize = self.row_block(row);
            let span = bsize * self.table_width;
            if target < row_start + span {
                let col = (target - row_start) / bsize;
                let index = row * self.table_width + col;
                c.p = 5 + ctx.so + os + index as usize * ctx.so;
                let child = c.addr(ctx.so)?.context("Unallocated HDF5 heap block")?;
                return Ok((
                    child,
                    row_start + col * bsize,
                    bsize,
                    row < self.max_direct_rows(),
                ));
            }
            row_start += span;
        }
        bail!("HDF5 heap offset outside indirect block")
    }
    fn direct(
        &self,
        ctx: &Ctx,
        addr: u64,
        block_offset: u64,
        size: u64,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        let os = self.offset_size();
        let header = 5 + ctx.so + os + if self.checksummed_direct { 4 } else { 0 };
        let rel = offset - block_offset;
        ensure!(
            rel >= header as u64 && rel + len <= size,
            "HDF5 heap object outside direct block"
        );
        if self.checksummed_direct {
            let raw = ctx.read(addr, size as usize)?;
            let mut c = Cur::new(&raw);
            c.signature(b"FHDB")?;
            let stored_at = header - 4;
            let stored = u32::from_le_bytes(raw[stored_at..stored_at + 4].try_into()?);
            let mut copy = raw.clone();
            copy[stored_at..stored_at + 4].fill(0);
            ensure!(
                super::io::lookup3(&copy, 0) == stored,
                "HDF5 fractal heap direct block checksum mismatch"
            );
            return Ok(raw[rel as usize..(rel + len) as usize].to_vec());
        }
        let head = ctx.read(addr, 4)?;
        ensure!(&head[..] == b"FHDB", "Expected HDF5 FHDB block");
        ctx.read(addr + rel, len as usize)
    }
}

/// Cache of global heap collections, keyed by collection address.
#[derive(Default)]
pub struct HeapCache {
    map: std::sync::Mutex<HashMap<u64, Arc<GlobalHeap>>>,
}
impl HeapCache {
    pub fn get(&self, ctx: &Ctx, addr: u64) -> Result<Arc<GlobalHeap>> {
        if let Some(h) = self.map.lock().unwrap().get(&addr) {
            return Ok(h.clone());
        }
        let heap = Arc::new(GlobalHeap::read(ctx, addr)?);
        let mut map = self.map.lock().unwrap();
        if map.len() > 4096 {
            map.clear();
        }
        map.insert(addr, heap.clone());
        Ok(heap)
    }
}
