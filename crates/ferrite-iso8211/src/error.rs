//! Error types for ISO 8211 parsing

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Iso8211Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid leader: {0}")]
    InvalidLeader(String),

    #[error("Invalid directory entry: {0}")]
    InvalidDirectory(String),

    #[error("Invalid field: {0}")]
    InvalidField(String),

    #[error("Invalid record: {0}")]
    InvalidRecord(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Encoding error: {0}")]
    Encoding(String),

    #[error("Unexpected end of data")]
    UnexpectedEof,
}

pub type Result<T> = std::result::Result<T, Iso8211Error>;
