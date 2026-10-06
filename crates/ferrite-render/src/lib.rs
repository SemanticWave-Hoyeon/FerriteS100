//! S-100 Rendering Abstractions
//!
//! This crate provides core rendering abstractions for S-100/S-101 chart display:
//! - Drawing instructions (Point, Line, Area, Text)
//! - Coordinate transformation (World <-> Screen)
//! - Render context and state management
//!
//! Based on S-100 standard rendering architecture, adapted for Rust/wgpu.

mod color;
mod context;
mod error;
mod instruction;
mod intern;
mod scaler;

pub use color::*;
pub use context::*;
pub use error::*;
pub use instruction::*;
pub use intern::*;
pub use scaler::*;

mod raster;
pub use raster::*;

mod selection;
pub use selection::{
    hit_geometry, hit_geometry_visible, hit_geometry_wrapped_visible, GeometryHit,
    WrappedGeometryHit,
};

mod line_relation_identity;
pub use line_relation_identity::StaticLineRelationEpoch;
mod suppression;
pub use suppression::{
    instruction_visible, LineSpan, LineSuppressionCache, LineSuppressionPlan,
    PreparedLineSuppression,
};

mod temporal_view;
pub use temporal_view::{TemporalView, TemporalViewMode};

mod text_placement;
pub use text_placement::{TextFootprint, TextPlacement};

mod line_pattern;
pub use line_pattern::{
    dash_line_spans, dash_projected_line_spans, dash_projected_line_spans_clipped,
};

mod portrayal_path;
pub use portrayal_path::PortrayalPath;

mod selection_index;
pub use selection_index::{SelectionIndex, SelectionIndexStats};

mod drawing_dependencies;
pub use drawing_dependencies::{DependencyResolution, DrawingDependency, DrawingDependencyGraph};

mod rotation;
pub use rotation::{
    flat_rotation, flat_text_rotation, screen_rotation, screen_text_rotation, RotationCrs,
};

mod line_symbol_placement;
pub use line_symbol_placement::{
    clip_curve_components, placed_curve_points, resolve_flat_line_symbol, sample_curve_position,
    CurveSample, LinePlacementMode, LineSymbolPlacement,
};

mod triangle_clip;
pub use triangle_clip::{clip_triangle_to_rect, ClippedTriangle};

mod background_coastline;
pub use background_coastline::{BackgroundCoastlines, CoastlineChunk};

mod scene_spatial;
pub use scene_spatial::SceneSpatialIndex;

mod portrayal_origin;
pub use portrayal_origin::{PointOriginCrs, PointOriginGeometry, PortrayalOrigin};


pub mod prepared_coverage;
pub use prepared_coverage::{InstructionCoverageClass, PreparedCoverage, PreparedCoveragePass};

mod pattern_crs;
pub use pattern_crs::{PatternCrs, HatchStroke, HatchLineSymbol};

mod pattern_lattice;
pub use pattern_lattice::{PatternLattice, PatternCellLimits, PatternCellPlan};

mod pattern_display_policy;
pub use pattern_display_policy::{ShallowPatternContract, pattern_display_allows};

/// Bounded opt-in 2D frame diagnostics; no visibility or render-policy changes.
pub mod flat_reuse_diagnostics;
