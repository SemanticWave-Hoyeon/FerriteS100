//! String Interning for Symbol References
//!
//! Game-style optimization: Instead of cloning String for each symbol,
//! intern them once and use small u32 handles throughout.
//!
//! Benefits:
//! - Memory: 24+ bytes per String → 4 bytes per handle
//! - Performance: No heap allocation, better cache locality
//! - Comparison: u32 compare vs String compare

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Interned symbol handle (4 bytes instead of 24+ for String)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId(pub u32);

impl SymbolId {
    /// Invalid/empty symbol
    pub const NONE: SymbolId = SymbolId(0);

    #[inline]
    pub fn is_none(&self) -> bool {
        self.0 == 0
    }
}

/// Global string interner for symbol references
/// Thread-safe singleton pattern
pub struct SymbolInterner {
    /// String to ID mapping
    str_to_id: HashMap<Arc<str>, SymbolId>,
    /// ID to String mapping (for reverse lookup)
    id_to_str: Vec<Arc<str>>,
}

impl SymbolInterner {
    /// Create new interner with reserved slot 0 for "none"
    pub fn new() -> Self {
        let mut interner = SymbolInterner {
            str_to_id: HashMap::with_capacity(256),
            id_to_str: Vec::with_capacity(256),
        };
        // Reserve slot 0 for "none"/empty
        interner.id_to_str.push(Arc::from(""));
        interner
    }

    /// Intern a symbol string, returning its ID
    /// If already interned, returns existing ID
    #[inline]
    pub fn intern(&mut self, s: &str) -> SymbolId {
        if s.is_empty() {
            return SymbolId::NONE;
        }

        if let Some(&id) = self.str_to_id.get(s) {
            return id;
        }

        // New string - intern it
        let arc_str: Arc<str> = Arc::from(s);
        let id = SymbolId(self.id_to_str.len() as u32);
        self.id_to_str.push(arc_str.clone());
        self.str_to_id.insert(arc_str, id);
        id
    }

    /// Read-only lookup: returns None if not yet interned
    #[inline]
    pub fn lookup(&self, s: &str) -> Option<SymbolId> {
        if s.is_empty() {
            return Some(SymbolId::NONE);
        }
        self.str_to_id.get(s).copied()
    }

    /// Get string for an ID
    #[inline]
    pub fn resolve(&self, id: SymbolId) -> &str {
        self.id_to_str
            .get(id.0 as usize)
            .map(|s| s.as_ref())
            .unwrap_or("")
    }

    /// Number of interned strings
    pub fn len(&self) -> usize {
        self.id_to_str.len()
    }

    /// Check if empty (only has the reserved slot)
    pub fn is_empty(&self) -> bool {
        self.id_to_str.len() <= 1
    }
}

impl Default for SymbolInterner {
    fn default() -> Self {
        Self::new()
    }
}

/// Global thread-safe interner instance
static GLOBAL_INTERNER: std::sync::OnceLock<RwLock<SymbolInterner>> = std::sync::OnceLock::new();

/// Get or initialize the global interner
fn global_interner() -> &'static RwLock<SymbolInterner> {
    GLOBAL_INTERNER.get_or_init(|| RwLock::new(SymbolInterner::new()))
}

/// Intern a symbol string globally.
/// Fast path: read lock for already-interned strings (vast majority of calls).
/// Slow path: write lock only for first-time interning.
#[inline]
pub fn intern_symbol(s: &str) -> SymbolId {
    // Fast path: read lock (shared, no contention)
    if let Some(id) = global_interner()
        .read()
        .expect("interner read lock")
        .lookup(s)
    {
        return id;
    }
    // Slow path: write lock (only for new strings)
    global_interner()
        .write()
        .expect("interner write lock")
        .intern(s)
}

/// Resolve a symbol ID to string globally
#[inline]
pub fn resolve_symbol(id: SymbolId) -> String {
    global_interner()
        .read()
        .expect("interner read lock")
        .resolve(id)
        .to_string()
}

/// Resolve a symbol ID to &str (requires holding the lock)
/// Use this in hot paths where you can hold the lock
#[inline]
pub fn with_symbol<F, R>(id: SymbolId, f: F) -> R
where
    F: FnOnce(&str) -> R,
{
    let interner = global_interner().read().expect("interner read lock");
    f(interner.resolve(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intern_basic() {
        let mut interner = SymbolInterner::new();

        let id1 = interner.intern("SOUNDG02");
        let id2 = interner.intern("SOUNDG02");
        let id3 = interner.intern("LIGHTS01");

        assert_eq!(id1, id2); // Same string = same ID
        assert_ne!(id1, id3); // Different string = different ID

        assert_eq!(interner.resolve(id1), "SOUNDG02");
        assert_eq!(interner.resolve(id3), "LIGHTS01");
    }

    #[test]
    fn test_empty_string() {
        let mut interner = SymbolInterner::new();
        let id = interner.intern("");
        assert_eq!(id, SymbolId::NONE);
        assert!(id.is_none());
    }
}
