//! Symbol Cache for S-100 SVG Symbols
//!
//! Uses resvg for accurate SVG rendering including even-odd fill rule.
//! Renders SVG symbols to pixel buffers that are uploaded as GPU textures.
//!
//! # S-100 Symbol Sizing
//!
//! S-100 Portrayal Catalogue symbols are defined with dimensions in millimeters
//! (SVG viewBox units). The S-100 standard specifies a reference display with
//! 0.3mm per pixel (~85 DPI). At the reference scale, symbols appear at their
//! nominal mm size on screen.
//!
//! The render_scale (pixels per mm) is used for SVG rasterization quality.
//! A higher render_scale produces sharper textures but uses more memory.
//! The actual display size is calculated at render time using S100_PX_PER_MM.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use ferrite_portrayal_catalog::ColorProfile;

/// Rendered symbol data (pixel buffer ready for GPU upload)
#[derive(Debug, Clone)]
pub struct SymbolGeometry {
    /// Symbol name
    pub name: String,
    /// Rendered pixel data (RGBA, premultiplied alpha)
    pub pixels: Vec<u8>,
    /// Rendered width in pixels
    pub width: u32,
    /// Rendered height in pixels
    pub height: u32,
    /// Symbol viewBox (min_x, min_y, width, height) in mm
    pub bounds: (f32, f32, f32, f32),
    /// Pivot point in SVG coordinates (in mm, relative to SVG origin)
    pub pivot: (f32, f32),
    /// SVG origin transformed into raster texture coordinates.
    pub texture_pivot: (f32, f32),
    /// Scale factor used for rendering (pixels per mm)
    pub render_scale: f32,
}

impl SymbolGeometry {
    /// Get pivot position within the texture (in pixels from top-left)
    /// Note: The texture is rendered by resvg which converts mm units to pixels at 96 DPI,
    /// then we apply render_scale. So pivot must also include the mm-to-pixel conversion.
    pub fn pivot_in_texture(&self) -> (f32, f32) {
        self.texture_pivot
    }

    /// Check if this symbol has even-odd fill data (always false for resvg, handled internally)
    pub fn has_even_odd_fill(&self) -> bool {
        false // resvg handles even-odd fill internally
    }
}

/// A fundamental lattice cell, distinct from a point-symbol billboard texture.
/// The sampler must repeat both axes and use premultiplied alpha. The authored
/// SVG reference lies at integer UV sites, including through the cell seam.
#[derive(Debug, Clone)]
pub struct PeriodicPatternGeometry {
    pub name: String,
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn lattice_cache_key(
    revision: u64,
    symbol_id: &str,
    color_profile: &ColorProfile,
    lattice: ferrite_render::PatternLattice,
    pixels_per_mm: f64,
) -> Result<String, String> {
    if !pixels_per_mm.is_finite() || pixels_per_mm <= 0. {
        return Err("Invalid pattern SVG device calibration".into());
    }
    const MAX_KEY_BYTES: usize = 64 * 1024;
    if symbol_id.len() > MAX_KEY_BYTES / 2 {
        return Err("Pattern cache key budget exceeded".into());
    }
    let mut key = format!(
        "lattice-cell-v2:{}:{}:{}:{:016x}",
        revision,
        symbol_id.len(),
        symbol_id,
        pixels_per_mm.to_bits()
    );
    for value in lattice.columns().iter().flatten() {
        use std::fmt::Write;
        write!(&mut key, ":{:016x}", value.to_bits()).unwrap();
    }
    // Exact sorted resolved color inputs, avoiding profile-name aliases or
    // a collision-prone hash. A SymbolCache holds one immutable PC snapshot.
    let mut colors: Vec<_> = color_profile.colors.iter().collect();
    colors.sort_by(|a, b| a.0.cmp(b.0));
    for (token, color) in colors {
        if key
            .len()
            .checked_add(token.len())
            .and_then(|n| n.checked_add(64))
            .is_none_or(|n| n > MAX_KEY_BYTES)
        {
            return Err("Pattern cache key budget exceeded".into());
        }
        use std::fmt::Write;
        write!(
            &mut key,
            ":{}:{}:{:?}",
            token.len(),
            token,
            color.get_srgb().map(|c| [c.r, c.g, c.b])
        )
        .unwrap();
    }
    Ok(key)
}

/// View-local exact full keys. Palette is borrowed for this entire view; resource
/// revision, calibration and every lattice f64 bit remain explicit identities.
/// Only successful original key construction is retained; errors are never memoized.
#[cfg(test)]
pub(crate) struct PreparedLatticeKeys<'a> {
    profile: &'a ColorProfile,
    entries: Vec<PreparedLatticeKey>,
    bytes: usize,
    budget: usize,
    pub(crate) hits: usize,
    pub(crate) misses: usize,
    pub(crate) refusals: usize,
}
#[cfg(test)]
struct PreparedLatticeKey {
    revision: u64,
    symbol: String,
    ppm: u64,
    columns: [u64; 4],
    key: String,
}
#[cfg(test)]
impl<'a> PreparedLatticeKeys<'a> {
    pub(crate) fn new(profile: &'a ColorProfile) -> Self {
        Self {
            profile,
            entries: Vec::new(),
            bytes: 0,
            budget: 1024 * 1024,
            hits: 0,
            misses: 0,
            refusals: 0,
        }
    }
    fn key(
        &mut self,
        revision: u64,
        symbol: &str,
        lattice: ferrite_render::PatternLattice,
        ppm: f64,
    ) -> Result<std::borrow::Cow<'_, String>, String> {
        let mut columns = [0; 4];
        for (dst, value) in columns.iter_mut().zip(lattice.columns().iter().flatten()) {
            *dst = value.to_bits();
        }
        if let Some(index) = self.entries.iter().position(|e| {
            e.revision == revision
                && e.ppm == ppm.to_bits()
                && e.columns == columns
                && e.symbol == symbol
        }) {
            self.hits += 1;
            return Ok(std::borrow::Cow::Borrowed(&self.entries[index].key));
        }
        self.misses += 1;
        // This executes the ORIGINAL validation/formatting/error order on every miss.
        let key = lattice_cache_key(revision, symbol, self.profile, lattice, ppm)?;
        let payload = key.capacity().checked_add(symbol.len());
        let total = payload
            .and_then(|n| self.bytes.checked_add(n))
            .and_then(|n| {
                n.checked_add(
                    (self.entries.len() + 1)
                        .checked_mul(std::mem::size_of::<PreparedLatticeKey>())?
                        .checked_mul(2)?,
                )
            });
        if self.entries.len() >= 256
            || total.is_none_or(|n| n > self.budget)
            || self.entries.try_reserve_exact(1).is_err()
        {
            self.refusals += 1;
            return Ok(std::borrow::Cow::Owned(key));
        }
        // Recheck actual retained capacity; a reserve implementation is allowed
        // to grow by more than requested. Refusal releases metadata and falls cold.
        if self
            .bytes
            .checked_add(payload.unwrap())
            .and_then(|n| {
                n.checked_add(self.entries.capacity() * std::mem::size_of::<PreparedLatticeKey>())
            })
            .is_none_or(|n| n > self.budget)
        {
            self.entries = Vec::new();
            self.bytes = 0;
            self.refusals += 1;
            return Ok(std::borrow::Cow::Owned(key));
        }
        let mut owned_symbol = String::new();
        if owned_symbol.try_reserve_exact(symbol.len()).is_err() {
            self.refusals += 1;
            return Ok(std::borrow::Cow::Owned(key));
        }
        owned_symbol.push_str(symbol);
        if self
            .bytes
            .checked_add(key.capacity())
            .and_then(|n| n.checked_add(owned_symbol.capacity()))
            .and_then(|n| {
                n.checked_add(self.entries.capacity() * std::mem::size_of::<PreparedLatticeKey>())
            })
            .is_none_or(|n| n > self.budget)
        {
            self.refusals += 1;
            return Ok(std::borrow::Cow::Owned(key));
        }
        let symbol = owned_symbol;
        self.bytes += key.capacity() + symbol.capacity();
        self.entries.push(PreparedLatticeKey {
            revision,
            symbol,
            ppm: ppm.to_bits(),
            columns,
            key,
        });
        Ok(std::borrow::Cow::Borrowed(
            &self.entries.last().unwrap().key,
        ))
    }
    pub(crate) fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"hits":self.hits,"misses":self.misses,"refusals":self.refusals,"entries":self.entries.len(),"accounted_bytes":self.bytes+self.entries.capacity()*std::mem::size_of::<PreparedLatticeKey>(),"budget_bytes":self.budget,"entry_limit":256})
    }
}

/// Symbol cache for efficient symbol rendering
#[derive(Debug)]
pub struct SymbolCache {
    fonts: crate::font_asset_cache::FontAssetCache,
    shallow_pattern_contract: Option<ferrite_render::ShallowPatternContract>,
    /// Cached symbol geometry (keyed by symbol ID)
    symbols: HashMap<String, SymbolGeometry>,
    /// Symbol IDs that failed to load (avoid re-attempting every frame)
    missing_symbols: HashSet<String>,
    lattice_patterns: HashMap<String, PeriodicPatternGeometry>,
    lattice_pattern_bytes: usize,
    whole_supports:
        HashMap<String, Option<std::sync::Arc<crate::svg_painted_support::SvgPaintedSupport>>>,
    whole_support_bytes: usize,
    whole_motifs: HashMap<String, Option<std::sync::Arc<crate::whole_motif::NaturalMotifResource>>>,
    whole_motif_bytes: usize,
    resource_revision: u64,
    /// Base path for symbol SVG files
    symbols_path: std::path::PathBuf,
    sources: Option<std::sync::Arc<ferrite_portrayal_catalog::CatalogueSources>>,
    /// Render scale (pixels per mm) - higher = better quality but more memory
    render_scale: f32,
}

/// S-100 standard: 96 DPI base (pixels per mm = 96/25.4 ≈ 3.78)
const BASE_PX_PER_MM: f32 = 96.0 / 25.4;

/// Quality multiplier for SVG rasterization (2x = sharper symbols)
const RENDER_QUALITY_MULTIPLIER: f32 = 2.0;

fn next_symbol_resource_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.try_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| v.checked_add(1),
    )
    .expect("Symbol resource revision exhausted")
}

impl SymbolCache {
    /// OVERSC01 is resolved only from the active immutable PC snapshot.
    /// A filename-only cache cannot authorize a standards annotation.
    pub(crate) fn overscale_pattern_definition(
        &self,
    ) -> Result<ferrite_portrayal_catalog::OverscalePatternDefinition, String> {
        let sources = self
            .sources
            .as_ref()
            .ok_or("OVERSC01 requires immutable active-PC sources")?;
        let root = self
            .symbols_path
            .parent()
            .ok_or("Missing PC symbol directory parent")?;
        ferrite_portrayal_catalog::OverscalePatternDefinition::from_sources(
            sources,
            &root.join("AreaFills/OVERSC01.xml"),
        )
        .map_err(|e| e.to_string())
    }
    pub fn resource_revision(&self) -> u64 {
        self.resource_revision
    }
    /// Independent unpublished caches sharing immutable PC source bytes only.
    /// A failed candidate cannot clear live raster/support/motif resources.
    pub fn fork_empty(&self) -> Self {
        let mut next = Self::new(&self.symbols_path);
        next.sources = self.sources.clone();
        next.shallow_pattern_contract = self.shallow_pattern_contract.clone();
        next.render_scale = self.render_scale;
        next
    }

    /// Create new symbol cache
    ///
    /// Symbols are rasterized at `BASE_PX_PER_MM × RENDER_QUALITY_MULTIPLIER` pixels per mm
    /// for high-quality display. The actual screen size is determined by S100_PX_PER_MM
    /// in the renderer.
    pub fn new<P: AsRef<Path>>(symbols_path: P) -> Self {
        SymbolCache {
            shallow_pattern_contract: None,
            symbols: HashMap::new(),
            missing_symbols: HashSet::new(),
            lattice_patterns: HashMap::new(),
            lattice_pattern_bytes: 0,
            whole_supports: HashMap::new(),
            whole_support_bytes: 0,
            whole_motifs: HashMap::new(),
            whole_motif_bytes: 0,
            resource_revision: next_symbol_resource_revision(),
            symbols_path: symbols_path.as_ref().to_path_buf(),
            sources: None,
            fonts: Default::default(),
            render_scale: BASE_PX_PER_MM * RENDER_QUALITY_MULTIPLIER,
        }
    }

    pub(crate) fn resolve_font_reference(
        &mut self,
        reference: &str,
    ) -> crate::Result<ferrite_portrayal_catalog::BoundFontReference> {
        let sources = self.sources.clone().ok_or_else(|| {
            crate::WgpuError::Render("FontReference requires captured PC sources".into())
        })?;
        self.fonts.resolve(sources, reference)
    }
    // Exact retained capture identity, never a digest-only permission comparison.
    pub(crate) fn has_exact_source_owner(
        &self,
        sources: &std::sync::Arc<ferrite_portrayal_catalog::CatalogueSources>,
    ) -> bool {
        self.sources
            .as_ref()
            .is_some_and(|own| std::sync::Arc::ptr_eq(own, sources))
    }
    pub fn new_with_sources<P: AsRef<Path>>(
        symbols_path: P,
        sources: std::sync::Arc<ferrite_portrayal_catalog::CatalogueSources>,
    ) -> Self {
        let mut cache = Self::new(symbols_path);
        cache.sources = Some(sources);
        cache
    }

    pub fn new_with_pattern_contract<P: AsRef<Path>>(
        symbols_path: P,
        sources: std::sync::Arc<ferrite_portrayal_catalog::CatalogueSources>,
        contract: Option<ferrite_render::ShallowPatternContract>,
    ) -> Self {
        let contract = contract.filter(|c| c.source_digest() == *sources.digest());
        let mut cache = Self::new_with_sources(symbols_path, sources);
        cache.shallow_pattern_contract = contract;
        cache
    }
    pub fn shallow_pattern_contract(&self) -> Option<&ferrite_render::ShallowPatternContract> {
        self.shallow_pattern_contract.as_ref()
    }

    fn source_exists(&self, path: &Path) -> bool {
        match &self.sources {
            Some(s) => match s.read_path(path) {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!("Immutable PC SVG input unavailable: {error}");
                    false
                }
            },
            None => path.exists(),
        }
    }

    fn read_source_svg(&self, path: &Path) -> Result<String, String> {
        match &self.sources {
            Some(s) => {
                let bytes = s.read_path(path).map_err(|e| e.to_string())?;
                if bytes.len() as u64 > MAX_SVG_BYTES {
                    return Err("SVG exceeds local byte limit".into());
                }
                String::from_utf8(bytes.to_vec()).map_err(|e| format!("SVG is not UTF-8: {e}"))
            }
            None => read_svg(path),
        }
    }

    fn svg_options(&self, svg_path: &Path) -> resvg::usvg::Options<'static> {
        let mut options = resvg::usvg::Options::default();
        if let Some(sources) = self.sources.clone() {
            let base = svg_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();
            options.image_href_resolver.resolve_string = Box::new(move |href, options| {
                let bytes = match sources.read_resource(&base, Path::new(href)) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!("Immutable PC SVG resource unavailable: {error}");
                        return None;
                    }
                };
                resvg::usvg::ImageHrefResolver::default_data_resolver()(
                    "text/plain",
                    std::sync::Arc::new(bytes.to_vec()),
                    options,
                )
            });
        }
        options
    }

    /// Independently painted fill/stroke support for an uncut motif, using the
    /// same immutable resource, palette injection, aspect and pivot as raster
    /// paths. Logical retained-cache budget excludes caller-held old Arcs and
    /// parsing/stroking/tessellation scratch. Not a full False renderer yet.
    pub fn get_whole_symbol_support(
        &mut self,
        symbol_id: &str,
        color_profile: &ColorProfile,
        pixels_per_mm: f64,
        limits: crate::svg_painted_support::SvgSupportLimits,
    ) -> Result<Option<std::sync::Arc<crate::svg_painted_support::SvgPaintedSupport>>, String> {
        limits.validate()?;
        if !pixels_per_mm.is_finite() || pixels_per_mm <= 0. {
            return Err("Invalid whole SVG physical calibration".into());
        }
        const KEY_LIMIT: usize = 64 * 1024;
        const CACHE_LIMIT: usize = 16 * 1024 * 1024;
        if symbol_id.is_empty()
            || symbol_id.len() > KEY_LIMIT / 2
            || symbol_id.contains(['/', '\\', '\0'])
        {
            return Err("Invalid whole SVG resource reference".into());
        }
        let mut key = format!(
            "whole-svg-v1:{}:{}:{}:{:016x}:{:?}",
            self.resource_revision,
            symbol_id.len(),
            symbol_id,
            pixels_per_mm.to_bits(),
            limits
        );
        let predicted = color_profile
            .colors
            .iter()
            .try_fold(key.len(), |n, (token, _)| {
                n.checked_add(token.len()).and_then(|n| n.checked_add(64))
            })
            .ok_or("Whole SVG cache key overflow")?;
        if predicted > KEY_LIMIT {
            return Err("Whole SVG cache key budget exceeded".into());
        }
        let mut colors: Vec<_> = color_profile.colors.iter().collect();
        colors.sort_by(|a, b| a.0.cmp(b.0));
        for (token, color) in colors {
            use std::fmt::Write;
            write!(
                &mut key,
                ":{}:{}:{:?}",
                token.len(),
                token,
                color.get_srgb().map(|c| [c.r, c.g, c.b])
            )
            .unwrap();
        }
        if let Some(resource) = self.whole_supports.get(&key) {
            return Ok(resource.clone());
        }
        let (tree, pivot, scale) = self.load_whole_svg(symbol_id, color_profile, pixels_per_mm)?;
        let resource =
            crate::svg_painted_support::svg_painted_support(&tree, pivot, scale, limits)?
                .map(std::sync::Arc::new);
        let bytes = resource
            .as_ref()
            .map_or(0, |r| r.retained_payload_bytes)
            .checked_add(
                key.capacity()
                    .checked_mul(2)
                    .ok_or("Whole SVG key accounting overflow")?,
            )
            .and_then(|n| n.checked_add(128))
            .ok_or("Whole SVG cache accounting overflow")?;
        if bytes > CACHE_LIMIT {
            return Err("Whole SVG resource exceeds cache payload budget".into());
        }
        if self.whole_supports.len() >= 256
            || self
                .whole_support_bytes
                .checked_add(bytes)
                .is_none_or(|n| n > CACHE_LIMIT)
        {
            self.whole_supports.clear();
            self.whole_support_bytes = 0;
        }
        self.whole_support_bytes += bytes;
        self.whole_supports.insert(key, resource.clone());
        Ok(resource)
    }

    fn load_whole_svg(
        &self,
        symbol_id: &str,
        profile: &ColorProfile,
        ppm: f64,
    ) -> Result<(resvg::usvg::Tree, [f64; 2], f64), String> {
        let path = self.symbols_path.join(format!("{symbol_id}.svg"));
        let content = self.read_source_svg(&path)?;
        let view_box = self
            .extract_viewbox(&content)
            .ok_or("Whole S100 SVG requires a finite positive root viewBox")?;
        let colored = self.inject_colors(&content, profile);
        let tree = resvg::usvg::Tree::from_str(&colored, &self.svg_options(&path))
            .map_err(|e| e.to_string())?;
        let size = tree.size();
        let pivot = svg_origin_in_viewport(Some(view_box), size.width(), size.height());
        Ok((
            tree,
            [f64::from(pivot.0), f64::from(pivot.1)],
            ppm / f64::from(BASE_PX_PER_MM),
        ))
    }

    /// One uncut, upright bitmap and support from the same parsed immutable SVG.
    /// It cannot be used as a repeating lattice cell or authorize a False fill.
    pub fn get_whole_motif(
        &mut self,
        symbol_id: &str,
        profile: &ColorProfile,
        ppm: f64,
        support_limits: crate::svg_painted_support::SvgSupportLimits,
        raster_limits: crate::whole_motif::MotifRasterLimits,
    ) -> Result<Option<std::sync::Arc<crate::whole_motif::NaturalMotifResource>>, String> {
        support_limits.validate()?;
        if !ppm.is_finite() || ppm <= 0. {
            return Err("Invalid whole motif physical calibration".into());
        }
        const KEY_LIMIT: usize = 64 * 1024;
        const CACHE_LIMIT: usize = 64 * 1024 * 1024;
        if symbol_id.is_empty()
            || symbol_id.len() > KEY_LIMIT / 2
            || symbol_id.contains(['/', '\\', '\0'])
        {
            return Err("Invalid whole motif resource reference".into());
        }
        let mut key = format!(
            "whole-motif-v1:{}:{}:{}:{:016x}:{:?}:{:?}",
            self.resource_revision,
            symbol_id.len(),
            symbol_id,
            ppm.to_bits(),
            support_limits,
            raster_limits
        );
        let predicted = profile
            .colors
            .iter()
            .try_fold(key.len(), |n, (token, _)| {
                n.checked_add(token.len()).and_then(|n| n.checked_add(64))
            })
            .ok_or("Whole motif cache key overflow")?;
        if predicted > KEY_LIMIT {
            return Err("Whole motif cache key budget exceeded".into());
        }
        let mut colors: Vec<_> = profile.colors.iter().collect();
        colors.sort_by(|a, b| a.0.cmp(b.0));
        for (token, color) in colors {
            use std::fmt::Write;
            write!(
                &mut key,
                ":{}:{}:{:?}",
                token.len(),
                token,
                color.get_srgb().map(|c| [c.r, c.g, c.b])
            )
            .unwrap();
        }
        if let Some(resource) = self.whole_motifs.get(&key) {
            return Ok(resource.clone());
        }
        let (tree, pivot, scale) = self.load_whole_svg(symbol_id, profile, ppm)?;
        let resource = crate::whole_motif::build_natural_motif(
            key.clone(),
            &tree,
            pivot,
            scale,
            support_limits,
            raster_limits,
        )?
        .map(std::sync::Arc::new);
        let bytes = resource
            .as_ref()
            .map_or(0, |r| r.retained_payload_bytes)
            .checked_add(
                key.capacity()
                    .checked_mul(2)
                    .ok_or("Whole motif key accounting overflow")?,
            )
            .and_then(|n| n.checked_add(128))
            .ok_or("Whole motif cache accounting overflow")?;
        if bytes > CACHE_LIMIT {
            return Err("Whole motif resource exceeds cache payload budget".into());
        }
        if self.whole_motifs.len() >= 256
            || self
                .whole_motif_bytes
                .checked_add(bytes)
                .is_none_or(|n| n > CACHE_LIMIT)
        {
            self.whole_motifs.clear();
            self.whole_motif_bytes = 0;
        }
        self.whole_motif_bytes += bytes;
        self.whole_motifs.insert(key, resource.clone());
        Ok(resource)
    }

    /// Get or load symbol geometry.
    /// Hot path: single HashMap::get (no contains_key + get double lookup).
    /// Cold path (first load): runs once per unique symbol, not per frame.
    pub fn get_symbol(
        &mut self,
        symbol_id: &str,
        color_profile: &ColorProfile,
    ) -> Option<&SymbolGeometry> {
        // Hot path: already cached → single lookup
        if self.symbols.contains_key(symbol_id) {
            return self.symbols.get(symbol_id);
        }

        // Already known to be missing → skip file I/O
        if self.missing_symbols.contains(symbol_id) {
            return None;
        }

        // Cold path: first-time load
        self.load_symbol(symbol_id, color_profile);
        self.symbols.get(symbol_id)
    }

    /// Load a symbol from SVG file into cache (cold path, called once per symbol)
    #[cold]
    fn load_symbol(&mut self, symbol_id: &str, color_profile: &ColorProfile) {
        tracing::debug!(
            "Loading symbol: '{}' from {}",
            symbol_id,
            self.symbols_path.display()
        );

        let svg_path = self.symbols_path.join(format!("{}.svg", symbol_id));
        if !self.source_exists(&svg_path) {
            tracing::debug!("Symbol SVG not found: {}", svg_path.display());
            self.missing_symbols.insert(symbol_id.to_string());
            return;
        }

        match self.render_svg(&svg_path, symbol_id, color_profile) {
            Ok(geometry) => {
                tracing::debug!(
                    "Rendered symbol '{}': {}x{} pixels",
                    symbol_id,
                    geometry.width,
                    geometry.height
                );
                self.symbols.insert(symbol_id.to_string(), geometry);
            }
            Err(e) => {
                tracing::warn!("Failed to render SVG '{}': {}", symbol_id, e);
                self.missing_symbols.insert(symbol_id.to_string());
            }
        }
    }

    /// Get or load symbol at a specific target pixel size (for pattern fills).
    /// Renders at a scale that produces a texture close to the target display size,
    /// avoiding downscale artifacts that make thin lines appear thick.
    pub fn get_symbol_for_pattern(
        &mut self,
        symbol_id: &str,
        color_profile: &ColorProfile,
        target_width_px: f32,
        target_height_px: f32,
        mm_to_px: f32,
    ) -> Option<&SymbolGeometry> {
        let (tile_w, tile_h) = raster_dimensions(target_width_px, target_height_px).ok()?;
        if !mm_to_px.is_finite() || mm_to_px <= 0.0 {
            return None;
        }
        let key = pattern_texture_key(symbol_id, target_width_px, target_height_px, mm_to_px);
        if self.symbols.contains_key(&key) {
            return self.symbols.get(&key);
        }

        let svg_path = self.symbols_path.join(format!("{}.svg", symbol_id));
        if !self.source_exists(&svg_path) {
            return None;
        }

        let svg_content = match self.read_source_svg(&svg_path) {
            Ok(c) => c,
            Err(_) => return None,
        };

        let view_box = self.extract_viewbox(&svg_content);
        let svg_with_colors = self.inject_colors(&svg_content, color_profile);

        let opts = self.svg_options(&svg_path);
        let tree = match resvg::usvg::Tree::from_str(&svg_with_colors, &opts) {
            Ok(t) => t,
            Err(_) => return None,
        };

        let tree_size = tree.size();
        let vb_width = tree_size.width();
        let vb_height = tree_size.height();
        let (vb_min_x, vb_min_y) = match view_box {
            Some((x, y, _, _)) => (x, y),
            None => (0.0, 0.0),
        };

        // S-100: The tile size (target_width_px × target_height_px) defines the
        // tiling period. The SVG symbol must be rendered at its natural size
        // and centered within the tile.
        //
        // usvg already converts SVG mm units to pixels at 96 DPI, so
        // tree_size.width() is already in screen pixels at 96 DPI baseline.
        // We only need dpi_scale (= mm_to_px / BASE_PX_PER_MM) to adjust
        // for HiDPI displays.
        let dpi_scale = mm_to_px / BASE_PX_PER_MM;

        // SVG native pixel size (tree_size is already px at 96 DPI, apply dpi_scale)
        let (svg_px_w, svg_px_h) =
            raster_dimensions(vb_width * dpi_scale, vb_height * dpi_scale).ok()?;

        let mut pixmap = resvg::tiny_skia::Pixmap::new(tile_w, tile_h)?;

        // Center the SVG symbol within the tile
        let offset_x = (tile_w as f32 - svg_px_w as f32) / 2.0;
        let offset_y = (tile_h as f32 - svg_px_h as f32) / 2.0;

        // Render at dpi_scale (1 SVG user unit = dpi_scale output pixels)
        let transform = resvg::tiny_skia::Transform::from_scale(dpi_scale, dpi_scale)
            .post_translate(offset_x, offset_y);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        let pattern_render_scale = dpi_scale;
        let pivot = self.extract_pivot_point(&svg_content, vb_width, vb_height);

        let geom = SymbolGeometry {
            name: key.clone(),
            pixels: pixmap.data().to_vec(),
            width: tile_w,
            height: tile_h,
            bounds: (vb_min_x, vb_min_y, vb_width, vb_height),
            pivot,
            texture_pivot: {
                let p = svg_origin_in_viewport(view_box, vb_width, vb_height);
                (p.0 * dpi_scale + offset_x, p.1 * dpi_scale + offset_y)
            },
            render_scale: pattern_render_scale,
        };
        tracing::debug!(
            "Pattern symbol '{}': tile {}x{} px, svg {}x{} px (dpi_scale={:.2})",
            symbol_id,
            tile_w,
            tile_h,
            svg_px_w,
            svg_px_h,
            dpi_scale
        );
        self.symbols.insert(key.clone(), geom);
        self.symbols.get(&key)
    }

    /// Rasterize a periodic lattice cell. The SVG shape remains upright in
    /// output coordinates even when sites are sheared or rotated. Unlike the
    /// legacy rectangular pattern resource, the authored SVG origin is UV (0,0).
    pub fn get_symbol_for_lattice(
        &mut self,
        symbol_id: &str,
        color_profile: &ColorProfile,
        lattice: ferrite_render::PatternLattice,
        pixels_per_mm: f64,
    ) -> Result<&PeriodicPatternGeometry, String> {
        let key = lattice_cache_key(
            self.resource_revision,
            symbol_id,
            color_profile,
            lattice,
            pixels_per_mm,
        )?;
        self.get_symbol_for_lattice_key(symbol_id, color_profile, lattice, pixels_per_mm, &key)
    }
    fn get_symbol_for_lattice_key(
        &mut self,
        symbol_id: &str,
        color_profile: &ColorProfile,
        lattice: ferrite_render::PatternLattice,
        pixels_per_mm: f64,
        key: &String,
    ) -> Result<&PeriodicPatternGeometry, String> {
        const MAX_CACHE_ENTRIES: usize = 256;
        if !self.lattice_patterns.contains_key(key) {
            let path = self.symbols_path.join(format!("{symbol_id}.svg"));
            let content = self.read_source_svg(&path)?;
            let colored = self.inject_colors(&content, color_profile);
            let options = self.svg_options(&path);
            let tree =
                resvg::usvg::Tree::from_str(&colored, &options).map_err(|e| e.to_string())?;
            let size = tree.size();
            let view_box = self
                .extract_viewbox(&content)
                .ok_or("S100 pattern SVG requires a finite positive root viewBox")?;
            let pivot = svg_origin_in_viewport(Some(view_box), size.width(), size.height());
            let scale = pixels_per_mm / f64::from(BASE_PX_PER_MM);
            let bounds = tree.root().abs_layer_bounding_box();
            let relative_bounds = [
                (f64::from(bounds.left()) - f64::from(pivot.0)) * scale,
                (f64::from(bounds.top()) - f64::from(pivot.1)) * scale,
                (f64::from(bounds.right()) - f64::from(pivot.0)) * scale,
                (f64::from(bounds.bottom()) - f64::from(pivot.1)) * scale,
            ];
            let plan = ferrite_render::PatternCellPlan::new(
                lattice,
                relative_bounds,
                2.,
                ferrite_render::PatternCellLimits::default(),
            )?;
            let bytes = plan.width as usize * plan.height as usize * 4;
            const CACHE_BYTES: usize = 64 * 1024 * 1024;
            let accounted_bytes = bytes
                .checked_add(
                    key.capacity()
                        .checked_mul(2)
                        .ok_or("Pattern cache key byte overflow")?,
                )
                .and_then(|n| {
                    n.checked_add(std::mem::size_of::<(String, PeriodicPatternGeometry)>())
                })
                .ok_or("Pattern cache accounting overflow")?;
            if accounted_bytes > CACHE_BYTES {
                return Err("Pattern cache byte budget exceeded".into());
            }
            if self
                .lattice_pattern_bytes
                .checked_add(accounted_bytes)
                .is_none_or(|n| n > CACHE_BYTES)
                || self.lattice_patterns.len() >= MAX_CACHE_ENTRIES
            {
                // Evict only this new material's resources; retain point symbols
                // and legacy raster behavior. Returned borrows cannot survive a
                // mutable cache access, so no live CPU resource is invalidated.
                self.lattice_patterns.clear();
                self.lattice_pattern_bytes = 0;
            }
            let mut pixmap = resvg::tiny_skia::Pixmap::new(plan.width, plan.height)
                .ok_or("Pattern cell allocation failed")?;
            let rows = plan.raster_rows;
            for site in &plan.copies {
                let matrix = [
                    rows[0][0] * scale,
                    rows[1][0] * scale,
                    rows[0][1] * scale,
                    rows[1][1] * scale,
                    site[0] as f64 * f64::from(plan.width)
                        - (rows[0][0] * f64::from(pivot.0) + rows[0][1] * f64::from(pivot.1))
                            * scale,
                    site[1] as f64 * f64::from(plan.height)
                        - (rows[1][0] * f64::from(pivot.0) + rows[1][1] * f64::from(pivot.1))
                            * scale,
                ];
                if matrix
                    .iter()
                    .any(|v| !v.is_finite() || !(*v as f32).is_finite())
                {
                    return Err("Pattern cell transform overflow".into());
                }
                let transform = resvg::tiny_skia::Transform::from_row(
                    matrix[0] as f32,
                    matrix[1] as f32,
                    matrix[2] as f32,
                    matrix[3] as f32,
                    matrix[4] as f32,
                    matrix[5] as f32,
                );
                resvg::render(&tree, transform, &mut pixmap.as_mut());
            }
            let geometry = PeriodicPatternGeometry {
                name: key.clone(),
                pixels: pixmap.take(),
                width: plan.width,
                height: plan.height,
            };
            self.lattice_pattern_bytes += accounted_bytes;
            self.lattice_patterns.insert(key.clone(), geometry);
        }
        Ok(self
            .lattice_patterns
            .get(key)
            .expect("prepared periodic SVG cell"))
    }

    /// Render SVG file to pixel buffer using resvg
    fn render_svg(
        &self,
        svg_path: &Path,
        symbol_id: &str,
        color_profile: &ColorProfile,
    ) -> Result<SymbolGeometry, String> {
        // Read SVG file
        let svg_content = self.read_source_svg(svg_path)?;

        // Extract viewBox from original SVG (minX, minY, width, height)
        let view_box = self.extract_viewbox(&svg_content);

        // Inject CSS with resolved colors from color profile
        let svg_with_colors = self.inject_colors(&svg_content, color_profile);

        // Parse SVG with usvg
        let opts = self.svg_options(svg_path);
        let tree = resvg::usvg::Tree::from_str(&svg_with_colors, &opts)
            .map_err(|e| format!("Failed to parse SVG: {}", e))?;

        // Get size info from parsed tree
        let tree_size = tree.size();
        let vb_width = tree_size.width();
        let vb_height = tree_size.height();

        // Use extracted viewBox origin, or default to (0, 0)
        let (vb_min_x, vb_min_y) = match view_box {
            Some((x, y, _, _)) => (x, y),
            None => (0.0, 0.0),
        };

        // Calculate render size
        let (render_width, render_height) =
            raster_dimensions(vb_width * self.render_scale, vb_height * self.render_scale)?;

        // Create pixel buffer
        let mut pixmap = resvg::tiny_skia::Pixmap::new(render_width, render_height)
            .ok_or("Failed to create pixmap")?;

        // Render SVG
        let transform =
            resvg::tiny_skia::Transform::from_scale(self.render_scale, self.render_scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // Extract pivot point from SVG (look for circle with class "pivotPoint")
        let pivot = self.extract_pivot_point(&svg_content, vb_width, vb_height);

        Ok(SymbolGeometry {
            name: symbol_id.to_string(),
            pixels: pixmap.data().to_vec(),
            width: render_width,
            height: render_height,
            bounds: (vb_min_x, vb_min_y, vb_width, vb_height),
            pivot,
            texture_pivot: {
                let p = svg_origin_in_viewport(view_box, vb_width, vb_height);
                (p.0 * self.render_scale, p.1 * self.render_scale)
            },
            render_scale: self.render_scale,
        })
    }

    /// Inject CSS colors from color profile into SVG
    /// S-100 SVGs use classes like: sXXXXX (stroke color), fXXXXX (fill color)
    // S-100 9-B-3.1 requires the default xMidYMid uniform fit even when
    // a document supplies preserveAspectRatio. Alter only the parsed root
    // attribute in memory; immutable catalogue bytes remain untouched.
    fn normalize_s100_aspect_ratio(svg: &str) -> String {
        let range = resvg::usvg::roxmltree::Document::parse(svg)
            .ok()
            .and_then(|doc| {
                doc.root_element()
                    .attributes()
                    .find(|a| a.name() == "preserveAspectRatio" && a.namespace().is_none())
                    .map(|a| a.range())
            });
        let mut normalized = svg.to_owned();
        if let Some(range) = range {
            normalized.replace_range(range, "preserveAspectRatio=\"xMidYMid meet\"");
        }
        normalized
    }

    fn inject_colors(&self, svg_content: &str, color_profile: &ColorProfile) -> String {
        let normalized = Self::normalize_s100_aspect_ratio(svg_content);
        let svg_content = normalized.as_str();
        // Build CSS rules for all color tokens
        let mut css_rules = String::new();

        // Debug: Log which profile is being used for color injection
        tracing::debug!(
            "Injecting SVG colors from profile: '{}'",
            color_profile.name
        );

        // Extract all color tokens used in the SVG
        let color_tokens = self.extract_color_tokens(svg_content);

        for token in &color_tokens {
            if let Some(srgb) = color_profile.get_srgb(token) {
                let hex = format!("#{:02x}{:02x}{:02x}", srgb.r, srgb.g, srgb.b);

                // Stroke class: sXXXXX
                css_rules.push_str(&format!(".s{} {{ stroke: {}; }}\n", token, hex));

                // Fill class: fXXXXX
                css_rules.push_str(&format!(".f{} {{ fill: {}; }}\n", token, hex));

                tracing::debug!("  SVG color: {} -> {}", token, hex);
            } else {
                tracing::warn!("Color token '{}' not found in profile", token);
            }
        }

        // Also add common classes
        css_rules.push_str(".f0 { fill: none; }\n");
        css_rules.push_str(".sl { fill: none; }\n"); // stroke-line
        css_rules.push_str(".layout { display: none; }\n"); // hide layout elements

        // Use parsed tag/attribute ranges, so self-closing roots/defs, namespace
        // prefixes, quoted '>' and class quoting do not corrupt the XML.
        let Ok(document) = resvg::usvg::roxmltree::Document::parse(svg_content) else {
            return svg_content.to_owned();
        };
        let root = document.root_element();
        if root.tag_name().name() != "svg" {
            return svg_content.to_owned();
        }
        const NS: &str = "http://www.w3.org/2000/svg";
        let defs = root.descendants().find(|n| {
            n.is_element() && n.tag_name().name() == "defs" && n.tag_name().namespace() == Some(NS)
        });
        let target = defs.unwrap_or(root);
        let payload = if defs.is_some() {
            format!("<style xmlns=\"{NS}\">{css_rules}</style>")
        } else {
            format!("<defs xmlns=\"{NS}\"><style>{css_rules}</style></defs>")
        };
        let start = target.range().start;
        let attributes_end = target
            .attributes()
            .map(|a| a.range().end)
            .max()
            .unwrap_or(start);
        let Some(end) = svg_content[attributes_end..]
            .find('>')
            .map(|n| attributes_end + n)
        else {
            return svg_content.to_owned();
        };
        if svg_content[start..end].trim_end().ends_with('/') {
            let slash = svg_content[..end].rfind('/').unwrap();
            let qname = svg_content[start + 1..end]
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap();
            format!(
                "{}>{payload}</{qname}>{}",
                &svg_content[..slash],
                &svg_content[end + 1..]
            )
        } else {
            format!(
                "{}{payload}{}",
                &svg_content[..end + 1],
                &svg_content[end + 1..]
            )
        }
    }

    fn extract_color_tokens(&self, svg_content: &str) -> Vec<String> {
        let Ok(document) = resvg::usvg::roxmltree::Document::parse(svg_content) else {
            return vec![];
        };
        let mut seen = std::collections::HashSet::new();
        let mut tokens = Vec::new();
        for node in document.descendants().filter(|n| n.is_element()) {
            if let Some(classes) = node.attribute("class") {
                for class in classes.split_whitespace() {
                    if let Some(token) = class.strip_prefix('s').or_else(|| class.strip_prefix('f'))
                    {
                        if !token.is_empty()
                            && token
                                .bytes()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                            && token != "0"
                            && seen.insert(token.to_owned())
                        {
                            tokens.push(token.to_owned());
                        }
                    }
                }
            }
        }
        tokens
    }

    /// Extract viewBox from SVG content (minX, minY, width, height)
    fn extract_viewbox(&self, svg_content: &str) -> Option<(f32, f32, f32, f32)> {
        let document = resvg::usvg::roxmltree::Document::parse(svg_content).ok()?;
        let value = document.root_element().attribute("viewBox")?;
        let values: Vec<_> = value
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|v| !v.is_empty())
            .map(str::parse::<f32>)
            .collect::<Result<_, _>>()
            .ok()?;
        if values.len() != 4
            || !values.iter().all(|v| v.is_finite())
            || values[2] <= 0.
            || values[3] <= 0.
        {
            return None;
        }
        Some((values[0], values[1], values[2], values[3]))
    }

    /// Extract pivot point from SVG content
    fn extract_pivot_point(
        &self,
        svg_content: &str,
        _vb_width: f32,
        _vb_height: f32,
    ) -> (f32, f32) {
        // S-100 Part 9 SVG pivot is always the user-coordinate origin (0, 0).
        let _ = svg_content;
        (0.0, 0.0)
    }

    /// Clear all cached symbols
    pub fn clear(&mut self) {
        self.resource_revision = next_symbol_resource_revision();
        self.symbols.clear();
        self.missing_symbols.clear();
        self.lattice_patterns.clear();
        self.lattice_pattern_bytes = 0;
        self.whole_supports.clear();
        self.whole_support_bytes = 0;
        self.whole_motifs.clear();
        self.whole_motif_bytes = 0;
    }

    /// Get number of cached symbols
    pub fn len(&self) -> usize {
        self.symbols.len()
            + self.lattice_patterns.len()
            + self.whole_supports.len()
            + self.whole_motifs.len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
            && self.lattice_patterns.is_empty()
            && self.whole_supports.is_empty()
            && self.whole_motifs.is_empty()
    }
}

// Local renderer resource limits, not S-100 normative size constraints.
// Check before float-to-int conversion and before any pixel buffer allocation.
const MAX_SVG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SYMBOL_EDGE: u32 = 8192;
const MAX_SYMBOL_PIXELS: u64 = 16 * 1024 * 1024; // 64 MiB RGBA per texture.
fn raster_dimensions(width: f32, height: f32) -> Result<(u32, u32), String> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err("SVG raster dimensions must be finite and positive".into());
    }
    let width = width.ceil();
    let height = height.ceil();
    if width > MAX_SYMBOL_EDGE as f32 || height > MAX_SYMBOL_EDGE as f32 {
        return Err("SVG raster exceeds local edge limit".into());
    }
    let (width, height) = (width as u32, height as u32);
    if u64::from(width) * u64::from(height) > MAX_SYMBOL_PIXELS {
        return Err("SVG raster exceeds local pixel budget".into());
    }
    Ok((width, height))
}
fn read_svg(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Failed to open SVG: {e}"))?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_SVG_BYTES {
        return Err("SVG exceeds local byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SVG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_SVG_BYTES {
        return Err("SVG exceeds local byte limit".into());
    }
    String::from_utf8(bytes).map_err(|e| format!("SVG is not UTF-8: {e}"))
}

/// S-100 SVG uses the default xMidYMid meet viewport mapping. ViewBox units
/// need not be millimetres: physical dimensions come from SVG width/height.
fn svg_origin_in_viewport(
    view_box: Option<(f32, f32, f32, f32)>,
    width: f32,
    height: f32,
) -> (f32, f32) {
    let Some((x, y, w, h)) = view_box else {
        return (0.0, 0.0);
    };
    let scale = (width / w).min(height / h);
    (
        (width - w * scale) * 0.5 - x * scale,
        (height - h * scale) * 0.5 - y * scale,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn natural_motif_cache_preserves_exact_scale_palette_quality_and_old_arcs() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-natural-motif-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let svg = |x| {
            format!("<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='4mm' viewBox='-2 -2 4 4'><rect x='{x}' y='-1' width='1' height='1' class='fCHBLK'/></svg>")
        };
        std::fs::write(root.join("M.svg"), svg(0.25)).unwrap();
        std::fs::write(
            root.join("EMPTY.svg"),
            "<svg xmlns='http://www.w3.org/2000/svg' width='1mm' height='1mm' viewBox='0 0 1 1'/>",
        )
        .unwrap();
        let mut a = ColorProfile::default();
        a.colors.insert(
            "CHBLK".into(),
            ferrite_portrayal_catalog::ColorDefinition {
                token: "CHBLK".into(),
                srgb: Some(ferrite_portrayal_catalog::SrgbColor::new(11, 22, 33)),
                cie: None,
            },
        );
        let mut b = a.clone();
        b.colors.get_mut("CHBLK").unwrap().srgb =
            Some(ferrite_portrayal_catalog::SrgbColor::new(44, 55, 66));
        let mut cache = SymbolCache::new(&root);
        let support = crate::svg_painted_support::SvgSupportLimits::default();
        let raster = crate::whole_motif::MotifRasterLimits::default();
        let first = cache
            .get_whole_motif("M", &a, 4., support, raster)
            .unwrap()
            .unwrap();
        assert!(first.pixels.chunks_exact(4).any(|p| p == [11, 22, 33, 255]));
        assert!(std::sync::Arc::ptr_eq(
            &first,
            &cache
                .get_whole_motif("M", &a, 4., support, raster)
                .unwrap()
                .unwrap()
        ));
        let other = cache
            .get_whole_motif("M", &b, 4., support, raster)
            .unwrap()
            .unwrap();
        assert_ne!(first.resource_key, other.resource_key);
        assert_ne!(first.pixels, other.pixels);
        assert_eq!(first.support.bounds_px, other.support.bounds_px);
        let twice = cache
            .get_whole_motif("M", &a, 8., support, raster)
            .unwrap()
            .unwrap();
        for (x, y) in first
            .support
            .bounds_px
            .into_iter()
            .zip(twice.support.bounds_px)
        {
            assert!((2. * x - y).abs() < 1e-5);
        }
        assert!(cache
            .get_whole_motif(
                "M",
                &a,
                4.,
                support,
                crate::whole_motif::MotifRasterLimits {
                    max_rgba_bytes: 1,
                    ..raster
                }
            )
            .is_err());
        assert!(cache
            .get_whole_motif("../M", &a, 4., support, raster)
            .is_err());
        assert!(cache
            .get_whole_motif("EMPTY", &a, 4., support, raster)
            .unwrap()
            .is_none());
        assert!(cache.whole_motifs.values().any(|r| r.is_none()));
        assert!(cache.whole_motif_bytes <= 64 * 1024 * 1024);
        let revision = cache.resource_revision();
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.whole_motif_bytes, 0);
        assert!(cache.resource_revision() > revision);
        std::fs::write(root.join("M.svg"), svg(-0.75)).unwrap();
        let replacement = cache
            .get_whole_motif("M", &a, 4., support, raster)
            .unwrap()
            .unwrap();
        assert_ne!(first.resource_key, replacement.resource_key);
        assert!((replacement.support.bounds_px[0] + 3.).abs() < 1e-5);
        assert!((first.support.bounds_px[0] - 1.).abs() < 1e-5);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "Requires explicitly bound actual PC source and output receipt; CPU only, no windows"]
    fn actual_bound_pc_all_natural_motifs() {
        let root = std::path::PathBuf::from(std::env::var("FERRITE_SUPPORT_PC_ROOT").unwrap());
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
        let profile = pc.color_profiles.get_profile("Day").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(root.join("Symbols"))
            .unwrap()
            .map(|p| p.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "svg"))
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(!names.is_empty());
        let mut cache = SymbolCache::new_with_sources(root.join("Symbols"), pc.sources());
        let mut outcomes = Vec::new();
        let mut failures = Vec::new();
        for name in names {
            match cache.get_whole_motif(&name,profile,4.,Default::default(),Default::default()) {
                Ok(r)=>outcomes.push(serde_json::json!({"symbol":name,"painted":r.is_some(),
                    "triangles":r.as_ref().map_or(0,|r|r.support.triangle_count),"bitmap_size":r.as_ref().map(|r|[r.width,r.height]),"has_raster_coverage":r.as_ref().map(|r|r.has_coverage),
                    "support_bounds":r.as_ref().map(|r|r.support.bounds_px),"bitmap_origin":r.as_ref().map(|r|r.bitmap_origin_px),
                    "root_raster_cast_error":r.as_ref().map(|r|r.root_raster_cast_error_bound_px),"retained_cache_payload":cache.whole_motif_bytes})),
                Err(error)=>failures.push(serde_json::json!({"symbol":name,"error":error})),
            }
        }
        let receipt = serde_json::json!({"pc_root":root,"pc_digest":format!("{:02x?}",pc.source_digest()),
            "supported":outcomes,"failures":failures,"scope":"Natural uncut graphic bitmap/support only; actual False Scene and combined AA error pending"});
        std::fs::write(
            std::env::var("FERRITE_SUPPORT_PC_RECEIPT").unwrap(),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        assert!(
            failures.is_empty(),
            "actual PC natural motif failures: {failures:?}"
        );
    }
    #[test]
    fn whole_support_letterbox_pivot_and_palette_values_use_exact_cache_inputs() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-whole-letterbox-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("L.svg"),"<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='4mm' viewBox='-25 -10 100 50' preserveAspectRatio='none'><rect width='25' height='10' class = 'fCHBLK'/></svg>").unwrap();
        let mut a = ColorProfile::default();
        a.colors.insert(
            "CHBLK".into(),
            ferrite_portrayal_catalog::ColorDefinition {
                token: "CHBLK".into(),
                srgb: Some(ferrite_portrayal_catalog::SrgbColor::new(1, 2, 3)),
                cie: None,
            },
        );
        let mut b = a.clone();
        b.colors.get_mut("CHBLK").unwrap().srgb =
            Some(ferrite_portrayal_catalog::SrgbColor::new(4, 5, 6));
        assert_eq!(a.name, b.name);
        let mut cache = SymbolCache::new(&root);
        let x = cache
            .get_whole_symbol_support("L", &a, 4., Default::default())
            .unwrap()
            .unwrap();
        let y = cache
            .get_whole_symbol_support("L", &b, 4., Default::default())
            .unwrap()
            .unwrap();
        assert!(!std::sync::Arc::ptr_eq(&x, &y));
        for (got, want) in x.bounds_px.into_iter().zip([0., 0., 4., 1.6]) {
            assert!((got - want).abs() < 1e-5, "{:?}", x.bounds_px);
        }
        assert_eq!(x.bounds_px, y.bounds_px);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn parsed_palette_injection_handles_quotes_prefixes_defs_and_empty_roots() {
        let cache = SymbolCache::new("unused");
        let mut profile = ColorProfile::default();
        profile.colors.insert(
            "CHBLK".into(),
            ferrite_portrayal_catalog::ColorDefinition {
                token: "CHBLK".into(),
                srgb: Some(ferrite_portrayal_catalog::SrgbColor::new(23, 47, 71)),
                cie: None,
            },
        );
        for svg in ["<svg xmlns='http://www.w3.org/2000/svg' width='1mm' height='1mm' data-x='a > b'><rect width='1' height='1' class = 'fCHBLK'/></svg>",
            "<s:svg xmlns:s='http://www.w3.org/2000/svg' width='1mm' height='1mm'><s:defs id='d'/><s:rect width='1' height='1' class='fCHBLK'/></s:svg>",
            "<svg xmlns='http://www.w3.org/2000/svg' width='1mm' height='1mm'/>"] {
            let colored=cache.inject_colors(svg,&profile);
            assert!(resvg::usvg::roxmltree::Document::parse(&colored).is_ok(),"{colored}");
            let tree=resvg::usvg::Tree::from_str(&colored,&Default::default()).unwrap();
            if svg.contains("rect") {
                assert!(colored.contains("#172f47"));
                let mut pixmap=resvg::tiny_skia::Pixmap::new(8,8).unwrap();
                resvg::render(&tree,resvg::tiny_skia::Transform::identity(),&mut pixmap.as_mut());
                assert!(pixmap.pixels().iter().any(|p|p.red()==23 && p.green()==47 && p.blue()==71 && p.alpha()==255));
            }
        }
    }
    #[test]
    #[ignore = "requires SHA-guarded actual PC directory and receipt destination"]
    fn actual_bound_pc_all_svg_supports() {
        let root = std::path::PathBuf::from(std::env::var("FERRITE_SUPPORT_PC_ROOT").unwrap());
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(&root).unwrap();
        let profile = pc.color_profiles.get_profile("Day").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(root.join("Symbols"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "svg"))
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(!names.is_empty());
        let mut cache = SymbolCache::new_with_sources(root.join("Symbols"), pc.sources());
        let mut outcomes = Vec::new();
        let mut failures = Vec::new();
        for name in names {
            match cache.get_whole_symbol_support(&name,profile,4.,Default::default()) {
                Ok(resource)=>outcomes.push(serde_json::json!({"symbol":name,"painted":resource.is_some(),
                    "triangles":resource.as_ref().map_or(0,|r|r.triangle_count),"bounds":resource.as_ref().map(|r|r.bounds_px),
                    "retained_cache_payload":cache.whole_support_bytes})),
                Err(error)=>{failures.push(serde_json::json!({"symbol":name,"error":error}));}
            }
        }
        let receipt = serde_json::json!({"pc_root":root,"pc_digest":format!("{:02x?}",pc.source_digest()),
            "supported":outcomes,"failures":failures,"scope":"CPU support extraction only; no False Scene/AA conformance proof"});
        std::fs::write(
            std::env::var("FERRITE_SUPPORT_PC_RECEIPT").unwrap(),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        assert!(failures.is_empty(), "actual PC SVG failures: {failures:?}");
    }
    #[test]
    fn whole_support_cache_keeps_physical_pivot_quality_and_revision() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-whole-support-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let svg = |x| {
            format!("<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='4mm' viewBox='-2 -2 4 4' preserveAspectRatio='none'><rect x='{x}' y='-1' width='1' height='1'/></svg>")
        };
        std::fs::write(root.join("M.svg"), svg(0.25)).unwrap();
        std::fs::write(root.join("EMPTY.svg"),"<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='4mm' viewBox='-2 -2 4 4'/>").unwrap();
        let mut cache = SymbolCache::new(&root);
        let profile = ColorProfile::default();
        let limits = crate::svg_painted_support::SvgSupportLimits::default();
        let first = cache
            .get_whole_symbol_support("M", &profile, 4., limits)
            .unwrap()
            .unwrap();
        let again = cache
            .get_whole_symbol_support("M", &profile, 4., limits)
            .unwrap()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &again));
        assert!((first.bounds_px[0] - 1.).abs() < 1e-5);
        let twice = cache
            .get_whole_symbol_support("M", &profile, 8., limits)
            .unwrap()
            .unwrap();
        assert!(!std::sync::Arc::ptr_eq(&first, &twice));
        for (a, b) in first.bounds_px.into_iter().zip(twice.bounds_px) {
            assert!((2. * a - b).abs() < 1e-5);
        }
        assert!(cache
            .get_whole_symbol_support(
                "M",
                &profile,
                4.,
                crate::svg_painted_support::SvgSupportLimits {
                    max_triangles: 1,
                    ..limits
                }
            )
            .is_err());
        assert!(cache
            .get_whole_symbol_support("../M", &profile, 4., limits)
            .is_err());
        assert!(cache
            .get_whole_symbol_support("EMPTY", &profile, 4., limits)
            .unwrap()
            .is_none());
        assert!(cache.whole_supports.values().any(|r| r.is_none()));
        assert!(cache.whole_support_bytes <= 16 * 1024 * 1024);
        let old = cache.resource_revision();
        cache.clear();
        assert!(cache.resource_revision() > old);
        assert!(cache.whole_supports.is_empty());
        assert_eq!(cache.whole_support_bytes, 0);
        std::fs::write(root.join("M.svg"), svg(-0.75)).unwrap();
        let replacement = cache
            .get_whole_symbol_support("M", &profile, 4., limits)
            .unwrap()
            .unwrap();
        assert!((replacement.bounds_px[0] + 3.).abs() < 1e-5);
        assert!(!std::sync::Arc::ptr_eq(&first, &replacement));
        assert!((first.bounds_px[0] - 1.).abs() < 1e-5); // retained old Arc remains immutable
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn periodic_lattice_svg_keeps_asymmetric_shape_and_reference_seam() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-lattice-svg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // Origin sits well away from the viewport center. The unwarped motif is
        // an asymmetric axis-aligned rectangle right/up from that reference.
        std::fs::write(root.join("ASYM.svg"), r#"<svg xmlns="http://www.w3.org/2000/svg" width="4mm" height="4mm" viewBox="-2 -2 4 4"><rect x="0.25" y="-1" width="1.25" height="1" fill="red"/></svg>"#).unwrap();
        let mut cache = SymbolCache::new(&root);
        for (v1, v2) in [
            ((4., 0.), (0., 4.)),
            ((4., 0.), (2., 4.)),
            ((-4., 0.), (2., -4.)),
            ((0., 4.), (4., 0.)),
        ] {
            let lattice = ferrite_render::PatternLattice::from_mm(v1, v2, 4.).unwrap();
            let image = cache
                .get_symbol_for_lattice("ASYM", &ColorProfile::default(), lattice, 4.)
                .unwrap();
            assert!(image.pixels.iter().skip(3).step_by(4).any(|a| *a > 0));
            // Independent physical site oracle: at half-pixel centers away from
            // motif edges, test whether any unwarped rectangle contains the pixel.
            let mut checked = 0;
            let mut positive_probes = 0;
            for y in -20..21 {
                for x in -20..21 {
                    let physical = [f64::from(x) + 0.5, f64::from(y) + 0.5];
                    let mut expected = false;
                    let mut boundary = false;
                    for m in -5..6 {
                        for n in -5..6 {
                            let site = [
                                f64::from(v1.0) * 4. * f64::from(n)
                                    + f64::from(v2.0) * 4. * f64::from(m),
                                -f64::from(v1.1) * 4. * f64::from(n)
                                    - f64::from(v2.1) * 4. * f64::from(m),
                            ];
                            let q = [physical[0] - site[0], physical[1] - site[1]];
                            expected |= q[0] > 1. && q[0] < 6. && q[1] > -4. && q[1] < 0.;
                            if q[0] > 0. && q[0] < 7. && q[1] > -5. && q[1] < 1. {
                                boundary |= (q[0] - 1.).abs() < 0.8
                                    || (q[0] - 6.).abs() < 0.8
                                    || (q[1] + 4.).abs() < 0.8
                                    || q[1].abs() < 0.8;
                            }
                        }
                    }
                    if boundary {
                        continue;
                    }
                    let uv = lattice.coordinates(physical);
                    let tx = (uv[0].rem_euclid(1.) * f64::from(image.width)).floor() as u32
                        % image.width;
                    let ty = (uv[1].rem_euclid(1.) * f64::from(image.height)).floor() as u32
                        % image.height;
                    let alpha = image.pixels[((ty * image.width + tx) * 4 + 3) as usize];
                    assert_eq!(
                        alpha > 127,
                        expected,
                        "shape differs at {physical:?}, vectors {v1:?}/{v2:?}"
                    );
                    checked += 1;
                    positive_probes += usize::from(expected);
                }
            }
            assert!(checked > 400);
            assert!(
                positive_probes > 10,
                "solid motif interiors were not probed"
            );
        }
        assert_eq!(cache.lattice_patterns.len(), 4);
        cache.clear();
        assert_eq!(cache.lattice_pattern_bytes, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn periodic_cache_bounds_entries_and_rejects_oversized_keys_without_mutation() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-pattern-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("A.svg"),r#"<svg xmlns="http://www.w3.org/2000/svg" width="1mm" height="1mm" viewBox="0 0 1 1"><rect width="1" height="1" fill="red"/></svg>"#).unwrap();
        let mut cache = SymbolCache::new(&root);
        for i in 0..257 {
            let lattice =
                ferrite_render::PatternLattice::from_mm((4. + i as f32 / 100., 0.), (0., 4.), 1.)
                    .unwrap();
            cache
                .get_symbol_for_lattice("A", &Default::default(), lattice, 1.)
                .unwrap();
            assert!(cache.lattice_patterns.len() <= 256);
            assert!(cache.lattice_pattern_bytes <= 64 * 1024 * 1024);
        }
        assert_eq!(cache.lattice_patterns.len(), 1);
        let before = cache.lattice_pattern_bytes;
        let lattice = ferrite_render::PatternLattice::from_mm((4., 0.), (0., 4.), 1.).unwrap();
        assert!(cache
            .get_symbol_for_lattice(&"A".repeat(32769), &Default::default(), lattice, 1.)
            .unwrap_err()
            .contains("key budget"));
        let mut profile = ColorProfile::default();
        profile.colors.insert(
            "X".repeat(65536),
            ferrite_portrayal_catalog::ColorDefinition {
                token: "X".into(),
                srgb: None,
                cie: None,
            },
        );
        assert!(cache
            .get_symbol_for_lattice("A", &profile, lattice, 1.)
            .unwrap_err()
            .contains("key budget"));
        assert_eq!(cache.lattice_pattern_bytes, before);
        assert_eq!(cache.lattice_patterns.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_revisions_distinguish_same_name_catalogue_replacement_and_clear() {
        let mut a = SymbolCache::new("same-path");
        let b = SymbolCache::new("same-path");
        let original = a.resource_revision();
        assert_ne!(original, b.resource_revision());
        a.clear();
        assert_ne!(a.resource_revision(), original);
        assert_ne!(a.resource_revision(), b.resource_revision());
    }

    #[test]
    fn s100_aspect_default_is_shared_by_symbol_and_periodic_rendering() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-aspect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut cache = SymbolCache::new(&root);
        let mut expected_point = None;
        let mut expected_pattern = None;
        let lattice = ferrite_render::PatternLattice::from_mm((4., 0.), (2., 5.), 4.).unwrap();
        for (i, attr) in [
            "",
            "preserveAspectRatio='none'",
            "preserveAspectRatio = 'xMinYMax slice'",
        ]
        .iter()
        .enumerate()
        {
            let name = format!("ASPECT{i}");
            let svg=format!("<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='5mm' viewBox='-1 -1 4 2' {attr}><rect x='0' y='0' width='1' height='1' fill='red'/></svg>");
            std::fs::write(root.join(format!("{name}.svg")), &svg).unwrap();
            let point = cache
                .get_symbol(&name, &Default::default())
                .unwrap()
                .pixels
                .clone();
            let pattern = cache
                .get_symbol_for_lattice(&name, &Default::default(), lattice, 4.)
                .unwrap()
                .pixels
                .clone();
            assert!(point.iter().skip(3).step_by(4).any(|a| *a > 0));
            if let Some(ref expected) = expected_point {
                assert_eq!(&point, expected);
            } else {
                expected_point = Some(point);
            }
            if let Some(ref expected) = expected_pattern {
                assert_eq!(&pattern, expected);
            } else {
                expected_pattern = Some(pattern);
            }
            assert_eq!(
                std::fs::read_to_string(root.join(format!("{name}.svg"))).unwrap(),
                svg
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn viewbox_xml_quotes_spacing_and_root_selection_preserve_reference() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-svg-xml-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut cache = SymbolCache::new(&root);
        let variants = [
            "viewBox=\"-1 -2 4 5\"",
            "viewBox = '-1,-2,4,5'",
            "viewBox=\"-1  -2  4  5\"",
        ];
        let mut expected = None;
        for (i, attr) in variants.iter().enumerate() {
            let name = format!("XML{i}");
            let svg=format!("<svg xmlns='http://www.w3.org/2000/svg' width='4mm' height='5mm' {attr}><rect x='0' y='0' width='1' height='1' fill='red'/></svg>");
            assert_eq!(cache.extract_viewbox(&svg), Some((-1., -2., 4., 5.)));
            std::fs::write(root.join(format!("{name}.svg")), &svg).unwrap();
            let lattice = ferrite_render::PatternLattice::from_mm((4., 0.), (2., 5.), 4.).unwrap();
            let image = cache
                .get_symbol_for_lattice(&name, &Default::default(), lattice, 4.)
                .unwrap();
            if let Some(ref bytes) = expected {
                assert_eq!(bytes, &image.pixels);
            } else {
                expected = Some(image.pixels.clone());
            }
        }
        assert_eq!(
            cache.extract_viewbox("<svg><g viewBox='-1 -1 2 2'/></svg>"),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn periodic_lattice_color_and_orientation_have_distinct_cache_keys() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-lattice-colors-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("A.svg"),r#"<svg xmlns="http://www.w3.org/2000/svg" width="1mm" height="1mm" viewBox="0 0 1 1"><rect width="1" height="1" class="fTEST"/></svg>"#).unwrap();
        let mut cache = SymbolCache::new(&root);
        let mut profile = ColorProfile::default();
        profile.colors.insert(
            "TEST".into(),
            ferrite_portrayal_catalog::ColorDefinition {
                token: "TEST".into(),
                srgb: Some(ferrite_portrayal_catalog::SrgbColor::new(255, 0, 0)),
                cie: None,
            },
        );
        let lattice = ferrite_render::PatternLattice::from_mm((2., 0.), (1., 2.), 4.).unwrap();
        let a = cache
            .get_symbol_for_lattice("A", &profile, lattice, 4.)
            .unwrap()
            .clone();
        profile.colors.get_mut("TEST").unwrap().srgb =
            Some(ferrite_portrayal_catalog::SrgbColor::new(0, 0, 255));
        let b = cache
            .get_symbol_for_lattice("A", &profile, lattice, 4.)
            .unwrap()
            .clone();
        assert_ne!(a.name, b.name);
        assert_ne!(a.pixels, b.pixels);
        let reversed = ferrite_render::PatternLattice::from_mm((2., 0.), (-1., 2.), 4.).unwrap();
        let c = cache
            .get_symbol_for_lattice("A", &profile, reversed, 4.)
            .unwrap();
        assert_ne!(b.name, c.name);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_svg_has_identical_raster_and_survives_live_deletion() {
        let root = std::env::temp_dir().join(format!(
            "ferrite-svg-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Symbols")).unwrap();
        let path = root.join("Symbols/X.svg");
        std::fs::write(&path, r#"<svg xmlns="http://www.w3.org/2000/svg" width="3mm" height="3mm" viewBox="0 0 3 3"><path d="M0 0 L3 0 L3 3 Z" fill="red"/></svg>"#).unwrap();
        let sources = ferrite_portrayal_catalog::CatalogueSources::capture(&root).unwrap();
        let legacy = SymbolCache::new(root.join("Symbols"));
        let retained = SymbolCache::new_with_sources(root.join("Symbols"), sources);
        let profile = ColorProfile::new("Day".into(), "Day".into());
        let a = legacy.render_svg(&path, "X", &profile).unwrap();
        let b = retained.render_svg(&path, "X", &profile).unwrap();
        assert_eq!(a.pixels, b.pixels);
        assert_eq!((a.width, a.height), (b.width, b.height));
        let bits = |g: &SymbolGeometry| {
            [
                g.bounds.0.to_bits(),
                g.bounds.1.to_bits(),
                g.bounds.2.to_bits(),
                g.bounds.3.to_bits(),
                g.pivot.0.to_bits(),
                g.pivot.1.to_bits(),
            ]
        };
        assert_eq!(bits(&a), bits(&b));
        std::fs::remove_dir_all(&root).unwrap();
        let c = retained.render_svg(&path, "X", &profile).unwrap();
        assert_eq!(a.pixels, c.pixels);
        assert_eq!(bits(&a), bits(&c));
        assert!(retained
            .read_source_svg(&root.join("Symbols/missing.svg"))
            .is_err());
    }
    #[test]
    fn raster_limits_reject_nonfinite_saturated_and_oversized_allocations() {
        for (w, h) in [
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
            (-1.0, 1.0),
            (0.0, 1.0),
            (f32::MAX, 1.0),
            (8192.1, 1.0),
            (8192.0, 8192.0),
            (4096.1, 4096.0),
        ] {
            assert!(raster_dimensions(w, h).is_err(), "{w} {h}");
        }
        assert_eq!(raster_dimensions(0.1, 0.1).unwrap(), (1, 1));
        assert_eq!(raster_dimensions(4096.0, 4096.0).unwrap(), (4096, 4096));
        assert_eq!(raster_dimensions(8192.0, 2048.0).unwrap(), (8192, 2048));
    }
    #[test]
    fn none_style_class_is_not_a_colour_token() {
        let cache = SymbolCache::new("unused");
        assert_eq!(
            cache.extract_color_tokens(r#"<path class="fNone sCHMGF fDEPDW sSNDG1 sBKAJ1"/>"#),
            vec!["CHMGF", "DEPDW", "SNDG1", "BKAJ1"]
        );
    }
    #[test]
    fn svg_origin_with_arbitrary_units() {
        assert_eq!(
            svg_origin_in_viewport(Some((-50.0, -50.0, 100.0, 100.0)), 40.0, 40.0),
            (20.0, 20.0)
        );
    }
    #[test]
    fn svg_origin_accounts_for_letterboxing() {
        assert_eq!(
            svg_origin_in_viewport(Some((-10.0, -20.0, 20.0, 40.0)), 100.0, 100.0),
            (50.0, 50.0)
        );
    }
    #[test]
    fn viewbox_accepts_comma_separators() {
        let cache = SymbolCache::new("unused");
        assert_eq!(
            cache.extract_viewbox(r#"<svg viewBox="-50,-50,100,100"/>"#),
            Some((-50.0, -50.0, 100.0, 100.0))
        );
    }
    #[test]
    fn color_classes_accept_utf8_bom_and_unicode_without_panicking() {
        let cache = SymbolCache::new("unused");
        let svg = "\u{feff}<svg><path class=\"한글 sCHBLK fDEPDW\"/></svg>";
        assert_eq!(cache.extract_color_tokens(svg), vec!["CHBLK", "DEPDW"]);
    }
}

/// Shared CPU/GPU identity for the rasterized physical pattern tile. Shear is
/// a shader parameter; width, height and SVG raster calibration affect pixels.
pub(crate) fn pattern_texture_key(
    symbol: &str,
    width: f32,
    height: f32,
    pixels_per_mm: f32,
) -> String {
    format!(
        "{}_pat_{:08x}_{:08x}_{:08x}",
        symbol,
        width.to_bits(),
        height.to_bits(),
        pixels_per_mm.to_bits()
    )
}

#[cfg(test)]
mod request_keys_contracts {
    use super::*;
    fn legacy_key(
        revision: u64,
        symbol_id: &str,
        color_profile: &ColorProfile,
        lattice: ferrite_render::PatternLattice,
        pixels_per_mm: f64,
    ) -> Result<String, String> {
        if !pixels_per_mm.is_finite() || pixels_per_mm <= 0. {
            return Err("Invalid pattern SVG device calibration".into());
        }
        const MAX_KEY_BYTES: usize = 64 * 1024;
        if symbol_id.len() > MAX_KEY_BYTES / 2 {
            return Err("Pattern cache key budget exceeded".into());
        }
        let mut key = format!(
            "lattice-cell-v2:{}:{}:{}:{:016x}",
            revision,
            symbol_id.len(),
            symbol_id,
            pixels_per_mm.to_bits()
        );
        for value in lattice.columns().iter().flatten() {
            use std::fmt::Write;
            write!(&mut key, ":{:016x}", value.to_bits()).unwrap();
        }
        // Exact sorted resolved color inputs, avoiding profile-name aliases or
        // a collision-prone hash. A SymbolCache holds one immutable PC snapshot.
        let mut colors: Vec<_> = color_profile.colors.iter().collect();
        colors.sort_by(|a, b| a.0.cmp(b.0));
        for (token, color) in colors {
            if key
                .len()
                .checked_add(token.len())
                .and_then(|n| n.checked_add(64))
                .is_none_or(|n| n > MAX_KEY_BYTES)
            {
                return Err("Pattern cache key budget exceeded".into());
            }
            use std::fmt::Write;
            write!(
                &mut key,
                ":{}:{}:{:?}",
                token.len(),
                token,
                color.get_srgb().map(|c| [c.r, c.g, c.b])
            )
            .unwrap();
        }
        Ok(key)
    }
    fn profile() -> ColorProfile {
        let mut p = ColorProfile::new("same-name".into(), "same-name".into());
        for i in (0..97u8).rev() {
            let token = format!("TOKEN-{i}-한글");
            p.colors.insert(
                token.clone(),
                ferrite_portrayal_catalog::ColorDefinition {
                    token,
                    srgb: if i % 3 == 0 {
                        None
                    } else {
                        Some(ferrite_portrayal_catalog::SrgbColor::new(i, 255 - i, i / 2))
                    },
                    cie: None,
                },
            );
        }
        p
    }
    fn lattice() -> ferrite_render::PatternLattice {
        ferrite_render::PatternLattice::from_mm((4., -0.), (-2., 3.), 4.).unwrap()
    }
    #[test]
    fn request_keys_preserve_full_string_capacity_revision_and_bits() {
        let p = profile();
        let mut keys = PreparedLatticeKeys::new(&p);
        for revision in [1, 2, 1] {
            for ppm in [4., f64::from_bits(4f64.to_bits() + 1), 4.] {
                let expected = legacy_key(revision, "한글", &p, lattice(), ppm).unwrap();
                for _ in 0..3 {
                    let actual = keys.key(revision, "한글", lattice(), ppm).unwrap();
                    assert_eq!(actual.as_bytes(), expected.as_bytes());
                    assert_eq!(actual.capacity(), expected.capacity());
                }
            }
        }
        assert!(keys.hits > 0);
        assert_eq!(keys.entries.len(), 4);
        assert!(
            keys.bytes + keys.entries.capacity() * std::mem::size_of::<PreparedLatticeKey>()
                <= keys.budget
        );
    }
    #[test]
    fn request_keys_errors_and_budget_refusal_remain_original() {
        let p = profile();
        let mut keys = PreparedLatticeKeys::new(&p);
        keys.budget = 0;
        let huge = "x".repeat(32769);
        for ppm in [f64::NAN, f64::INFINITY, 0., -1., 4.] {
            let expected = legacy_key(1, &huge, &p, lattice(), ppm);
            let actual = keys.key(1, &huge, lattice(), ppm).map(|v| v.into_owned());
            assert_eq!(actual, expected);
            assert!(keys.entries.is_empty());
        }
        for _ in 0..3 {
            let expected = legacy_key(1, "valid", &p, lattice(), 4.).unwrap();
            let actual = keys.key(1, "valid", lattice(), 4.).unwrap();
            assert_eq!(actual.as_bytes(), expected.as_bytes());
            assert_eq!(actual.capacity(), expected.capacity());
        }
        assert_eq!(keys.hits, 0);
        assert_eq!(keys.refusals, 3);
    }
    #[test]
    fn request_keys_palette_identity_is_view_local_not_name() {
        let p = profile();
        let mut changed = profile();
        changed.colors.clear();
        let mut a = PreparedLatticeKeys::new(&p);
        let mut b = PreparedLatticeKeys::new(&changed);
        assert_ne!(
            a.key(1, "symbol", lattice(), 4.).unwrap().as_bytes(),
            b.key(1, "symbol", lattice(), 4.).unwrap().as_bytes()
        );
        for i in 0..300 {
            let symbol = format!("symbol{i}");
            let expected = legacy_key(1, &symbol, &p, lattice(), 4.).unwrap();
            let actual = a.key(1, &symbol, lattice(), 4.).unwrap();
            assert_eq!(actual.as_bytes(), expected.as_bytes());
        }
        assert!(a.entries.len() <= 256);
        assert!(
            a.bytes + a.entries.capacity() * std::mem::size_of::<PreparedLatticeKey>() <= a.budget
        );
        assert!(a.refusals > 0);
    }
    #[test]
    fn exact_formatter_snapshot_and_lattice_bit_changes() {
        let profile = profile();
        let mut keys = PreparedLatticeKeys::new(&profile);
        for name in ["", "ABC", "한글"] {
            for lattice in [
                lattice(),
                ferrite_render::PatternLattice::from_mm((4., 0.), (-2., 3.000000000000001), 4.)
                    .unwrap(),
            ] {
                let legacy = legacy_key(99, name, &profile, lattice, 4.).unwrap();
                let helper = lattice_cache_key(99, name, &profile, lattice, 4.).unwrap();
                assert_eq!(helper.as_bytes(), legacy.as_bytes());
                assert_eq!(helper.capacity(), legacy.capacity());
                let memo = keys.key(99, name, lattice, 4.).unwrap();
                assert_eq!(memo.as_bytes(), legacy.as_bytes());
                assert_eq!(memo.capacity(), legacy.capacity());
            }
        }
        assert_eq!(keys.entries.len(), 6);
        let mut huge = ColorProfile::new("large".into(), "large".into());
        let token = "X".repeat(65536);
        huge.colors.insert(
            token.clone(),
            ferrite_portrayal_catalog::ColorDefinition {
                token,
                srgb: None,
                cie: None,
            },
        );
        let mut keys = PreparedLatticeKeys::new(&huge);
        assert_eq!(
            keys.key(1, "A", lattice(), 4.).map(|v| v.into_owned()),
            legacy_key(1, "A", &huge, lattice(), 4.)
        );
        assert!(keys.entries.is_empty());
    }
}

#[cfg(test)]
mod publication_fork_tests {
    use super::*;
    #[test]
    fn empty_fork_preserves_source_configuration_without_invalidating_live_cache() {
        let mut live = SymbolCache::new("unused-publication-test-path");
        live.render_scale = 7.25;
        live.missing_symbols
            .insert("retained-missing-symbol".into());
        let revision = live.resource_revision();
        let next = live.fork_empty();
        assert_eq!(live.resource_revision(), revision);
        assert!(live.missing_symbols.contains("retained-missing-symbol"));
        assert!(next.missing_symbols.is_empty());
        assert!(
            next.symbols.is_empty()
                && next.whole_motifs.is_empty()
                && next.whole_supports.is_empty()
        );
        assert_ne!(next.resource_revision(), revision);
        assert_eq!(next.symbols_path, live.symbols_path);
        assert_eq!(next.render_scale.to_bits(), live.render_scale.to_bits());
    }
}
