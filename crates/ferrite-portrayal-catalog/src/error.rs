//! Error types for Portrayal Catalogue parsing

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PCError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("XML parsing error: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("Missing element: {0}")]
    MissingElement(String),

    #[error("Invalid value: {0}")]
    InvalidValue(String),

    #[error("Resource not found: {0}")]
    ResourceNotFound(String),
}

pub type Result<T> = std::result::Result<T, PCError>;
