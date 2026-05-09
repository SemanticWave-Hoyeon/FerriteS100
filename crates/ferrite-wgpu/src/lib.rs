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
mod error;
mod pipeline;
pub mod profiler;
mod renderer;
mod renderer_internals;
mod state;
mod symbol_cache;
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
