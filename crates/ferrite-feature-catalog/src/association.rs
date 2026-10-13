//! Roles, associations and other catalogue-level data that is kept outside
//! [`crate::FeatureCatalogue`] so existing struct literals stay valid.

use std::collections::HashMap;

use crate::AttributeBinding;
use serde::{Deserialize, Serialize};

/// `S100_FC_Role`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Role {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub definition: Option<String>,
}

/// `S100_FC_FeatureAssociation` / `S100_FC_InformationAssociation`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Association {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default)]
    pub is_abstract: bool,
    #[serde(default)]
    pub super_type: Option<String>,
    /// Role codes referenced by `<role ref=".."/>`, in document order.
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub attribute_bindings: Vec<AttributeBinding>,
}

/// Structured `S100_UnitOfMeasure` (S-100 5.x) or a legacy plain-text `uom`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UnitOfMeasure {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub definition: Option<String>,
}

/// `S100_NumericRange` from `<constraints><range>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NumericRange {
    #[serde(default)]
    pub lower_bound: Option<f64>,
    #[serde(default)]
    pub upper_bound: Option<f64>,
    /// `S100_IntervalType` value verbatim, e.g. `closedInterval`, `geSemiInterval`.
    #[serde(default)]
    pub closure: Option<String>,
}

/// `S100_CD_AttributeConstraints`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AttributeConstraints {
    #[serde(default)]
    pub string_length: Option<u32>,
    #[serde(default)]
    pub text_pattern: Option<String>,
    #[serde(default)]
    pub range: Option<NumericRange>,
    #[serde(default)]
    pub precision: Option<u32>,
}

/// Catalogue content not carried by [`crate::FeatureCatalogue`] itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureCatalogueExtras {
    /// `S100_FC_Roles`, keyed by role code.
    pub roles: HashMap<String, Role>,
    /// `S100_FC_FeatureAssociations`, keyed by association code.
    pub feature_associations: HashMap<String, Association>,
    /// `S100_FC_InformationAssociations`, keyed by association code.
    pub information_associations: HashMap<String, Association>,
    /// Full unit of measure per simple attribute code (the attribute's `uom` holds the symbol).
    pub units_of_measure: HashMap<String, UnitOfMeasure>,
    /// Constraints per simple attribute code.
    pub attribute_constraints: HashMap<String, AttributeConstraints>,
}
