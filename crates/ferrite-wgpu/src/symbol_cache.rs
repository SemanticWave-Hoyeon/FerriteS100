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
    /// Scale factor used for rendering (pixels per mm)
    pub render_scale: f32,
}

impl SymbolGeometry {
    /// Get pivot position within the texture (in pixels from top-left)
    /// Note: The texture is rendered by resvg which converts mm units to pixels at 96 DPI,
    /// then we apply render_scale. So pivot must also include the mm-to-pixel conversion.
    pub fn pivot_in_texture(&self) -> (f32, f32) {
        let (vb_x, vb_y, _, _) = self.bounds;
        let (piv_x, piv_y) = self.pivot;
        // usvg converts mm to pixels at 96 DPI (96/25.4 ≈ 3.78 pixels per mm)
        const MM_TO_PX: f32 = 96.0 / 25.4;
        (
            (piv_x - vb_x) * MM_TO_PX * self.render_scale,
            (piv_y - vb_y) * MM_TO_PX * self.render_scale,
        )
    }

    /// Check if this symbol has even-odd fill data (always false for resvg, handled internally)
    pub fn has_even_odd_fill(&self) -> bool {
        false // resvg handles even-odd fill internally
    }
}

/// Symbol cache for efficient symbol rendering
#[derive(Debug)]
pub struct SymbolCache {
    /// Cached symbol geometry (keyed by symbol ID)
    symbols: HashMap<String, SymbolGeometry>,
    /// Symbol IDs that failed to load (avoid re-attempting every frame)
    missing_symbols: HashSet<String>,
    /// Base path for symbol SVG files
    symbols_path: std::path::PathBuf,
    /// Render scale (pixels per mm) - higher = better quality but more memory
    render_scale: f32,
}

/// S-100 standard: 96 DPI base (pixels per mm = 96/25.4 ≈ 3.78)
const BASE_PX_PER_MM: f32 = 96.0 / 25.4;

/// Quality multiplier for SVG rasterization (2x = sharper symbols)
const RENDER_QUALITY_MULTIPLIER: f32 = 2.0;

impl SymbolCache {
    /// Create new symbol cache
    ///
    /// Symbols are rasterized at `BASE_PX_PER_MM × RENDER_QUALITY_MULTIPLIER` pixels per mm
    /// for high-quality display. The actual screen size is determined by S100_PX_PER_MM
    /// in the renderer.
    pub fn new<P: AsRef<Path>>(symbols_path: P) -> Self {
        SymbolCache {
            symbols: HashMap::new(),
            missing_symbols: HashSet::new(),
            symbols_path: symbols_path.as_ref().to_path_buf(),
            render_scale: BASE_PX_PER_MM * RENDER_QUALITY_MULTIPLIER,
        }
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
        if !svg_path.exists() {
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
        let key = format!("{}_pat", symbol_id);
        if self.symbols.contains_key(&key) {
            return self.symbols.get(&key);
        }

        let svg_path = self.symbols_path.join(format!("{}.svg", symbol_id));
        if !svg_path.exists() {
            return None;
        }

        let svg_content = match std::fs::read_to_string(&svg_path) {
            Ok(c) => c,
            Err(_) => return None,
        };

        let view_box = self.extract_viewbox(&svg_content);
        let svg_with_colors = self.inject_colors(&svg_content, color_profile);

        let opts = resvg::usvg::Options::default();
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

        let tile_w = target_width_px.ceil() as u32;
        let tile_h = target_height_px.ceil() as u32;
        let tile_w = tile_w.max(1);
        let tile_h = tile_h.max(1);

        // SVG native pixel size (tree_size is already px at 96 DPI, apply dpi_scale)
        let svg_px_w = (vb_width * dpi_scale).ceil() as u32;
        let svg_px_h = (vb_height * dpi_scale).ceil() as u32;

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

    /// Render SVG file to pixel buffer using resvg
    fn render_svg(
        &self,
        svg_path: &Path,
        symbol_id: &str,
        color_profile: &ColorProfile,
    ) -> Result<SymbolGeometry, String> {
        // Read SVG file
        let svg_content =
            std::fs::read_to_string(svg_path).map_err(|e| format!("Failed to read SVG: {}", e))?;

        // Extract viewBox from original SVG (minX, minY, width, height)
        let view_box = self.extract_viewbox(&svg_content);

        // Inject CSS with resolved colors from color profile
        let svg_with_colors = self.inject_colors(&svg_content, color_profile);

        // Parse SVG with usvg
        let opts = resvg::usvg::Options::default();
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
        let render_width = (vb_width * self.render_scale).ceil() as u32;
        let render_height = (vb_height * self.render_scale).ceil() as u32;

        // Ensure minimum size
        let render_width = render_width.max(1);
        let render_height = render_height.max(1);

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
            render_scale: self.render_scale,
        })
    }

    /// Inject CSS colors from color profile into SVG
    /// S-100 SVGs use classes like: sXXXXX (stroke color), fXXXXX (fill color)
    fn inject_colors(&self, svg_content: &str, color_profile: &ColorProfile) -> String {
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

        // Inject CSS into SVG
        if svg_content.contains("<defs>") {
            svg_content.replace("<defs>", &format!("<defs><style>{}</style>", css_rules))
        } else if let Some(svg_start) = svg_content.find("<svg") {
            // Find the closing '>' of the <svg> tag
            let after_svg = &svg_content[svg_start..];
            if let Some(svg_end) = after_svg.find('>') {
                let insert_pos = svg_start + svg_end + 1;
                let (before, after) = svg_content.split_at(insert_pos);
                format!(
                    "{}<defs><style>{}</style></defs>{}",
                    before, css_rules, after
                )
            } else {
                svg_content.to_string()
            }
        } else {
            svg_content.to_string()
        }
    }

    /// Extract color tokens from SVG content
    fn extract_color_tokens(&self, svg_content: &str) -> Vec<String> {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut tokens = Vec::new();

        // Look for class attributes containing color tokens
        // Pattern: class="... sXXXXX ..." or class="... fXXXXX ..."
        // S-100 color tokens are uppercase (e.g., CHBLK, CHMGD, DEPDW)
        for class_match in svg_content.split("class=\"") {
            if let Some(end) = class_match.find('"') {
                let class_str = &class_match[..end];
                for class in class_str.split_whitespace() {
                    // Check for stroke color (sXXXXX) or fill color (fXXXXX)
                    if class.len() > 1 {
                        let prefix = &class[..1];
                        let token = &class[1..];
                        if (prefix == "s" || prefix == "f")
                            && !token.is_empty()
                            && token != "0"
                            && token != "l"
                            // S-100 color tokens are uppercase (CHBLK, CHMGD, etc.)
                            // Skip non-color classes like "symbolBox", "svgBox", "sl"
                            && token.chars().next().is_some_and(|c| c.is_uppercase())
                            && seen.insert(token)
                        // O(1) check + insert
                        {
                            tokens.push(token.to_string());
                        }
                    }
                }
            }
        }

        tokens
    }

    /// Extract viewBox from SVG content (minX, minY, width, height)
    fn extract_viewbox(&self, svg_content: &str) -> Option<(f32, f32, f32, f32)> {
        // Look for viewBox attribute: viewBox="minX minY width height"
        let pattern = "viewBox=\"";
        if let Some(start) = svg_content.find(pattern) {
            let value_start = start + pattern.len();
            let remaining = &svg_content[value_start..];
            if let Some(end) = remaining.find('"') {
                let viewbox_str = &remaining[..end];
                let parts: Vec<f32> = viewbox_str
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if parts.len() == 4 {
                    return Some((parts[0], parts[1], parts[2], parts[3]));
                }
            }
        }
        None
    }

    /// Extract pivot point from SVG content
    fn extract_pivot_point(
        &self,
        svg_content: &str,
        _vb_width: f32,
        _vb_height: f32,
    ) -> (f32, f32) {
        // Look for circle with class="pivotPoint"
        // Example: <circle class="pivotPoint layout" cx="0" cy="0" r="1"/>

        // Find pivotPoint circle
        if let Some(start) = svg_content.find("pivotPoint") {
            // Find the <circle tag before this
            let before = &svg_content[..start];
            if let Some(circle_start) = before.rfind("<circle") {
                let circle_content = &svg_content[circle_start..];
                if let Some(end) = circle_content.find("/>") {
                    let circle_tag = &circle_content[..end];

                    // Extract cx and cy attributes
                    let cx = self.extract_attribute(circle_tag, "cx").unwrap_or(0.0);
                    let cy = self.extract_attribute(circle_tag, "cy").unwrap_or(0.0);

                    return (cx, cy);
                }
            }
        }

        // Default pivot at origin
        (0.0, 0.0)
    }

    /// Extract numeric attribute from XML tag
    fn extract_attribute(&self, tag: &str, attr: &str) -> Option<f32> {
        let pattern = format!("{}=\"", attr);
        if let Some(start) = tag.find(&pattern) {
            let value_start = start + pattern.len();
            let remaining = &tag[value_start..];
            if let Some(end) = remaining.find('"') {
                let value_str = &remaining[..end];
                return value_str.parse().ok();
            }
        }
        None
    }

    /// Clear all cached symbols
    pub fn clear(&mut self) {
        self.symbols.clear();
    }

    /// Get number of cached symbols
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
}
