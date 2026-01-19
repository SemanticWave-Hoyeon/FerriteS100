//! Common types for Feature Catalogue

use serde::{Deserialize, Serialize};

/// Attribute value type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttributeValueType {
    Boolean,
    Enumeration,
    Integer,
    Real,
    Text,
    Date,
    Time,
    DateTime,
    TruncatedDate,
    #[serde(rename = "URI")]
    Uri,
    #[serde(rename = "URL")]
    Url,
    #[serde(rename = "URN")]
    Urn,
    #[serde(rename = "S100_CodeList")]
    S100CodeList,
    #[serde(rename = "S100_TruncatedDate")]
    S100TruncatedDate,
}

impl Default for AttributeValueType {
    fn default() -> Self {
        AttributeValueType::Text
    }
}

/// Spatial primitive type for features
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpatialPrimitive {
    Point,
    Curve,
    Surface,
    Coverage,
    NoGeometry,
}

/// Multiplicity specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Multiplicity {
    #[serde(default)]
    pub lower: u32,
    #[serde(default)]
    pub upper: Option<u32>, // None = unbounded
}

impl Default for Multiplicity {
    fn default() -> Self {
        Multiplicity { lower: 1, upper: Some(1) }
    }
}

impl Multiplicity {
    pub fn optional() -> Self {
        Multiplicity { lower: 0, upper: Some(1) }
    }

    pub fn required() -> Self {
        Multiplicity { lower: 1, upper: Some(1) }
    }

    pub fn unbounded() -> Self {
        Multiplicity { lower: 0, upper: None }
    }

    pub fn is_required(&self) -> bool {
        self.lower > 0
    }

    pub fn is_unbounded(&self) -> bool {
        self.upper.is_none()
    }
}

/// Listed value for enumerated attributes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListedValue {
    pub label: String,
    pub code: u32,
    #[serde(default)]
    pub definition: Option<String>,
}

/// Alias for multi-language support
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alias {
    pub language: String,
    pub value: String,
}

/// Role type in associations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RoleType {
    Association,
    Aggregation,
    Composition,
}

impl Default for RoleType {
    fn default() -> Self {
        RoleType::Association
    }
}
