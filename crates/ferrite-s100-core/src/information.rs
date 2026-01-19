//! Information record types for S-100

use crate::{RecordId, Attribute, InformationAssociation};

/// Information Record Identifier (IRID)
#[derive(Debug, Clone)]
pub struct IRID {
    /// Record identifier
    pub rcid: u32,
    /// Numeric information type code
    pub nitc: u16,
    /// Record version
    pub rver: u16,
    /// Record update instruction
    pub ruin: u8,
}

/// Complete information record
#[derive(Debug, Clone)]
pub struct InformationRecord {
    pub irid: IRID,
    pub attributes: Vec<Attribute>,
    pub information_associations: Vec<InformationAssociation>,
    /// Information type code string (after mapping)
    pub info_code: Option<String>,
}

impl InformationRecord {
    /// Get record ID
    pub fn record_id(&self) -> RecordId {
        RecordId::new(110, self.irid.rcid) // 110 = information record name
    }

    /// Get attribute by code
    pub fn get_attribute(&self, code: &str) -> Option<&Attribute> {
        self.attributes
            .iter()
            .find(|a| a.code.as_deref() == Some(code))
    }
}
