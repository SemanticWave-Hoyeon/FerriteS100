//! Lua Session Management
//!
//! Wraps the Lua state and provides high-level API for executing portrayal rules.
//! Based on S-100 standard's lua_session.cpp

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mlua::Lua;

use crate::{
    CellData, ContextParameters, FeatureInfo, HostFunctions, LuaError, PortrayalResult, Result,
    SpatialInfo, TypeCatalogue,
};

/// Lua session for portrayal rule execution
pub struct LuaSession {
    lua: Lua,
    host: HostFunctions,
    rules_path: PathBuf,
    initialized: bool,
}

impl LuaSession {
    /// Create new Lua session with sandboxed environment
    pub fn new() -> Result<Self> {
        let lua = Lua::new();

        // Sandbox: Disable dangerous package library functions
        // This prevents loading arbitrary C modules or searching system paths
        Self::sandbox_lua(&lua)?;

        let host = HostFunctions::new();

        // Register host functions
        host.register(&lua)?;

        Ok(LuaSession {
            lua,
            host,
            rules_path: PathBuf::new(),
            initialized: false,
        })
    }

    /// Apply security sandbox to Lua state
    /// Disables dangerous functions that could be exploited
    fn sandbox_lua(lua: &Lua) -> Result<()> {
        let globals = lua.globals();

        // Disable loadfile/dofile (load arbitrary files)
        globals.set("loadfile", mlua::Value::Nil)?;
        globals.set("dofile", mlua::Value::Nil)?;

        // Restrict package library
        if let Ok(package) = globals.get::<mlua::Table>("package") {
            // Disable C module loading (prevents loading arbitrary .dll/.so)
            package.set("loadlib", mlua::Value::Nil)?;
            package.set("cpath", "")?;

            // Clear searchers except for preload and Lua file loader
            // This prevents searching system paths for modules
            if let Ok(searchers) = package.get::<mlua::Table>("searchers") {
                // Keep only first two searchers (preload, lua loader)
                // Remove C loader and all-in-one loader
                searchers.set(3, mlua::Value::Nil)?;
                searchers.set(4, mlua::Value::Nil)?;
            }
        }

        tracing::debug!(
            "Lua sandbox applied: disabled loadfile, dofile, package.loadlib, C module loading"
        );
        Ok(())
    }

    /// Set the rules path (directory containing Lua scripts)
    pub fn set_rules_path<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref().to_path_buf();

        if !path.exists() {
            return Err(LuaError::ScriptNotFound(path.display().to_string()));
        }

        self.rules_path = path.clone();

        // Set Lua package.path
        let path_str = path.to_string_lossy();
        let lua_path = format!("{}\\?.lua;{}/?.lua", path_str, path_str);

        self.lua
            .globals()
            .get::<mlua::Table>("package")?
            .set("path", lua_path)?;

        tracing::debug!("Lua rules path set to: {}", path.display());

        Ok(())
    }

    /// Load the main portrayal script
    pub fn load_main(&mut self) -> Result<()> {
        let main_path = self.rules_path.join("main.lua");

        if !main_path.exists() {
            return Err(LuaError::ScriptNotFound(main_path.display().to_string()));
        }

        let script = std::fs::read_to_string(&main_path)
            .map_err(|e| LuaError::ScriptNotFound(e.to_string()))?;

        self.lua
            .load(&script)
            .set_name(main_path.to_string_lossy())
            .exec()?;

        self.initialized = true;
        tracing::info!("Loaded main portrayal script: {}", main_path.display());

        Ok(())
    }

    /// Set feature and spatial data for the session
    pub fn set_data(
        &mut self,
        features: HashMap<i64, FeatureInfo>,
        spatials: HashMap<i64, SpatialInfo>,
    ) {
        self.host.set_features(features);
        self.host.set_spatials(spatials);
    }

    /// Set all cell data (features, information types, spatials, associations)
    pub fn set_cell_data(&mut self, cell_data: &CellData) {
        self.host.from_cell_data(cell_data);
    }

    /// Set type catalogue (from Feature Catalogue)
    pub fn set_type_catalogue(&mut self, catalogue: TypeCatalogue) {
        self.host.set_type_catalogue(catalogue);
    }

    /// Set context parameters
    pub fn set_context(&mut self, params: ContextParameters) {
        self.host.set_context(params);
    }

    /// Initialize the portrayal context with context parameters
    /// Context parameters are now dynamically loaded from PC XML via ContextParameters::from_pc_context()
    pub fn initialize_context(&mut self, params: &ContextParameters) -> Result<()> {
        // Get PortrayalCreateContextParameter function to properly convert values
        // This function calls ConvertEncodedValue which creates ScaledDecimal for "real" types
        let create_param_func: mlua::Function = self
            .lua
            .globals()
            .get("PortrayalCreateContextParameter")
            .map_err(|_| {
                LuaError::FunctionNotFound("PortrayalCreateContextParameter".to_string())
            })?;

        // Create context parameters array for Lua
        let context_params = self.lua.create_table()?;

        // Use to_lua_params() to get parameters dynamically (from PC XML)
        // This removes hardcoded parameter names and follows S-100 standard pattern
        for (name, param_type, value_str) in params.to_lua_params() {
            let param: mlua::Table =
                create_param_func.call((name.as_str(), param_type.as_str(), value_str.as_str()))?;
            context_params.push(param)?;
        }

        // Call PortrayalInitializeContextParameters
        let init_func: mlua::Function = self
            .lua
            .globals()
            .get("PortrayalInitializeContextParameters")
            .map_err(|_| {
                LuaError::FunctionNotFound("PortrayalInitializeContextParameters".to_string())
            })?;

        init_func.call::<()>(context_params)?;

        tracing::debug!(
            "Portrayal context initialized with {} parameters",
            params.to_lua_params().len()
        );
        Ok(())
    }

    /// Reset the Lua state for a new cell
    /// This clears all caches (feature, information, spatial) to prevent cross-cell contamination
    pub fn reset_for_new_cell(&mut self) -> Result<()> {
        // Create fresh Lua state
        self.lua = Lua::new();

        // Apply sandbox to new Lua state
        Self::sandbox_lua(&self.lua)?;

        self.host = HostFunctions::new();

        // Re-register host functions
        self.host.register(&self.lua)?;

        // Reset initialized flag
        self.initialized = false;

        // Re-set package path
        if !self.rules_path.as_os_str().is_empty() {
            let path_str = self.rules_path.to_string_lossy();
            let lua_path = format!("{}\\?.lua;{}/?.lua", path_str, path_str);

            self.lua
                .globals()
                .get::<mlua::Table>("package")?
                .set("path", lua_path)?;
        }

        // Re-load main script
        let main_path = self.rules_path.join("main.lua");
        if main_path.exists() {
            let script = std::fs::read_to_string(&main_path)
                .map_err(|e| LuaError::ScriptNotFound(e.to_string()))?;

            self.lua
                .load(&script)
                .set_name(main_path.to_string_lossy())
                .exec()?;

            self.initialized = true;
        }

        Ok(())
    }

    /// Execute portrayal for all features
    pub fn execute_portrayal(&mut self) -> Result<Vec<PortrayalResult>> {
        if !self.initialized {
            return Err(LuaError::Portrayal("Session not initialized".to_string()));
        }

        // Clear previous results
        self.host.clear_results();

        // Call PortrayalMain()
        let portrayal_main: mlua::Function = self
            .lua
            .globals()
            .get("PortrayalMain")
            .map_err(|_| LuaError::FunctionNotFound("PortrayalMain".to_string()))?;

        let result: bool = portrayal_main.call(mlua::Value::Nil)?;

        if !result {
            tracing::warn!("PortrayalMain returned false");
        }

        Ok(self.host.get_results())
    }

    /// Execute portrayal for specific feature IDs
    pub fn execute_portrayal_for(&mut self, feature_ids: Vec<i64>) -> Result<Vec<PortrayalResult>> {
        if !self.initialized {
            return Err(LuaError::Portrayal("Session not initialized".to_string()));
        }

        self.host.clear_results();

        // Create Lua table with feature IDs
        let ids_table = self.lua.create_table()?;
        for (i, id) in feature_ids.iter().enumerate() {
            ids_table.set(i + 1, *id)?;
        }

        // Call PortrayalMain(featureIDs)
        let portrayal_main: mlua::Function = self
            .lua
            .globals()
            .get("PortrayalMain")
            .map_err(|_| LuaError::FunctionNotFound("PortrayalMain".to_string()))?;

        let _: bool = portrayal_main.call(ids_table)?;

        Ok(self.host.get_results())
    }

    /// Execute a simple Lua expression
    pub fn eval<T: mlua::FromLuaMulti>(&self, expr: &str) -> Result<T> {
        Ok(self.lua.load(expr).eval()?)
    }

    /// Load and execute a Lua file
    pub fn load_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let script = std::fs::read_to_string(path)
            .map_err(|e| LuaError::ScriptNotFound(format!("{}: {}", path.display(), e)))?;

        self.lua
            .load(&script)
            .set_name(path.to_string_lossy())
            .exec()?;

        Ok(())
    }

    /// Call a Lua function by name
    pub fn call_function<T: mlua::FromLuaMulti>(
        &self,
        name: &str,
        args: impl mlua::IntoLuaMulti,
    ) -> Result<T> {
        let func: mlua::Function = self
            .lua
            .globals()
            .get(name)
            .map_err(|_| LuaError::FunctionNotFound(name.to_string()))?;

        Ok(func.call(args)?)
    }

    /// Check if a function exists
    pub fn has_function(&self, name: &str) -> bool {
        self.lua.globals().get::<mlua::Function>(name).is_ok()
    }

    /// Get the Lua state for advanced usage
    pub fn lua(&self) -> &Lua {
        &self.lua
    }
}

impl Default for LuaSession {
    fn default() -> Self {
        Self::new().expect("Failed to create Lua session")
    }
}

/// Portrayal engine that manages Lua execution
pub struct PortrayalEngine {
    session: LuaSession,
    /// Stored type catalogue for re-application after reset
    type_catalogue: Option<TypeCatalogue>,
}

impl PortrayalEngine {
    /// Create new portrayal engine with rules path
    pub fn new<P: AsRef<Path>>(rules_path: P) -> Result<Self> {
        let mut session = LuaSession::new()?;
        session.set_rules_path(rules_path)?;

        Ok(PortrayalEngine {
            session,
            type_catalogue: None,
        })
    }

    /// Initialize the engine (load main script)
    pub fn initialize(&mut self) -> Result<()> {
        self.session.load_main()
    }

    /// Initialize with context parameters (must be called once after initialize)
    pub fn initialize_context(&mut self, params: &ContextParameters) -> Result<()> {
        self.session.initialize_context(params)
    }

    /// Set type catalogue (from Feature Catalogue)
    pub fn set_type_catalogue(&mut self, catalogue: TypeCatalogue) {
        self.type_catalogue = Some(catalogue.clone());
        self.session.set_type_catalogue(catalogue);
    }

    /// Process features and generate drawing instructions
    pub fn process(
        &mut self,
        features: HashMap<i64, FeatureInfo>,
        spatials: HashMap<i64, SpatialInfo>,
        context: ContextParameters,
    ) -> Result<Vec<PortrayalResult>> {
        self.session.set_data(features, spatials);
        self.session.set_context(context.clone());
        self.session.execute_portrayal()
    }

    /// Process cell data and generate drawing instructions
    /// Note: This reinitializes the Lua state for each cell to clear caches
    pub fn process_cell(
        &mut self,
        cell_data: &CellData,
        context: ContextParameters,
    ) -> Result<Vec<PortrayalResult>> {
        // Reset Lua state to clear all caches (feature, information, spatial)
        // This prevents cross-cell contamination from cached feature data
        self.session.reset_for_new_cell()?;

        // Re-apply type catalogue after reset
        if let Some(ref catalogue) = self.type_catalogue {
            self.session.set_type_catalogue(catalogue.clone());
        }

        // Set cell data BEFORE initializing context (so HostGetFeatureIDs works)
        self.session.set_cell_data(cell_data);
        self.session.set_context(context.clone());

        // Initialize portrayal context to populate FeaturePortrayalItems
        self.session.initialize_context(&context)?;

        // Execute portrayal
        self.session.execute_portrayal()
    }

    /// Get session for advanced usage
    pub fn session(&self) -> &LuaSession {
        &self.session
    }

    /// Get mutable session
    pub fn session_mut(&mut self) -> &mut LuaSession {
        &mut self.session
    }
}
