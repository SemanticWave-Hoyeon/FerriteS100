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
mod error;
mod pipeline;
pub mod profiler;
mod renderer;
mod draw_range_index;
mod state;
mod symbol_cache;
pub mod svg_painted_support;
pub mod whole_motif;
mod vertex;

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

mod ui_chrome;

pub mod screen_stroke;






/// Fragment clipping shared by chart portrayal products.
pub mod coverage_clip;

pub mod coverage_pipeline;

pub mod coverage_gpu_frame;

/// Native diagnostics that leave the user desktop alone.
pub mod background_test;

mod symbol_instance;


// Staged continuous-source selector; no existing raster path is replaced yet.
mod continuous_raster_selector;
mod continuous_frame_binding;
pub use continuous_raster_selector::ValidatedContinuousFrame;


mod raster_publication_budget;

mod immutable_payload_cache;
