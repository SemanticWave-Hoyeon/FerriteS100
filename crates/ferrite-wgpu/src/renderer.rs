//! Main wgpu Renderer
//!
//! Orchestrates rendering of drawing instructions to the screen.
//! Uses resvg for SVG symbol rendering via textures.

// =============================================================================
// S-100/S-101 Symbol Scaling Constants
// =============================================================================
// Reference: S-100 Edition 5.0, Part 12 - Portrayal; IHO S-52 Presentation Library
//
// Symbol sizes in the Portrayal Catalogue (SVG viewBox) are defined in millimeters
// for vector quality. However, these are NOT the intended physical display sizes.
//
// S-100/S-52 specifies that symbols should be displayed at sizes that ensure:
// - Readability at normal viewing distance (约70cm for ECDIS)
// - Consistent appearance across different display densities
// - Symbols typically appear 2-5mm physical size on screen
//
// The 0.3mm/pixel reference in S-100 is for MINIMUM LEGIBLE FEATURE SIZE
// (line widths, text heights), not for symbol scaling.

/// Standard screen DPI (96 DPI = 3.78 pixels per mm)
/// This is the typical display density for computer monitors.
const SCREEN_PX_PER_MM: f32 = 96.0 / 25.4;

/// S-101 symbol display scale factor
/// Converts from SVG definition mm to intended physical display size.
/// SVG symbols are defined larger than display size for vector quality.
/// This factor ensures symbols appear at appropriate 2-5mm physical sizes.
/// Reference: S-52 Presentation Library symbol specifications
const S101_SYMBOL_SCALE: f32 = 0.35;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::egui_integration;
use ferrite_portrayal_catalog::ColorProfile;
use ferrite_render::{
    intern_symbol, Color, DrawingInstruction, RenderContext, ScreenPoint, SymbolId, WorldPoint,
};

use crate::egui_integration::{AppUiState, EguiIntegration, SettingsState};
use crate::pipeline::TextureVertex;
use crate::{GpuState, RenderPipelines, Result, SymbolCache, Vertex2D, ViewUniforms, WgpuError};

/// Cached triangulation for an area (optimization)
#[derive(Clone)]
struct CachedTriangulation {
    vertices: Vec<Vertex2D>,
    indices: Vec<u32>,
    /// Hash of the geometry for invalidation
    geometry_hash: u64,
}

/// Batched symbols grouped by texture for efficient rendering
#[allow(dead_code)]
struct SymbolBatch {
    /// All vertices for this texture batch
    vertices: Vec<TextureVertex>,
    /// All indices for this texture batch
    indices: Vec<u32>,
    /// The texture bind group
    bind_group_idx: usize,
}

/// Simple Quadtree node for spatial indexing
#[allow(dead_code)]
struct QuadTreeNode {
    bounds: (f64, f64, f64, f64), // (min_x, min_y, max_x, max_y)
    feature_ids: Vec<i64>,
    children: Option<Box<[QuadTreeNode; 4]>>,
}

#[allow(dead_code)]
impl QuadTreeNode {
    fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        QuadTreeNode {
            bounds: (min_x, min_y, max_x, max_y),
            feature_ids: Vec::new(),
            children: None,
        }
    }

    /// Query features intersecting with viewport
    fn query(&self, viewport: (f64, f64, f64, f64), result: &mut Vec<i64>) {
        // Check if this node intersects viewport
        if !Self::intersects(self.bounds, viewport) {
            return;
        }

        // Add features from this node
        result.extend(&self.feature_ids);

        // Recurse into children
        if let Some(ref children) = self.children {
            for child in children.iter() {
                child.query(viewport, result);
            }
        }
    }

    #[inline]
    fn intersects(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
        a.0 <= b.2 && a.2 >= b.0 && a.1 <= b.3 && a.3 >= b.1
    }
}

/// Hash a slice of world points for cache key
fn hash_geometry(points: &[WorldPoint]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in points {
        ((p.x * 1_000_000.0) as i64).hash(&mut hasher);
        ((p.y * 1_000_000.0) as i64).hash(&mut hasher);
    }
    hasher.finish()
}

/// Cached GPU texture for a symbol
struct SymbolTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// Pivot position within texture (in pixels from top-left at render_scale)
    pivot_in_texture: (f32, f32),
    render_scale: f32,
}

/// Symbol instance to render
/// Memory optimized: uses interned SymbolId (4 bytes) instead of String (24 bytes)
#[derive(Clone, Copy)]
struct SymbolInstance {
    symbol_id: ferrite_render::SymbolId,
    screen_x: f32,
    screen_y: f32,
    scale: f32,
    rotation: f32,
}

/// wgpu-based chart renderer
pub struct WgpuRenderer {
    pub state: GpuState,
    pub pipelines: RenderPipelines,
    pub view_buffer: wgpu::Buffer,
    pub view_bind_group: wgpu::BindGroup,
    /// Collected area vertices
    area_vertices: Vec<Vertex2D>,
    area_indices: Vec<u32>,
    /// Collected line vertices
    line_vertices: Vec<Vertex2D>,
    line_indices: Vec<u32>,
    /// GPU texture cache for symbols (keyed by interned SymbolId for cache efficiency)
    symbol_textures: HashMap<ferrite_render::SymbolId, SymbolTexture>,
    /// Symbol instances to render
    symbol_instances: Vec<SymbolInstance>,
    /// Background color
    pub background_color: Color,
    /// User-adjustable symbol scale factor (default 1.0)
    /// This is applied ON TOP of the S-100 standard sizing.
    /// 1.0 = standard S-100 size, 0.5 = half size, 2.0 = double size
    pub symbol_scale: f32,
    /// Show sounding symbols (viewing group 33010)
    pub show_soundings: bool,
    /// Current zoom level (1.0 = default, higher = zoomed in)
    pub zoom_level: f64,
    /// Chart compilation scale (e.g., 22000 for 1:22000)
    /// Used to calculate viewing scale for S-101 feature filtering
    pub compilation_scale: u32,
    /// Grid for symbol decluttering (screen space) - for non-sounding symbols
    symbol_grid: std::collections::HashSet<(i32, i32)>,
    /// Screen-space grid for sounding decluttering
    /// Key: (screen_x / cell_size) as i32, (screen_y / cell_size) as i32
    /// Value: (world_position_key, depth) - keeps the shallowest sounding for safety
    sounding_screen_grid: std::collections::HashMap<(i32, i32), ((i64, i64), f64)>,
    /// Exact positions of soundings that have been allowed through (world coordinates)
    /// Key: (world_x * 1000000) as i64, (world_y * 1000000) as i64
    /// This ensures all digits of the same sounding value are rendered
    sounding_exact_positions: std::collections::HashSet<(i64, i64)>,
    /// World-coordinate deduplication (to remove exact duplicates from multiple charts)
    /// Key: (world_x * 1000000) as i64, (world_y * 1000000) as i64, symbol_type_hash
    world_dedup: std::collections::HashSet<(i64, i64, u64)>,
    /// Separate grid for danger symbols (ISODGR, DANGER02) decluttering
    /// These are low-priority but should still declutter among themselves
    danger_grid: std::collections::HashSet<(i32, i32)>,
    /// Grid cell size in pixels (adjusted by zoom)
    grid_cell_size: f32,
    /// Sounding grid cell size in pixels (screen-space)
    /// Fixed size for consistent density regardless of zoom
    sounding_cell_size_px: f32,
    /// Danger symbol grid cell size in pixels
    danger_cell_size_px: f32,
    /// Skip screen-space decluttering during animation (when preserve_declutter is true)
    skip_screen_declutter: bool,
    /// Screen-space pan offset (pixels) for fast panning during drag
    screen_pan_offset: (f32, f32),
    /// egui integration for UI overlay
    egui: EguiIntegration,
    /// UI state shared with main app
    pub ui_state: AppUiState,
    // === OPTIMIZATION FIELDS ===
    /// Cached triangulations by feature ID (optimization)
    triangulation_cache: HashMap<i64, CachedTriangulation>,
    /// Batched symbols by texture (optimization, keyed by interned SymbolId)
    symbol_batches: HashMap<SymbolId, (Vec<TextureVertex>, Vec<u32>)>,
    /// Animation/drag mode - enables fast-path rendering
    pub animation_mode: bool,
    /// LOD level (0=full detail, 1=medium, 2=low)
    lod_level: u8,
    /// Viewport bounds in world coordinates for culling
    viewport_world_bounds: Option<(f64, f64, f64, f64)>,
    // === S-101 PRIORITY GROUP RENDERING ===
    /// Area index ranges by priority: (priority, start_index, end_index)
    area_priority_ranges: Vec<(i32, usize, usize)>,
    /// Line index ranges by priority: (priority, start_index, end_index)
    line_priority_ranges: Vec<(i32, usize, usize)>,
    /// Symbol instance ranges by priority: (priority, start_index, end_index)
    symbol_priority_ranges: Vec<(i32, usize, usize)>,
}

impl WgpuRenderer {
    /// Create new renderer for window
    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let state = GpuState::new(window.clone()).await?;
        let pipelines = RenderPipelines::new(&state)?;

        let (width, height) = state.viewport_size();
        let uniforms = ViewUniforms::new(width, height, 1.0);
        let view_buffer = state.create_uniform_buffer(&uniforms, "view_uniforms");
        let view_bind_group = pipelines.create_view_bind_group(&state.device, &view_buffer);

        // Create egui integration (render without MSAA for crisp text)
        let egui = EguiIntegration::new(
            &state.device,
            state.format(),
            1, // No MSAA for egui
            window,
        );

        Ok(WgpuRenderer {
            state,
            pipelines,
            view_buffer,
            view_bind_group,
            // Pre-allocate with typical initial capacities to avoid reallocation
            area_vertices: Vec::with_capacity(10000),
            area_indices: Vec::with_capacity(30000),
            line_vertices: Vec::with_capacity(5000),
            line_indices: Vec::with_capacity(15000),
            symbol_textures: HashMap::with_capacity(100),
            symbol_instances: Vec::with_capacity(2000),
            background_color: Color::from_hex("#DEEBF7").unwrap_or(Color::WHITE),
            symbol_scale: 1.0, // S-100 standard: 1.0 = nominal symbol size at 0.3mm/pixel
            show_soundings: false, // Hide soundings by default (too dense when zoomed out)
            zoom_level: 1.0,
            compilation_scale: 22000, // Default compilation scale (1:22000)
            symbol_grid: std::collections::HashSet::with_capacity(1000),
            sounding_screen_grid: std::collections::HashMap::with_capacity(2000),
            sounding_exact_positions: std::collections::HashSet::with_capacity(5000),
            world_dedup: std::collections::HashSet::with_capacity(5000),
            danger_grid: std::collections::HashSet::with_capacity(500),
            grid_cell_size: 30.0,         // Default grid cell size in pixels
            sounding_cell_size_px: 150.0, // Fixed pixel spacing between soundings
            danger_cell_size_px: 80.0, // Danger symbols: smaller cell for higher density than soundings
            skip_screen_declutter: false,
            screen_pan_offset: (0.0, 0.0),
            egui,
            ui_state: AppUiState::default(),
            // Optimization fields
            triangulation_cache: HashMap::with_capacity(500),
            symbol_batches: HashMap::with_capacity(50),
            animation_mode: false,
            lod_level: 0,
            viewport_world_bounds: None,
            // S-101 priority group rendering
            area_priority_ranges: Vec::with_capacity(10),
            line_priority_ranges: Vec::with_capacity(10),
            symbol_priority_ranges: Vec::with_capacity(10),
        })
    }

    /// Set animation mode for fast-path rendering during drag/zoom
    #[inline]
    pub fn set_animation_mode(&mut self, animating: bool) {
        self.animation_mode = animating;
        // During animation, use lower LOD
        self.lod_level = if animating { 1 } else { 0 };
    }

    /// Update viewport world bounds for frustum culling
    pub fn update_viewport_bounds(&mut self, scaler: &ferrite_render::Scaler) {
        let (vw, vh) = (scaler.viewport.width, scaler.viewport.height);
        let top_left = scaler.screen_to_world(ScreenPoint { x: 0.0, y: 0.0 });
        let bottom_right = scaler.screen_to_world(ScreenPoint { x: vw, y: vh });
        self.viewport_world_bounds = Some((
            top_left.x.min(bottom_right.x),
            top_left.y.min(bottom_right.y),
            top_left.x.max(bottom_right.x),
            top_left.y.max(bottom_right.y),
        ));
    }

    /// Check if a world point is within the viewport (with margin)
    #[inline]
    fn is_point_visible(&self, x: f64, y: f64) -> bool {
        if let Some((min_x, min_y, max_x, max_y)) = self.viewport_world_bounds {
            // Add 10% margin for symbols that extend beyond their position
            let margin_x = (max_x - min_x) * 0.1;
            let margin_y = (max_y - min_y) * 0.1;
            x >= min_x - margin_x
                && x <= max_x + margin_x
                && y >= min_y - margin_y
                && y <= max_y + margin_y
        } else {
            true // No bounds set, assume visible
        }
    }

    /// Clear triangulation cache (call when chart data changes)
    pub fn clear_triangulation_cache(&mut self) {
        self.triangulation_cache.clear();
    }

    /// Clear symbol textures (call when color profile changes)
    pub fn clear_symbol_textures(&mut self) {
        self.symbol_textures.clear();
    }

    /// Build symbol batches for efficient rendering
    /// Groups symbol instances by texture and creates batched vertex/index arrays
    /// Build symbol batches for a specific range of symbol instances (S-101 priority rendering)
    /// Returns batches grouped by texture for efficient rendering
    /// Note: Currently unused - we render symbols individually to maintain Z-order
    #[allow(dead_code)]
    fn build_symbol_batches_for_range(
        &self,
        start: usize,
        end: usize,
    ) -> HashMap<SymbolId, (Vec<TextureVertex>, Vec<u32>)> {
        let mut batches: HashMap<SymbolId, (Vec<TextureVertex>, Vec<u32>)> = HashMap::new();

        for instance in self.symbol_instances[start..end].iter() {
            if let Some(tex) = self.symbol_textures.get(&instance.symbol_id) {
                // S-101 compliant symbol scaling:
                // - instance.scale: scale factor from portrayal rules (typically 1.0)
                // - SCREEN_PX_PER_MM: standard screen density (96 DPI = 3.78 px/mm)
                // - tex.render_scale: pixels per mm used during SVG rasterization
                // - S101_SYMBOL_SCALE: standard S-101 symbol sizing factor (0.35)
                // - self.symbol_scale: user-adjustable multiplier (default 1.0)
                let display_scale = instance.scale * SCREEN_PX_PER_MM / tex.render_scale
                    * S101_SYMBOL_SCALE
                    * self.symbol_scale;

                let half_w = (tex.width as f32 * display_scale) / 2.0;
                let half_h = (tex.height as f32 * display_scale) / 2.0;

                let pivot_x = tex.pivot_in_texture.0 * display_scale;
                let pivot_y = tex.pivot_in_texture.1 * display_scale;

                let rotation = instance.rotation.to_radians();
                let cos_r = rotation.cos();
                let sin_r = rotation.sin();

                // Transform helper
                let transform = |dx: f32, dy: f32| -> (f32, f32) {
                    let px = dx + half_w - pivot_x;
                    let py = dy + half_h - pivot_y;
                    let rx = px * cos_r - py * sin_r;
                    let ry = px * sin_r + py * cos_r;
                    (instance.screen_x + rx, instance.screen_y + ry)
                };

                let (x0, y0) = transform(-half_w, -half_h);
                let (x1, y1) = transform(half_w, -half_h);
                let (x2, y2) = transform(half_w, half_h);
                let (x3, y3) = transform(-half_w, half_h);

                // Get or create batch for this symbol (SymbolId is Copy, no clone needed)
                let batch = batches
                    .entry(instance.symbol_id)
                    .or_insert_with(|| (Vec::new(), Vec::new()));

                let base_idx = batch.0.len() as u32;

                // Add vertices
                batch.0.push(TextureVertex::new(x0, y0, 0.0, 0.0));
                batch.0.push(TextureVertex::new(x1, y1, 1.0, 0.0));
                batch.0.push(TextureVertex::new(x2, y2, 1.0, 1.0));
                batch.0.push(TextureVertex::new(x3, y3, 0.0, 1.0));

                // Add indices (two triangles per quad)
                batch.1.push(base_idx);
                batch.1.push(base_idx + 1);
                batch.1.push(base_idx + 2);
                batch.1.push(base_idx);
                batch.1.push(base_idx + 2);
                batch.1.push(base_idx + 3);
            }
        }

        batches
    }

    /// Handle window resize
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.state.resize(new_size);
        self.update_view_uniforms();
    }

    /// Handle winit window event for egui, returns true if egui consumed the event
    pub fn handle_egui_event(&mut self, event: &WindowEvent) -> bool {
        self.egui.handle_event(&self.state.window, event)
    }

    /// Check if egui wants pointer input (mouse is over UI element)
    /// Call this before handling clicks to avoid clicking through UI
    pub fn egui_wants_pointer(&self) -> bool {
        self.egui.wants_pointer_input()
    }

    /// Update cursor position in UI state (world coordinates)
    #[inline]
    pub fn set_cursor_world(&mut self, x: f64, y: f64) {
        self.ui_state.cursor_world = (x, y);
    }

    /// Update cursor position in UI state (screen coordinates)
    #[inline]
    pub fn set_cursor_screen(&mut self, x: f32, y: f32) {
        self.ui_state.cursor_screen = (x, y);
    }

    /// Check and clear UI action requests
    #[inline]
    pub fn take_open_file_request(&mut self) -> bool {
        let requested = self.ui_state.open_file_requested;
        self.ui_state.open_file_requested = false;
        requested
    }

    #[inline]
    pub fn take_screenshot_request(&mut self) -> bool {
        let requested = self.ui_state.screenshot_requested;
        self.ui_state.screenshot_requested = false;
        requested
    }

    #[inline]
    pub fn take_open_fc_request(&mut self) -> bool {
        let requested = self.ui_state.open_fc_requested;
        self.ui_state.open_fc_requested = false;
        requested
    }

    #[inline]
    pub fn take_open_pc_request(&mut self) -> bool {
        let requested = self.ui_state.open_pc_requested;
        self.ui_state.open_pc_requested = false;
        requested
    }

    #[inline]
    pub fn take_zoom_in_request(&mut self) -> bool {
        let requested = self.ui_state.zoom_in_requested;
        self.ui_state.zoom_in_requested = false;
        requested
    }

    #[inline]
    pub fn take_zoom_out_request(&mut self) -> bool {
        let requested = self.ui_state.zoom_out_requested;
        self.ui_state.zoom_out_requested = false;
        requested
    }

    #[inline]
    pub fn take_reset_view_request(&mut self) -> bool {
        let requested = self.ui_state.reset_view_requested;
        self.ui_state.reset_view_requested = false;
        requested
    }

    /// Take and reset clear charts request
    #[inline]
    pub fn take_clear_charts_request(&mut self) -> bool {
        let requested = self.ui_state.clear_charts_requested;
        self.ui_state.clear_charts_requested = false;
        requested
    }

    /// Take color profile change request, returns new profile name if changed
    #[inline]
    pub fn take_color_profile_change(&mut self) -> Option<String> {
        if self.ui_state.color_profile_changed {
            self.ui_state.color_profile_changed = false;
            Some(self.ui_state.color_profile.clone())
        } else {
            None
        }
    }

    /// Set the current color profile name in UI state
    #[inline]
    pub fn set_color_profile(&mut self, profile: &str) {
        self.ui_state.color_profile = profile.to_string();
    }

    /// Take settings change request, returns current settings if changed
    #[inline]
    pub fn take_settings_change(&mut self) -> Option<SettingsState> {
        if self.ui_state.settings_changed {
            self.ui_state.settings_changed = false;
            Some(self.ui_state.settings.clone())
        } else {
            None
        }
    }

    /// Take pan adjustment (in pixels) when panel state changes
    #[inline]
    pub fn take_pan_adjust_pixels(&mut self) -> Option<f32> {
        self.ui_state.pan_adjust_pixels.take()
    }

    /// Get current settings state (read-only)
    #[inline]
    pub fn settings(&self) -> &SettingsState {
        &self.ui_state.settings
    }

    /// Update settings state
    #[inline]
    pub fn set_settings(&mut self, settings: SettingsState) {
        self.ui_state.settings = settings;
    }

    /// Take plugin toggle request, returns plugin_id if a toggle was requested
    #[inline]
    pub fn take_plugin_toggle_request(&mut self) -> Option<String> {
        self.ui_state.plugin_toggle_requested.take()
    }

    /// Update plugin toolbar buttons
    #[inline]
    pub fn set_plugin_buttons(&mut self, buttons: Vec<egui_integration::PluginButton>) {
        self.ui_state.plugin_buttons = buttons;
    }

    /// Update plugin UI data
    #[inline]
    pub fn set_plugin_ui_data(&mut self, data: Vec<(String, String)>) {
        self.ui_state.plugin_ui_data = data;
    }

    /// Take pending plugin UI events
    #[inline]
    pub fn take_plugin_ui_events(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.ui_state.plugin_ui_events)
    }

    /// Update view uniforms after resize or zoom
    fn update_view_uniforms(&self) {
        let (width, height) = self.state.viewport_size();
        let uniforms = ViewUniforms::with_pan(
            width,
            height,
            1.0,
            self.screen_pan_offset.0,
            self.screen_pan_offset.1,
        );
        self.state
            .update_view_uniforms(&self.view_buffer, &uniforms);
    }

    /// Set screen-space pan offset for fast panning during drag.
    /// This only updates the GPU uniform, no vertex rebuild needed.
    #[inline]
    pub fn set_pan_offset(&mut self, dx: f32, dy: f32) {
        self.screen_pan_offset = (dx, dy);
        self.update_view_uniforms();
    }

    /// Add to the current screen-space pan offset.
    /// Returns the new total offset.
    pub fn add_pan_offset(&mut self, dx: f32, dy: f32) -> (f32, f32) {
        self.screen_pan_offset.0 += dx;
        self.screen_pan_offset.1 += dy;
        self.update_view_uniforms();
        self.screen_pan_offset
    }

    /// Get the current screen-space pan offset
    #[inline]
    pub fn get_pan_offset(&self) -> (f32, f32) {
        self.screen_pan_offset
    }

    /// Reset pan offset to zero (call before full rebuild)
    #[inline]
    pub fn reset_pan_offset(&mut self) {
        self.screen_pan_offset = (0.0, 0.0);
        self.update_view_uniforms();
    }

    /// Begin a new frame - clears buffers
    pub fn begin_frame(&mut self) {
        self.begin_frame_ex(false);
    }

    /// Begin a new frame with optional preservation of declutter state
    /// If `preserve_declutter` is true, skip screen-space decluttering during animation
    pub fn begin_frame_ex(&mut self, preserve_declutter: bool) {
        self.area_vertices.clear();
        self.area_indices.clear();
        self.line_vertices.clear();
        self.line_indices.clear();
        self.symbol_instances.clear();
        // Clear symbol batches for new frame
        self.symbol_batches.clear();
        // Clear priority ranges for S-101 compliant rendering
        self.area_priority_ranges.clear();
        self.line_priority_ranges.clear();
        self.symbol_priority_ranges.clear();

        // During animation (preserve_declutter=true), skip screen-space declutter
        // to prevent symbols from disappearing due to changed screen coordinates
        self.skip_screen_declutter = preserve_declutter;

        // world_dedup must ALWAYS be cleared - it's rebuilt each frame from scratch
        // Only screen-space grids (symbol_grid, sounding_screen_grid) should be preserved
        // during animation to prevent flickering
        self.world_dedup.clear();

        if !preserve_declutter {
            self.symbol_grid.clear();
            self.sounding_screen_grid.clear();
            self.sounding_exact_positions.clear();
            self.danger_grid.clear();

            // Adjust grid cell size based on zoom level
            // At low zoom (zoomed out), use larger cells to declutter more aggressively
            // At high zoom (zoomed in), use smaller cells to show more detail
            self.grid_cell_size = (40.0 / self.zoom_level.sqrt() as f32).clamp(20.0, 120.0);

            // Sounding cell size is fixed in screen pixels for consistent density
            // At maximum zoom (>= 45x), show ALL soundings (no filtering)
            if self.zoom_level >= 45.0 {
                self.sounding_cell_size_px = 0.0; // No filtering
            } else {
                // Fixed screen-space cell size for consistent visual density
                // 150px provides good spacing between soundings at most zoom levels
                self.sounding_cell_size_px = 150.0;
            }

            // Danger symbol cell size: smaller than soundings for higher density
            // At high zoom (>= 20x), show all danger symbols
            if self.zoom_level >= 20.0 {
                self.danger_cell_size_px = 0.0; // No filtering
            } else {
                // 80px spacing provides reasonable density for danger markers
                self.danger_cell_size_px = 80.0;
            }
        }
    }

    /// Set current zoom level for symbol filtering
    #[inline]
    pub fn set_zoom_level(&mut self, zoom: f64) {
        self.zoom_level = zoom;
    }

    /// Set chart compilation scale (e.g., 22000 for 1:22000)
    #[inline]
    pub fn set_compilation_scale(&mut self, scale: u32) {
        self.compilation_scale = scale;
    }

    /// Calculate the current viewing scale based on zoom level
    /// viewing_scale = compilation_scale / zoom_level
    /// Example: At zoom 2.0 with 1:22000 chart -> viewing scale is 1:11000
    #[inline]
    pub fn viewing_scale(&self) -> u32 {
        ((self.compilation_scale as f64) / self.zoom_level.max(0.01)) as u32
    }

    /// Check if a feature should be visible at the current viewing scale
    /// Based on S-101 scale_minimum rule
    /// scale_minimum: the smallest scale (largest denominator) at which the feature is visible
    #[inline]
    pub fn is_visible_at_scale(&self, scale_minimum: Option<u32>) -> bool {
        match scale_minimum {
            Some(min_scale) => self.viewing_scale() <= min_scale,
            None => true, // No scale restriction
        }
    }

    /// Add drawing instructions from render context
    pub fn add_instructions(&mut self, context: &mut RenderContext) {
        self.add_instructions_with_symbols(context, None, None, None);
    }

    /// Add drawing instructions with symbol rendering support
    /// Uses S-101 compliant priority grouping for correct render order
    ///
    /// # Arguments
    /// * `context` - The render context containing drawing instructions
    /// * `symbol_cache` - Optional symbol cache for SVG rendering
    /// * `color_profile` - Optional color profile for symbol coloring
    /// * `visible_viewing_groups` - Optional set of viewing group IDs that should be visible.
    ///   If None, all viewing groups are visible. Used for Display Mode filtering.
    pub fn add_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) {
        // Update viewport bounds for frustum culling
        self.update_viewport_bounds(&context.scaler);

        // Set animation mode on context
        context.set_animation_mode(self.animation_mode);

        // Clone the instructions to avoid borrow issues
        let instructions: Vec<_> = context.get_sorted_instructions().to_vec();

        // Track skipped counts for debugging
        let mut _culled_count = 0usize;

        // S-101 Priority tracking: track index ranges per priority
        let mut current_priority: Option<i32> = None;
        let mut area_start_idx = 0usize;
        let mut line_start_idx = 0usize;
        let mut symbol_start_idx = 0usize;

        for instruction in &instructions {
            // Display Mode filtering: skip instructions not in visible viewing groups
            if let Some(visible) = visible_viewing_groups {
                let vg = instruction.viewing_group().0;
                if !visible.contains(&vg) {
                    continue;
                }
            }

            let inst_priority = instruction.priority().0;

            // Check if priority changed - record ranges for previous priority
            if let Some(prev_priority) = current_priority {
                if prev_priority != inst_priority {
                    // Record area range if any areas were added for previous priority
                    if self.area_indices.len() > area_start_idx {
                        self.area_priority_ranges.push((
                            prev_priority,
                            area_start_idx,
                            self.area_indices.len(),
                        ));
                    }
                    area_start_idx = self.area_indices.len();

                    // Record line range if any lines were added for previous priority
                    if self.line_indices.len() > line_start_idx {
                        self.line_priority_ranges.push((
                            prev_priority,
                            line_start_idx,
                            self.line_indices.len(),
                        ));
                    }
                    line_start_idx = self.line_indices.len();

                    // Record symbol range if any symbols were added for previous priority
                    if self.symbol_instances.len() > symbol_start_idx {
                        self.symbol_priority_ranges.push((
                            prev_priority,
                            symbol_start_idx,
                            self.symbol_instances.len(),
                        ));
                    }
                    symbol_start_idx = self.symbol_instances.len();
                }
            }
            current_priority = Some(inst_priority);

            match instruction {
                DrawingInstruction::Area(area) => {
                    // LOD: Skip small areas when zoomed out (animation mode)
                    if self.animation_mode && self.lod_level > 0 {
                        // Skip areas with few points during animation
                        if area.exterior.len() < 10 {
                            _culled_count += 1;
                            continue;
                        }
                    }
                    self.add_area_cached(area, &context.scaler);
                }
                DrawingInstruction::Line(line) => {
                    // LOD: Skip short lines when zoomed out
                    if self.animation_mode && self.lod_level > 0 && line.points.len() < 5 {
                        _culled_count += 1;
                        continue;
                    }
                    self.add_line(line, &context.scaler);
                }
                DrawingInstruction::Point(point) => {
                    // Frustum culling: skip points outside viewport
                    if !self.is_point_visible(point.position.x, point.position.y) {
                        _culled_count += 1;
                        continue;
                    }

                    // S-101 Scale-based feature filtering
                    // Calculate current viewing scale
                    let viewing_scale = self.viewing_scale();

                    // Soundings: visible at scales <= 1:45000 (larger scales = more detail)
                    // At 1:22000 chart, this means zoom >= ~0.5x
                    let is_sounding = point.symbol_ref.starts_with("SOUNDG")
                        || point.symbol_ref.starts_with("SOUNDS");
                    let sounding_scale_min = 45000; // S-101 typical scale_minimum for soundings
                    if is_sounding && !self.show_soundings && viewing_scale > sounding_scale_min {
                        continue;
                    }

                    // ISODGR01 (Isolated Danger) and DANGER02: visible at scales <= 1:90000
                    // These are important safety features, shown at medium-large scales
                    let danger_scale_min = 90000;
                    if (point.symbol_ref == "ISODGR01" || point.symbol_ref == "DANGER02")
                        && viewing_scale > danger_scale_min
                    {
                        continue;
                    }

                    // LOD: Skip non-essential symbols during animation
                    if self.animation_mode && self.lod_level > 0 {
                        // Keep only important symbols during animation
                        if !is_sounding
                            && !point.symbol_ref.starts_with("LIGHTS")
                            && !point.symbol_ref.starts_with("BUOY")
                            && !point.symbol_ref.starts_with("BCNLAT")
                        {
                            _culled_count += 1;
                            continue;
                        }
                    }

                    // Try to render as symbol if cache is available
                    let rendered = if let (Some(cache), Some(profile)) =
                        (symbol_cache.as_mut(), color_profile)
                    {
                        self.try_add_symbol(point, &context.scaler, cache, profile)
                    } else {
                        false
                    };

                    // Fallback to placeholder if symbol not found
                    if !rendered {
                        self.add_point_fallback(point, &context.scaler);
                    }
                }
                DrawingInstruction::Text(text) => {
                    // LOD: Skip text during animation for performance
                    if self.animation_mode {
                        continue;
                    }
                    // Text rendering requires separate handling (glyph atlas)
                    // For now, skip
                    tracing::trace!("Skipping text: {}", text.text);
                }
            }
        }

        // Record final priority ranges
        if let Some(final_priority) = current_priority {
            if self.area_indices.len() > area_start_idx {
                self.area_priority_ranges.push((
                    final_priority,
                    area_start_idx,
                    self.area_indices.len(),
                ));
            }
            if self.line_indices.len() > line_start_idx {
                self.line_priority_ranges.push((
                    final_priority,
                    line_start_idx,
                    self.line_indices.len(),
                ));
            }
            if self.symbol_instances.len() > symbol_start_idx {
                self.symbol_priority_ranges.push((
                    final_priority,
                    symbol_start_idx,
                    self.symbol_instances.len(),
                ));
            }
        }
    }

    /// Add area with triangulation caching (optimization)
    fn add_area_cached(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        scaler: &ferrite_render::Scaler,
    ) {
        // Check if we have cached triangulation for this feature
        if let Some(feature_id) = area.feature_id {
            let geom_hash = hash_geometry(&area.exterior);

            // Check cache
            if let Some(cached) = self.triangulation_cache.get(&feature_id) {
                if cached.geometry_hash == geom_hash {
                    // Use cached triangulation - just transform to current screen coords
                    let base_idx = self.area_vertices.len() as u32;

                    // Get fill color
                    let color = match &area.fill {
                        ferrite_render::AreaFillType::Solid(c) => c.to_array(),
                        _ => [0.5, 0.5, 0.5, 0.5],
                    };

                    // Transform cached vertices to screen coordinates
                    for v in &cached.vertices {
                        // Cached vertices store world coordinates in x,y
                        let world_pt = WorldPoint {
                            x: v.position[0] as f64,
                            y: v.position[1] as f64,
                        };
                        let screen_pt = scaler.world_to_screen(world_pt);
                        if screen_pt.x.is_finite() && screen_pt.y.is_finite() {
                            self.area_vertices.push(Vertex2D {
                                position: [screen_pt.x, screen_pt.y],
                                color,
                            });
                        }
                    }

                    // Add indices with offset
                    for idx in &cached.indices {
                        self.area_indices.push(base_idx + idx);
                    }
                    return;
                }
            }

            // Cache miss or invalidated - compute and cache
            let (vertices, indices) = self.triangulate_area(area, scaler);
            if !vertices.is_empty() {
                // Store in cache (using world coordinates for reuse across zoom levels)
                let world_vertices: Vec<Vertex2D> = area
                    .exterior
                    .iter()
                    .map(|p| Vertex2D {
                        position: [p.x as f32, p.y as f32],
                        color: [0.0; 4], // Color not cached
                    })
                    .collect();

                self.triangulation_cache.insert(
                    feature_id,
                    CachedTriangulation {
                        vertices: world_vertices,
                        indices: indices.clone(),
                        geometry_hash: geom_hash,
                    },
                );

                // Add to current frame
                let base_idx = self.area_vertices.len() as u32;
                self.area_vertices.extend(vertices);
                for idx in indices {
                    self.area_indices.push(base_idx + idx);
                }
            }
        } else {
            // No feature ID, fall back to non-cached version
            self.add_area(area, scaler);
        }
    }

    /// Triangulate area and return vertices/indices
    fn triangulate_area(
        &self,
        area: &ferrite_render::AreaInstruction,
        scaler: &ferrite_render::Scaler,
    ) -> (Vec<Vertex2D>, Vec<u32>) {
        let color = match &area.fill {
            ferrite_render::AreaFillType::Solid(c) => c.to_array(),
            _ => [0.5, 0.5, 0.5, 0.5],
        };

        let screen_points: Vec<ScreenPoint> = area
            .exterior
            .iter()
            .map(|p| scaler.world_to_screen(*p))
            .filter(|p| p.x.is_finite() && p.y.is_finite())
            .collect();

        if screen_points.len() < 3 {
            return (Vec::new(), Vec::new());
        }

        // Clean duplicate points
        let mut cleaned_points: Vec<ScreenPoint> = Vec::with_capacity(screen_points.len());
        for point in &screen_points {
            if cleaned_points.is_empty() {
                cleaned_points.push(*point);
            } else {
                let last = cleaned_points.last().unwrap();
                let dx = (point.x - last.x).abs();
                let dy = (point.y - last.y).abs();
                if dx > 0.01 || dy > 0.01 {
                    cleaned_points.push(*point);
                }
            }
        }

        if cleaned_points.len() < 3 {
            return (Vec::new(), Vec::new());
        }

        // Flatten for earcut
        let mut flat_coords: Vec<f64> = Vec::with_capacity(cleaned_points.len() * 2);
        for p in &cleaned_points {
            flat_coords.push(p.x as f64);
            flat_coords.push(p.y as f64);
        }

        // Handle holes
        let mut hole_indices: Vec<usize> = Vec::new();
        for hole in &area.interiors {
            let hole_start = flat_coords.len() / 2;
            hole_indices.push(hole_start);

            let hole_screen: Vec<ScreenPoint> = hole
                .iter()
                .map(|p| scaler.world_to_screen(*p))
                .filter(|p| p.x.is_finite() && p.y.is_finite())
                .collect();

            for p in &hole_screen {
                flat_coords.push(p.x as f64);
                flat_coords.push(p.y as f64);
            }
        }

        // Triangulate
        let indices = earcutr::earcut(&flat_coords, &hole_indices, 2).unwrap_or_default();

        if indices.is_empty() {
            return (Vec::new(), Vec::new());
        }

        // Create vertices
        let mut vertices = Vec::with_capacity(flat_coords.len() / 2);
        for i in 0..(flat_coords.len() / 2) {
            vertices.push(Vertex2D {
                position: [flat_coords[i * 2] as f32, flat_coords[i * 2 + 1] as f32],
                color,
            });
        }

        let indices: Vec<u32> = indices.into_iter().map(|i| i as u32).collect();
        (vertices, indices)
    }

    /// Add area instruction - uses earcut for proper concave polygon triangulation
    fn add_area(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        scaler: &ferrite_render::Scaler,
    ) {
        // Get fill color
        let color = match &area.fill {
            ferrite_render::AreaFillType::Solid(c) => c.to_array(),
            _ => [0.5, 0.5, 0.5, 0.5], // Default gray for patterns
        };

        // Convert exterior ring to screen coordinates, filtering out invalid points
        let screen_points: Vec<ScreenPoint> = area
            .exterior
            .iter()
            .map(|p| scaler.world_to_screen(*p))
            .filter(|p| p.x.is_finite() && p.y.is_finite())
            .collect();

        // Need at least 3 points for a polygon
        if screen_points.len() < 3 {
            return;
        }

        // Remove consecutive duplicate points (within small tolerance)
        // Use very small tolerance - only remove truly duplicate points
        let mut cleaned_points: Vec<ScreenPoint> = Vec::with_capacity(screen_points.len());
        for point in &screen_points {
            if cleaned_points.is_empty() {
                cleaned_points.push(*point);
            } else {
                let last = cleaned_points.last().unwrap();
                let dx = (point.x - last.x).abs();
                let dy = (point.y - last.y).abs();
                // Only remove truly identical points (< 0.01 pixel)
                if dx > 0.01 || dy > 0.01 {
                    cleaned_points.push(*point);
                }
            }
        }

        // Remove closing point if it duplicates the first point
        if cleaned_points.len() > 3 {
            let first = cleaned_points.first().unwrap();
            let last = cleaned_points.last().unwrap();
            let dx = (first.x - last.x).abs();
            let dy = (first.y - last.y).abs();
            if dx < 0.1 && dy < 0.1 {
                cleaned_points.pop();
            }
        }

        // Need at least 3 points after cleaning
        if cleaned_points.len() < 3 {
            return;
        }

        // Calculate polygon area for logging (not filtering)
        let mut signed_area: f64 = 0.0;
        for i in 0..cleaned_points.len() {
            let j = (i + 1) % cleaned_points.len();
            signed_area += (cleaned_points[j].x - cleaned_points[i].x) as f64
                * (cleaned_points[j].y + cleaned_points[i].y) as f64;
        }
        let abs_area = signed_area.abs() / 2.0;

        // Prepare data for earcutr triangulation
        // Flatten coordinates to [x0, y0, x1, y1, ...] format
        let mut vertices: Vec<f64> = Vec::with_capacity(cleaned_points.len() * 2);
        for point in &cleaned_points {
            vertices.push(point.x as f64);
            vertices.push(point.y as f64);
        }

        // No holes for now
        let hole_indices: Vec<usize> = vec![];

        // Triangulate using ear clipping (works for concave polygons)
        let indices = earcutr::earcut(&vertices, &hole_indices, 2);

        let use_fallback = match &indices {
            Ok(idx) => idx.is_empty() || idx.len() < 3,
            Err(_) => true,
        };

        if use_fallback {
            // Fallback to fan triangulation for degenerate cases
            // This works for convex polygons and simple concave ones
            tracing::trace!(
                "Earcut failed for {} points (area={:.1}), using fan triangulation",
                cleaned_points.len(),
                abs_area
            );
            let base_index = self.area_vertices.len() as u32;
            for point in &cleaned_points {
                self.area_vertices
                    .push(Vertex2D::new(point.x, point.y, color));
            }
            // Fan triangulation from first vertex
            for i in 1..(cleaned_points.len() - 1) {
                self.area_indices.push(base_index);
                self.area_indices.push(base_index + i as u32);
                self.area_indices.push(base_index + i as u32 + 1);
            }
            return;
        }

        let indices = indices.unwrap();

        // Validate indices
        let max_idx = cleaned_points.len();
        let valid_indices: Vec<usize> = indices.into_iter().filter(|&idx| idx < max_idx).collect();

        if valid_indices.len() < 3 || !valid_indices.len().is_multiple_of(3) {
            // Fallback if indices are invalid
            let base_index = self.area_vertices.len() as u32;
            for point in &cleaned_points {
                self.area_vertices
                    .push(Vertex2D::new(point.x, point.y, color));
            }
            for i in 1..(cleaned_points.len() - 1) {
                self.area_indices.push(base_index);
                self.area_indices.push(base_index + i as u32);
                self.area_indices.push(base_index + i as u32 + 1);
            }
            return;
        }

        // Add vertices first
        let base_index = self.area_vertices.len() as u32;
        for point in &cleaned_points {
            self.area_vertices
                .push(Vertex2D::new(point.x, point.y, color));
        }

        // Add triangle indices from earcut result
        for idx in valid_indices {
            self.area_indices.push(base_index + idx as u32);
        }
    }

    /// Add line instruction
    fn add_line(
        &mut self,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
    ) {
        let color = line.style.color.to_array();
        let width = line.style.width;

        // Convert points to screen coordinates
        let screen_points: Vec<ScreenPoint> = line
            .points
            .iter()
            .map(|p| scaler.world_to_screen(*p))
            .collect();

        // Generate line geometry (quads for each segment)
        // Use windows iterator to avoid bounds checking overhead
        for window in screen_points.windows(2) {
            let (p0, p1) = (window[0], window[1]);

            // Calculate perpendicular direction
            let dx = p1.x - p0.x;
            let dy = p1.y - p0.y;
            let len = (dx * dx + dy * dy).sqrt();

            if len < 0.001 {
                continue;
            }

            let nx = -dy / len * width * 0.5;
            let ny = dx / len * width * 0.5;

            let base_index = self.line_vertices.len() as u32;

            // Four corners of the line segment quad
            self.line_vertices
                .push(Vertex2D::new(p0.x - nx, p0.y - ny, color));
            self.line_vertices
                .push(Vertex2D::new(p0.x + nx, p0.y + ny, color));
            self.line_vertices
                .push(Vertex2D::new(p1.x + nx, p1.y + ny, color));
            self.line_vertices
                .push(Vertex2D::new(p1.x - nx, p1.y - ny, color));

            // Two triangles
            self.line_indices.push(base_index);
            self.line_indices.push(base_index + 1);
            self.line_indices.push(base_index + 2);
            self.line_indices.push(base_index);
            self.line_indices.push(base_index + 2);
            self.line_indices.push(base_index + 3);
        }
    }

    /// Try to render point as SVG symbol, returns true if successful
    fn try_add_symbol(
        &mut self,
        point: &ferrite_render::PointInstruction,
        scaler: &ferrite_render::Scaler,
        symbol_cache: &mut SymbolCache,
        color_profile: &ColorProfile,
    ) -> bool {
        let symbol_str = &point.symbol_ref;
        if symbol_str.is_empty() {
            return false;
        }

        // Intern the symbol ID once for cache-efficient lookups (u32 instead of String)
        let symbol_id = intern_symbol(symbol_str);

        // Log first few symbol requests for debugging
        static SYMBOL_DEBUG_COUNT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let debug_idx = SYMBOL_DEBUG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if debug_idx < 20 {
            tracing::debug!("Symbol request [{}]: '{}'", debug_idx, symbol_str);
        }

        // Get symbol geometry from cache (this will render via resvg if not cached)
        // Use reference to avoid cloning the pixel buffer
        let geom = match symbol_cache.get_symbol(symbol_str, color_profile) {
            Some(g) => g,
            None => return false,
        };

        // Create GPU texture if not already cached (using interned SymbolId for O(1) lookup)
        if !self.symbol_textures.contains_key(&symbol_id) {
            let (_texture, view) = self.state.create_texture_from_rgba(
                &geom.pixels,
                geom.width,
                geom.height,
                &format!("symbol_{}", symbol_str),
            );

            let bind_group = self
                .pipelines
                .create_texture_bind_group(&self.state.device, &view);

            let pivot_in_tex = geom.pivot_in_texture();
            self.symbol_textures.insert(
                symbol_id,
                SymbolTexture {
                    texture: _texture,
                    bind_group,
                    width: geom.width,
                    height: geom.height,
                    pivot_in_texture: pivot_in_tex,
                    render_scale: geom.render_scale,
                },
            );

            tracing::debug!(
                "Created GPU texture for symbol '{}': {}x{}, pivot_in_tex: ({:.2}, {:.2})",
                symbol_str,
                geom.width,
                geom.height,
                pivot_in_tex.0,
                pivot_in_tex.1
            );
        }

        // Convert screen position
        let screen = scaler.world_to_screen(point.position);

        // === STAGE 1: World-coordinate deduplication ===
        // Remove exact duplicates from multiple charts at the same geographic position
        // Use high precision (6 decimal places ≈ 0.1 meter) for deduplication
        let world_x_key = (point.position.x * 1_000_000.0) as i64;
        let world_y_key = (point.position.y * 1_000_000.0) as i64;
        // Use interned symbol ID as hash (already unique per symbol type)
        let symbol_hash = symbol_id.0 as u64;
        let world_key = (world_x_key, world_y_key, symbol_hash);

        if self.world_dedup.contains(&world_key) {
            // Exact duplicate from another chart - skip
            return true;
        }
        self.world_dedup.insert(world_key);

        // === STAGE 2: Screen-space decluttering ===
        // Classify symbol types (using original string for pattern matching)
        let is_nav_aid = symbol_str.starts_with("LIGHTS")
            || symbol_str.starts_with("BUOY")
            || symbol_str.starts_with("BCN")
            || symbol_str.starts_with("TOPMAR");

        let is_low_priority = symbol_str == "ISODGR01" || symbol_str == "DANGER02";
        let is_sounding = symbol_str.starts_with("SOUND");

        // Skip screen-space decluttering during animation to prevent symbols from disappearing
        // Only world-coordinate deduplication (Stage 1) applies during drag/inertia
        if !self.skip_screen_declutter {
            // Soundings: Use SCREEN-SPACE grid for decluttering
            // This ensures consistent visual density regardless of zoom level
            // Two-stage approach:
            // 1. sounding_exact_positions: tracks exact world positions to allow all digits of same sounding
            // 2. sounding_screen_grid: screen-space grid to filter out visually nearby soundings
            // At high zoom (cell_size == 0), skip grid filtering and show all soundings
            if is_sounding && self.sounding_cell_size_px > 0.1 {
                // World-space key for exact position (all digits of one sounding share this)
                let exact_key = (world_x_key, world_y_key);

                // Check if we've already allowed a sounding at this exact world position
                if self.sounding_exact_positions.contains(&exact_key) {
                    // This is another digit of an already-allowed sounding - let it through
                    // (skip screen grid check)
                } else {
                    // First time seeing this exact position - check screen-space grid
                    let sounding_grid_x = (screen.x / self.sounding_cell_size_px) as i32;
                    let sounding_grid_y = (screen.y / self.sounding_cell_size_px) as i32;
                    let sounding_grid_key = (sounding_grid_x, sounding_grid_y);

                    // Get current sounding's depth (default to MAX if not set)
                    let current_depth = point.depth().unwrap_or(f64::MAX);

                    if let Some(&(old_exact_key, old_depth)) =
                        self.sounding_screen_grid.get(&sounding_grid_key)
                    {
                        // Another sounding already claimed this screen cell
                        // For SAFETY: keep the SHALLOWEST (lowest numerical depth) sounding
                        if current_depth < old_depth {
                            // This sounding is shallower - replace the old one
                            self.sounding_exact_positions.remove(&old_exact_key);
                            self.sounding_screen_grid
                                .insert(sounding_grid_key, (exact_key, current_depth));
                            self.sounding_exact_positions.insert(exact_key);
                        } else {
                            // Existing sounding is shallower or equal - skip this one
                            return true;
                        }
                    } else {
                        // Cell is empty - this sounding claims it
                        self.sounding_screen_grid
                            .insert(sounding_grid_key, (exact_key, current_depth));
                        self.sounding_exact_positions.insert(exact_key);
                    }
                }
            }
            // When sounding_cell_size_px <= 0.1 (max zoom), all soundings pass through
            else if is_low_priority && self.danger_cell_size_px > 0.1 {
                // Danger symbols (ISODGR, DANGER02) use their own grid for decluttering
                // This ensures they declutter among themselves without affecting other symbols
                let grid_x = (screen.x / self.danger_cell_size_px) as i32;
                let grid_y = (screen.y / self.danger_cell_size_px) as i32;
                let grid_key = (grid_x, grid_y);

                if self.danger_grid.contains(&grid_key) {
                    return true; // Cell already has a danger symbol
                }

                // Danger symbols claim cells in their own grid
                self.danger_grid.insert(grid_key);
            }
            // When danger_cell_size_px <= 0.1 (high zoom), all danger symbols pass through
            else if !is_sounding && !is_low_priority {
                // Regular symbols use the standard grid
                // Nav aids get smaller cells (show more) at higher zoom
                let effective_cell_size = if is_nav_aid {
                    // Nav aids: smaller cells at high zoom to show more detail
                    if self.zoom_level >= 5.0 {
                        15.0 // Show most nav aids when zoomed in
                    } else {
                        self.grid_cell_size
                    }
                } else {
                    // Other symbols: standard grid
                    self.grid_cell_size
                };

                let grid_x = (screen.x / effective_cell_size) as i32;
                let grid_y = (screen.y / effective_cell_size) as i32;
                let grid_key = (grid_x, grid_y);

                if self.symbol_grid.contains(&grid_key) {
                    return true; // Cell occupied
                }

                // Regular symbols claim cells
                self.symbol_grid.insert(grid_key);
            }
        }

        // Add symbol instance for rendering (uses interned SymbolId - 4 bytes vs 24+ for String)
        self.symbol_instances.push(SymbolInstance {
            symbol_id,
            screen_x: screen.x,
            screen_y: screen.y,
            scale: point.scale,
            rotation: point.rotation,
        });

        true
    }

    /// Fallback point rendering (small square) when symbol not available
    fn add_point_fallback(
        &mut self,
        point: &ferrite_render::PointInstruction,
        scaler: &ferrite_render::Scaler,
    ) {
        let screen = scaler.world_to_screen(point.position);
        let size = 4.0 * point.scale;
        let color = [1.0, 0.0, 0.0, 1.0]; // Red for missing symbols

        let base_index = self.area_vertices.len() as u32;

        // Small square
        self.area_vertices
            .push(Vertex2D::new(screen.x - size, screen.y - size, color));
        self.area_vertices
            .push(Vertex2D::new(screen.x + size, screen.y - size, color));
        self.area_vertices
            .push(Vertex2D::new(screen.x + size, screen.y + size, color));
        self.area_vertices
            .push(Vertex2D::new(screen.x - size, screen.y + size, color));

        self.area_indices.push(base_index);
        self.area_indices.push(base_index + 1);
        self.area_indices.push(base_index + 2);
        self.area_indices.push(base_index);
        self.area_indices.push(base_index + 2);
        self.area_indices.push(base_index + 3);
    }

    /// Render the frame
    pub fn render(&mut self) -> Result<()> {
        let output = self.state.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Begin egui frame
        self.egui.begin_frame(&self.state.window);

        // Draw egui UI
        self.egui.draw_ui(&mut self.ui_state);

        // End egui frame and get output
        let egui_output = self.egui.end_frame(&self.state.window);

        let mut encoder =
            self.state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("render_encoder"),
                });

        // Create buffers from collected vertices
        let area_vertex_buffer = if !self.area_vertices.is_empty() {
            Some(
                self.state
                    .create_vertex_buffer(&self.area_vertices, "area_vertices"),
            )
        } else {
            None
        };

        let area_index_buffer = if !self.area_indices.is_empty() {
            Some(
                self.state
                    .create_index_buffer(&self.area_indices, "area_indices"),
            )
        } else {
            None
        };

        let line_vertex_buffer = if !self.line_vertices.is_empty() {
            Some(
                self.state
                    .create_vertex_buffer(&self.line_vertices, "line_vertices"),
            )
        } else {
            None
        };

        let line_index_buffer = if !self.line_indices.is_empty() {
            Some(
                self.state
                    .create_index_buffer(&self.line_indices, "line_indices"),
            )
        } else {
            None
        };

        // NOTE: Symbol batches are now built per-priority in the render loop below
        // for S-101 compliant priority-based rendering

        let bg = self.background_color.to_array();

        // Use MSAA texture as render target if available, resolve to surface
        let (target_view, resolve_target) = if let Some(ref msaa_view) = self.state.msaa_view {
            (msaa_view, Some(&view))
        } else {
            (&view, None)
        };

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // S-101 Priority-based rendering:
            // Collect all unique priorities and render in order
            // For each priority: Areas -> Lines -> Symbols
            let mut all_priorities: Vec<i32> = Vec::new();
            for (p, _, _) in &self.area_priority_ranges {
                if !all_priorities.contains(p) {
                    all_priorities.push(*p);
                }
            }
            for (p, _, _) in &self.line_priority_ranges {
                if !all_priorities.contains(p) {
                    all_priorities.push(*p);
                }
            }
            for (p, _, _) in &self.symbol_priority_ranges {
                if !all_priorities.contains(p) {
                    all_priorities.push(*p);
                }
            }
            all_priorities.sort();

            // Render by priority groups
            for priority in &all_priorities {
                // Render areas for this priority
                if let (Some(vb), Some(ib)) = (&area_vertex_buffer, &area_index_buffer) {
                    for (p, start, end) in &self.area_priority_ranges {
                        if p == priority && end > start {
                            render_pass.set_pipeline(&self.pipelines.area_pipeline);
                            render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                            render_pass.set_vertex_buffer(0, vb.slice(..));
                            render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                            render_pass.draw_indexed(*start as u32..*end as u32, 0, 0..1);
                        }
                    }
                }

                // Render lines for this priority
                if let (Some(vb), Some(ib)) = (&line_vertex_buffer, &line_index_buffer) {
                    for (p, start, end) in &self.line_priority_ranges {
                        if p == priority && end > start {
                            render_pass.set_pipeline(&self.pipelines.line_pipeline);
                            render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                            render_pass.set_vertex_buffer(0, vb.slice(..));
                            render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                            render_pass.draw_indexed(*start as u32..*end as u32, 0, 0..1);
                        }
                    }
                }

                // Render symbols for this priority (in sorted order to prevent Z-fighting)
                for (p, start, end) in &self.symbol_priority_ranges {
                    if p == priority && end > start {
                        render_pass.set_pipeline(&self.pipelines.texture_pipeline);
                        render_pass.set_bind_group(0, &self.view_bind_group, &[]);

                        // Render symbols one by one in their sorted order to maintain Z-order
                        for i in *start..*end {
                            let instance = &self.symbol_instances[i];
                            if let Some(tex) = self.symbol_textures.get(&instance.symbol_id) {
                                // S-101 compliant symbol scaling
                                let display_scale = instance.scale * SCREEN_PX_PER_MM
                                    / tex.render_scale
                                    * S101_SYMBOL_SCALE
                                    * self.symbol_scale;

                                let half_w = (tex.width as f32 * display_scale) / 2.0;
                                let half_h = (tex.height as f32 * display_scale) / 2.0;

                                let pivot_x = tex.pivot_in_texture.0 * display_scale;
                                let pivot_y = tex.pivot_in_texture.1 * display_scale;

                                let rotation = instance.rotation.to_radians();
                                let cos_r = rotation.cos();
                                let sin_r = rotation.sin();

                                let transform = |dx: f32, dy: f32| -> (f32, f32) {
                                    let px = dx + half_w - pivot_x;
                                    let py = dy + half_h - pivot_y;
                                    let rx = px * cos_r - py * sin_r;
                                    let ry = px * sin_r + py * cos_r;
                                    (instance.screen_x + rx, instance.screen_y + ry)
                                };

                                let (x0, y0) = transform(-half_w, -half_h);
                                let (x1, y1) = transform(half_w, -half_h);
                                let (x2, y2) = transform(half_w, half_h);
                                let (x3, y3) = transform(-half_w, half_h);

                                let vertices = vec![
                                    TextureVertex::new(x0, y0, 0.0, 0.0),
                                    TextureVertex::new(x1, y1, 1.0, 0.0),
                                    TextureVertex::new(x2, y2, 1.0, 1.0),
                                    TextureVertex::new(x3, y3, 0.0, 1.0),
                                ];
                                let indices: Vec<u32> = vec![0, 1, 2, 0, 2, 3];

                                let vb = self.state.create_vertex_buffer(&vertices, "symbol_vb");
                                let ib = self.state.create_index_buffer(&indices, "symbol_ib");

                                render_pass.set_bind_group(1, &tex.bind_group, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(0..6, 0, 0..1);
                            }
                        }
                    }
                }
            }
        }

        // Render egui UI overlay (after chart rendering, to surface texture directly)
        let (width, height) = self.state.viewport_size();
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width as u32, height as u32],
            pixels_per_point: self.state.window.scale_factor() as f32,
        };

        self.egui.render(
            &self.state.device,
            &self.state.queue,
            &mut encoder,
            &view,
            screen_descriptor,
            egui_output,
        );

        self.state.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok(())
    }

    /// Get window reference
    #[inline]
    pub fn window(&self) -> &Window {
        &self.state.window
    }

    /// Save screenshot to file
    pub fn save_screenshot<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        use crate::state::MSAA_SAMPLE_COUNT;

        let (width, height) = self.state.viewport_size();
        let width = width as u32;
        let height = height as u32;

        // Use the same format as the surface for pipeline compatibility
        let screenshot_format = self.state.format();

        // Create MSAA texture for rendering (pipelines are configured for MSAA)
        let msaa_texture = self.state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screenshot_msaa_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: MSAA_SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: screenshot_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let msaa_view = msaa_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Create resolve texture (non-MSAA, COPY_SRC for screenshot)
        let resolve_texture = self.state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screenshot_resolve_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: screenshot_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let resolve_view = resolve_texture.create_view(&wgpu::TextureViewDescriptor::default());

        tracing::debug!(
            "Screenshot format: {:?}, MSAA samples: {}",
            screenshot_format,
            MSAA_SAMPLE_COUNT
        );

        // Calculate buffer dimensions (aligned to 256 bytes)
        let bytes_per_pixel = 4u32;
        let unpadded_bytes_per_row = width * bytes_per_pixel;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let buffer_size = (padded_bytes_per_row * height) as u64;

        // Create output buffer
        let output_buffer = self.state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot_buffer"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            self.state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("screenshot_encoder"),
                });

        // Recreate buffers for rendering
        let area_vertex_buffer = if !self.area_vertices.is_empty() {
            Some(
                self.state
                    .create_vertex_buffer(&self.area_vertices, "area_vertices"),
            )
        } else {
            None
        };

        let area_index_buffer = if !self.area_indices.is_empty() {
            Some(
                self.state
                    .create_index_buffer(&self.area_indices, "area_indices"),
            )
        } else {
            None
        };

        let line_vertex_buffer = if !self.line_vertices.is_empty() {
            Some(
                self.state
                    .create_vertex_buffer(&self.line_vertices, "line_vertices"),
            )
        } else {
            None
        };

        let line_index_buffer = if !self.line_indices.is_empty() {
            Some(
                self.state
                    .create_index_buffer(&self.line_indices, "line_indices"),
            )
        } else {
            None
        };

        // Render to MSAA texture, resolve to screenshot texture
        {
            let bg = self.background_color.to_array();
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("screenshot_render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &msaa_view,
                    resolve_target: Some(&resolve_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // Render areas
            if let (Some(vb), Some(ib)) = (&area_vertex_buffer, &area_index_buffer) {
                render_pass.set_pipeline(&self.pipelines.area_pipeline);
                render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                render_pass.set_vertex_buffer(0, vb.slice(..));
                render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..self.area_indices.len() as u32, 0, 0..1);
            }

            // Render lines
            if let (Some(vb), Some(ib)) = (&line_vertex_buffer, &line_index_buffer) {
                render_pass.set_pipeline(&self.pipelines.line_pipeline);
                render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                render_pass.set_vertex_buffer(0, vb.slice(..));
                render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..self.line_indices.len() as u32, 0, 0..1);
            }

            // Render symbols
            if !self.symbol_instances.is_empty() {
                render_pass.set_pipeline(&self.pipelines.texture_pipeline);
                render_pass.set_bind_group(0, &self.view_bind_group, &[]);

                // Iterate by index to avoid borrow conflicts with self.state
                for i in 0..self.symbol_instances.len() {
                    let instance = &self.symbol_instances[i];
                    if let Some(tex) = self.symbol_textures.get(&instance.symbol_id) {
                        // S-101 compliant symbol scaling
                        let display_scale = instance.scale * SCREEN_PX_PER_MM / tex.render_scale
                            * S101_SYMBOL_SCALE
                            * self.symbol_scale;

                        let half_w = (tex.width as f32 * display_scale) / 2.0;
                        let half_h = (tex.height as f32 * display_scale) / 2.0;

                        // Pivot offset (in screen pixels)
                        // pivot_in_texture is in texture pixels (from top-left)
                        // display_scale converts texture pixels to screen pixels
                        let pivot_x = tex.pivot_in_texture.0 * display_scale;
                        let pivot_y = tex.pivot_in_texture.1 * display_scale;

                        let rotation = instance.rotation.to_radians();
                        let cos_r = rotation.cos();
                        let sin_r = rotation.sin();

                        let transform = |dx: f32, dy: f32| -> (f32, f32) {
                            // Offset from top-left, then shift so pivot aligns with origin
                            let px = dx + half_w - pivot_x;
                            let py = dy + half_h - pivot_y;
                            let rx = px * cos_r - py * sin_r;
                            let ry = px * sin_r + py * cos_r;
                            (instance.screen_x + rx, instance.screen_y + ry)
                        };

                        let (x0, y0) = transform(-half_w, -half_h);
                        let (x1, y1) = transform(half_w, -half_h);
                        let (x2, y2) = transform(half_w, half_h);
                        let (x3, y3) = transform(-half_w, half_h);

                        let vertices = [
                            TextureVertex::new(x0, y0, 0.0, 0.0),
                            TextureVertex::new(x1, y1, 1.0, 0.0),
                            TextureVertex::new(x2, y2, 1.0, 1.0),
                            TextureVertex::new(x3, y3, 0.0, 1.0),
                        ];
                        let indices: [u32; 6] = [0, 1, 2, 0, 2, 3];

                        let vb = self.state.create_vertex_buffer(&vertices, "symbol_quad_vb");
                        let ib = self.state.create_index_buffer(&indices, "symbol_quad_ib");

                        render_pass.set_bind_group(1, &tex.bind_group, &[]);
                        render_pass.set_vertex_buffer(0, vb.slice(..));
                        render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                        render_pass.draw_indexed(0..6, 0, 0..1);
                    }
                }
            }
        }

        // Copy resolved texture to buffer
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &resolve_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.state.queue.submit(std::iter::once(encoder.finish()));

        // Map buffer and read data
        let buffer_slice = output_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        self.state.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| WgpuError::Render(format!("Failed to receive map result: {}", e)))?
            .map_err(|e| WgpuError::Render(format!("Buffer mapping failed: {:?}", e)))?;

        // Copy data and remove padding
        let data = buffer_slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let start = (row * padded_bytes_per_row) as usize;
            let end = start + (width * bytes_per_pixel) as usize;
            pixels.extend_from_slice(&data[start..end]);
        }
        drop(data);
        output_buffer.unmap();

        // Handle BGRA to RGBA conversion if needed
        let is_bgra = matches!(
            screenshot_format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        if is_bgra {
            // Swap B and R channels
            for chunk in pixels.chunks_exact_mut(4) {
                chunk.swap(0, 2); // Swap B and R
            }
        }

        // Save as PNG
        let image = image::RgbaImage::from_raw(width, height, pixels)
            .ok_or_else(|| WgpuError::Render("Failed to create image from pixels".to_string()))?;
        image
            .save(path.as_ref())
            .map_err(|e| WgpuError::Render(format!("Failed to save screenshot: {}", e)))?;

        tracing::info!("Screenshot saved to: {}", path.as_ref().display());
        Ok(())
    }

    /// Get rendering statistics
    pub fn statistics(&self) -> RenderStats {
        RenderStats {
            area_vertices: self.area_vertices.len(),
            area_triangles: self.area_indices.len() / 3,
            line_vertices: self.line_vertices.len(),
            line_triangles: self.line_indices.len() / 3,
            symbol_instances: self.symbol_instances.len(),
            symbol_textures: self.symbol_textures.len(),
        }
    }
}

/// Rendering statistics
#[derive(Debug, Clone, Default)]
pub struct RenderStats {
    pub area_vertices: usize,
    pub area_triangles: usize,
    pub line_vertices: usize,
    pub line_triangles: usize,
    pub symbol_instances: usize,
    pub symbol_textures: usize,
}

impl std::fmt::Display for RenderStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Areas: {} verts, {} tris | Lines: {} verts, {} tris | Symbols: {} instances, {} textures",
            self.area_vertices,
            self.area_triangles,
            self.line_vertices,
            self.line_triangles,
            self.symbol_instances,
            self.symbol_textures
        )
    }
}
