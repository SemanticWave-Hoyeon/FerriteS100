//! High-level ISO 8211 file parser

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{DDR, DR, Leader, Iso8211Error, Result};

/// ISO 8211 file parser
pub struct Iso8211Parser {
    data: Vec<u8>,
    position: usize,
}

impl Iso8211Parser {
    /// Create parser from file path
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        Ok(Iso8211Parser { data, position: 0 })
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
