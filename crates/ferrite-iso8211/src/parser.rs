//! High-level ISO 8211 file parser
//!
//! Supports two parsing modes:
//! 1. `Iso8211Parser` - Traditional in-memory parsing (Vec<u8>)
//! 2. `MmapIso8211Parser` - Explicit unsafe mapping for caller-guaranteed immutable files

use std::fs::File;
use std::io::Read;
use std::path::Path;

use memmap2::Mmap;

use crate::{Iso8211Error, Leader, Result, DDR, DR};

/// ISO 8211 file parser (traditional in-memory)
pub struct Iso8211Parser {
    data: Vec<u8>,
    position: usize,
}

impl Iso8211Parser {
    /// Capture externally mutable input into bounded owned bytes.
    /// This is not an atomic read or an authentication guarantee.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_file_bounded(path, 512 * 1024 * 1024)
    }

    /// Capture at most `max_bytes`; probe one extra byte on the stack, never
    /// append it or trust metadata alone. Uses the current S-101 512 MiB default.
    pub fn from_file_bounded<P: AsRef<Path>>(path: P, max_bytes: usize) -> Result<Self> {
        let file = File::open(path)?;
        let hint = file.metadata()?.len();
        if hint > max_bytes as u64 {
            return Err(Iso8211Error::Parse(
                "ISO 8211 input byte budget exceeded".into(),
            ));
        }
        Self::capture_bounded(file, max_bytes, hint as usize)
    }

    fn capture_bounded(mut file: impl Read, max_bytes: usize, hint: usize) -> Result<Self> {
        let mut data = Vec::new();
        data.try_reserve_exact(hint)
            .map_err(|_| Iso8211Error::Parse("ISO 8211 owned input allocation failed".into()))?;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let remaining = max_bytes - data.len();
            if remaining == 0 {
                let mut extra = [0u8; 1];
                if file.read(&mut extra)? != 0 {
                    return Err(Iso8211Error::Parse(
                        "ISO 8211 input byte budget exceeded".into(),
                    ));
                }
                break;
            }
            let count = file.read(&mut chunk[..remaining.min(64 * 1024)])?;
            if count == 0 {
                break;
            }
            data.try_reserve_exact(count).map_err(|_| {
                Iso8211Error::Parse("ISO 8211 owned input allocation failed".into())
            })?;
            data.extend_from_slice(&chunk[..count]);
        }
        Ok(Self { data, position: 0 })
    }

    /// Create parser from bytes
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Iso8211Parser { data, position: 0 }
    }

    /// Get remaining data
    pub fn remaining(&self) -> &[u8] {
        &self.data[self.position..]
    }

    /// Check if at end of data
    pub fn is_eof(&self) -> bool {
        self.position >= self.data.len()
    }

    /// Read DDR (Data Descriptive Record)
    /// This should be the first record in the file
    pub fn read_ddr(&mut self) -> Result<DDR> {
        if self.position != 0 {
            tracing::warn!("Reading DDR when position is not at start");
        }

        let remaining = self.remaining();
        if remaining.len() < Leader::SIZE {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let leader = Leader::parse(remaining)?;
        let record_len = leader.record_length as usize;

        if remaining.len() < record_len {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let ddr = DDR::parse(&remaining[..record_len])?;
        self.position += record_len;

        tracing::debug!(
            "DDR parsed: {} field definitions",
            ddr.field_definitions.len()
        );

        Ok(ddr)
    }

    /// Read next DR (Data Record)
    pub fn read_dr(&mut self) -> Result<Option<DR>> {
        let remaining = self.remaining();

        if remaining.is_empty() {
            return Ok(None);
        }

        if remaining.len() < Leader::SIZE {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let leader = Leader::parse(remaining)?;
        let record_len = leader.record_length as usize;

        if remaining.len() < record_len {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let dr = DR::parse(&remaining[..record_len])?;
        self.position += record_len;

        Ok(Some(dr))
    }

    /// Read all records from file
    pub fn read_all(&mut self) -> Result<(DDR, Vec<DR>)> {
        let ddr = self.read_ddr()?;
        let mut records = Vec::new();

        while let Some(dr) = self.read_dr()? {
            records.push(dr);
        }

        tracing::info!("Parsed {} data records", records.len());

        Ok((ddr, records))
    }

    /// Get current position
    pub fn position(&self) -> usize {
        self.position
    }

    /// Get total data length
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if data is empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Memory-mapped ISO 8211 file parser (zero-copy, 3-10x faster for large files)
///
/// Uses OS-level memory mapping for zero-copy file access:
/// - No upfront file read: file pages loaded on-demand via page faults
/// - Memory efficient: only accessed pages are in RAM
/// - Fast: eliminates memcpy from kernel to userspace
///
/// Ideal for:
/// - Large chart files (>1 MB)
/// - Loading multiple charts simultaneously
/// - Memory-constrained environments
pub struct MmapIso8211Parser {
    /// Memory-mapped file (must outlive any references to data)
    _mmap: Mmap,
    /// Pointer to mapped data (valid as long as _mmap exists)
    data: *const u8,
    data_len: usize,
    position: usize,
}

// Safety: MmapIso8211Parser is Send because:
// - Mmap is Send (OS guarantees thread-safe memory mapping)
// - We only read from the mapped memory, never write
// - The data pointer is derived from the Mmap and valid for its lifetime
// - The unsafe constructor requires external file immutability for that lifetime
unsafe impl Send for MmapIso8211Parser {}

// Safety: MmapIso8211Parser is Sync because:
// - All access is read-only
// - Multiple readers can safely access mmap concurrently
// - The unsafe constructor requires external file immutability for that lifetime
unsafe impl Sync for MmapIso8211Parser {}

impl MmapIso8211Parser {
    /// Create parser from file path using memory mapping (zero-copy)
    ///
    /// # Performance
    /// - No upfront file read: instant "load" time
    /// - Pages loaded on-demand as data is accessed
    /// - 3-10x faster than `Iso8211Parser::from_file()` for large files
    ///
    /// # Safety
    /// Caller must guarantee that the mapped file is not modified, truncated,
    /// or otherwise invalidated by any thread/process for the entire parser
    /// lifetime (including every borrowed slice). An open file descriptor alone
    /// does not provide that guarantee. Use `Iso8211Parser::from_file` for untrusted input.
    pub unsafe fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path)?;

        // Safety: upheld by this unsafe constructor's caller contract;
        // read-only access and an open descriptor alone are insufficient.
        let mmap = unsafe { Mmap::map(&file)? };

        let data = mmap.as_ptr();
        let data_len = mmap.len();

        Ok(MmapIso8211Parser {
            _mmap: mmap,
            data,
            data_len,
            position: 0,
        })
    }

    /// Get the underlying data slice
    #[inline]
    fn data(&self) -> &[u8] {
        // Safety: data pointer is valid for the lifetime of _mmap
        // and we ensure _mmap outlives all borrows
        unsafe { std::slice::from_raw_parts(self.data, self.data_len) }
    }

    /// Get remaining data
    #[inline]
    pub fn remaining(&self) -> &[u8] {
        &self.data()[self.position..]
    }

    /// Check if at end of data
    #[inline]
    pub fn is_eof(&self) -> bool {
        self.position >= self.data_len
    }

    /// Read DDR (Data Descriptive Record)
    pub fn read_ddr(&mut self) -> Result<DDR> {
        if self.position != 0 {
            tracing::warn!("Reading DDR when position is not at start");
        }

        let remaining = self.remaining();
        if remaining.len() < Leader::SIZE {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let leader = Leader::parse(remaining)?;
        let record_len = leader.record_length as usize;

        if remaining.len() < record_len {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let ddr = DDR::parse(&remaining[..record_len])?;
        self.position += record_len;

        tracing::debug!(
            "DDR parsed (mmap): {} field definitions",
            ddr.field_definitions.len()
        );

        Ok(ddr)
    }

    /// Read next DR (Data Record)
    pub fn read_dr(&mut self) -> Result<Option<DR>> {
        let remaining = self.remaining();

        if remaining.is_empty() {
            return Ok(None);
        }

        if remaining.len() < Leader::SIZE {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let leader = Leader::parse(remaining)?;
        let record_len = leader.record_length as usize;

        if remaining.len() < record_len {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let dr = DR::parse(&remaining[..record_len])?;
        self.position += record_len;

        Ok(Some(dr))
    }

    /// Read all records from file
    pub fn read_all(&mut self) -> Result<(DDR, Vec<DR>)> {
        let ddr = self.read_ddr()?;
        let mut records = Vec::new();

        while let Some(dr) = self.read_dr()? {
            records.push(dr);
        }

        tracing::info!("Parsed {} data records (mmap)", records.len());

        Ok((ddr, records))
    }

    /// Get current position
    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Get total data length
    #[inline]
    pub fn len(&self) -> usize {
        self.data_len
    }

    /// Check if data is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data_len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_creation() {
        let parser = Iso8211Parser::from_bytes(vec![]);
        assert!(parser.is_empty());
        assert!(parser.is_eof());
    }
}

#[cfg(test)]
mod owned_input_controls {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "ferrite-owned-{}-{}.bin",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    #[test]
    fn overwrite_and_truncate_after_construction_preserve_owned_bytes() {
        let path = path();
        let original = vec![b'x'; 128];
        std::fs::write(&path, &original).unwrap();
        let parser = Iso8211Parser::from_file(&path).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(parser.remaining(), original.as_slice());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn exact_limit_and_one_extra_byte_admission() {
        let path = path();
        std::fs::write(&path, [1u8; 8]).unwrap();
        assert_eq!(Iso8211Parser::from_file_bounded(&path, 8).unwrap().len(), 8);
        assert!(Iso8211Parser::from_file_bounded(&path, 7).is_err());
        std::fs::write(&path, []).unwrap();
        assert!(Iso8211Parser::from_file_bounded(&path, 0)
            .unwrap()
            .is_empty());
        std::fs::write(&path, [1]).unwrap();
        assert!(Iso8211Parser::from_file_bounded(&path, 0).is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn stale_metadata_hint_cannot_allow_read_beyond_limit() {
        let bytes = std::io::Cursor::new(vec![9u8; 9]);
        assert!(Iso8211Parser::capture_bounded(bytes, 8, 0).is_err());
        let bytes = std::io::Cursor::new(vec![9u8; 8]);
        assert_eq!(
            Iso8211Parser::capture_bounded(bytes, 8, 0)
                .unwrap()
                .remaining(),
            &[9u8; 8]
        );
    }
}
