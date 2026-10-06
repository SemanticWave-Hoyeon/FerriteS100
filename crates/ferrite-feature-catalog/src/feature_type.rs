//! Feature type definitions

use crate::{AttributeBinding, Multiplicity, RoleType, SpatialPrimitive};
use serde::{Deserialize, Serialize};

/// S-100 2a-4.2.5 and Part 5 Table 5-A-8 feature classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeatureUseType {
    Geographic,
    Meta,
    Cartographic,
    Theme,
}
impl FeatureUseType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Geographic => "geographic",
            Self::Meta => "meta",
            Self::Cartographic => "cartographic",
            Self::Theme => "theme",
        }
    }
    pub fn parse(value: &str) -> crate::Result<Self> {
        match value.trim() {
            "geographic" => Ok(Self::Geographic),
            "meta" => Ok(Self::Meta),
            "cartographic" => Ok(Self::Cartographic),
            "theme" => Ok(Self::Theme),
            other => Err(crate::FCError::InvalidValue(format!(
                "featureUseType: {other}"
            ))),
        }
    }
}

/// Feature type definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureType {
    /// None means absent in a legacy/incomplete catalogue; never assume geographic.
    #[serde(default)]
    pub feature_use_type: Option<FeatureUseType>,
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
    pub information_bindings: Vec<InformationBinding>,
    #[serde(default)]
    pub feature_bindings: Vec<FeatureBinding>,
    #[serde(default)]
    pub permitted_primitives: Vec<SpatialPrimitive>,
}

/// Information binding in feature type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformationBinding {
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

/// Feature binding (association between features)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureBinding {
    pub feature_type_code: String,
    #[serde(default)]
    pub association: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub role_type: RoleType,
    #[serde(default)]
    pub multiplicity: Multiplicity,
}

impl FeatureType {
    /// Check if feature can have given primitive type
    pub fn permits_primitive(&self, primitive: SpatialPrimitive) -> bool {
        self.permitted_primitives.is_empty() || self.permitted_primitives.contains(&primitive)
    }

    /// Get all attribute codes
    pub fn attribute_codes(&self) -> Vec<&str> {
        self.attribute_bindings
            .iter()
            .map(|b| b.attribute_code.as_str())
            .collect()
    }
}
