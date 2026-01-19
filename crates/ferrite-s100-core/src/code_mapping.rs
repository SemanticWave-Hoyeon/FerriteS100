//! Code mapping between numeric codes and string codes
//!
//! S-101 files use numeric codes internally, which are mapped
//! to string codes via the ATCS, FTCS, ITCS, etc. fields.

use std::collections::HashMap;

/// Bidirectional code mapping
#[derive(Debug, Clone, Default)]
pub struct CodeMapping {
    /// Numeric code to string code
    pub num_to_str: HashMap<u16, String>,
    /// String code to numeric code
    pub str_to_num: HashMap<String, u16>,
}

impl CodeMapping {
    pub fn new() -> Self {
        CodeMapping::default()
    }

    /// Add a mapping
    pub fn insert(&mut self, num: u16, code: String) {
        self.str_to_num.insert(code.clone(), num);
        self.num_to_str.insert(num, code);
    }

    /// Get string code from numeric code
    pub fn get_string(&self, num: u16) -> Option<&String> {
        self.num_to_str.get(&num)
    }

    /// Get numeric code from string code
    pub fn get_numeric(&self, code: &str) -> Option<u16> {
        self.str_to_num.get(code).copied()
    }

    /// Number of mappings
    pub fn len(&self) -> usize {
        self.num_to_str.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.num_to_str.is_empty()
    }
}

/// All code mappings for a dataset
#[derive(Debug, Clone, Default)]
pub struct DatasetCodeMappings {
    /// Attribute codes (ATCS)
    pub attributes: CodeMapping,
    /// Information type codes (ITCS)
    pub information_types: CodeMapping,
    /// Feature type codes (FTCS)
    pub feature_types: CodeMapping,
    /// Information association codes (IACS)
    pub information_associations: CodeMapping,
    /// Feature association codes (FACS)
    pub feature_associations: CodeMapping,
    /// Association role codes (ARCS)
    pub association_roles: CodeMapping,
}

impl DatasetCodeMappings {
    pub fn new() -> Self {
        DatasetCodeMappings::default()
    }

    /// Get attribute code string from numeric
    pub fn attribute_code(&self, num: u16) -> Option<&String> {
        self.attributes.get_string(num)
    }

    /// Get feature type code string from numeric
    pub fn feature_type_code(&self, num: u16) -> Option<&String> {
        self.feature_types.get_string(num)
    }

    /// Get information type code string from numeric
    pub fn info_type_code(&self, num: u16) -> Option<&String> {
        self.information_types.get_string(num)
    }

    /// Log summary of mappings
    pub fn log_summary(&self) {
        tracing::debug!(
            "Code mappings: {} attributes, {} feature types, {} info types, {} feature assoc, {} info assoc, {} roles",
            self.attributes.len(),
            self.feature_types.len(),
            self.information_types.len(),
            self.feature_associations.len(),
            self.information_associations.len(),
            self.association_roles.len()
        );
    }
}
