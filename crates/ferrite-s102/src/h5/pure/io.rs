//! Byte access, little-endian field cursor and the two HDF5 checksums.
use anyhow::{bail, ensure, Context, Result};
use std::sync::Arc;

/// Random access to the bytes of one HDF5 file image.
pub trait ReadAt: Send + Sync {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// A complete in-memory file image (DataSource bytes, web uploads, tests).
pub struct MemoryImage(pub Arc<[u8]>);
impl ReadAt for MemoryImage {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset).context("HDF5 offset exceeds address space")?;
        let end = start
            .checked_add(buf.len())
            .context("HDF5 read range overflow")?;
        let src = self
            .0
            .get(start..end)
            .with_context(|| format!("HDF5 read {start}..{end} beyond file end"))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// A local file read with positioned reads (no shared seek cursor).
#[cfg(any(unix, windows))]
pub struct FileImage {
    file: std::fs::File,
    len: u64,
}
#[cfg(any(unix, windows))]
impl FileImage {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("Cannot open HDF5 file {}", path.display()))?;
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "HDF5 input is not a regular file");
        Ok(Self {
            file,
            len: meta.len(),
        })
    }
}
#[cfg(any(unix, windows))]
impl ReadAt for FileImage {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .context("HDF5 read range overflow")?;
        ensure!(end <= self.len, "HDF5 read {offset}..{end} beyond file end");
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, offset)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                let n = self
                    .file
                    .seek_read(&mut buf[done..], offset + done as u64)?;
                ensure!(n > 0, "HDF5 short read");
                done += n;
            }
        }
        Ok(())
    }
}

/// Field cursor over one decoded metadata block.
pub struct Cur<'a> {
    pub b: &'a [u8],
    pub p: usize,
}
impl<'a> Cur<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.p)
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).context("HDF5 field overflow")?;
        ensure!(end <= self.b.len(), "Truncated HDF5 metadata");
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into()?))
    }
    /// Unsigned little-endian integer of 1..=8 bytes.
    pub fn uint(&mut self, n: usize) -> Result<u64> {
        ensure!((1..=8).contains(&n), "Unsupported HDF5 integer width {n}");
        Ok(le(self.bytes(n)?))
    }
    /// Address of the superblock's offset width; `None` is the undefined address.
    pub fn addr(&mut self, width: usize) -> Result<Option<u64>> {
        let raw = self.bytes(width)?;
        if raw.iter().all(|b| *b == 0xff) {
            Ok(None)
        } else {
            Ok(Some(le(raw)))
        }
    }
    pub fn signature(&mut self, sig: &[u8; 4]) -> Result<()> {
        let got = self.bytes(4)?;
        if got != sig {
            bail!(
                "Expected HDF5 {} block, found {:?}",
                String::from_utf8_lossy(sig),
                String::from_utf8_lossy(got)
            );
        }
        Ok(())
    }
    /// NUL-terminated name starting here; the cursor moves past the NUL.
    pub fn cstr(&mut self) -> Result<&'a [u8]> {
        let rest = &self.b[self.p.min(self.b.len())..];
        let n = rest
            .iter()
            .position(|b| *b == 0)
            .context("Unterminated HDF5 name")?;
        let s = &rest[..n];
        self.p += n + 1;
        Ok(s)
    }
}

pub fn le(raw: &[u8]) -> u64 {
    raw.iter()
        .rev()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

/// Bytes needed to encode `value` (HDF5 H5VM_limit_enc_size).
pub fn limit_enc_size(value: u64) -> usize {
    (log2_floor(value) / 8 + 1) as usize
}
pub fn log2_floor(value: u64) -> u32 {
    if value == 0 {
        0
    } else {
        63 - value.leading_zeros()
    }
}

/// Bob Jenkins' lookup3 `hashlittle`, as used by H5_checksum_lookup3.
pub fn lookup3(data: &[u8], initval: u32) -> u32 {
    fn rot(x: u32, k: u32) -> u32 {
        x.rotate_left(k)
    }
    let mut a: u32 = 0xdeadbeef_u32
        .wrapping_add(data.len() as u32)
        .wrapping_add(initval);
    let mut b = a;
    let mut c = a;
    let mut k = data;
    while k.len() > 12 {
        a = a.wrapping_add(u32::from_le_bytes(k[0..4].try_into().unwrap()));
        b = b.wrapping_add(u32::from_le_bytes(k[4..8].try_into().unwrap()));
        c = c.wrapping_add(u32::from_le_bytes(k[8..12].try_into().unwrap()));
        a = a.wrapping_sub(c);
        a ^= rot(c, 4);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= rot(a, 6);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= rot(b, 8);
        b = b.wrapping_add(a);
        a = a.wrapping_sub(c);
        a ^= rot(c, 16);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= rot(a, 19);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= rot(b, 4);
        b = b.wrapping_add(a);
        k = &k[12..];
    }
    if k.is_empty() {
        return c;
    }
    let mut tail = [0u8; 12];
    tail[..k.len()].copy_from_slice(k);
    a = a.wrapping_add(u32::from_le_bytes(tail[0..4].try_into().unwrap()));
    b = b.wrapping_add(u32::from_le_bytes(tail[4..8].try_into().unwrap()));
    c = c.wrapping_add(u32::from_le_bytes(tail[8..12].try_into().unwrap()));
    c ^= b;
    c = c.wrapping_sub(rot(b, 14));
    a ^= c;
    a = a.wrapping_sub(rot(c, 11));
    b ^= a;
    b = b.wrapping_sub(rot(a, 25));
    c ^= b;
    c = c.wrapping_sub(rot(b, 16));
    a ^= c;
    a = a.wrapping_sub(rot(c, 4));
    b ^= a;
    b = b.wrapping_sub(rot(a, 14));
    c ^= b;
    c = c.wrapping_sub(rot(b, 24));
    c
}

/// Verify the trailing 4-byte lookup3 checksum of a metadata block.
pub fn verify_checksum(block: &[u8], what: &str) -> Result<()> {
    ensure!(block.len() >= 4, "Truncated HDF5 {what}");
    let (body, sum) = block.split_at(block.len() - 4);
    ensure!(
        lookup3(body, 0) == u32::from_le_bytes(sum.try_into()?),
        "HDF5 {what} checksum mismatch"
    );
    Ok(())
}

/// H5_checksum_fletcher32 over the filtered chunk payload.
pub fn fletcher32(data: &[u8]) -> u32 {
    let mut sum1: u32 = 0;
    let mut sum2: u32 = 0;
    let mut words = data.len() / 2;
    let mut p = 0;
    while words > 0 {
        let mut tlen = words.min(360);
        words -= tlen;
        while tlen > 0 {
            sum1 = sum1.wrapping_add((u32::from(data[p]) << 8) | u32::from(data[p + 1]));
            p += 2;
            sum2 = sum2.wrapping_add(sum1);
            tlen -= 1;
        }
        sum1 = (sum1 & 0xffff) + (sum1 >> 16);
        sum2 = (sum2 & 0xffff) + (sum2 >> 16);
    }
    if data.len() % 2 == 1 {
        sum1 = sum1.wrapping_add(u32::from(data[p]) << 8);
        sum2 = sum2.wrapping_add(sum1);
        sum1 = (sum1 & 0xffff) + (sum1 >> 16);
        sum2 = (sum2 & 0xffff) + (sum2 >> 16);
    }
    sum1 = (sum1 & 0xffff) + (sum1 >> 16);
    sum2 = (sum2 & 0xffff) + (sum2 >> 16);
    (sum2 << 16) | sum1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lookup3_reference_vectors() {
        // Values from Bob Jenkins' lookup3.c driver5.
        assert_eq!(lookup3(b"", 0), 0xdeadbeef);
        assert_eq!(lookup3(b"Four score and seven years ago", 0), 0x17770551);
        assert_eq!(lookup3(b"Four score and seven years ago", 1), 0xcd628161);
    }
    #[test]
    fn enc_size() {
        assert_eq!(limit_enc_size(1), 1);
        assert_eq!(limit_enc_size(255), 1);
        assert_eq!(limit_enc_size(256), 2);
        assert_eq!(limit_enc_size(0), 1);
    }
}
