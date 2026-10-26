//! wgpu-based Renderer for S-100 Charts
//!
//! This crate provides GPU-accelerated rendering of S-100/S-101 charts
//! using the wgpu graphics API (WebGPU). It does not depend on a windowing or
//! GUI toolkit: the host supplies the presentation surface ([`SurfaceHost`]),
//! the chart text system ([`ChartFontContext`]) and the overlay compositor
//! ([`UiLayer`]). `ferrite-ui` provides the egui/winit implementation.
//!
//! ## Architecture
//!
//! ```text
//! DrawingInstructions → WgpuRenderer → GPU Pipeline → Screen
//!                            ↓
//!                      Vertex Buffers
//!                      Symbol Cache (resvg textures)
//!                      Line Renderer
//!                      Area Renderer
//! ```

mod draw_range_index;
mod error;
#[doc(hidden)]
pub mod native_route_leg;
#[doc(hidden)]
pub mod native_route_overlay;
pub use native_route_leg::{NativeRouteLegDeclaration, NativeRouteLegGeometry};
mod area_candidates;
mod area_index_upload_reuse;
mod audit_buffer_export;
pub mod chart_fonts;
mod emitter_wave_diagnostics;
mod exact_line_quad;
pub use area_index_upload_reuse::Work as AreaIndexUploadReuseWork;
mod immutable_line_topology;
mod line_preparation_diagnostics;
mod motion_preview;
mod pipeline;
mod prepared_vector_coverage;
mod primary_line_geometry;
pub mod profiler;
mod shared_cell;
pub use audit_buffer_export::digest_only_enabled as audit_digest_only_enabled;
mod renderer;
pub use line_preparation_diagnostics::Work as LinePreparationWork;
pub use native_route_overlay::*;
mod retained_world_area;
mod state;
mod surface_pacing;
pub mod svg_painted_support;
mod symbol_cache;
mod vertex;
pub mod whole_motif;

pub use error::*;
pub use pipeline::*;
pub use profiler::*;
pub use renderer::*;
pub use state::*;
pub use symbol_cache::*;

mod overlay_symbols;
pub use overlay_symbols::{
    is_overlay_symbol_id, overlay_symbol_source, register_overlay_catalogue,
    register_overlay_symbol_source, registered_overlay_prefixes, resolve_overlay_symbol,
    unregister_overlay_symbol_source, OverlaySymbolSource,
};
pub use vertex::*;

/// Host-supplied chart text system (layout contexts and glyph atlases).
pub mod chart_text_backend;
pub use chart_text_backend::{
    ChartFontContext, ChartTextAtlas, FontPassInput, FontPassOutput, PrivateFontSpec,
};
#[cfg(test)]
mod test_text_backend;
/// Host-supplied presentation surface and overlay compositor.
pub mod ui_layer;
pub use ferrite_render::host_settings::{CatalogueStatus, DisplayMode, SettingsState};
pub use ui_layer::{
    CoverageScaleIndication, DebugGpuStats, PhysicalSize, SelectedFeature, SelectionOverlay,
    SurfaceHost, UiLayer,
};

pub mod screen_stroke;

/// Fragment clipping shared by chart portrayal products.
pub mod coverage_clip;

pub mod coverage_pipeline;

pub mod coverage_gpu_frame;

/// Native diagnostics that leave the user desktop alone.
pub mod background_test {
    /// `FERRITE_BACKGROUND_TEST=1`: hidden native test presentation.
    pub fn enabled() -> bool {
        std::env::var_os("FERRITE_BACKGROUND_TEST").is_some_and(|v| v == "1")
    }
}

mod symbol_instance;

// Staged continuous-source selector; no existing raster path is replaced yet.
mod continuous_frame_binding;
mod continuous_raster_selector;
pub use continuous_raster_selector::ValidatedContinuousFrame;

mod raster_publication_budget;
mod raster_suppression;

mod immutable_payload_cache;

mod accepted_screen_line_packet;
mod moving_line_northing;
mod source_batch_parallel;
mod source_line_projection_arena;

mod overscale_annotation;

mod portrayal_resource_owners;
pub use portrayal_resource_owners::{
    CellPortrayalResourceBinding, CellPortrayalResources, OwnedSymbolKey, PortrayalResourceOwner,
    ResolvedPortrayalResources,
};

mod coverage_trial;

mod static_line_bounds;

mod compact_owner_admission;
mod owner_group_plan;

mod native_route_gpu;
pub use native_route_gpu::*;

mod suppression_tail;

mod gpu_frame_timestamp;

mod font_asset_cache;
mod referenced_chart_font;
mod referenced_chart_owner;

/// Bounded, explicitly scoped debug performance metrics.
pub mod debug_metrics;
