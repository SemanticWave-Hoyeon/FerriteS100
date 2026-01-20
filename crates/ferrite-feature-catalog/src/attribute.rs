//! Attribute definitions

use crate::{AttributeValueType, ListedValue, Multiplicity};
use serde::{Deserialize, Serialize};

/// Simple attribute definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleAttribute {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default)]
    pub value_type: AttributeValueType,
    #[serde(default)]
    pub uom: Option<String>,
    #[serde(default)]
    pub listed_values: Vec<ListedValue>,
    #[serde(default)]
    pub quantitative_range: Option<QuantitativeRange>,
}

/// Quantitative range for numeric attributes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantitativeRange {
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}

/// Complex attribute definition (contains sub-attributes)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexAttribute {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default)]
    pub sub_attributes: Vec<AttributeBinding>,
}

/// Attribute binding in feature/complex attribute
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeBinding {
    pub attribute_code: String,
    #[serde(default)]
    pub multiplicity: Multiplicity,
    #[serde(default)]
    pub sequential: bool,
    #[serde(default)]
    pub permitted_values: Vec<u32>, // Subset of listed value codes
}

impl SimpleAttribute {
    /// Get listed value by code
    pub fn get_listed_value(&self, code: u32) -> Option<&ListedValue> {
        self.listed_values.iter().find(|v| v.code == code)
    }

    /// Get listed value by label
    pub fn get_listed_value_by_label(&self, label: &str) -> Option<&ListedValue> {
        self.listed_values.iter().find(|v| v.label == label)
    }
}
