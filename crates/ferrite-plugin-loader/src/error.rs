//! Plugin error types

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PluginError {
    #[error("Failed to load plugin DLL: {0}")]
    LoadError(String),

    #[error("Plugin signature verification failed")]
    InvalidSignature,

    #[error("Plugin hash mismatch - DLL may have been tampered with")]
    HashMismatch,

    #[error("Signature file not found: {0}")]
    SignatureNotFound(String),

    #[error("Manifest file not found: {0}")]
    ManifestNotFound(String),

    #[error("Invalid manifest format: {0}")]
    InvalidManifest(String),

    #[error("API version mismatch - plugin: {plugin}, host: {host}")]
    ApiVersionMismatch { plugin: u32, host: u32 },

    #[error("Host version too old - required: {required}, current: {current}")]
    HostVersionTooOld { required: String, current: String },

    #[error("Plugin ID mismatch - manifest: {manifest}, dll: {dll}")]
    IdMismatch { manifest: String, dll: String },

    #[error("Plugin symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Plugin directory not found: {0}")]
    DirectoryNotFound(String),

    #[error("Plugin not found: {0}")]
    PluginNotFound(String),

    #[error("Plugin already loaded: {0}")]
    AlreadyLoaded(String),
}
