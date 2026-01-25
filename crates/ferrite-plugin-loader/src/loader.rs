//! Plugin DLL loader
//!
//! Safely loads plugin DLLs and extracts the plugin module.

use std::path::Path;

use abi_stable::std_types::RBox;
use libloading::Library;
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
    ) -> Result<(PluginManifest, Library, Plugin_TO<'static, RBox<()>>), PluginError> {
        // Load manifest
        let manifest_path = plugin_dir.join("manifest.json");
        if !manifest_path.exists() {
            return Err(PluginError::ManifestNotFound(
                manifest_path.display().to_string(),
            ));
        }

        let manifest_content = std::fs::read_to_string(&manifest_path)?;
        let manifest: PluginManifest = serde_json::from_str(&manifest_content)
            .map_err(|e| PluginError::InvalidManifest(e.to_string()))?;

        info!("Loading plugin: {} v{}", manifest.name, manifest.version);

        // Check host version compatibility
        if !self.check_version_compatibility(&manifest.min_host_version) {
            return Err(PluginError::HostVersionTooOld {
                required: manifest.min_host_version.clone(),
                current: self.host_version.clone(),
            });
        }

        // Locate DLL
        let dll_path = plugin_dir.join(&manifest.dll_file);
        if !dll_path.exists() {
            return Err(PluginError::LoadError(format!(
                "DLL not found: {}",
                dll_path.display()
            )));
        }

        // Verify hash
        self.verifier.verify_hash(&dll_path, &manifest.dll_hash)?;

        // Verify signature (if required)
        self.verifier.verify_signature(&dll_path)?;

        // Load DLL
        debug!("Loading DLL: {}", dll_path.display());
        let library = unsafe { Library::new(&dll_path) }
            .map_err(|e| PluginError::LoadError(e.to_string()))?;

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
            if parts.len() >= 2 {
                let major = parts[0].parse().ok()?;
                let minor = parts[1].parse().ok()?;
                let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
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
                true // Allow if version parsing fails
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
    }
}
