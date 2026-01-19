//! wgpu renderer error types

use thiserror::Error;

#[derive(Error, Debug)]
pub enum WgpuError {
    #[error("Failed to create surface: {0}")]
    SurfaceCreation(String),

    #[error("Failed to find suitable adapter")]
    AdapterNotFound,

    #[error("Failed to create device: {0}")]
    DeviceCreation(#[from] wgpu::RequestDeviceError),

    #[error("Surface configuration error: {0}")]
    SurfaceConfig(String),

    #[error("Shader compilation error: {0}")]
    Shader(String),

    #[error("Pipeline creation error: {0}")]
    Pipeline(String),

    #[error("Buffer creation error: {0}")]
    Buffer(String),

    #[error("Texture error: {0}")]
    Texture(String),

    #[error("Render error: {0}")]
    Render(String),
}

pub type Result<T> = std::result::Result<T, WgpuError>;
