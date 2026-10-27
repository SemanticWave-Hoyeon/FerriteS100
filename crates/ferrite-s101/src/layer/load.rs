//! Products of a background S-101 chart load.
use crate::layer::{
    lifecycle_metadata as s101_lifecycle_metadata, update_plan as s101_update_plan,
};
use ferrite_s100_core::{CellSourceIdentity, S101Cell};
use std::path::PathBuf;
use std::sync::Arc;

/// Result of background chart loading
pub struct ChartLoadResult {
    pub base_metadata: Option<s101_lifecycle_metadata::MetadataEvidence>,
    pub metadata: Option<s101_lifecycle_metadata::MetadataEvidence>,
    pub input_paths: Vec<PathBuf>,
    pub cell: S101Cell,
    pub source_identity: CellSourceIdentity,
    pub hydration: Option<CellHydrationSource>,
}

/// How to re-materialize a cell whose records were released: the exact load
/// plan (retained authenticated inputs) and the feature codes it was
/// normalized with, in order (load, then each catalogue change).
#[derive(Clone)]
pub struct CellHydrationSource {
    pub plan: Arc<s101_update_plan::Plan>,
    pub codes: Vec<Arc<Vec<String>>>,
}
