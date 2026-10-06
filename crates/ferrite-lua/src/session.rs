//! Lua Session Management
//!
//! Wraps the Lua state and provides high-level API for executing portrayal rules.
//! Based on S-100 standard's lua_session.cpp

use crate::mlua;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    sources: Option<Arc<ferrite_portrayal_catalog::CatalogueSources>>,
    initialized: bool,
    chunk_cache: ferrite_lua_runtime::ChunkCache,
}

impl LuaSession {
    /// Create new Lua session with sandboxed environment
    pub fn new() -> Result<Self> {
        let lua = ferrite_lua_runtime::new_vm();

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
            sources: None,
            initialized: false,
            chunk_cache: Default::default(),
        })
    }

    /// Apply security sandbox to Lua state
    /// Disables dangerous functions that could be exploited
    fn sandbox_lua(lua: &Lua) -> Result<()> {
        let globals = lua.globals();

        // Older IHO catalogues target Lua 5.1 and call the global unpack.
        let table: mlua::Table = globals.get("table")?;
        globals.set("unpack", table.get::<mlua::Function>("unpack")?)?;

        // Disable loadfile/dofile (load arbitrary files)
        globals.set("loadfile", mlua::Value::Nil)?;
        globals.set("dofile", mlua::Value::Nil)?;

        // Restrict package library
        if let Ok(package) = globals.get::<mlua::Table>("package") {
            // Disable C module loading (prevents loading arbitrary .dll/.so)
            package.set("loadlib", mlua::Value::Nil)?;
            package.set("cpath", "")?;

            // Replace all searchers with only preload (searcher #1).
            // Searcher #2 (Lua file loader) uses simple string substitution
            // on package.path and could allow path traversal via module names
            // containing ".." or path separators.
            // We add a custom safe searcher that validates the resolved path.
            if let Ok(searchers) = package.get::<mlua::Table>("searchers") {
                // Remove searchers #2, #3, #4 (lua file, C loader, all-in-one)
                searchers.set(2, mlua::Value::Nil)?;
                searchers.set(3, mlua::Value::Nil)?;
                searchers.set(4, mlua::Value::Nil)?;
            }
        }

        tracing::debug!(
            "Lua sandbox applied: disabled loadfile, dofile, package.loadlib, C module loading"
        );
        Ok(())
    }

    /// Set the rules path (directory containing Lua scripts).
    ///
    /// Security: rejects paths containing `..` components to prevent
    /// directory traversal.  Lua `package.path` is restricted to this directory.
    pub fn set_rules_path<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref().to_path_buf();

        if !path.exists() {
            return Err(LuaError::ScriptNotFound(path.display().to_string()));
        }

        // Reject path traversal components
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err(LuaError::ScriptNotFound(format!(
                    "Security: rules path contains '..' traversal: {}",
                    path.display()
                )));
            }
        }

        self.rules_path = path.clone();
        self.sources = None;

        // Install a custom safe searcher that validates resolved paths
        // stay within the rules directory (prevents require("../../evil") escapes)
        Self::install_safe_searcher(&self.lua, &path, self.chunk_cache.clone(), None)?;

        tracing::debug!("Lua rules path set to: {}", path.display());

        Ok(())
    }

    /// Use a retained immutable PC map; no live disk fallback is permitted.
    pub fn set_rules_sources(&mut self, sources: Arc<ferrite_portrayal_catalog::CatalogueSources>) -> Result<()> {
        let path = sources.root_path().join("Rules");
        self.rules_path = path.clone();
        self.sources = Some(Arc::clone(&sources));
        Self::install_safe_searcher(&self.lua, &path, self.chunk_cache.clone(), Some(sources))
    }

    fn main_available(&self, path: &Path) -> bool {
        match &self.sources { Some(s) => s.read_path(path).is_ok(), None => path.exists() }
    }

    /// Install a custom Lua searcher that validates resolved file paths
    /// stay within the allowed rules directory. This prevents `require("../../evil")`
    /// from escaping the sandbox via path traversal in module names.
    fn install_safe_searcher(
        lua: &Lua,
        rules_dir: &Path,
        cache: ferrite_lua_runtime::ChunkCache,
        sources: Option<Arc<ferrite_portrayal_catalog::CatalogueSources>>,
    ) -> Result<()> {
        if let Some(sources) = sources {
            let rules = rules_dir.to_path_buf();
            let searcher = lua.create_function(move |lua, module_name: String| {
                if module_name.contains('/') || module_name.contains('\\') || module_name.split('.').any(str::is_empty) {
                    return Err(mlua::Error::RuntimeError("PC snapshot require path escape rejected".into()));
                }
                let relative = module_name.replace('.', std::path::MAIN_SEPARATOR_STR);
                let path = rules.join(format!("{relative}.lua"));
                let source = sources.read_path(&path).map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                cache.compile(lua, &source, &format!("@{}", path.display())).map(mlua::Value::Function)
            })?;
            let package: mlua::Table = lua.globals().get("package")?;
            let searchers: mlua::Table = package.get("searchers")?;
            searchers.set(2, searcher)?;
            package.set("path", "")?;
            return Ok(());
        }
        let canonical_rules = rules_dir.canonicalize().map_err(|e| {
            LuaError::ScriptNotFound(format!("Cannot canonicalize rules path: {}", e))
        })?;

        let searcher = lua.create_function(move |lua, module_name: String| {
            // Replace Lua module separator '.' with OS path separator
            let rel_path = module_name.replace('.', std::path::MAIN_SEPARATOR_STR);
            let file_path = canonical_rules.join(format!("{}.lua", rel_path));

            // Canonicalize to resolve any ".." or symlinks, then verify prefix
            let canonical = match file_path.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    // File doesn't exist — return nil + message (standard Lua searcher protocol)
                    return Ok(mlua::Value::Nil);
                }
            };

            if !canonical.starts_with(&canonical_rules) {
                tracing::warn!(
                    "Security: require('{}') resolved to '{}' outside rules directory, blocked",
                    module_name,
                    canonical.display()
                );
                return Ok(mlua::Value::Nil);
            }

            // Read and return a loader function
            let source = match std::fs::read(&canonical) {
                Ok(s) => s,
                Err(_) => return Ok(mlua::Value::Nil),
            };

            let chunk_name = format!("@{}", canonical.display());
            let func = cache.compile(lua, &source, &chunk_name)?;

            Ok(mlua::Value::Function(func))
        })?;

        // Add as searcher #2
        let package: mlua::Table = lua.globals().get("package")?;
        let searchers: mlua::Table = package.get("searchers")?;
        searchers.set(2, searcher)?;

        // Clear package.path since our custom searcher doesn't use it
        package.set("path", "")?;

        Ok(())
    }

    fn read_main_source(&self, main_path: &Path) -> Result<Vec<u8>> {
        if let Some(sources) = &self.sources {
            if main_path.parent() != Some(self.rules_path.as_path()) {
                return Err(LuaError::ScriptNotFound("PC snapshot main path escapes rules root".into()));
            }
            return sources.read_path(main_path).map(|b| b.to_vec()).map_err(|e| LuaError::ScriptNotFound(e.to_string()));
        }
        let root = self
            .rules_path
            .canonicalize()
            .map_err(|e| LuaError::ScriptNotFound(e.to_string()))?;
        let main = main_path
            .canonicalize()
            .map_err(|e| LuaError::ScriptNotFound(e.to_string()))?;
        if main.parent() != Some(root.as_path()) {
            return Err(LuaError::ScriptNotFound(format!(
                "Security: main script {} escapes rules root {}",
                main.display(),
                root.display()
            )));
        }
        std::fs::read(main).map_err(|e| LuaError::ScriptNotFound(e.to_string()))
    }

    /// Load the main portrayal script.
    ///
    /// Security: verifies the script file is a direct child of `rules_path`
    /// (no traversal via symlinks or `..`).
    pub fn load_main(&mut self) -> Result<()> {
        let main_path = self.rules_path.join("main.lua");

        if !self.main_available(&main_path) {
            return Err(LuaError::ScriptNotFound(main_path.display().to_string()));
        }

        // Verify parent directory matches rules_path (catches symlink escapes)
        if let Some(parent) = main_path.parent() {
            if parent != self.rules_path {
                return Err(LuaError::ScriptNotFound(format!(
                    "Security: script parent {} != rules_path {}",
                    parent.display(),
                    self.rules_path.display()
                )));
            }
        }

        let script = self.read_main_source(&main_path)?;

        self.chunk_cache
            .compile(&self.lua, &script, &main_path.to_string_lossy())?
            .call::<()>(())?;

        self.install_decimal_equality_compatibility()?;
        self.initialized = true;
        tracing::info!("Loaded main portrayal script: {}", main_path.display());

        Ok(())
    }

    /// Lua 5.1 only invokes table equality when both operands share __eq.
    /// Lua 5.4 invokes either operand's method. Legacy PC compares a scaled
    /// decimal with the unknown-value sentinel and assumes the 5.1 behavior.
    fn install_decimal_equality_compatibility(&self) -> Result<()> {
        self.lua
            .load(
                r#"
            if CreateScaledDecimal then
                local sample = CreateScaledDecimal(0, 0)
                local mt = getmetatable(sample)
                if mt and mt.__eq then
                    local original = mt.__eq
                    mt.__eq = function(a, b)
                        if type(a) ~= 'table' or type(b) ~= 'table'
                            or rawget(a, 'Type') ~= 'ScaledDecimal'
                            or rawget(b, 'Type') ~= 'ScaledDecimal' then
                            return false
                        end
                        return original(a, b)
                    end
                end
            end
        "#,
            )
            .set_name("@host-decimal-compatibility")
            .exec()?;
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
        self.lua = ferrite_lua_runtime::new_vm();

        // Apply sandbox to new Lua state
        Self::sandbox_lua(&self.lua)?;

        self.host = HostFunctions::new();

        // Re-register host functions
        self.host.register(&self.lua)?;

        // Reset initialized flag
        self.initialized = false;

        // Re-install safe searcher
        if !self.rules_path.as_os_str().is_empty() {
            Self::install_safe_searcher(
                &self.lua,
                &self.rules_path.clone(),
                self.chunk_cache.clone(),
                self.sources.clone(),
            )?;
        }

        // Re-load main script
        let main_path = self.rules_path.join("main.lua");
        if self.main_available(&main_path) {
            let script = self.read_main_source(&main_path)?;

            self.chunk_cache
                .compile(&self.lua, &script, &main_path.to_string_lossy())?
                .call::<()>(())?;

            self.install_decimal_equality_compatibility()?;
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

        let result = portrayal_main.call::<bool>(mlua::Value::Nil);
        self.completed_results(result)
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

        let result = portrayal_main.call::<bool>(ids_table);
        self.completed_results(result)
    }

    /// S-100 9a-14.1.1: false means terminated, never a completed portrayal.
    fn completed_results(&self, result: mlua::Result<bool>) -> Result<Vec<PortrayalResult>> {
        match result {
            Ok(true) => Ok(self.host.take_results()),
            Ok(false) => {
                self.host.clear_results();
                Err(LuaError::Portrayal(
                    "PortrayalMain terminated before completion".into(),
                ))
            }
            Err(error) => {
                self.host.clear_results();
                Err(error.into())
            }
        }
    }

    /// Execute a simple Lua expression
    pub fn eval<T: mlua::FromLuaMulti>(&self, expr: &str) -> Result<T> {
        Ok(self.lua.load(expr).eval()?)
    }

    /// Load and execute a Lua file
    pub fn load_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let script = match &self.sources {
            Some(s) => s.read_path(path.as_ref()).map(|b| b.to_vec()).map_err(|e| std::io::Error::other(e.to_string())),
            None => std::fs::read(path),
        }
            .map_err(|e| LuaError::ScriptNotFound(format!("{}: {}", path.display(), e)))?;

        self.lua
            .load(script.as_slice())
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

    pub fn chunk_cache_stats(&self) -> ferrite_lua_runtime::ChunkCacheStats {
        self.chunk_cache.stats()
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
    type_catalogue: Option<Arc<TypeCatalogue>>,
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

    pub fn new_with_sources(sources: Arc<ferrite_portrayal_catalog::CatalogueSources>) -> Result<Self> {
        Self::new_with_sources_and_chunk_cache(sources, ferrite_lua_runtime::ChunkCache::process_shared())
    }

    /// Choose a bounded compiler cache without changing snapshot/VM semantics.
    /// A fresh Default cache reproduces engine-local retention; a zero-budget
    /// cache disables retention. Neither choice can reuse execution results.
    pub fn new_with_sources_and_chunk_cache(
        sources: Arc<ferrite_portrayal_catalog::CatalogueSources>,
        cache: ferrite_lua_runtime::ChunkCache,
    ) -> Result<Self> {
        let mut session = LuaSession::new()?;
        session.chunk_cache = cache;
        session.set_rules_sources(sources)?;
        Ok(Self { session, type_catalogue: None })
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
        let catalogue = Arc::new(catalogue);
        self.type_catalogue = Some(Arc::clone(&catalogue));
        self.session.host.set_shared_type_catalogue(catalogue);
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
            self.session
                .host
                .set_shared_type_catalogue(Arc::clone(catalogue));
        }

        // Set cell data BEFORE initializing context (so HostGetFeatureIDs works)
        self.session.set_cell_data(cell_data);
        self.session.set_context(context.clone());

        // Initialize portrayal context to populate FeaturePortrayalItems
        self.session.initialize_context(&context)?;

        // Execute against fresh host data even when compiler output is reused.
        let result = self.session.execute_portrayal();
        let stats = self.session.chunk_cache_stats();
        tracing::info!("Lua compiler cache: compiled={} hits={} entries={} retained_payload_bytes={} bypasses={}",
            stats.compiled_chunks, stats.bytecode_hits, stats.entries, stats.retained_payload_bytes, stats.bypasses);
        result
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

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn legacy_decimal_sentinel_equality_and_unpack() {
        let session = LuaSession::new().unwrap();
        session.lua.load(r#"
            local mt={__eq=function(a,b) return a.Scale == b.Scale and a.Value == b.Value end}
            function CreateScaledDecimal(v,s) return setmetatable({Type='ScaledDecimal',Value=v,Scale=s},mt) end
        "#).exec().unwrap();
        session.install_decimal_equality_compatibility().unwrap();
        let ok: bool = session
            .lua
            .load(
                r#"
            local a=CreateScaledDecimal(10,1)
            local b=CreateScaledDecimal(10,1)
            local c=CreateScaledDecimal(20,1)
            local x,y=unpack({3,4})
            return a == b and a ~= c and a ~= {Type='UnknownValue'} and x == 3 and y == 4
        "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn successful_repeated_execution_returns_independent_output_without_retained_copy() {
        for selected in [false, true] {
            let mut session = LuaSession::new().unwrap();
            session.initialized = true;
            session.lua.load("iteration = 0; function PortrayalMain(ids) iteration = iteration + 1; HostPortrayalEmit(tostring(iteration), 'PointInstruction:WRECKS01', 'SafetyDepth'); return true end").exec().unwrap();
            let first = if selected {
                session.execute_portrayal_for(vec![1])
            } else {
                session.execute_portrayal()
            }
            .unwrap();
            assert!(session.host.get_results().is_empty());
            let second = if selected {
                session.execute_portrayal_for(vec![2])
            } else {
                session.execute_portrayal()
            }
            .unwrap();
            assert_eq!(first[0].feature_id, "1");
            assert_eq!(second[0].feature_id, "2");
            assert_eq!(first[0].observed_parameters, ["SafetyDepth"]);
            assert!(session.host.get_results().is_empty());
        }
    }

    #[test]
    fn incomplete_portrayal_discards_emitted_results_for_all_and_selected_features() {
        for selected in [false, true] {
            for ending in [
                "return false",
                "error('injected failure')",
                "HostPortrayalEmit('2', 'ColorFill:DEPVS,2', ''); return true",
            ] {
                let mut session = LuaSession::new().unwrap();
                session.initialized = true;
                session.lua.load(format!("function PortrayalMain(ids) HostPortrayalEmit('1','PointInstruction:WRECKS01',''); {ending} end")).exec().unwrap();
                let result = if selected {
                    session.execute_portrayal_for(vec![1, 2])
                } else {
                    session.execute_portrayal()
                };
                assert!(result.is_err(), "{selected}: {ending}");
                assert!(
                    session.host.get_results().is_empty(),
                    "Partial results survived failure"
                );
                session.lua.load("function PortrayalMain(ids) HostPortrayalEmit('3','PointInstruction:WRECKS01',''); return true end").exec().unwrap();
                let results = if selected {
                    session.execute_portrayal_for(vec![3])
                } else {
                    session.execute_portrayal()
                }
                .unwrap();
                assert_eq!(
                    results.len(),
                    1,
                    "Successful retry contains stale partial results"
                );
                assert_eq!(results[0].feature_id, "3");
            }
        }
    }
}

#[cfg(test)]
mod chunk_cache_tests {
    use super::*;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn directory() -> PathBuf {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("ferrite-chunk-{}-{n}", std::process::id()));
        std::fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn resetting_cells_reuses_compilation_but_not_global_or_module_state() {
        let dir = directory();
        std::fs::write(
            dir.join("main.lua"),
            "counter=(counter or 0)+1; require('child')",
        )
        .unwrap();
        std::fs::write(dir.join("child.lua"), "child=(child or 0)+1").unwrap();
        let mut s = LuaSession::new().unwrap();
        s.set_rules_path(&dir).unwrap();
        s.load_main().unwrap();
        for _ in 0..2 {
            s.reset_for_new_cell().unwrap();
            assert_eq!(s.lua.globals().get::<i64>("counter").unwrap(), 1);
            assert_eq!(s.lua.globals().get::<i64>("child").unwrap(), 1);
        }
        assert_eq!(s.chunk_cache_stats().compiled_chunks, 2);
        assert_eq!(s.chunk_cache_stats().bytecode_hits, 4);
        std::fs::write(dir.join("child.lua"), "child=77").unwrap();
        s.reset_for_new_cell().unwrap();
        assert_eq!(s.lua.globals().get::<i64>("child").unwrap(), 77);
        assert_eq!(s.chunk_cache_stats().compiled_chunks, 3);
        std::fs::write(dir.join("child.lua"), "syntax ???").unwrap();
        assert!(s.reset_for_new_cell().is_err());
        assert!(!s.initialized);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn cached_main_and_modules_cannot_follow_symlinks_outside_rules_root() {
        let dir = directory();
        let outside = directory();
        std::fs::write(dir.join("main.lua"), "return true").unwrap();
        std::fs::write(outside.join("outside.lua"), "escaped=true").unwrap();
        let mut s = LuaSession::new().unwrap();
        s.set_rules_path(&dir).unwrap();
        s.load_main().unwrap();
        std::fs::remove_file(dir.join("main.lua")).unwrap();
        std::os::unix::fs::symlink(outside.join("outside.lua"), dir.join("main.lua")).unwrap();
        assert!(s.load_main().unwrap_err().to_string().contains("escapes"));
        assert!(s
            .reset_for_new_cell()
            .unwrap_err()
            .to_string()
            .contains("escapes"));
        std::fs::remove_file(dir.join("main.lua")).unwrap();
        std::fs::write(dir.join("main.lua"), "require('escaped')").unwrap();
        std::os::unix::fs::symlink(outside.join("outside.lua"), dir.join("escaped.lua")).unwrap();
        assert!(s.reset_for_new_cell().is_err());
        assert!(s
            .lua
            .globals()
            .get::<Option<bool>>("escaped")
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}

#[cfg(test)]
mod engine_catalogue_transfer_tests {
    use super::*;
    #[test]
    fn engine_metadata_survives_vm_reset_and_replacement_without_extra_catalogue_copy() {
        let mut engine = PortrayalEngine {
            session: LuaSession::new().unwrap(),
            type_catalogue: None,
        };
        engine.set_type_catalogue(TypeCatalogue {
            feature_codes: vec!["Wreck".into()],
            ..Default::default()
        });
        let old = Arc::clone(engine.type_catalogue.as_ref().unwrap());
        assert_eq!(Arc::strong_count(&old), 3);
        engine.session.reset_for_new_cell().unwrap();
        assert_eq!(Arc::strong_count(&old), 2);
        engine
            .session
            .host
            .set_shared_type_catalogue(Arc::clone(&old));
        assert_eq!(
            engine
                .session
                .eval::<String>("return HostGetFeatureTypeCodes()[1]")
                .unwrap(),
            "Wreck"
        );
        engine.set_type_catalogue(TypeCatalogue {
            feature_codes: vec!["Sounding".into()],
            ..Default::default()
        });
        assert_eq!(Arc::strong_count(&old), 1);
        assert_eq!(old.feature_codes, ["Wreck"]);
        assert_eq!(
            engine
                .session
                .eval::<String>("return HostGetFeatureTypeCodes()[1]")
                .unwrap(),
            "Sounding"
        );
    }
}

#[cfg(test)]
mod immutable_source_tests {
    use super::*;
    #[test]
    fn shared_engine_compilation_reruns_with_changed_fc_and_context() {
        let root = std::env::temp_dir().join(format!("ferrite-lua-process-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("Rules")).unwrap();
        std::fs::write(root.join("Rules/main.lua"), "iteration=0; function PortrayalMain() iteration=iteration+1; HostPortrayalEmit(HostGetFeatureTypeCodes()[1] .. ':' .. tostring(HostGetContextParameter('IsolatedDangers')) .. ':' .. iteration, 'PointInstruction:WRECKS01', 'IsolatedDangers'); return true end").unwrap();
        let sources = ferrite_portrayal_catalog::CatalogueSources::capture(&root).unwrap();
        for (code, danger) in [("Wreck", true), ("Sounding", false)] {
            let mut engine = PortrayalEngine::new_with_sources(Arc::clone(&sources)).unwrap();
            engine.set_type_catalogue(TypeCatalogue { feature_codes:vec![code.into()], ..Default::default() });
            engine.initialize().unwrap();
            engine.session_mut().set_context(ContextParameters { isolated_dangers:danger, ..Default::default() });
            for iteration in 1..=2 {
                let result=engine.session_mut().execute_portrayal().unwrap();
                assert_eq!(result.len(),1);
                assert_eq!(result[0].feature_id,format!("{code}:{danger}:{iteration}"));
                assert_eq!(result[0].observed_parameters,["IsolatedDangers"]);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn main_require_reset_and_vm_tables_use_retained_sources_after_original_changes() {
        let root = std::env::temp_dir().join(format!("ferrite-lua-source-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("Rules")).unwrap();
        let main = "loaded = require('dep'); function PortrayalMain() return true end";
        std::fs::write(root.join("Rules/main.lua"), main).unwrap();
        std::fs::write(root.join("Rules/dep.lua"), "return {tag='A'}").unwrap();
        let source_a = ferrite_portrayal_catalog::CatalogueSources::capture(&root).unwrap();
        let mut a = PortrayalEngine::new_with_sources(Arc::clone(&source_a)).unwrap();
        a.initialize().unwrap();
        assert_eq!(a.session().eval::<String>("loaded.tag").unwrap(), "A");
        a.session().lua.load("loaded.tag = 'mutated'; leak = true").exec().unwrap();
        std::fs::write(root.join("Rules/main.lua"), main).unwrap();
        std::fs::write(root.join("Rules/dep.lua"), "return {tag='B'}").unwrap();
        let source_b = ferrite_portrayal_catalog::CatalogueSources::capture(&root).unwrap();
        assert_ne!(source_a.digest(), source_b.digest());
        let mut b = PortrayalEngine::new_with_sources(source_b).unwrap();
        b.initialize().unwrap();
        assert_eq!(b.session().eval::<String>("loaded.tag").unwrap(), "B");
        std::fs::remove_dir_all(&root).unwrap();
        a.session_mut().reset_for_new_cell().unwrap();
        assert_eq!(a.session().eval::<String>("loaded.tag").unwrap(), "A");
        assert!(a.session().eval::<bool>("leak == nil").unwrap());
        assert!(a.session().lua.load("require('../escape')").exec().is_err());
        assert!(a.session().lua.load("require('missing')").exec().is_err());
        a.session().load_file(root.join("Rules/dep.lua")).unwrap();
        assert!(a.session().load_file(root.join("outside.lua")).is_err());
        b.session_mut().reset_for_new_cell().unwrap();
        assert_eq!(b.session().eval::<String>("loaded.tag").unwrap(), "B");
    }
}
