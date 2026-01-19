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

mod error;
mod state;
mod vertex;
mod pipeline;
mod renderer;
mod symbol_cache;
mod egui_integration;

pub use error::*;
pub use state::*;
pub use vertex::*;
pub use pipeline::*;
pub use renderer::*;
pub use symbol_cache::*;
pub use egui_integration::*;

/// Initialize wgpu for the given window
pub async fn create_renderer(
    window: std::sync::Arc<winit::window::Window>,
) -> Result<WgpuRenderer> {
    WgpuRenderer::new(window).await
}
