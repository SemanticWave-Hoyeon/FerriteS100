//! wgpu-based Renderer for S-100 Charts
//!
//! This crate provides GPU-accelerated rendering of S-100/S-101 charts
//! using the wgpu graphics API (WebGPU).
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

mod egui_integration;
mod object_details;
pub use object_details::{draw_selected_object_details, ObjectDetailSections};
mod draw_range_index;
mod error;
mod native_route_leg;
mod native_route_overlay;
pub use native_route_leg::{NativeRouteLegDeclaration, NativeRouteLegGeometry};
mod area_candidates;
mod area_index_upload_reuse;
mod audit_buffer_export;
mod chart_fonts;
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

pub use egui_integration::*;
pub use error::*;
pub use pipeline::*;
pub use profiler::*;
pub use renderer::*;
pub use state::*;
pub use symbol_cache::*;
pub use vertex::*;

/// Initialize wgpu for the given window
pub async fn create_renderer(
    window: std::sync::Arc<winit::window::Window>,
) -> Result<WgpuRenderer> {
    WgpuRenderer::new(window).await
}

mod coordinate_rulers;
mod ui_chrome;
mod ui_overlay_layout;

pub mod screen_stroke;

/// Fragment clipping shared by chart portrayal products.
pub mod coverage_clip;

pub mod coverage_pipeline;

pub mod coverage_gpu_frame;

/// Native diagnostics that leave the user desktop alone.
pub mod background_test;

mod symbol_instance;

// Staged continuous-source selector; no existing raster path is replaced yet.
mod continuous_frame_binding;
mod continuous_raster_selector;
pub use continuous_raster_selector::ValidatedContinuousFrame;

mod raster_publication_budget;

mod immutable_payload_cache;

/// Bounded diagnostic log and view model for the external tracing adapter.
pub mod diagnostics;
pub use diagnostics::{DiagnosticEntry, DiagnosticLevel, DiagnosticLog, DiagnosticView};

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

mod dataset_catalogue_ui;
pub use dataset_catalogue_ui::DatasetCatalogueBinding;

mod coverage_trial;

mod object_symbol_preview;
pub use object_symbol_preview::{prepare_selected_symbol_preview, SelectedSymbolPreview};

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

mod route_turn_radius_ui;

pub mod mcp_ui;
