//! Information type definitions

use crate::{AttributeBinding, Multiplicity, RoleType};
use serde::{Deserialize, Serialize};

/// Information type definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformationType {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default)]
    pub is_abstract: bool,
    #[serde(default)]
    pub super_type: Option<String>,
    #[serde(default)]
    pub attribute_bindings: Vec<AttributeBinding>,
    #[serde(default)]
    pub information_bindings: Vec<InfoToInfoBinding>,
}

/// Information to information binding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoToInfoBinding {
    pub information_type_code: String,
    #[serde(default)]
    pub association: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub role_type: RoleType,
    #[serde(default)]
    pub multiplicity: Multiplicity,
}

impl InformationType {
    /// Get all attribute codes
    pub fn attribute_codes(&self) -> Vec<&str> {
        self.attribute_bindings
            .iter()
            .map(|b| b.attribute_code.as_str())
            .collect()
    }
}
