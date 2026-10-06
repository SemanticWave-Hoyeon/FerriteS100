//! Bounded, process-local compiler output reuse. Only our own freshly compiled
//! chunks enter this cache; it never loads persisted or cross-version bytecode.
use crate::mlua::{ChunkMode, Function, Lua, Result};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkCacheStats {
    pub compiled_chunks: u64,
    pub bytecode_hits: u64,
    pub bypasses: u64,
    /// Source bytes + bytecode bytes + owned chunk names, excluding allocator
    /// metadata and active VM memory. Entry count is independently bounded.
    pub retained_payload_bytes: usize,
    pub entries: usize,
}
struct Entry {
    source: Vec<u8>,
    bytecode: Arc<[u8]>,
    bytes: usize,
}
struct State {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    stats: ChunkCacheStats,
}
/// Compiled code is shareable; globals, host callbacks, closures and execution
/// results always belong to a newly instantiated function in the caller's VM.
#[derive(Clone)]
pub struct ChunkCache {
    state: Arc<Mutex<State>>,
    max_bytes: usize,
    max_entries: usize,
}
impl Default for ChunkCache {
    fn default() -> Self {
        Self::new(16 * 1024 * 1024, 1024)
    }
}
impl ChunkCache {
    pub fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                entries: HashMap::new(),
                order: VecDeque::new(),
                stats: Default::default(),
            })),
            max_bytes,
            max_entries,
        }
    }
    pub fn stats(&self) -> ChunkCacheStats {
        self.state.lock().expect("chunk cache lock").stats
    }
    pub fn compile(&self, lua: &Lua, source: &[u8], name: &str) -> Result<Function> {
        let cached = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| crate::mlua::Error::external("chunk cache lock poisoned"))?;
            let code = state
                .entries
                .get(name)
                .filter(|e| e.source == source)
                .map(|e| e.bytecode.clone());
            if code.is_some() {
                state.stats.bytecode_hits += 1;
            }
            code
        };
        if let Some(code) = cached {
            return lua
                .load(code.as_ref())
                .set_mode(ChunkMode::Binary)
                .set_name(name)
                .into_function();
        }
        // Compare delivered source bytes on every access, not timestamps or a
        // collision-prone digest. Syntax errors never fall back to stale code.
        let function = lua.load(source).set_name(name).into_function()?;
        let bytecode = function.dump(false); // Keep source names/line tables.
        let bytes = source
            .len()
            .checked_add(bytecode.len())
            .and_then(|n| n.checked_add(name.len()))
            .and_then(|n| n.checked_add(name.len()));
        let mut state = self
            .state
            .lock()
            .map_err(|_| crate::mlua::Error::external("chunk cache lock poisoned"))?;
        state.stats.compiled_chunks += 1;
        let Some(bytes) = bytes.filter(|n| *n <= self.max_bytes && self.max_entries > 0) else {
            state.stats.bypasses += 1;
            return Ok(function);
        };
        if let Some(old) = state.entries.remove(name) {
            state.stats.retained_payload_bytes -= old.bytes;
            state.order.retain(|n| n != name);
        }
        while state.entries.len() >= self.max_entries
            || bytes > self.max_bytes - state.stats.retained_payload_bytes
        {
            let oldest = state.order.pop_front().expect("entry has FIFO name");
            let old = state.entries.remove(&oldest).expect("FIFO entry exists");
            state.stats.retained_payload_bytes -= old.bytes;
        }
        state.entries.insert(
            name.into(),
            Entry {
                source: source.to_vec(),
                bytecode: bytecode.into(),
                bytes,
            },
        );
        state.order.push_back(name.into());
        state.stats.retained_payload_bytes += bytes;
        state.stats.entries = state.entries.len();
        Ok(function)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bytecode_reuse_does_not_reuse_vm_globals_or_closures() {
        let cache = ChunkCache::default();
        for n in [1_i64, 77, 42] {
            let lua = crate::new_vm();
            lua.globals()
                .set("host", lua.create_function(move |_, ()| Ok(n)).unwrap())
                .unwrap();
            let f = cache
                .compile(
                    &lua,
                    b"calls=(calls or 0)+1; return host(),calls",
                    "@same.lua",
                )
                .unwrap();
            assert_eq!(f.call::<(i64, i64)>(()).unwrap(), (n, 1));
        }
        assert_eq!(cache.stats().compiled_chunks, 1);
        assert_eq!(cache.stats().bytecode_hits, 2);
    }
    #[test]
    fn same_path_edits_and_syntax_errors_never_hit_stale_code() {
        let lua = crate::new_vm();
        let cache = ChunkCache::default();
        assert_eq!(
            cache
                .compile(&lua, b"return 11", "@edit.lua")
                .unwrap()
                .call::<i64>(())
                .unwrap(),
            11
        );
        assert_eq!(
            cache
                .compile(&lua, b"return 22", "@edit.lua")
                .unwrap()
                .call::<i64>(())
                .unwrap(),
            22
        );
        assert!(cache.compile(&lua, b"return ???", "@edit.lua").is_err());
        assert_eq!(cache.stats().bytecode_hits, 0);
        assert_eq!(
            cache
                .compile(&lua, b"return 22", "@edit.lua")
                .unwrap()
                .call::<i64>(())
                .unwrap(),
            22
        );
        assert_eq!(cache.stats().entries, 1);
    }
    #[test]
    fn errors_keep_debug_source_and_budgets_evict_and_bypass() {
        let lua = crate::new_vm();
        let cache = ChunkCache::new(4096, 1);
        for _ in 0..2 {
            let e = cache
                .compile(&lua, b"\nerror('fixture')", "@original.lua")
                .unwrap()
                .call::<()>(())
                .unwrap_err()
                .to_string();
            assert!(e.contains("original.lua:2"), "{e}");
        }
        cache.compile(&lua, b"return 9", "@other.lua").unwrap();
        assert_eq!(cache.stats().entries, 1);
        assert!(cache.stats().retained_payload_bytes <= 4096);
        cache.compile(&lua, b"return 1", "@original.lua").unwrap();
        assert_eq!(cache.stats().compiled_chunks, 3);
        let bypass = ChunkCache::new(0, 0);
        assert_eq!(
            bypass
                .compile(&lua, b"return 3", "@bypass")
                .unwrap()
                .call::<i64>(())
                .unwrap(),
            3
        );
        assert_eq!(bypass.stats().entries, 0);
        assert_eq!(bypass.stats().bypasses, 1);
    }
}
