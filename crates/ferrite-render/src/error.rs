//! Render error types

use thiserror::Error;

#[derive(Error, Debug)]
pub enum RenderError {
    #[error("Invalid coordinate: {0}")]
    InvalidCoordinate(String),

    #[error("Symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("Color not found: {0}")]
    ColorNotFound(String),

    #[error("Line style not found: {0}")]
    LineStyleNotFound(String),

    #[error("Area fill not found: {0}")]
    AreaFillNotFound(String),

    #[error("Transform error: {0}")]
    Transform(String),

    #[error("Render error: {0}")]
    Render(String),
}

pub type Result<T> = std::result::Result<T, RenderError>;
