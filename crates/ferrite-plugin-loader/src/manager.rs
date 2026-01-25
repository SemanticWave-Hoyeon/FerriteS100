//! Plugin Manager
//!
//! Manages loading, unloading, and lifecycle of plugins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use abi_stable::std_types::RBox;
use libloading::Library;
use tracing::{debug, info, warn};

use ferrite_plugin_api::Plugin_TO;

use crate::error::PluginError;
use crate::host_impl::{create_host_api, HostContext, SharedHostContext};
use crate::loader::PluginLoader;
use crate::verifier::PluginVerifier;
use crate::PluginManifest;

/// A loaded and active plugin
struct ActivePlugin {
    manifest: PluginManifest,
    instance: Plugin_TO<'static, RBox<()>>,
    _library: Library, // Keep library alive
}

/// Plugin manager handles plugin lifecycle
pub struct PluginManager {
    /// Plugin loader
    loader: PluginLoader,
    /// Directory containing plugins
    plugins_dir: PathBuf,
    /// Loaded plugins by ID
    plugins: HashMap<String, ActivePlugin>,
    /// Shared host context
    host_context: SharedHostContext,
}

impl PluginManager {
    /// Create a new plugin manager
    pub fn new(plugins_dir: PathBuf, host_version: &str, development_mode: bool) -> Self {
        let verifier = if development_mode {
            PluginVerifier::development_mode()
        } else {
            // In production, load public key from embedded resource
            // For now, use development mode
            warn!("Production signature verification not yet implemented, using development mode");
            PluginVerifier::development_mode()
        };

        let loader = PluginLoader::new(verifier, host_version);
        let host_context = Arc::new(Mutex::new(HostContext::default()));

        Self {
            loader,
            plugins_dir,
            plugins: HashMap::new(),
            host_context,
        }
    }

    /// Get mutable reference to host context for configuration
    pub fn host_context_mut(&self) -> SharedHostContext {
        self.host_context.clone()
    }

    /// Update host context values
    pub fn update_context<F>(&self, updater: F)
    where
        F: FnOnce(&mut HostContext),
    {
        let mut guard = self.host_context.lock().unwrap();
        updater(&mut guard);
    }

    /// Discover all plugins in the plugins directory
    pub fn discover_plugins(&self) -> Vec<PathBuf> {
        let mut plugin_dirs = Vec::new();

        if !self.plugins_dir.exists() {
            warn!(
                "Plugins directory not found: {}",
                self.plugins_dir.display()
            );
            return plugin_dirs;
        }

        if let Ok(entries) = std::fs::read_dir(&self.plugins_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() && path.join("manifest.json").exists() {
                    plugin_dirs.push(path);
                }
            }
        }

        debug!("Discovered {} plugins", plugin_dirs.len());
        plugin_dirs
    }

    /// Load a specific plugin by directory
    pub fn load_plugin(&mut self, plugin_dir: &Path) -> Result<String, PluginError> {
        let (manifest, library, mut instance) = self.loader.load_plugin(plugin_dir)?;

        let plugin_id = manifest.id.clone();

        if self.plugins.contains_key(&plugin_id) {
            return Err(PluginError::AlreadyLoaded(plugin_id));
        }

        // Initialize plugin with host API
        let host_api = create_host_api(self.host_context.clone());
        instance.initialize(host_api);

        info!("Plugin initialized: {}", plugin_id);

        self.plugins.insert(
            plugin_id.clone(),
            ActivePlugin {
                manifest,
                instance,
                _library: library,
            },
        );

        Ok(plugin_id)
    }

    /// Load all discovered plugins
    pub fn load_all_plugins(&mut self) -> Vec<Result<String, PluginError>> {
        let plugin_dirs = self.discover_plugins();
        let mut results = Vec::new();

        for dir in plugin_dirs {
            results.push(self.load_plugin(&dir));
        }

        results
    }

    /// Unload a plugin by ID
    pub fn unload_plugin(&mut self, plugin_id: &str) -> Result<(), PluginError> {
        if let Some(mut plugin) = self.plugins.remove(plugin_id) {
            plugin.instance.shutdown();
            info!("Plugin unloaded: {}", plugin_id);
            Ok(())
        } else {
            Err(PluginError::PluginNotFound(plugin_id.to_string()))
        }
    }

    /// Get list of loaded plugins
    pub fn loaded_plugins(&self) -> Vec<&PluginManifest> {
        self.plugins.values().map(|p| &p.manifest).collect()
    }

    /// Check if a plugin is loaded
    pub fn is_loaded(&self, plugin_id: &str) -> bool {
        self.plugins.contains_key(plugin_id)
    }

    /// Get a plugin instance by ID
    pub fn get_plugin(&self, plugin_id: &str) -> Option<&Plugin_TO<'static, RBox<()>>> {
        self.plugins.get(plugin_id).map(|p| &p.instance)
    }

    /// Get a mutable plugin instance by ID
    pub fn get_plugin_mut(&mut self, plugin_id: &str) -> Option<&mut Plugin_TO<'static, RBox<()>>> {
        self.plugins.get_mut(plugin_id).map(|p| &mut p.instance)
    }

    /// Get all active plugin instances
    pub fn active_plugins(&self) -> impl Iterator<Item = &Plugin_TO<'static, RBox<()>>> {
        self.plugins
            .values()
            .filter(|p| p.instance.is_active())
            .map(|p| &p.instance)
    }

    /// Get all active plugin instances (mutable)
    pub fn active_plugins_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut Plugin_TO<'static, RBox<()>>> {
        self.plugins
            .values_mut()
            .filter(|p| p.instance.is_active())
            .map(|p| &mut p.instance)
    }

    /// Get ALL loaded plugin instances (regardless of active state)
    pub fn all_plugins(&self) -> impl Iterator<Item = &Plugin_TO<'static, RBox<()>>> {
        self.plugins.values().map(|p| &p.instance)
    }

    /// Shutdown all plugins
    pub fn shutdown_all(&mut self) {
        for (id, mut plugin) in self.plugins.drain() {
            plugin.instance.shutdown();
            info!("Plugin shutdown: {}", id);
        }
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}
