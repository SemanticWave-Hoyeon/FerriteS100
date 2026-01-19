//! Symbol Cache for S-100 SVG Symbols
//!
//! Uses resvg for accurate SVG rendering including even-odd fill rule.
//! Renders SVG symbols to pixel buffers that are uploaded as GPU textures.

use std::collections::HashMap;
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
    /// Base path for symbol SVG files
    symbols_path: std::path::PathBuf,
    /// Render scale (pixels per mm) - higher = better quality but more memory
    render_scale: f32,
}

impl SymbolCache {
    /// Create new symbol cache
    /// render_scale: pixels per mm (default ~3.78 for 96 DPI)
    pub fn new<P: AsRef<Path>>(symbols_path: P) -> Self {
        SymbolCache {
            symbols: HashMap::new(),
            symbols_path: symbols_path.as_ref().to_path_buf(),
            render_scale: 3.78 * 2.0, // 2x scale for better quality
        }
    }

    /// Get or load symbol geometry
    pub fn get_symbol(
        &mut self,
        symbol_id: &str,
        color_profile: &ColorProfile,
    ) -> Option<&SymbolGeometry> {
        // Return cached if exists
        if self.symbols.contains_key(symbol_id) {
            return self.symbols.get(symbol_id);
        }

        tracing::debug!("Loading symbol: '{}' from {}", symbol_id, self.symbols_path.display());

        // Try to load from file
        let svg_path = self.symbols_path.join(format!("{}.svg", symbol_id));
        if !svg_path.exists() {
            tracing::debug!("Symbol SVG not found: {}", svg_path.display());
            return None;
        }

        match self.render_svg(&svg_path, symbol_id, color_profile) {
            Ok(geometry) => {
                tracing::debug!(
                    "Rendered symbol '{}': {}x{} pixels",
                    symbol_id, geometry.width, geometry.height
                );
                self.symbols.insert(symbol_id.to_string(), geometry);
                self.symbols.get(symbol_id)
            }
            Err(e) => {
                tracing::warn!("Failed to render SVG '{}': {}", symbol_id, e);
                None
            }
        }
    }

    /// Render SVG file to pixel buffer using resvg
    fn render_svg(
        &self,
        svg_path: &Path,
        symbol_id: &str,
        color_profile: &ColorProfile,
    ) -> Result<SymbolGeometry, String> {
        // Read SVG file
        let svg_content = std::fs::read_to_string(svg_path)
            .map_err(|e| format!("Failed to read SVG: {}", e))?;

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
        let transform = resvg::tiny_skia::Transform::from_scale(
            self.render_scale,
            self.render_scale,
        );
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

        // Extract all color tokens used in the SVG
        let color_tokens = self.extract_color_tokens(svg_content);

        for token in &color_tokens {
            if let Some(srgb) = color_profile.get_srgb(token) {
                let hex = format!("#{:02x}{:02x}{:02x}", srgb.r, srgb.g, srgb.b);

                // Stroke class: sXXXXX
                css_rules.push_str(&format!(".s{} {{ stroke: {}; }}\n", token, hex));

                // Fill class: fXXXXX
                css_rules.push_str(&format!(".f{} {{ fill: {}; }}\n", token, hex));

                tracing::trace!("Color token '{}' -> {}", token, hex);
            } else {
                tracing::warn!("Color token '{}' not found in profile", token);
            }
        }

        // Also add common classes
        css_rules.push_str(".f0 { fill: none; }\n");
        css_rules.push_str(".sl { fill: none; }\n");  // stroke-line
        css_rules.push_str(".layout { display: none; }\n");  // hide layout elements

        // Inject CSS into SVG
        if svg_content.contains("<defs>") {
            svg_content.replace("<defs>", &format!("<defs><style>{}</style>", css_rules))
        } else if let Some(svg_start) = svg_content.find("<svg") {
            // Find the closing '>' of the <svg> tag
            let after_svg = &svg_content[svg_start..];
            if let Some(svg_end) = after_svg.find('>') {
                let insert_pos = svg_start + svg_end + 1;
                let (before, after) = svg_content.split_at(insert_pos);
                format!("{}<defs><style>{}</style></defs>{}", before, css_rules, after)
            } else {
                svg_content.to_string()
            }
        } else {
            svg_content.to_string()
        }
    }

    /// Extract color tokens from SVG content
    fn extract_color_tokens(&self, svg_content: &str) -> Vec<String> {
        let mut tokens = Vec::new();

        // Look for class attributes containing color tokens
        // Pattern: class="... sXXXXX ..." or class="... fXXXXX ..."
        for class_match in svg_content.split("class=\"") {
            if let Some(end) = class_match.find('"') {
                let class_str = &class_match[..end];
                for class in class_str.split_whitespace() {
                    // Check for stroke color (sXXXXX) or fill color (fXXXXX)
                    if class.len() > 1 {
                        let prefix = &class[..1];
                        let token = &class[1..];
                        if (prefix == "s" || prefix == "f") && !token.is_empty() && token != "0" && token != "l" {
                            if !tokens.contains(&token.to_string()) {
                                tokens.push(token.to_string());
                            }
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
    fn extract_pivot_point(&self, svg_content: &str, _vb_width: f32, _vb_height: f32) -> (f32, f32) {
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
