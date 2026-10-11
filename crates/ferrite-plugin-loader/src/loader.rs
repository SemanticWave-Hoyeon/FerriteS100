//! Plugin DLL loader
//!
//! Safely loads plugin DLLs and extracts the plugin module.

use std::path::Path;

use crate::snapshot::{canonical_root, capture_regular, PrivateSnapshot, MAX_PLUGIN_BYTES};
use crate::PluginLibrary;
use abi_stable::std_types::RBox;
use tracing::{debug, info, warn};

use ferrite_plugin_api::{
    GetPluginModuleFn, PluginModule, Plugin_TO, PLUGIN_API_VERSION, PLUGIN_MODULE_SYMBOL,
};

use crate::error::PluginError;
use crate::verifier::PluginVerifier;
use crate::PluginManifest;

/// Plugin loader handles DLL loading with verification
pub struct PluginLoader {
    verifier: PluginVerifier,
    host_version: String,
}

impl PluginLoader {
    /// Create a new plugin loader
    pub fn new(verifier: PluginVerifier, host_version: &str) -> Self {
        Self {
            verifier,
            host_version: host_version.to_string(),
        }
    }

    /// Load a plugin from a directory containing manifest.json and DLL
    pub fn load_plugin(
        &self,
        plugin_dir: &Path,
    ) -> Result<(PluginManifest, PluginLibrary, Plugin_TO<'static, RBox<()>>), PluginError> {
        self.verifier.ensure_loading_allowed()?;
        let plugin_dir = canonical_root(plugin_dir)?;
        let manifest_bytes = capture_regular(&plugin_dir, "manifest.json", 64 * 1024)?;
        let manifest: PluginManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| PluginError::InvalidManifest(error.to_string()))?;

        info!("Loading plugin: {} v{}", manifest.name, manifest.version);

        // Check host version compatibility
        if !self.check_version_compatibility(&manifest.min_host_version) {
            return Err(PluginError::HostVersionTooOld {
                required: manifest.min_host_version.clone(),
                current: self.host_version.clone(),
            });
        }

        // Capture once; hash/signature authorize this exact owned byte sequence.
        let captured = capture_regular(&plugin_dir, &manifest.dll_file, MAX_PLUGIN_BYTES)?;
        self.verifier
            .verify_hash_bytes(&captured, &manifest.dll_hash)?;
        let signature = self
            .verifier
            .capture_signature(&plugin_dir, &manifest.dll_file)?;
        self.verifier
            .verify_signature_bytes(&captured, signature.as_deref())?;
        let snapshot = PrivateSnapshot::new(&manifest.dll_file, &captured)?;
        debug!("Loading verified private plugin snapshot");
        let library = unsafe { PluginLibrary::load(snapshot) }?;

        // Get plugin module
        let get_module: GetPluginModuleFn = unsafe {
            *library
                .get(PLUGIN_MODULE_SYMBOL.as_bytes())
                .map_err(|e| PluginError::SymbolNotFound(e.to_string()))?
        };

        let module: &'static PluginModule = get_module();

        // Check API version
        if module.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersionMismatch {
                plugin: module.api_version,
                host: PLUGIN_API_VERSION,
            });
        }

        // Create plugin instance
        let plugin = (module.create_plugin)();

        // Verify ID matches manifest
        let plugin_id = plugin.id().as_str().to_string();
        if plugin_id != manifest.id {
            return Err(PluginError::IdMismatch {
                manifest: manifest.id.clone(),
                dll: plugin_id,
            });
        }

        info!(
            "Plugin loaded successfully: {} v{}",
            manifest.name, manifest.version
        );

        Ok((manifest, library, plugin))
    }

    /// Check if host version meets minimum requirement
    fn check_version_compatibility(&self, min_version: &str) -> bool {
        // Simple semver comparison (major.minor.patch)
        let parse_version = |v: &str| -> Option<(u32, u32, u32)> {
            let parts: Vec<&str> = v.split('.').collect();
            if matches!(parts.len(), 2 | 3) {
                let major = parts[0].parse().ok()?;
                let minor = parts[1].parse().ok()?;
                let patch = match parts.get(2) {
                    Some(p) => p.parse().ok()?,
                    None => 0,
                };
                Some((major, minor, patch))
            } else {
                None
            }
        };

        match (
            parse_version(&self.host_version),
            parse_version(min_version),
        ) {
            (Some(host), Some(required)) => {
                host.0 > required.0
                    || (host.0 == required.0 && host.1 > required.1)
                    || (host.0 == required.0 && host.1 == required.1 && host.2 >= required.2)
            }
            _ => {
                warn!(
                    "Could not parse versions: host={}, required={}",
                    self.host_version, min_version
                );
                false // Invalid minimum versions do not authorize loading
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_compatibility() {
        let loader = PluginLoader::new(PluginVerifier::development_mode(), "0.2.0");

        assert!(loader.check_version_compatibility("0.1.0"));
        assert!(loader.check_version_compatibility("0.2.0"));
        assert!(!loader.check_version_compatibility("0.3.0"));
        assert!(!loader.check_version_compatibility("1.0.0"));
        for invalid in ["invalid", "0.2.bad", "0.2.0.4", "", "0"] {
            assert!(!loader.check_version_compatibility(invalid));
        }
    }
}
