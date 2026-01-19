//! Error types for Feature Catalogue parsing

use thiserror::Error;

#[derive(Error, Debug)]
pub enum FCError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("XML parsing error: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("XML deserialization error: {0}")]
    XmlDe(#[from] quick_xml::DeError),

    #[error("Missing element: {0}")]
    MissingElement(String),

    #[error("Invalid value: {0}")]
    InvalidValue(String),
}

pub type Result<T> = std::result::Result<T, FCError>;
