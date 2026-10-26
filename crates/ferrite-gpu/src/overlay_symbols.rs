//! Process-wide registry of overlay-product symbol sources (S-124, S-129, ...).
//!
//! Overlay products are portrayed with their own portrayal catalogue. Their
//! drawing instructions reference symbols with namespaced ids
//! `<prefix>/<symbol>` (see
//! [`ferrite_portrayal_catalog::overlay_symbol_id`]); every [`SymbolCache`]
//! (the active S-101 cache, forks and per-cell resource-owner caches) resolves
//! such ids from the source registered here for `<prefix>`, using the overlay
//! catalogue's own colour profile whose name matches the active profile's
//! (e.g. "Day").  Plain ids (no `/`) are never affected.
//!
//! [`SymbolCache`]: crate::SymbolCache

use ferrite_portrayal_catalog::{
    split_overlay_symbol_id, BoundPortrayalCatalogue, CatalogueSources, ColorProfile,
    ColorProfiles, PortrayalCatalogue,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Symbols directory and palettes of one overlay portrayal catalogue.
#[derive(Debug)]
pub struct OverlaySymbolSource {
    symbols_dir: PathBuf,
    profiles: ColorProfiles,
    sources: Option<Arc<CatalogueSources>>,
}

impl OverlaySymbolSource {
    /// SVGs are read from `symbols_dir` on the filesystem.
    pub fn new(symbols_dir: impl Into<PathBuf>, profiles: ColorProfiles) -> Self {
        Self {
            symbols_dir: symbols_dir.into(),
            profiles,
            sources: None,
        }
    }
    /// `<PC>/Symbols` and all palettes of a loaded overlay catalogue.
    pub fn from_catalogue(pc: &PortrayalCatalogue) -> Self {
        Self::new(pc.symbols.base_path.clone(), pc.color_profiles.clone())
    }
    /// Like [`Self::from_catalogue`], but SVG bytes (and their resources) are
    /// read from the immutable captured catalogue sources.
    pub fn from_bound(pc: &BoundPortrayalCatalogue) -> Self {
        let mut source = Self::from_catalogue(pc);
        source.sources = Some(pc.sources());
        source
    }
    pub fn symbols_dir(&self) -> &Path {
        &self.symbols_dir
    }
    pub fn profiles(&self) -> &ColorProfiles {
        &self.profiles
    }
    pub fn sources(&self) -> Option<&Arc<CatalogueSources>> {
        self.sources.as_ref()
    }
    pub(crate) fn svg_path(&self, symbol: &str) -> PathBuf {
        self.symbols_dir.join(format!("{symbol}.svg"))
    }
    /// Overlay palette for the active (S-101) profile: same name, then a
    /// case-insensitive name/id match, then the overlay default ("Day"), then
    /// the first palette by name. With no overlay palettes the active profile
    /// itself is used.
    pub fn palette_for<'a>(&'a self, active: &'a ColorProfile) -> &'a ColorProfile {
        let profiles = &self.profiles.profiles;
        profiles
            .get(&active.name)
            .or_else(|| profiles.get(&active.id))
            .or_else(|| {
                profiles.values().find(|p| {
                    p.name.eq_ignore_ascii_case(&active.name)
                        || p.name.eq_ignore_ascii_case(&active.id)
                        || p.id.eq_ignore_ascii_case(&active.id)
                })
            })
            .or_else(|| self.profiles.get_default())
            .or_else(|| profiles.iter().min_by(|a, b| a.0.cmp(b.0)).map(|(_, p)| p))
            .unwrap_or(active)
    }
}

type Registry = RwLock<HashMap<String, Arc<OverlaySymbolSource>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Bumped on every registration change; caches drop overlay entries lazily.
pub(crate) fn overlay_generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 64
        && !prefix.contains(['/', '\\', '\0'])
        && prefix.trim() == prefix
}

/// Register (or replace) the symbol source for overlay ids `<prefix>/<id>`.
/// Returns false (and registers nothing) for an empty prefix or one that
/// contains `/`, `\`, NUL or surrounding whitespace.
pub fn register_overlay_symbol_source(prefix: &str, source: OverlaySymbolSource) -> bool {
    if !valid_prefix(prefix) {
        return false;
    }
    let mut map = registry().write().unwrap_or_else(|e| e.into_inner());
    map.insert(prefix.to_owned(), Arc::new(source));
    GENERATION.fetch_add(1, Ordering::AcqRel);
    true
}

/// Convenience: register `<PC>/Symbols` and all palettes of a loaded overlay
/// catalogue (e.g. `register_overlay_catalogue("S-124", &s124_pc)`).
pub fn register_overlay_catalogue(prefix: &str, pc: &PortrayalCatalogue) -> bool {
    register_overlay_symbol_source(prefix, OverlaySymbolSource::from_catalogue(pc))
}

/// Remove a registered overlay source. Cached overlay symbols are dropped on
/// next access by every cache.
pub fn unregister_overlay_symbol_source(prefix: &str) -> bool {
    let mut map = registry().write().unwrap_or_else(|e| e.into_inner());
    let removed = map.remove(prefix).is_some();
    if removed {
        GENERATION.fetch_add(1, Ordering::AcqRel);
    }
    removed
}

/// The source currently registered for `prefix`.
pub fn overlay_symbol_source(prefix: &str) -> Option<Arc<OverlaySymbolSource>> {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(prefix)
        .cloned()
}

/// Registered prefixes, sorted.
pub fn registered_overlay_prefixes() -> Vec<String> {
    let mut v: Vec<_> = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    v.sort();
    v
}

/// True for namespaced overlay ids (`<prefix>/<symbol>`), registered or not.
pub fn is_overlay_symbol_id(id: &str) -> bool {
    split_overlay_symbol_id(id).is_some()
}

/// Valid catalogue-local symbol file stem (no path syntax).
pub(crate) fn valid_local_symbol(symbol: &str) -> bool {
    !symbol.is_empty() && symbol != ".." && symbol != "." && !symbol.contains(['/', '\\', '\0'])
}

/// Resolve an overlay id to its registered source and local symbol id.
/// None for plain ids, unregistered prefixes, or path-like local ids.
pub fn resolve_overlay_symbol(id: &str) -> Option<(Arc<OverlaySymbolSource>, &str)> {
    let (prefix, local) = split_overlay_symbol_id(id)?;
    if !valid_local_symbol(local) {
        return None;
    }
    Some((overlay_symbol_source(prefix)?, local))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile(name: &str) -> ColorProfile {
        ColorProfile::new(name.into(), name.into())
    }
    #[test]
    fn palette_selection_falls_back_sensibly() {
        let mut profiles = ColorProfiles::new();
        for n in ["Day", "Dusk", "Night"] {
            profiles.profiles.insert(n.into(), profile(n));
        }
        profiles.default_profile = Some("Day".into());
        let src = OverlaySymbolSource::new("/nowhere", profiles);
        assert_eq!(src.palette_for(&profile("Night")).name, "Night");
        assert_eq!(src.palette_for(&profile("night")).name, "Night");
        assert_eq!(src.palette_for(&profile("DayBright")).name, "Day");
        let empty = OverlaySymbolSource::new("/nowhere", ColorProfiles::new());
        let active = profile("Dusk");
        assert!(std::ptr::eq(empty.palette_for(&active), &active));
    }
    #[test]
    fn registry_validates_prefix_and_local_ids() {
        assert!(!register_overlay_symbol_source(
            "bad/prefix",
            OverlaySymbolSource::new("/x", ColorProfiles::new())
        ));
        assert!(!register_overlay_symbol_source(
            "",
            OverlaySymbolSource::new("/x", ColorProfiles::new())
        ));
        let prefix = "TEST-REGISTRY-VALIDATION";
        assert!(register_overlay_symbol_source(
            prefix,
            OverlaySymbolSource::new("/x", ColorProfiles::new())
        ));
        assert!(resolve_overlay_symbol(&format!("{prefix}/A")).is_some());
        assert!(resolve_overlay_symbol(&format!("{prefix}/../A")).is_none());
        assert!(resolve_overlay_symbol(&format!("{prefix}/..")).is_none());
        assert!(resolve_overlay_symbol("UNREGISTERED-X/A").is_none());
        assert!(resolve_overlay_symbol("PLAIN").is_none());
        assert!(registered_overlay_prefixes().contains(&prefix.to_owned()));
        assert!(unregister_overlay_symbol_source(prefix));
        assert!(!unregister_overlay_symbol_source(prefix));
        assert!(resolve_overlay_symbol(&format!("{prefix}/A")).is_none());
    }

    fn overlay_pc(product: &str) -> Option<ferrite_portrayal_catalog::BoundPortrayalCatalogue> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../Catalogues/PC")
            .join(product);
        dir.join("portrayal_catalogue.xml")
            .exists()
            .then(|| PortrayalCatalogue::load_bound(&dir).unwrap())
    }
    fn active(name: &str) -> ColorProfile {
        // S-101-like active profile lacking every overlay colour token.
        ColorProfile::new(name.into(), name.into())
    }
    fn empty_symbols_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferrite-overlay-symbols-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_cache_resolves_registered_overlay_symbols_with_overlay_palette() {
        let Some(pc) = overlay_pc("S-129") else {
            return;
        };
        let prefix = "S-129-TEST-RESOLVE";
        let id = format!("{prefix}/UKCCONPT");
        let mut cache = crate::SymbolCache::new(empty_symbols_dir());
        // Unregistered: missing, and plain ids still come from the own PC dir.
        assert!(cache.get_symbol(&id, &active("Day")).is_none());
        assert!(cache.get_symbol("UKCCONPT", &active("Day")).is_none());
        let revision = cache.resource_revision();
        assert!(register_overlay_symbol_source(
            prefix,
            OverlaySymbolSource::from_bound(&pc)
        ));
        // Registration invalidates the stale "missing" entry on next access.
        let day = cache
            .get_symbol(&id, &active("Day"))
            .expect("overlay symbol")
            .clone();
        assert_eq!(day.name, id);
        assert!(day.width > 0 && day.pixels.iter().any(|&b| b != 0));
        // No overlay raster was dropped, so plain resources keep their revision.
        assert_eq!(cache.resource_revision(), revision);
        assert!(cache.get_symbol("UKCCONPT", &active("Day")).is_none());

        // Forks and independently created caches resolve the same ids.
        let mut fork = cache.fork_empty();
        let night = fork.get_symbol(&id, &active("Night")).unwrap().clone();
        assert_eq!((night.width, night.height), (day.width, day.height));
        assert_ne!(
            night.pixels, day.pixels,
            "Night palette must differ from Day"
        );
        let mut other = crate::SymbolCache::new(empty_symbols_dir());
        other.prefetch_symbols([id.as_str()], &active("Day"));
        assert_eq!(
            other.get_symbol(&id, &active("Day")).unwrap().pixels,
            day.pixels
        );

        // Patterns, lattices and whole supports accept namespaced ids too.
        assert!(cache
            .get_symbol_for_pattern(&id, &active("Day"), 20., 20., 4.)
            .is_some());
        let lattice = ferrite_render::PatternLattice::from_mm((6., 0.), (0., 6.), 4.).unwrap();
        assert!(cache
            .get_symbol_for_lattice(&id, &active("Day"), lattice, 4.)
            .is_ok());
        assert!(cache
            .get_whole_symbol_support(&id, &active("Day"), 4., Default::default())
            .unwrap()
            .is_some());
        assert!(cache
            .get_whole_symbol_support(
                &format!("{prefix}/../x"),
                &active("Day"),
                4.,
                Default::default()
            )
            .is_err());

        // Unregistering drops overlay entries (new revision); plain ids unaffected.
        let before = cache.resource_revision();
        assert!(unregister_overlay_symbol_source(prefix));
        assert!(cache.get_symbol(&id, &active("Day")).is_none());
        assert_ne!(cache.resource_revision(), before);
        assert!(cache
            .get_symbol_for_lattice(&id, &active("Day"), lattice, 4.)
            .is_err());
    }

    #[test]
    fn s124_line_style_symbols_resolve_after_namespacing() {
        let Some(bound) = overlay_pc("S-124") else {
            return;
        };
        let prefix = "S-124-TEST-LINES";
        let mut pc = PortrayalCatalogue::load(bound.root_path.clone()).unwrap();
        pc.namespace_symbol_references(prefix);
        assert!(register_overlay_catalogue(prefix, &pc));
        let mut cache = crate::SymbolCache::new(empty_symbols_dir());
        for style in ["NavigationalWarningFeaturePartLine", "QUESMRK1"] {
            let ferrite_portrayal_catalog::LineStyle::Simple(line) = &pc.line_styles[style] else {
                panic!("simple line style expected")
            };
            for symbol in &line.symbols {
                assert!(
                    cache
                        .get_symbol(&symbol.reference, &active("Dusk"))
                        .is_some(),
                    "{}",
                    symbol.reference
                );
            }
        }
        for symbol in ["NavigationalWarningFeaturePart", "ApproximatePosition"] {
            assert!(cache
                .get_symbol(&format!("{prefix}/{symbol}"), &active("Day"))
                .is_some());
        }
        unregister_overlay_symbol_source(prefix);
    }
}
