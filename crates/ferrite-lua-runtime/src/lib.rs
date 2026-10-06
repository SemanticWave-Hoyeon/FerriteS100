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

/// Construct only the interpreter. Product-specific policy is applied by the
/// caller before any catalogue code is evaluated.
pub fn new_vm() -> mlua::Lua {
    mlua::Lua::new()
}

#[cfg(test)]
mod tests {
    use super::*;
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
