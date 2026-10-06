use crate::{selected_version, RuntimeVersion};
use std::{ffi::CStr, os::raw::c_char};

include!(concat!(env!("OUT_DIR"), "/runtime_build_identity.rs"));

// Both selected vendored PUC Lua backends export this NUL-terminated static
// character array in lapi.c. It contains the actual LUA_RELEASE and author/vendor.
unsafe extern "C" {
    static lua_ident: c_char;
}

/// Cache discriminator. The linked release string is actual runtime evidence;
/// the build fingerprint describes pinned dependency/source/compiler/target inputs.
/// It is deliberately conservative and is not a hash of the final linked object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeIdentity {
    pub backend: RuntimeVersion,
    pub linked_lua_ident: &'static str,
    pub build_fingerprint: &'static str,
}

/// Identify the selected linked interpreter without creating a Lua VM.
/// Unknown or invalid identities disable the caller's persistent result cache.
/// Custom vendor patches retaining the same version need an updated lock/source
/// fingerprint; the release label alone cannot attest such private modifications.
pub fn runtime_identity() -> Option<RuntimeIdentity> {
    if !BUILD_ID_COMPLETE {
        return None;
    }
    // SAFETY: lua_ident is the immutable, NUL-terminated array defined by the
    // statically linked Lua lapi.c; it lives for the process lifetime. No Lua
    // handles, mutable memory, or foreign VM state are accessed.
    let linked = unsafe { CStr::from_ptr(std::ptr::addr_of!(lua_ident)) }
        .to_str()
        .ok()?;
    let expected = format!("$LuaVersion: {}.", selected_version().name());
    if !linked.starts_with(&expected) || !linked.contains("$LuaAuthors:") {
        return None;
    }
    Some(RuntimeIdentity {
        backend: selected_version(),
        linked_lua_ident: linked,
        build_fingerprint: BUILD_FINGERPRINT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};
    fn key(value: impl Hash) -> u64 {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hash);
        hash.finish()
    }
    #[test]
    fn linked_runtime_reports_actual_patch_and_legacy_cache_is_invalidated() {
        // The VM and linked interpreter remain available even when an untracked
        // configuration makes only persistent output identity unavailable.
        // SAFETY: the same immutable, NUL-terminated vendored static as the API.
        let linked = unsafe { CStr::from_ptr(std::ptr::addr_of!(lua_ident)) }
            .to_str().expect("vendored Lua release is UTF-8");
        assert!(linked.starts_with(&format!("$LuaVersion: {}.", selected_version().name())));
        assert!(linked.contains("Lua.org, PUC-Rio"));
        assert!(linked.contains("$LuaAuthors:"));
        let identity = RuntimeIdentity {
            backend: selected_version(),
            linked_lua_ident: linked,
            build_fingerprint: BUILD_FINGERPRINT,
        };
        if BUILD_ID_COMPLETE {
            assert_eq!(runtime_identity().expect("managed vendored Lua identity"), identity);
        } else {
            assert_eq!(runtime_identity(), None, "Incomplete build must disable persistent cache identity");
        }
        assert_eq!(identity.backend, selected_version());
        assert_eq!(identity.build_fingerprint.len(), 64);
        assert!(identity
            .build_fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit()));
        let lua = crate::new_vm();
        assert_eq!(
            lua.globals().get::<String>("_VERSION").unwrap(),
            identity.backend.name()
        );
        assert_ne!(
            key(identity),
            key(selected_version().name()),
            "Legacy major/minor cache key reused"
        );
        let changed_patch = RuntimeIdentity {
            linked_lua_ident: "$LuaVersion: changed patch $",
            ..identity
        };
        let changed_build = RuntimeIdentity {
            build_fingerprint: "different compiled inputs",
            ..identity
        };
        assert_ne!(key(identity), key(changed_patch));
        assert_ne!(key(identity), key(changed_build));
        println!("LINKED_RUNTIME {:?}; BUILD_ID_COMPLETE={BUILD_ID_COMPLETE}; PUBLIC_IDENTITY={:?}", identity, runtime_identity());
    }
}
