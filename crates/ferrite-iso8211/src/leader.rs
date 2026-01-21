//! ISO 8211 Record Leader parsing
//!
//! The leader is a fixed 24-byte header at the start of each record.

use crate::{Iso8211Error, Result};

/// ISO 8211 Record Leader (24 bytes fixed)
#[derive(Debug, Clone)]
pub struct Leader {
    /// Total record length including leader
    pub record_length: u32,
    /// Interchange level (always '3' for S-100)
    pub interchange_level: u8,
    /// Leader identifier ('L' for DDR, 'D' for DR)
    pub leader_identifier: char,
    /// In-line code extension indicator
    pub inline_code_ext: char,
    /// Version number
    pub version_number: char,
    /// Application indicator
    pub application_indicator: char,
    /// Field control length (DDR only)
    pub field_control_length: u8,
    /// Base address of field area
    pub base_address: u32,
    /// Extended character set indicator
    pub extended_charset: [char; 3],
    /// Size of field length field in directory
    pub size_field_length: u8,
    /// Size of field position field in directory
    pub size_field_position: u8,
    /// Reserved (always '0')
    pub reserved: char,
    /// Size of field tag in directory
    pub size_field_tag: u8,
}

impl Leader {
    /// Leader size is always 24 bytes
    pub const SIZE: usize = 24;

    /// Parse leader from bytes
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::SIZE {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let record_length = parse_numeric(&data[0..5])?;
        let interchange_level = data[5];
        let leader_identifier = data[6] as char;
        let inline_code_ext = data[7] as char;
        let version_number = data[8] as char;
        let application_indicator = data[9] as char;
        let field_control_length = parse_numeric(&data[10..12])? as u8;
        let base_address = parse_numeric(&data[12..17])?;
        let extended_charset = [data[17] as char, data[18] as char, data[19] as char];

        // Security: Use checked arithmetic to prevent integer underflow
        // Malformed files with bytes < b'0' would cause wrapping in release builds
        let size_field_length = parse_ascii_digit(data[20])?;
        let size_field_position = parse_ascii_digit(data[21])?;
        let reserved = data[22] as char;
        let size_field_tag = parse_ascii_digit(data[23])?;

        Ok(Leader {
            record_length,
            interchange_level,
            leader_identifier,
            inline_code_ext,
            version_number,
            application_indicator,
            field_control_length,
            base_address,
            extended_charset,
            size_field_length,
            size_field_position,
            reserved,
            size_field_tag,
        })
    }

    /// Check if this is a DDR (Data Descriptive Record)
    pub fn is_ddr(&self) -> bool {
        self.leader_identifier == 'L'
    }

    /// Check if this is a DR (Data Record)
    pub fn is_dr(&self) -> bool {
        self.leader_identifier == 'D'
    }

    /// Calculate directory entry size
    pub fn directory_entry_size(&self) -> usize {
        self.size_field_tag as usize
            + self.size_field_length as usize
            + self.size_field_position as usize
    }
}

/// Parse single ASCII digit ('0'-'9') to u8 with bounds checking
///
/// Security: Prevents integer underflow from malformed data
fn parse_ascii_digit(byte: u8) -> Result<u8> {
    if byte.is_ascii_digit() {
        Ok(byte - b'0')
    } else {
        Err(Iso8211Error::Parse(format!(
            "Invalid ASCII digit: 0x{:02X}",
            byte
        )))
    }
}

/// Parse ASCII numeric string to u32
fn parse_numeric(data: &[u8]) -> Result<u32> {
    let s = std::str::from_utf8(data)
        .map_err(|e| Iso8211Error::Parse(e.to_string()))?
        .trim();

    // Handle empty or whitespace-only strings
    if s.is_empty() {
        return Ok(0);
    }

    s.parse()
        .map_err(|e: std::num::ParseIntError| Iso8211Error::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leader_size() {
        assert_eq!(Leader::SIZE, 24);
    }
}
