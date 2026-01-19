//! S-100 Rendering Abstractions
//!
//! This crate provides core rendering abstractions for S-100/S-101 chart display:
//! - Drawing instructions (Point, Line, Area, Text)
//! - Coordinate transformation (World <-> Screen)
//! - Render context and state management
//!
//! Based on S-100 standard rendering architecture, adapted for Rust/wgpu.

mod error;
mod color;
mod instruction;
mod scaler;
mod context;

pub use error::*;
pub use color::*;
pub use instruction::*;
pub use scaler::*;
pub use context::*;
