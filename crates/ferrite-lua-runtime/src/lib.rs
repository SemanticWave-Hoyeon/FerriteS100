//! Product-independent interpreter ownership and build-time version selection.
//! S-100 host callbacks, catalogue loading and portrayal remain in ferrite-lua.
//! Exactly one backend is linked into a build; no Lua/Rust handles cross VM ABIs.
#[cfg(all(feature = "lua54", feature = "lua55"))]
compile_error!("Choose exactly one Lua backend: lua54 or lua55, not both");
#[cfg(not(any(feature = "lua54", feature = "lua55")))]
compile_error!("Choose one Lua backend: lua54 or lua55");

pub use mlua;
mod chunk_cache;
mod identity;
pub use chunk_cache::{ChunkCache, ChunkCacheStats};
pub use identity::{runtime_identity, RuntimeIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeVersion {
    Lua54,
    Lua55,
}

pub const fn selected_version() -> RuntimeVersion {
    #[cfg(feature = "lua55")]
    {
        RuntimeVersion::Lua55
    }
    #[cfg(not(feature = "lua55"))]
    {
        RuntimeVersion::Lua54
    }
}
impl RuntimeVersion {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lua54 => "Lua 5.4",
            Self::Lua55 => "Lua 5.5",
        }
    }
}

/// Construct an interpreter without process or filesystem capabilities.
/// Product-specific module resolution is further restricted by the caller.
/// `ALL_SAFE` means Rust FFI safety, not a sandbox: it includes `os` and `io`.
pub fn new_vm() -> mlua::Lua {
    let libraries = mlua::StdLib::TABLE
        | mlua::StdLib::STRING
        | mlua::StdLib::MATH
        | mlua::StdLib::UTF8
        | mlua::StdLib::COROUTINE
        | mlua::StdLib::PACKAGE;
    mlua::Lua::new_with(libraries, mlua::LuaOptions::default())
        .expect("Restricted standard libraries must initialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vm_never_exposes_process_or_filesystem_libraries_through_globals_or_require() {
        let lua = new_vm();
        let restricted: bool = lua
            .load(
                r#"
            local os_ok = pcall(require, "os")
            local io_ok = pcall(require, "io")
            return os == nil and io == nil and debug == nil
                and package.loaded.os == nil and package.loaded.io == nil
                and not os_ok and not io_ok
        "#,
            )
            .eval()
            .unwrap();
        assert!(restricted);
        let sum: i64 = lua
            .load("return math.floor(3.5) + string.len('ENC')")
            .eval()
            .unwrap();
        assert_eq!(sum, 6);
    }
    #[test]
    fn compiled_backend_matches_the_actual_interpreter() {
        let lua = new_vm();
        let version: String = lua.globals().get("_VERSION").unwrap();
        assert_eq!(version, selected_version().name());
        let sum: i64 = lua
            .load("local n=0; for i=1,1000 do n=n+i end; return n")
            .eval()
            .unwrap();
        assert_eq!(sum, 500500);
    }
    #[test]
    fn callback_and_errors_use_the_selected_vm() {
        let lua = new_vm();
        lua.globals()
            .set("host", lua.create_function(|_, n: i64| Ok(n + 1)).unwrap())
            .unwrap();
        let value: i64 = lua.load("return host(41)").eval().unwrap();
        assert_eq!(value, 42);
        assert!(lua.load("error('runtime error')").exec().is_err());
    }
}
