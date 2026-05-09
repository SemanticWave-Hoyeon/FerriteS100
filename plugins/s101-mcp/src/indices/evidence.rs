//! Evidence chain construction.
//!
//! Plan2 §4 calls out an "Evidence index" whose job is to attach every
//! answer to its source — feature ID, attribute name, catalogue entry —
//! so M6 (evidence-constrained tool use) can refuse to answer without
//! ground truth. We materialise these on demand rather than precomputing,
//! since each tool call has a different evidence shape.

use serde::Serialize;

use ferrite_s100_core::{Coordinate, FeatureRecord, S101Cell};

use super::feature::FeatureId;
use super::Indices;

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceRef {
    /// What this evidence pin supports (e.g. "feature-type", "attribute-value",
    /// "catalogue-definition").
    pub kind: String,
    /// Stable id of the supporting record.
    pub id: String,
    /// Human-readable hint for inclusion in the answer.
    pub hint: String,
}

impl EvidenceRef {
    pub fn feature(id: FeatureId, type_code: Option<&str>) -> Self {
        EvidenceRef {
            kind: "feature".to_string(),
            id: id.to_string(),
            hint: format!("feature {} (type {})", id, type_code.unwrap_or("UNKNOWN")),
        }
    }

    pub fn catalogue_feature(code: &str) -> Self {
        EvidenceRef {
            kind: "catalogue-feature".to_string(),
            id: code.to_string(),
            hint: format!("Feature Catalogue feature type {}", code),
        }
    }

    pub fn catalogue_attribute(code: &str) -> Self {
        EvidenceRef {
            kind: "catalogue-attribute".to_string(),
            id: code.to_string(),
            hint: format!("Feature Catalogue attribute {}", code),
        }
    }
}

/// Collect a feature's representative coordinate (centroid of its bbox)
/// for evidence display. Returns `None` for features with no resolved
/// geometry.
#[allow(dead_code)] // exposed for future evidence-rich tools (plan2 §4.2 viewer_highlight)
pub fn representative_point(idx: &Indices, id: FeatureId) -> Option<Coordinate> {
    let g = idx.geometry.get(id)?;
    Some(Coordinate::new(g.centroid.0, g.centroid.1))
}

/// Lift the catalogue definition for a feature record's type code, if any.
pub fn catalogue_definition_for_feature<'a>(
    idx: &'a Indices,
    feature: &FeatureRecord,
) -> Option<&'a str> {
    let code = feature.feature_code.as_deref()?;
    idx.catalogue
        .describe_feature(code)
        .and_then(|ft| ft.definition.as_deref())
}

/// True if a given attribute code is a member of the feature type's binding
/// list per the catalogue. Used to flag attributes that show up in the
/// dataset but aren't sanctioned by the FC — those are dataset-quality red
/// flags and worth surfacing in `feature_get` evidence.
pub fn attribute_is_bound(idx: &Indices, type_code: &str, attribute_code: &str) -> bool {
    let Some(ft) = idx.catalogue.describe_feature(type_code) else {
        return false;
    };
    ft.attribute_bindings
        .iter()
        .any(|b| b.attribute_code.eq_ignore_ascii_case(attribute_code))
}

/// Convenience: cell reference (kept here so callers don't need to know the
/// internal index layout).
#[allow(dead_code)]
pub fn cell(idx: &Indices) -> &S101Cell {
    &idx.cell
}
