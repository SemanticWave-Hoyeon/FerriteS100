//! Error types for S-100 core

use thiserror::Error;

#[derive(Error, Debug)]
pub enum S100Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ISO 8211 error: {0}")]
    Iso8211(#[from] ferrite_iso8211::Iso8211Error),

    #[error("Invalid record: {0}")]
    InvalidRecord(String),

    #[error("Missing field: {0}")]
    MissingField(String),

    #[error("Invalid field data: {0}")]
    InvalidFieldData(String),

    #[error("Code not found: {0}")]
    CodeNotFound(String),

    #[error("Parse error: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, S100Error>;
