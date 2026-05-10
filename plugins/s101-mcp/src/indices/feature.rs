//! Feature lookup index: feature record id → record reference, plus a
//! type-bucket so `feature_query_by_type` doesn't have to scan every feature.

use rustc_hash::FxHashMap;

use ferrite_s100_core::{FeatureRecord, S101Cell};

/// Stable feature id used throughout the MCP surface.
///
/// Internally this is the FRID `rcid`, which is the same key the host uses
/// to index `cell.features`. We expose it as `i64` to match the underlying
/// `FeatureRecord::record_id().key()`.
pub type FeatureId = i64;

pub struct FeatureIndex {
    /// `feature_id → cell.features index key`. The actual `FeatureRecord` is
    /// fetched from the cell via `Indices::cell` when needed (zero copy).
    by_id: FxHashMap<FeatureId, FeatureId>,
    /// Lowercased `feature_code → Vec<feature_id>` for fast type lookup.
    by_type: FxHashMap<String, Vec<FeatureId>>,
}

impl FeatureIndex {
    pub fn build(cell: &S101Cell) -> Self {
        let mut by_id = FxHashMap::default();
        let mut by_type: FxHashMap<String, Vec<FeatureId>> = FxHashMap::default();

        for (&fid, feature) in &cell.features {
            by_id.insert(fid, fid);
            if let Some(code) = feature.feature_code.as_deref() {
                by_type.entry(code.to_lowercase()).or_default().push(fid);
            }
        }

        FeatureIndex { by_id, by_type }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    #[allow(dead_code)]
    pub fn contains(&self, id: FeatureId) -> bool {
        self.by_id.contains_key(&id)
    }

    /// All feature ids of the given type code (case-insensitive). Empty slice
    /// if the code isn't present in the dataset (which is meaningful — it's
    /// how we answer "is there a lighthouse here?" with "no, none in cell").
    pub fn ids_by_type<'a>(&'a self, type_code: &str) -> &'a [FeatureId] {
        self.by_type
            .get(&type_code.to_lowercase())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// All feature ids in the cell (used to seed bbox iteration when a
    /// type filter isn't given).
    pub fn all_ids(&self) -> impl Iterator<Item = FeatureId> + '_ {
        self.by_id.keys().copied()
    }

    /// Type codes present in the cell, with counts. Used by `dataset_metadata`.
    pub fn type_histogram(&self) -> Vec<(String, usize)> {
        let mut hist: Vec<_> = self
            .by_type
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect();
        hist.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hist
    }

    /// Number of feature codes present in the cell.
    #[allow(dead_code)]
    pub fn type_count(&self) -> usize {
        self.by_type.len()
    }
}

/// Resolve a feature record from its id. The returned reference is borrowed
/// from the cell — callers should stage their JSON output before dropping.
#[allow(dead_code)]
pub fn resolve(cell: &S101Cell, id: FeatureId) -> Option<&FeatureRecord> {
    cell.features.get(&id)
}
