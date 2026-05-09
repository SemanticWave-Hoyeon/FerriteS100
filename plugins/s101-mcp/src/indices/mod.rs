//! Read-only indices over a loaded S-101 cell + Feature Catalogue.
//!
//! Built once at startup. All four indices share the same `S101Cell` /
//! `FeatureCatalogue` references via Arc so tools never have to re-walk the
//! raw record tables.
//!
//! Why four separate indices: plan2.md §4.1 uses these to draw a clean line
//! between "what does this thing mean?" (catalogue) and "what is at this
//! location?" (geometry / feature) so the MCP tool surface can answer each
//! kind of question without falling back to LLM speculation.

pub mod catalogue;
pub mod evidence;
pub mod feature;
pub mod geometry;

use std::sync::Arc;

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_core::S101Cell;

pub struct Indices {
    pub cell: Arc<S101Cell>,
    pub fc: Arc<FeatureCatalogue>,
    pub feature: feature::FeatureIndex,
    pub geometry: geometry::GeometryIndex,
    pub catalogue: catalogue::CatalogueIndex,
}

impl Indices {
    pub fn build(cell: S101Cell, fc: FeatureCatalogue) -> Self {
        let cell = Arc::new(cell);
        let fc = Arc::new(fc);
        let feature = feature::FeatureIndex::build(&cell);
        let geometry = geometry::GeometryIndex::build(&cell, &feature);
        let catalogue = catalogue::CatalogueIndex::build(&fc);
        Indices {
            cell,
            fc,
            feature,
            geometry,
            catalogue,
        }
    }
}
