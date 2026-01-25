//! Plugin Loader for FerriteS100
//!
//! Handles DLL loading with security verification:
//! - Ed25519 signature verification
//! - SHA-256 hash validation
//! - API version compatibility check

mod error;
mod host_impl;
mod loader;
mod manager;
mod verifier;

pub use error::PluginError;
pub use host_impl::{create_host_api, HostContext, SharedHostContext};
pub use loader::PluginLoader;
pub use manager::PluginManager;
pub use verifier::PluginVerifier;

use serde::{Deserialize, Serialize};

/// Plugin manifest (manifest.json)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin ID (must match DLL's reported ID)
    pub id: String,
    /// Plugin name
    pub name: String,
    /// Plugin version
    pub version: String,
    /// Author
    pub author: String,
    /// Description
    pub description: String,
    /// DLL filename
    pub dll_file: String,
    /// Minimum host version required
    pub min_host_version: String,
    /// SHA-256 hash of DLL (hex string)
    pub dll_hash: String,
    /// Whether plugin is enabled by default
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Loaded plugin information
#[derive(Debug)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub path: std::path::PathBuf,
    pub verified: bool,
}
