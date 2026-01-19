//! ISO 8211 Directory Entry parsing
//!
//! The directory follows the leader and contains entries
//! describing each field in the record.

use crate::{Leader, Iso8211Error, Result};

/// Directory entry for a single field
#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    /// Field tag (e.g., "DSID", "FRID", "ATTR")
    pub tag: String,
    /// Length of the field data
    pub length: u32,
    /// Position of field data relative to base address
    pub position: u32,
}

impl DirectoryEntry {
    /// Parse a single directory entry
    pub fn parse(data: &[u8], leader: &Leader) -> Result<Self> {
        let tag_size = leader.size_field_tag as usize;
        let len_size = leader.size_field_length as usize;
        let pos_size = leader.size_field_position as usize;
        let total_size = tag_size + len_size + pos_size;

        if data.len() < total_size {
            return Err(Iso8211Error::UnexpectedEof);
        }

        let tag = std::str::from_utf8(&data[0..tag_size])
            .map_err(|e| Iso8211Error::Parse(e.to_string()))?
            .to_string();

        let length = parse_numeric(&data[tag_size..tag_size + len_size])?;
        let position = parse_numeric(&data[tag_size + len_size..total_size])?;

        Ok(DirectoryEntry { tag, length, position })
    }
}

/// Directory containing all field entries for a record
#[derive(Debug, Clone)]
pub struct Directory {
    pub entries: Vec<DirectoryEntry>,
}

impl Directory {
    /// Field terminator character
    pub const FIELD_TERMINATOR: u8 = 0x1E;

    /// Parse directory from data following leader
    pub fn parse(data: &[u8], leader: &Leader) -> Result<Self> {
        let entry_size = leader.directory_entry_size();
        let mut entries = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            // Check for field terminator
            if data[offset] == Self::FIELD_TERMINATOR {
                break;
            }

            if offset + entry_size > data.len() {
                return Err(Iso8211Error::UnexpectedEof);
            }

            let entry = DirectoryEntry::parse(&data[offset..], leader)?;
            entries.push(entry);
            offset += entry_size;
        }

        Ok(Directory { entries })
    }

    /// Get number of entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if directory is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Find entry by tag
    pub fn find_by_tag(&self, tag: &str) -> Option<&DirectoryEntry> {
        self.entries.iter().find(|e| e.tag == tag)
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
