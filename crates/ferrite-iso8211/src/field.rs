//! ISO 8211 Field types for S-100/S-101
//!
//! Contains field definitions and parsing for S-101 ENC data.

use crate::{Iso8211Error, Result};

/// Unit terminator (separates subfields)
pub const UNIT_TERMINATOR: u8 = 0x1F;
/// Field terminator (ends a field)
pub const FIELD_TERMINATOR: u8 = 0x1E;

/// Field tag constants for S-101
pub mod tags {
    // Dataset records
    pub const DSID: &str = "DSID"; // Dataset Identification
    pub const DSSI: &str = "DSSI"; // Dataset Structure Information
    pub const CSID: &str = "CSID"; // Coordinate System Identifier

    // Code mapping
    pub const ATCS: &str = "ATCS"; // Attribute Code/String
    pub const ITCS: &str = "ITCS"; // Information Type Code/String
    pub const FTCS: &str = "FTCS"; // Feature Type Code/String
    pub const IACS: &str = "IACS"; // Information Association Code/String
    pub const FACS: &str = "FACS"; // Feature Association Code/String
    pub const ARCS: &str = "ARCS"; // Association Role Code/String

    // Information records
    pub const IRID: &str = "IRID"; // Information Record Identifier

    // Point records
    pub const PRID: &str = "PRID"; // Point Record Identifier
    pub const C2IT: &str = "C2IT"; // 2D Integer Coordinate Tuple (for single points)
    pub const C3IT: &str = "C3IT"; // 3D Integer Coordinate Tuple (for single points)
    pub const C2IL: &str = "C2IL"; // 2D Integer Coordinates (Line/List)
    pub const C3IL: &str = "C3IL"; // 3D Integer Coordinates (Line/List)

    // Multi-point records
    pub const MRID: &str = "MRID"; // Multi-point Record Identifier

    // Curve records
    pub const CRID: &str = "CRID"; // Curve Record Identifier
    pub const PTAS: &str = "PTAS"; // Point Association
    pub const SEGH: &str = "SEGH"; // Segment Header
    pub const SECC: &str = "SECC"; // Segment Control Code

    // Composite Curve records
    pub const CCID: &str = "CCID"; // Composite Curve Identifier
    pub const CUCO: &str = "CUCO"; // Curve Components

    // Surface records
    pub const SRID: &str = "SRID"; // Surface Record Identifier
    pub const RIAS: &str = "RIAS"; // Ring Association

    // Feature records
    pub const FRID: &str = "FRID"; // Feature Record Identifier
    pub const FOID: &str = "FOID"; // Feature Object Identifier
    pub const ATTR: &str = "ATTR"; // Attributes
    pub const INAS: &str = "INAS"; // Information Association
    pub const SPAS: &str = "SPAS"; // Spatial Association
    pub const FASC: &str = "FASC"; // Feature Association
    pub const MASK: &str = "MASK"; // Mask Pointer
}

/// Raw field data with tag
#[derive(Debug, Clone)]
pub struct RawField {
    pub tag: String,
    pub data: Vec<u8>,
}

impl RawField {
    /// Create new raw field
    pub fn new(tag: String, data: Vec<u8>) -> Self {
        RawField { tag, data }
    }

    /// Get data without terminator
    pub fn data_trimmed(&self) -> &[u8] {
        let mut end = self.data.len();
        while end > 0 && (self.data[end - 1] == FIELD_TERMINATOR || self.data[end - 1] == UNIT_TERMINATOR) {
            end -= 1;
        }
        &self.data[..end]
    }
}

/// Extract null/unit-terminated string from buffer
pub fn read_string(data: &[u8]) -> Result<(String, usize)> {
    let mut end = 0;
    while end < data.len() && data[end] != UNIT_TERMINATOR && data[end] != FIELD_TERMINATOR && data[end] != 0 {
        end += 1;
    }

    let s = String::from_utf8_lossy(&data[..end]).to_string();
    let consumed = if end < data.len() { end + 1 } else { end };

    Ok((s, consumed))
}

/// Read unsigned integer from buffer (little-endian)
pub fn read_uint(data: &[u8], size: usize) -> Result<u64> {
    if data.len() < size {
        return Err(Iso8211Error::UnexpectedEof);
    }

    let mut value: u64 = 0;
    for i in 0..size {
        value |= (data[i] as u64) << (i * 8);
    }

    Ok(value)
}

/// Read signed integer from buffer (little-endian)
pub fn read_int(data: &[u8], size: usize) -> Result<i64> {
    if data.len() < size {
        return Err(Iso8211Error::UnexpectedEof);
    }

    let value = read_uint(data, size)?;

    // Sign extend if needed
    let sign_bit = 1u64 << (size * 8 - 1);
    if value & sign_bit != 0 {
        let mask = !((1u64 << (size * 8)) - 1);
        Ok((value | mask) as i64)
    } else {
        Ok(value as i64)
    }
}

/// Read coordinate multiplier factor
pub fn read_coordinate_factor(data: &[u8]) -> Result<(f64, usize)> {
    if data.len() < 4 {
        return Err(Iso8211Error::UnexpectedEof);
    }

    let factor = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    Ok((10f64.powi(-factor), 4))
}
