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

// Note: S-100 symbol sizing works as follows:
// 1. SVG symbols have mm dimensions (e.g., ACHBRT07 = 5.38mm wide)
// 2. usvg converts mm → user units (px at 96 DPI): 5.38mm → 20.3 user units
// 3. Texture is rendered at tree_size × render_scale (7.56) → ~154px
// 4. To display at correct mm size: display_scale = 1.0 / render_scale
//    (which recovers the original user-unit size = physical mm size at 96 DPI)
// The formula is: display_scale = instance.scale / tex.render_scale * self.symbol_scale

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::egui_integration;
use crate::profiler::{CpuProfiler, GpuProfilerWrapper, ScopeTimer};
use ferrite_portrayal_catalog::ColorProfile;
use ferrite_render::{
    intern_symbol, Color, DrawingInstruction, RenderContext, ScreenPoint, SymbolId, WorldPoint,
};

use crate::egui_integration::{AppUiState, EguiIntegration, SettingsState};
use crate::pipeline::{PatternVertex, TextureVertex};
use crate::{GpuState, RenderPipelines, Result, SymbolCache, Vertex2D, ViewUniforms, WgpuError};

/// Ray-casting point-in-polygon test.
/// Returns true if point (px, py) is inside the given ring (list of (x,y) vertices).
fn point_in_ring(px: f32, py: f32, ring: &[(f32, f32)]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Cached triangulation result (world-coordinate earcut)
/// Stores indices and cleaned world vertices so earcut runs only once per polygon shape.
struct CachedTriangulation {
    /// Earcut triangle indices (into world_vertices)
    indices: Vec<usize>,
    /// Cleaned world-coordinate vertices [x0, y0, x1, y1, ...] (exterior + holes)
    world_vertices: Vec<f64>,
    /// World-coordinate axis-aligned bounding box (min_x, min_y, max_x, max_y)
    /// Used for O(1) viewport frustum culling — skip entire area if AABB is off-screen
    #[allow(dead_code)]
    world_aabb: (f64, f64, f64, f64),
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
#[allow(dead_code)]
fn hash_geometry(points: &[WorldPoint]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for p in points {
        ((p.x * 1_000_000.0) as i64).hash(&mut hasher);
        ((p.y * 1_000_000.0) as i64).hash(&mut hasher);
    }
    hasher.finish()
}

/// Cached GPU texture for a pattern fill (uses Repeat sampler)
struct PatternTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    /// Texture dimensions in pixels (matches tiling period exactly)
    width: u32,
    height: u32,
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

/// Pending text label to render via egui painter overlay
struct TextLabel {
    screen_x: f32,
    screen_y: f32,
    text: String,
    font_size: f32,
    color: [f32; 4],
    #[allow(dead_code)]
    bold: bool,
    italic: bool,
    h_align: ferrite_render::HAlign,
    v_align: ferrite_render::VAlign,
}

/// Grid-based text collision avoidance (S-100 Part 9: overplot removal)
struct TextCollisionGrid {
    occupied: std::collections::HashSet<(i32, i32)>,
    cell_size: f32,
}

impl TextCollisionGrid {
    fn new(cell_size: f32) -> Self {
        Self {
            occupied: std::collections::HashSet::with_capacity(2000),
            cell_size: cell_size.max(1.0),
        }
    }

    fn clear(&mut self) {
        self.occupied.clear();
    }

    /// Try to place a text label. Returns true if space is available.
    fn try_place(&mut self, x: f32, y: f32, width: f32, height: f32) -> bool {
        let x0 = (x / self.cell_size).floor() as i32;
        let y0 = (y / self.cell_size).floor() as i32;
        let x1 = ((x + width) / self.cell_size).floor() as i32;
        let y1 = ((y + height) / self.cell_size).floor() as i32;

        // Check if any cell in the bounding box is occupied
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                if self.occupied.contains(&(gx, gy)) {
                    return false;
                }
            }
        }

        // Claim cells
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                self.occupied.insert((gx, gy));
            }
        }
        true
    }
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
    symbol_grid: FxHashSet<(i32, i32)>,
    /// Screen-space grid for sounding decluttering
    sounding_screen_grid: FxHashMap<(i32, i32), ((i64, i64), f64)>,
    /// Exact positions of soundings that have been allowed through (world coordinates)
    sounding_exact_positions: FxHashSet<(i64, i64)>,
    /// World-coordinate deduplication (to remove exact duplicates from multiple charts)
    world_dedup: FxHashSet<(i64, i64, u64)>,
    /// Grid cell size in pixels (adjusted by zoom)
    grid_cell_size: f32,
    /// Sounding grid cell size in pixels (screen-space)
    /// Fixed size for consistent density regardless of zoom
    sounding_cell_size_px: f32,
    /// Skip screen-space decluttering during animation (when preserve_declutter is true)
    skip_screen_declutter: bool,
    /// Screen-space pan offset (pixels) for fast panning during drag
    screen_pan_offset: (f32, f32),
    /// GPU zoom scale for smooth zooming (1.0 = no zoom delta, rebuilt at this level)
    screen_zoom_scale: f32,
    /// Zoom pivot point in screen coordinates
    screen_zoom_pivot: (f32, f32),
    /// egui integration for UI overlay
    egui: EguiIntegration,
    /// UI state shared with main app
    pub ui_state: AppUiState,
    // === OPTIMIZATION FIELDS ===
    /// Cached triangulations by feature ID (optimization)
    triangulation_cache: HashMap<i64, CachedTriangulation>,
    /// Batched symbols by texture (optimization, keyed by interned SymbolId)
    symbol_batches: FxHashMap<SymbolId, (Vec<TextureVertex>, Vec<u32>)>,
    /// Packed symbol vertices for single-buffer rendering
    packed_symbol_vertices: Vec<TextureVertex>,
    /// Packed symbol indices for single-buffer rendering
    packed_symbol_indices: Vec<u32>,
    /// Ranges into packed arrays per symbol texture: (symbol_id, index_start, index_count)
    packed_symbol_ranges: Vec<(SymbolId, u32, u32)>,
    /// Animation/drag mode - enables fast-path rendering
    pub animation_mode: bool,
    /// LOD level (0=full detail, 1=medium, 2=low)
    lod_level: u8,
    /// Viewport bounds in world coordinates for culling
    viewport_world_bounds: Option<(f64, f64, f64, f64)>,
    // === S-101 PRIORITY GROUP RENDERING ===
    /// Area index ranges by priority: (display_plane, priority, start_index, end_index)
    area_priority_ranges: Vec<(u8, i32, usize, usize)>,
    /// Line index ranges by priority: (display_plane, priority, start_index, end_index)
    line_priority_ranges: Vec<(u8, i32, usize, usize)>,
    /// Symbol instance ranges by priority: (display_plane, priority, start_index, end_index)
    symbol_priority_ranges: Vec<(u8, i32, usize, usize)>,
    // === PATTERN FILL (S-100 GPU texture-repeat tiling) ===
    /// Pattern fill vertices (TextureVertex: position + inv_tile_size)
    pattern_vertices: Vec<PatternVertex>,
    /// Pattern fill indices
    pattern_indices: Vec<u32>,
    /// Pattern fill ranges: (display_plane, priority, index_start, index_end, pattern_texture_key)
    pattern_ranges: Vec<(u8, i32, usize, usize, String)>,
    /// Pattern fill GPU textures (keyed by "{symbol}_pat")
    pattern_textures: HashMap<String, PatternTexture>,
    /// Pending text labels to render via egui painter
    text_labels: Vec<TextLabel>,
    /// Grid for text collision avoidance
    text_collision_grid: TextCollisionGrid,
    // === CACHED GPU BUFFERS (avoid recreating every frame) ===
    /// Cached area vertex buffer (rebuilt only when geometry changes)
    cached_area_vb: Option<wgpu::Buffer>,
    cached_area_ib: Option<wgpu::Buffer>,
    cached_area_index_count: u32,
    /// Cached line vertex buffer
    cached_line_vb: Option<wgpu::Buffer>,
    cached_line_ib: Option<wgpu::Buffer>,
    cached_line_index_count: u32,
    /// Cached pattern vertex buffer
    cached_pattern_vb: Option<wgpu::Buffer>,
    cached_pattern_ib: Option<wgpu::Buffer>,
    cached_pattern_index_count: u32,
    /// Whether cached GPU buffers are stale and need rebuild
    gpu_buffers_dirty: bool,
    /// Cached symbol GPU buffers per priority range (avoid recreating every frame)
    #[allow(clippy::type_complexity)]
    cached_symbol_buffers: Vec<(
        u8,
        i32,
        usize,
        usize,
        wgpu::Buffer,
        wgpu::Buffer,
        Vec<(SymbolId, u32, u32)>,
    )>,
    // === INSTRUCTION CACHE (reused across rebuilds when only view changes) ===
    /// Cached line suppression set (stable between rebuilds, only invalidated on chart reload)
    cached_suppressed_lines: Option<(usize, usize, FxHashSet<usize>)>, // (ptr, len, set)
    // === PROFILING ===
    /// CPU-side performance profiler
    pub cpu_profiler: CpuProfiler,
    /// GPU-side profiler (wgpu-profiler)
    gpu_profiler: GpuProfilerWrapper,
}

impl WgpuRenderer {
    /// Create new renderer for window
    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let state = GpuState::new(window.clone()).await?;
        let pipelines = RenderPipelines::new(&state)?;
        let gpu_profiler = GpuProfilerWrapper::new(&state.device);

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
            show_soundings: true, // Visibility controlled by S-101 viewing groups
            zoom_level: 1.0,
            compilation_scale: 22000, // Default compilation scale (1:22000)
            symbol_grid: FxHashSet::with_capacity_and_hasher(1000, Default::default()),
            sounding_screen_grid: FxHashMap::with_capacity_and_hasher(2000, Default::default()),
            sounding_exact_positions: FxHashSet::with_capacity_and_hasher(5000, Default::default()),
            world_dedup: FxHashSet::with_capacity_and_hasher(5000, Default::default()),

            grid_cell_size: 30.0,         // Default grid cell size in pixels
            sounding_cell_size_px: 150.0, // Fixed pixel spacing between soundings
            skip_screen_declutter: false,
            screen_pan_offset: (0.0, 0.0),
            screen_zoom_scale: 1.0,
            screen_zoom_pivot: (0.0, 0.0),
            egui,
            ui_state: AppUiState::default(),
            // Optimization fields
            triangulation_cache: HashMap::with_capacity(500),
            symbol_batches: FxHashMap::with_capacity_and_hasher(50, Default::default()),
            packed_symbol_vertices: Vec::with_capacity(4000),
            packed_symbol_indices: Vec::with_capacity(6000),
            packed_symbol_ranges: Vec::with_capacity(50),
            animation_mode: false,
            lod_level: 0,
            viewport_world_bounds: None,
            // S-101 priority group rendering
            area_priority_ranges: Vec::with_capacity(10),
            line_priority_ranges: Vec::with_capacity(10),
            symbol_priority_ranges: Vec::with_capacity(10),
            // Pattern fill
            pattern_vertices: Vec::with_capacity(5000),
            pattern_indices: Vec::with_capacity(15000),
            pattern_ranges: Vec::with_capacity(10),
            pattern_textures: HashMap::new(),
            text_labels: Vec::with_capacity(500),
            text_collision_grid: TextCollisionGrid::new(12.0),
            // GPU buffer cache
            cached_area_vb: None,
            cached_area_ib: None,
            cached_area_index_count: 0,
            cached_line_vb: None,
            cached_line_ib: None,
            cached_line_index_count: 0,
            cached_pattern_vb: None,
            cached_pattern_ib: None,
            cached_pattern_index_count: 0,
            gpu_buffers_dirty: true,
            cached_symbol_buffers: Vec::new(),
            cached_suppressed_lines: None,
            // Profiling
            cpu_profiler: CpuProfiler::new(),
            gpu_profiler,
        })
    }

    /// Enable or disable profiling (both CPU and GPU)
    pub fn set_profiling_enabled(&mut self, enabled: bool) {
        crate::profiler::set_profiling_enabled(enabled);
        self.gpu_profiler.set_enabled(enabled);
    }

    /// Flush profiler reports (call on shutdown)
    pub fn flush_profiler(&mut self) {
        self.cpu_profiler.flush();
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
            // Add 50% margin to accommodate GPU pan offset during drag/inertia.
            // Without this, symbols near the viewport edge get culled and then
            // "pop in" when the view rebuilds after drag ends.
            let margin_x = (max_x - min_x) * 0.5;
            let margin_y = (max_y - min_y) * 0.5;
            x >= min_x - margin_x
                && x <= max_x + margin_x
                && y >= min_y - margin_y
                && y <= max_y + margin_y
        } else {
            true // No bounds set, assume visible
        }
    }

    /// Check if a world-space AABB intersects the viewport (with margin for GPU pan/zoom)
    #[inline]
    fn is_aabb_visible(
        &self,
        aabb_min_x: f64,
        aabb_min_y: f64,
        aabb_max_x: f64,
        aabb_max_y: f64,
    ) -> bool {
        if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) = self.viewport_world_bounds {
            let margin_x = (vp_max_x - vp_min_x) * 0.5;
            let margin_y = (vp_max_y - vp_min_y) * 0.5;
            // Standard AABB intersection test with margin
            aabb_max_x >= vp_min_x - margin_x
                && aabb_min_x <= vp_max_x + margin_x
                && aabb_max_y >= vp_min_y - margin_y
                && aabb_min_y <= vp_max_y + margin_y
        } else {
            true
        }
    }

    /// Static frustum culling for a ring of world points against viewport bounds.
    /// Used by tile_area_with_pattern/hatch where &self is already mutably borrowed.
    #[inline]
    fn is_ring_visible_static(
        ring: &[WorldPoint],
        viewport_world_bounds: Option<(f64, f64, f64, f64)>,
    ) -> bool {
        if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) = viewport_world_bounds {
            let margin_x = (vp_max_x - vp_min_x) * 0.5;
            let margin_y = (vp_max_y - vp_min_y) * 0.5;
            let mut ax = f64::MAX;
            let mut ay = f64::MAX;
            let mut bx = f64::MIN;
            let mut by = f64::MIN;
            for p in ring {
                if p.x < ax {
                    ax = p.x;
                }
                if p.y < ay {
                    ay = p.y;
                }
                if p.x > bx {
                    bx = p.x;
                }
                if p.y > by {
                    by = p.y;
                }
            }
            bx >= vp_min_x - margin_x
                && ax <= vp_max_x + margin_x
                && by >= vp_min_y - margin_y
                && ay <= vp_max_y + margin_y
        } else {
            true
        }
    }

    /// Clear triangulation cache (call when chart data changes)
    pub fn clear_triangulation_cache(&mut self) {
        self.triangulation_cache.clear();
        self.cached_suppressed_lines = None; // Invalidate when chart data changes
    }

    /// Clear symbol textures (call when color profile changes)
    pub fn clear_symbol_textures(&mut self) {
        self.symbol_textures.clear();
    }

    /// Pack symbol instances into contiguous vertex/index arrays for single-buffer rendering.
    /// Produces packed_symbol_vertices, packed_symbol_indices, and packed_symbol_ranges.
    fn pack_symbol_batch_range(&mut self, start: usize, end: usize) {
        self.packed_symbol_vertices.clear();
        self.packed_symbol_indices.clear();
        self.packed_symbol_ranges.clear();

        // First, group by symbol_id using the existing batches map
        self.symbol_batches.clear();
        for instance in &self.symbol_instances[start..end] {
            if let Some(tex) = self.symbol_textures.get(&instance.symbol_id) {
                let display_scale = instance.scale / tex.render_scale * self.symbol_scale;
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

                let batch = self
                    .symbol_batches
                    .entry(instance.symbol_id)
                    .or_insert_with(|| (Vec::new(), Vec::new()));
                let base = batch.0.len() as u32;
                batch.0.push(TextureVertex::new(x0, y0, 0.0, 0.0));
                batch.0.push(TextureVertex::new(x1, y1, 1.0, 0.0));
                batch.0.push(TextureVertex::new(x2, y2, 1.0, 1.0));
                batch.0.push(TextureVertex::new(x3, y3, 0.0, 1.0));
                batch
                    .1
                    .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }

        // Now pack all batches into contiguous arrays
        for (&sym_id, (verts, idxs)) in &self.symbol_batches {
            let vertex_offset = self.packed_symbol_vertices.len() as u32;
            let index_start = self.packed_symbol_indices.len() as u32;

            self.packed_symbol_vertices.extend_from_slice(verts);
            // Offset indices by vertex_offset
            for &idx in idxs {
                self.packed_symbol_indices.push(idx + vertex_offset);
            }

            let index_count = idxs.len() as u32;
            self.packed_symbol_ranges
                .push((sym_id, index_start, index_count));
        }
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

    /// Check if egui has requested a repaint (e.g., animations, hover effects)
    #[inline]
    pub fn egui_needs_repaint(&self) -> bool {
        self.egui.ctx.has_requested_repaint()
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
        let uniforms = ViewUniforms::with_pan_zoom(
            width,
            height,
            1.0,
            self.screen_pan_offset.0,
            self.screen_pan_offset.1,
            self.screen_zoom_scale,
            self.screen_zoom_pivot.0,
            self.screen_zoom_pivot.1,
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
        self.screen_zoom_scale = 1.0;
        self.screen_zoom_pivot = (0.0, 0.0);
        self.update_view_uniforms();
    }

    /// Set GPU zoom scale for smooth zooming without geometry rebuild.
    /// The shader scales all geometry around the pivot point.
    #[inline]
    pub fn set_gpu_zoom(&mut self, scale: f32, pivot_x: f32, pivot_y: f32) {
        self.screen_zoom_scale = scale;
        self.screen_zoom_pivot = (pivot_x, pivot_y);
        self.update_view_uniforms();
    }

    /// Get current GPU zoom scale
    #[inline]
    pub fn gpu_zoom_scale(&self) -> f32 {
        self.screen_zoom_scale
    }

    /// Begin a new frame - clears buffers
    pub fn begin_frame(&mut self) {
        self.begin_frame_ex(false);
    }

    /// Begin a new frame with optional preservation of declutter state
    /// If `preserve_declutter` is true, skip screen-space decluttering during animation
    pub fn begin_frame_ex(&mut self, preserve_declutter: bool) {
        // Mark GPU buffers as needing rebuild
        self.gpu_buffers_dirty = true;
        // Invalidate cached symbol GPU buffers (geometry changed)
        self.cached_symbol_buffers.clear();

        // Preserve previous frame counts for pre-allocation (avoids realloc during build)
        let prev_area_v = self.area_vertices.len();
        let prev_area_i = self.area_indices.len();
        let prev_line_v = self.line_vertices.len();
        let prev_line_i = self.line_indices.len();

        self.area_vertices.clear();
        self.area_indices.clear();
        self.line_vertices.clear();
        self.line_indices.clear();
        self.symbol_instances.clear();

        // Reserve capacity based on previous frame (amortized zero reallocs in steady state)
        if prev_area_v > self.area_vertices.capacity() / 2 {
            self.area_vertices.reserve(prev_area_v);
        }
        if prev_area_i > self.area_indices.capacity() / 2 {
            self.area_indices.reserve(prev_area_i);
        }
        if prev_line_v > self.line_vertices.capacity() / 2 {
            self.line_vertices.reserve(prev_line_v);
        }
        if prev_line_i > self.line_indices.capacity() / 2 {
            self.line_indices.reserve(prev_line_i);
        }
        // Clear symbol batches for new frame
        self.symbol_batches.clear();
        // Clear priority ranges for S-101 compliant rendering
        self.area_priority_ranges.clear();
        self.line_priority_ranges.clear();
        self.symbol_priority_ranges.clear();
        self.pattern_vertices.clear();
        self.pattern_indices.clear();
        self.pattern_ranges.clear();
        self.text_labels.clear();
        self.text_collision_grid.clear();

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
        let profiling = crate::profiler::is_profiling_enabled();
        let total_timer = if profiling {
            Some(ScopeTimer::new("add_instructions_total"))
        } else {
            None
        };

        // Set animation mode and sort instructions, then extract what we need
        context.set_animation_mode(self.animation_mode);
        // get_sorted_instructions() sorts in-place on first call, then returns &slice
        // Clone scaler (cheap: a few f64 fields) to avoid borrow conflict with &mut self methods
        let scaler = context.scaler.clone();
        let instructions = context.get_sorted_instructions();
        let _instruction_count = instructions.len();

        // Update viewport bounds for frustum culling
        self.update_viewport_bounds(&scaler);

        // =====================================================================
        // S-100 Part 9-11.1.9: Line suppression pre-pass
        // =====================================================================
        // When multiple features share the same curve geometry, only the
        // highest-priority LineInstruction is rendered. Lines marked as
        // unsuppressible (LineInstructionUnsuppressed) always render.
        //
        // Build a map from curve geometry hash -> highest priority that claims it.
        // A curve is identified by hashing all its world-coordinate points, so two
        // line instructions referencing the same spatial curve produce the same key.
        // =====================================================================
        // Line suppression: use cached set if instructions haven't changed
        // =====================================================================
        let suppression_timer = if profiling {
            Some(ScopeTimer::new("line_suppression"))
        } else {
            None
        };
        let inst_ptr = instructions.as_ptr() as usize;
        let inst_len = instructions.len();

        // Check if cached suppression set is still valid (same instruction slice)
        let cache_hit = matches!(
            &self.cached_suppressed_lines,
            Some((cached_ptr, cached_len, _)) if *cached_ptr == inst_ptr && *cached_len == inst_len
        );
        if !cache_hit {
            let set = Self::compute_line_suppression(instructions);
            self.cached_suppressed_lines = Some((inst_ptr, inst_len, set));
        }
        // Clone the Arc-like reference for use in the loop (FxHashSet clone is cheap for small sets)
        let suppressed_lines = self.cached_suppressed_lines.as_ref().unwrap().2.clone();
        if let Some(t) = suppression_timer {
            self.cpu_profiler.record("line_suppression", t.elapsed());
        }

        // Pre-compute world→screen transform once for all areas
        let area_transform = Self::scaler_transform(&scaler);

        // Track skipped counts for debugging
        let mut _culled_count = 0usize;

        // Per-type timing accumulators
        let mut area_time = std::time::Duration::ZERO;
        let mut line_time = std::time::Duration::ZERO;
        let mut symbol_time = std::time::Duration::ZERO;
        let mut text_time = std::time::Duration::ZERO;
        let mut area_count = 0u32;
        let mut line_count = 0u32;
        let mut symbol_count = 0u32;
        let mut text_count = 0u32;

        // S-101 Priority tracking: track index ranges per (display_plane, priority)
        let mut current_priority: Option<i32> = None;
        let mut current_plane: u8 = 0; // 0=UnderRadar, 1=OverRadar
        let mut area_start_idx = 0usize;
        let mut line_start_idx = 0usize;
        let mut symbol_start_idx = 0usize;

        for (inst_idx, instruction) in instructions.iter().enumerate() {
            // Display Mode filtering: skip instructions not in visible viewing groups
            if let Some(visible) = visible_viewing_groups {
                let vg = instruction.viewing_group().0;
                if !visible.contains(&vg) {
                    // Allow sounding viewing group (33010) through when show_soundings is enabled
                    if !(self.show_soundings && vg == 33010) {
                        continue;
                    }
                }
            }

            let inst_priority = instruction.priority().0;
            let inst_plane = match instruction.display_plane() {
                ferrite_render::DisplayPlane::UnderRadar => 0u8,
                ferrite_render::DisplayPlane::OverRadar => 1u8,
            };

            // Check if priority or display plane changed - record ranges for previous group
            if let Some(prev_priority) = current_priority {
                if prev_priority != inst_priority || current_plane != inst_plane {
                    // Record area range if any areas were added for previous group
                    if self.area_indices.len() > area_start_idx {
                        self.area_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            area_start_idx,
                            self.area_indices.len(),
                        ));
                    }
                    area_start_idx = self.area_indices.len();

                    // Record line range if any lines were added for previous group
                    if self.line_indices.len() > line_start_idx {
                        self.line_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            line_start_idx,
                            self.line_indices.len(),
                        ));
                    }
                    line_start_idx = self.line_indices.len();

                    // Record symbol range if any symbols were added for previous group
                    if self.symbol_instances.len() > symbol_start_idx {
                        self.symbol_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            symbol_start_idx,
                            self.symbol_instances.len(),
                        ));
                    }
                    symbol_start_idx = self.symbol_instances.len();
                }
            }
            current_priority = Some(inst_priority);
            current_plane = inst_plane;

            // S-100 Scale-dependent visibility: skip instructions outside their scale range
            {
                let viewing_scale = self.viewing_scale();
                if !instruction.scale_range().is_visible_at(viewing_scale) {
                    continue;
                }
            }

            let inst_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };

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

                    // Pattern fills: tile symbols inside the polygon area
                    if let ferrite_render::AreaFillType::Pattern {
                        ref symbol_ref,
                        v1,
                        v2,
                    } = area.fill
                    {
                        // Render pattern overlay only when enabled
                        if self.ui_state.settings.show_shallow_pattern {
                            if let (Some(cache), Some(profile)) =
                                (symbol_cache.as_mut(), color_profile)
                            {
                                self.tile_area_with_pattern(
                                    area,
                                    &symbol_ref.clone(),
                                    v1,
                                    v2,
                                    &scaler,
                                    cache,
                                    profile,
                                    inst_priority,
                                );
                            }
                        }
                    } else if let ferrite_render::AreaFillType::HatchFill {
                        color,
                        width,
                        spacing,
                        angle,
                    } = &area.fill
                    {
                        self.tile_area_with_hatch(
                            area,
                            *color,
                            *width,
                            *spacing,
                            *angle,
                            &scaler,
                            inst_priority,
                        );
                    } else {
                        self.add_area_cached(area, area_transform);
                    }
                    if let Some(s) = inst_start {
                        area_time += s.elapsed();
                        area_count += 1;
                    }
                }
                DrawingInstruction::Line(line) => {
                    // S-100 Part 9-11.1.9: Skip suppressed lines (lower-priority
                    // suppressible lines on curves already claimed by higher priority)
                    if suppressed_lines.contains(&inst_idx) {
                        _culled_count += 1;
                        continue;
                    }
                    // LOD: Skip short lines when zoomed out
                    if self.animation_mode && self.lod_level > 0 && line.points.len() < 5 {
                        _culled_count += 1;
                        continue;
                    }
                    self.add_line(line, &scaler);
                    if let Some(s) = inst_start {
                        line_time += s.elapsed();
                        line_count += 1;
                    }
                }
                DrawingInstruction::Point(point) => {
                    // Frustum culling: skip points outside viewport
                    if !self.is_point_visible(point.position.x, point.position.y) {
                        _culled_count += 1;
                        continue;
                    }

                    // Soundings: respect show_soundings toggle
                    let is_sounding = point.symbol_ref.starts_with("SOUNDG")
                        || point.symbol_ref.starts_with("SOUNDS");
                    if is_sounding && !self.show_soundings {
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
                        self.try_add_symbol(point, &scaler, cache, profile)
                    } else {
                        false
                    };

                    // Fallback to placeholder if symbol not found
                    if !rendered {
                        self.add_point_fallback(point, &scaler);
                    }
                    if let Some(s) = inst_start {
                        symbol_time += s.elapsed();
                        symbol_count += 1;
                    }
                }
                DrawingInstruction::Text(text) => {
                    // LOD: Skip text during animation for performance
                    if self.animation_mode {
                        continue;
                    }

                    // Frustum culling: skip text outside viewport
                    if !self.is_point_visible(text.position.x, text.position.y) {
                        continue;
                    }

                    // Convert world position to screen coordinates
                    let screen = scaler.world_to_screen(text.position);

                    // S-100 Part 9a-11.2.2.4: FontSize is in typographic points (pt).
                    // S-101 Lua rules emit values like 10 (= 10pt standard body text).
                    // Convert points → pixels: pts * (DPI / 72), where 1pt = 1/72 inch.
                    let dpi_scale = self.state.window.scale_factor() as f32;
                    let screen_dpi = 96.0 * dpi_scale;
                    let font_size_px = (text.font_size * screen_dpi / 72.0).clamp(6.0, 40.0);

                    // Apply offset (in mm from Lua LocalOffset, convert to pixels)
                    let offset_x = text.offset.x * SCREEN_PX_PER_MM * dpi_scale;
                    let offset_y = text.offset.y * SCREEN_PX_PER_MM * dpi_scale;
                    let sx = screen.x + offset_x;
                    let sy = screen.y + offset_y;

                    // Estimate text bounding box for collision avoidance
                    let est_width = font_size_px * 0.6 * text.text.len() as f32;
                    let est_height = font_size_px * 1.3;

                    // Apply alignment offset for collision box
                    let box_x = match text.h_align {
                        ferrite_render::HAlign::Left => sx,
                        ferrite_render::HAlign::Center => sx - est_width * 0.5,
                        ferrite_render::HAlign::Right => sx - est_width,
                    };
                    let box_y = match text.v_align {
                        ferrite_render::VAlign::Top => sy,
                        ferrite_render::VAlign::Middle => sy - est_height * 0.5,
                        ferrite_render::VAlign::Bottom => sy - est_height,
                    };

                    // Collision avoidance: skip if overlapping existing text
                    if !self
                        .text_collision_grid
                        .try_place(box_x, box_y, est_width, est_height)
                    {
                        continue;
                    }

                    self.text_labels.push(TextLabel {
                        screen_x: sx,
                        screen_y: sy,
                        text: text.text.clone(),
                        font_size: font_size_px,
                        color: [text.color.r, text.color.g, text.color.b, text.color.a],
                        bold: text.bold,
                        italic: text.italic,
                        h_align: text.h_align,
                        v_align: text.v_align,
                    });
                    if let Some(s) = inst_start {
                        text_time += s.elapsed();
                        text_count += 1;
                    }
                }
            }
        }

        // Log per-type instruction timing
        if profiling {
            self.cpu_profiler.record("inst_area", area_time);
            self.cpu_profiler.record("inst_line", line_time);
            self.cpu_profiler.record("inst_symbol", symbol_time);
            self.cpu_profiler.record("inst_text", text_time);
            tracing::debug!(
                "[PROFILER] Instructions: area={} ({:.2}ms), line={} ({:.2}ms), symbol={} ({:.2}ms), text={} ({:.2}ms)",
                area_count, area_time.as_secs_f64() * 1000.0,
                line_count, line_time.as_secs_f64() * 1000.0,
                symbol_count, symbol_time.as_secs_f64() * 1000.0,
                text_count, text_time.as_secs_f64() * 1000.0,
            );
        }

        if let Some(t) = total_timer {
            let elapsed = t.elapsed();
            self.cpu_profiler.record("add_instructions_total", elapsed);
            tracing::debug!(
                "[PROFILER] add_instructions_total: {:.2}ms (areas: {}v/{}i, lines: {}v/{}i, symbols: {}, texts: {})",
                elapsed.as_secs_f64() * 1000.0,
                self.area_vertices.len(), self.area_indices.len(),
                self.line_vertices.len(), self.line_indices.len(),
                self.symbol_instances.len(), self.text_labels.len(),
            );
        }

        // Record final priority ranges
        if let Some(final_priority) = current_priority {
            if self.area_indices.len() > area_start_idx {
                self.area_priority_ranges.push((
                    current_plane,
                    final_priority,
                    area_start_idx,
                    self.area_indices.len(),
                ));
            }
            if self.line_indices.len() > line_start_idx {
                self.line_priority_ranges.push((
                    current_plane,
                    final_priority,
                    line_start_idx,
                    self.line_indices.len(),
                ));
            }
            if self.symbol_instances.len() > symbol_start_idx {
                self.symbol_priority_ranges.push((
                    current_plane,
                    final_priority,
                    symbol_start_idx,
                    self.symbol_instances.len(),
                ));
            }
        }
    }

    /// Fast O(1) cache key for area geometry based on slice pointer + length.
    /// Since instructions are borrowed from a stable slice, the exterior Vec's data pointer
    /// uniquely identifies the polygon geometry (same data = same pointer).
    fn area_geometry_key(area: &ferrite_render::AreaInstruction) -> i64 {
        // Use data pointer as unique identifier (stable while instructions slice is alive)
        let ptr = area.exterior.as_ptr() as usize;
        let len = area.exterior.len();
        // Combine pointer and length into a single i64 key
        // Pointer is unique per allocation, length adds extra discrimination
        (ptr as i64) ^ ((len as i64) << 48)
    }

    /// Clean a ring of world points: remove consecutive duplicates and closing duplicate
    fn clean_world_ring(points: &[WorldPoint], epsilon: f64) -> Vec<f64> {
        let mut cleaned: Vec<f64> = Vec::with_capacity(points.len() * 2);
        for p in points {
            if !p.x.is_finite() || !p.y.is_finite() {
                continue;
            }
            // Skip consecutive duplicates
            if cleaned.len() >= 2 {
                let prev_x = cleaned[cleaned.len() - 2];
                let prev_y = cleaned[cleaned.len() - 1];
                if (p.x - prev_x).abs() <= epsilon && (p.y - prev_y).abs() <= epsilon {
                    continue;
                }
            }
            cleaned.push(p.x);
            cleaned.push(p.y);
        }
        // Remove closing duplicate
        if cleaned.len() >= 6 {
            let n = cleaned.len();
            if (cleaned[0] - cleaned[n - 2]).abs() < epsilon * 10.0
                && (cleaned[1] - cleaned[n - 1]).abs() < epsilon * 10.0
            {
                cleaned.truncate(n - 2);
            }
        }
        cleaned
    }

    /// Compute signed area of a ring stored as [x0,y0,x1,y1,...] pairs
    fn ring_signed_area_flat(vertices: &[f64]) -> f64 {
        let n = vertices.len() / 2;
        if n < 3 {
            return 0.0;
        }
        let mut area = 0.0;
        for i in 0..n {
            let j = (i + 1) % n;
            area +=
                (vertices[j * 2] - vertices[i * 2]) * (vertices[j * 2 + 1] + vertices[i * 2 + 1]);
        }
        area
    }

    /// Get or compute cached triangulation for an area polygon.
    /// Triangulation is done in world coordinates so it only needs to run once per unique polygon.
    /// Ensure triangulation is cached for this area, returning the cache key.
    /// Returns None if the area cannot be triangulated.
    fn ensure_triangulated(&mut self, area: &ferrite_render::AreaInstruction) -> Option<i64> {
        let cache_key = Self::area_geometry_key(area);

        // Check cache first
        if self.triangulation_cache.contains_key(&cache_key) {
            return Some(cache_key);
        }

        // World-coordinate epsilon (degrees, ~0.01m precision)
        let epsilon = 1e-8;

        // Clean exterior ring in world coordinates
        let ext_verts = Self::clean_world_ring(&area.exterior, epsilon);
        if ext_verts.len() < 6 {
            return None; // < 3 points
        }

        let exterior_area = Self::ring_signed_area_flat(&ext_verts);
        let exterior_is_cw = exterior_area > 0.0;

        let mut vertices = ext_verts;
        let mut hole_indices: Vec<usize> = Vec::new();

        // Handle interior rings (holes)
        for hole in &area.interiors {
            let mut hole_verts = Self::clean_world_ring(hole, epsilon);
            if hole_verts.len() < 6 {
                continue;
            }

            let hole_area = Self::ring_signed_area_flat(&hole_verts);
            let hole_is_cw = hole_area > 0.0;
            if hole_is_cw == exterior_is_cw {
                // Reverse the hole ring
                let n = hole_verts.len() / 2;
                for i in 0..n / 2 {
                    let j = n - 1 - i;
                    hole_verts.swap(i * 2, j * 2);
                    hole_verts.swap(i * 2 + 1, j * 2 + 1);
                }
            }

            let hole_start = vertices.len() / 2;
            hole_indices.push(hole_start);
            vertices.extend_from_slice(&hole_verts);
        }

        // Triangulate in world coordinates
        let total_vertex_count = vertices.len() / 2;
        let indices = match earcutr::earcut(&vertices, &hole_indices, 2) {
            Ok(idx) if idx.len() >= 3 => idx
                .into_iter()
                .filter(|&i| i < total_vertex_count)
                .collect::<Vec<_>>(),
            _ => {
                // Fan triangulation fallback (exterior only)
                let n = vertices.len().min(area.exterior.len() * 2) / 2;
                if n < 3 {
                    return None;
                }
                let mut fan = Vec::with_capacity((n - 2) * 3);
                for i in 1..(n - 1) {
                    fan.push(0);
                    fan.push(i);
                    fan.push(i + 1);
                }
                fan
            }
        };

        if indices.len() < 3 {
            return None;
        }

        // Compute world AABB for frustum culling
        let mut aabb_min_x = f64::MAX;
        let mut aabb_min_y = f64::MAX;
        let mut aabb_max_x = f64::MIN;
        let mut aabb_max_y = f64::MIN;
        let vc = vertices.len() / 2;
        for i in 0..vc {
            let x = vertices[i * 2];
            let y = vertices[i * 2 + 1];
            if x < aabb_min_x {
                aabb_min_x = x;
            }
            if y < aabb_min_y {
                aabb_min_y = y;
            }
            if x > aabb_max_x {
                aabb_max_x = x;
            }
            if y > aabb_max_y {
                aabb_max_y = y;
            }
        }

        let cached = CachedTriangulation {
            indices,
            world_vertices: vertices,
            world_aabb: (aabb_min_x, aabb_min_y, aabb_max_x, aabb_max_y),
        };

        self.triangulation_cache.insert(cache_key, cached);
        Some(cache_key)
    }

    /// Pre-computed world→screen transform parameters (avoids per-area scaler lookups)
    #[inline]
    fn scaler_transform(scaler: &ferrite_render::Scaler) -> (f64, f64, f64, f64, f64, f64) {
        (
            scaler.scale_x(),
            scaler.scale_y(),
            scaler.offset_x(),
            scaler.offset_y(),
            scaler.geo_bounds.min_x,
            scaler.geo_bounds.max_y,
        )
    }

    fn add_area_cached(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        transform: (f64, f64, f64, f64, f64, f64),
    ) {
        // Get fill color — pattern/centroid/hatch fills are overlays, not solid fills
        let color = match &area.fill {
            ferrite_render::AreaFillType::Solid(c) => c.to_array(),
            ferrite_render::AreaFillType::Pattern { .. }
            | ferrite_render::AreaFillType::HatchFill { .. }
            | ferrite_render::AreaFillType::CentroidSymbol(_) => return,
        };

        // Early frustum culling from exterior ring AABB (BEFORE HashMap lookups).
        // This avoids ensure_triangulated + cache lookup for fully off-screen areas.
        if !Self::is_ring_visible_static(&area.exterior, self.viewport_world_bounds) {
            return;
        }

        // Ensure triangulation is cached, get cache key
        let cache_key = match self.ensure_triangulated(area) {
            Some(k) => k,
            None => return,
        };

        // Split borrow: access cache and output buffers as separate fields
        let cached = match self.triangulation_cache.get(&cache_key) {
            Some(c) => c,
            None => return,
        };

        let total_vertex_count = cached.world_vertices.len() / 2;

        // Copy data we need to local variables to release the borrow on triangulation_cache
        // We use raw slices to avoid cloning the Vecs
        let wv_ptr = cached.world_vertices.as_ptr();
        let wv_len = cached.world_vertices.len();
        let idx_ptr = cached.indices.as_ptr();
        let idx_len = cached.indices.len();
        // SAFETY: triangulation_cache is not modified during the loops below,
        // and these pointers remain valid because we don't mutate the cache.
        let wv = unsafe { std::slice::from_raw_parts(wv_ptr, wv_len) };
        let indices = unsafe { std::slice::from_raw_parts(idx_ptr, idx_len) };

        let base_index = self.area_vertices.len() as u32;

        // CPU-side world→screen transform (f64 precision, no GPU artifacts)
        let (scale_x, scale_y, offset_x, offset_y, min_x, max_y) = transform;

        self.area_vertices.extend((0..total_vertex_count).map(|i| {
            let wx = wv[i * 2];
            let wy = wv[i * 2 + 1];
            let sx = ((wx - min_x) * scale_x + offset_x) as f32;
            let sy = ((max_y - wy) * scale_y + offset_y) as f32;
            Vertex2D::new(sx, sy, color)
        }));

        self.area_indices
            .extend(indices.iter().map(|&idx| base_index + idx as u32));
    }

    /// Fill an area polygon with a tiled pattern texture (S-100 standard).
    ///
    /// Uses GPU texture repeat mode (like OpenS100's D2D1_EXTEND_MODE_WRAP):
    /// triangulates the polygon and assigns UV coordinates with optional shear
    /// for parallelogram tiling (S-100 Part 9a: v1/v2 lattice vectors).
    #[allow(clippy::too_many_arguments)]
    fn tile_area_with_pattern(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        symbol_ref: &str,
        v1: (f32, f32),
        v2: (f32, f32),
        scaler: &ferrite_render::Scaler,
        symbol_cache: &mut SymbolCache,
        color_profile: &ColorProfile,
        priority: i32,
    ) {
        // Frustum culling: quick AABB check on exterior ring
        if !Self::is_ring_visible_static(&area.exterior, self.viewport_world_bounds) {
            return;
        }

        // Apply HiDPI scale factor so pattern matches physical mm on screen
        let dpi_scale = self.state.window.scale_factor() as f32;
        let mm_to_px = SCREEN_PX_PER_MM * dpi_scale;

        // S-100: v1 is the horizontal period, v2 defines the row offset
        // Texture tile size = |v1| width × |v2.y| height (rectangular tile)
        // Parallelogram offset = v2.x (horizontal shift per row)
        let v1_len = (v1.0 * v1.0 + v1.1 * v1.1).sqrt();
        let spacing_x_px = (v1_len * mm_to_px).max(4.0);
        let spacing_y_px = (v2.1.abs() * mm_to_px).max(4.0);
        // Shear ratio: how much each row shifts horizontally (in UV units)
        let shear = if v2.1.abs() > 0.001 { v2.0 / v2.1 } else { 0.0 };

        // Ensure pattern texture exists in GPU cache
        let pat_key = format!("{}_pat", symbol_ref);
        if !self.pattern_textures.contains_key(&pat_key) {
            let geom = match symbol_cache.get_symbol_for_pattern(
                symbol_ref,
                color_profile,
                spacing_x_px,
                spacing_y_px,
                mm_to_px,
            ) {
                Some(g) => g,
                None => {
                    tracing::warn!("Pattern fill symbol '{}' not found", symbol_ref);
                    return;
                }
            };
            let tex_w = geom.width;
            let tex_h = geom.height;
            let (texture, view) = self.state.create_texture_from_rgba(
                &geom.pixels,
                tex_w,
                tex_h,
                &format!("pattern_{}", symbol_ref),
            );
            let bind_group = self
                .pipelines
                .create_pattern_bind_group(&self.state.device, &view);
            self.pattern_textures.insert(
                pat_key.clone(),
                PatternTexture {
                    texture,
                    bind_group,
                    width: tex_w,
                    height: tex_h,
                },
            );
        }

        // inv_tile_size: use actual texture pixel dimensions for seamless tiling
        let pat_tex = self.pattern_textures.get(&pat_key).unwrap();
        let inv_tx = 1.0 / pat_tex.width as f32;
        let inv_ty = 1.0 / pat_tex.height as f32;

        // Triangulate the polygon (same approach as add_area_cached)
        let screen_points: Vec<(f32, f32)> = area
            .exterior
            .iter()
            .map(|p| {
                let s = scaler.world_to_screen(*p);
                (s.x, s.y)
            })
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .collect();

        if screen_points.len() < 3 {
            return;
        }

        // Build earcut input
        let mut coords: Vec<f64> = Vec::with_capacity(screen_points.len() * 2);
        for &(x, y) in &screen_points {
            coords.push(x as f64);
            coords.push(y as f64);
        }

        let mut hole_indices: Vec<usize> = Vec::new();
        for hole in &area.interiors {
            let hole_screen: Vec<(f32, f32)> = hole
                .iter()
                .map(|p| {
                    let s = scaler.world_to_screen(*p);
                    (s.x, s.y)
                })
                .filter(|(x, y)| x.is_finite() && y.is_finite())
                .collect();
            if hole_screen.len() >= 3 {
                hole_indices.push(coords.len() / 2);
                for &(x, y) in &hole_screen {
                    coords.push(x as f64);
                    coords.push(y as f64);
                }
            }
        }

        let indices = earcutr::earcut(&coords, &hole_indices, 2).unwrap_or_default();

        if indices.is_empty() {
            return;
        }

        // S-100: parallelogram shear ratio (dimensionless).
        // shear = v2.x / v2.y: for each pixel of Y movement, X shifts by shear pixels.
        // The shader computes: u = (pos.x - shear * pos.y) * inv_tx
        let shear_screen = shear;
        let base_index = self.pattern_vertices.len() as u32;
        let total_points = coords.len() / 2;
        for i in 0..total_points {
            let x = coords[i * 2] as f32;
            let y = coords[i * 2 + 1] as f32;
            self.pattern_vertices
                .push(PatternVertex::new(x, y, inv_tx, inv_ty, shear_screen));
        }

        let idx_start = self.pattern_indices.len();
        for idx in &indices {
            self.pattern_indices.push(base_index + *idx as u32);
        }
        let idx_end = self.pattern_indices.len();

        let plane = match area.display_plane {
            ferrite_render::DisplayPlane::OverRadar => 1u8,
            _ => 0u8,
        };
        self.pattern_ranges
            .push((plane, priority, idx_start, idx_end, pat_key));
    }

    /// S-100 Part 9a hatch fill: render parallel lines inside a polygon area.
    /// Lines are drawn at the specified angle, spacing, and width within the
    /// polygon boundary using line-polygon clipping.
    #[allow(clippy::too_many_arguments)]
    fn tile_area_with_hatch(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        color: Color,
        width: f32,
        spacing_mm: f32,
        angle_deg: f32,
        scaler: &ferrite_render::Scaler,
        _priority: i32,
    ) {
        // Frustum culling: quick AABB check on exterior ring
        if !Self::is_ring_visible_static(&area.exterior, self.viewport_world_bounds) {
            return;
        }

        let dpi_scale = self.state.window.scale_factor() as f32;
        let spacing_px = (spacing_mm * SCREEN_PX_PER_MM * dpi_scale).max(2.0);
        let line_width = (width * SCREEN_PX_PER_MM * dpi_scale).max(0.5);
        let color_arr = color.to_array();

        // Convert polygon exterior to screen coordinates
        let screen_ring: Vec<(f32, f32)> = area
            .exterior
            .iter()
            .map(|p| {
                let s = scaler.world_to_screen(*p);
                (s.x, s.y)
            })
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .collect();

        if screen_ring.len() < 3 {
            return;
        }

        // Compute bounding box
        let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
        let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
        for &(x, y) in &screen_ring {
            if x < min_x {
                min_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if x > max_x {
                max_x = x;
            }
            if y > max_y {
                max_y = y;
            }
        }

        // Angle in radians (S-100: 0 = horizontal, CCW positive)
        let angle_rad = angle_deg.to_radians();
        let cos_a = angle_rad.cos();
        let sin_a = angle_rad.sin();

        // Direction perpendicular to the hatch lines (used for spacing)
        let perp_x = -sin_a;
        let perp_y = cos_a;

        // Project bounding box corners onto the perpendicular axis to find range
        let corners = [
            (min_x, min_y),
            (max_x, min_y),
            (max_x, max_y),
            (min_x, max_y),
        ];
        let mut proj_min = f32::MAX;
        let mut proj_max = f32::MIN;
        for &(cx, cy) in &corners {
            let proj = cx * perp_x + cy * perp_y;
            if proj < proj_min {
                proj_min = proj;
            }
            if proj > proj_max {
                proj_max = proj;
            }
        }

        // Diagonal length for extending lines across the entire bounding box
        let diag = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2)).sqrt();

        // Generate hatch lines at regular spacing
        let mut d = proj_min;
        while d <= proj_max {
            // Line center point on the perpendicular axis
            let cx = perp_x * d;
            let cy = perp_y * d;

            // Line endpoints extending in the hatch direction across the bbox
            let lx0 = cx - cos_a * diag;
            let ly0 = cy - sin_a * diag;
            let lx1 = cx + cos_a * diag;
            let ly1 = cy + sin_a * diag;

            // Clip this line segment to the polygon using intersection tests
            let segments = Self::clip_line_to_polygon(lx0, ly0, lx1, ly1, &screen_ring);
            for (sx, sy, ex, ey) in segments {
                // Render as a line quad (same approach as add_line)
                let ldx = ex - sx;
                let ldy = ey - sy;
                let len = (ldx * ldx + ldy * ldy).sqrt();
                if len < 0.001 {
                    continue;
                }
                let nx = -ldy / len * line_width * 0.5;
                let ny = ldx / len * line_width * 0.5;

                let base_index = self.line_vertices.len() as u32;
                self.line_vertices
                    .push(Vertex2D::new(sx - nx, sy - ny, color_arr));
                self.line_vertices
                    .push(Vertex2D::new(sx + nx, sy + ny, color_arr));
                self.line_vertices
                    .push(Vertex2D::new(ex + nx, ey + ny, color_arr));
                self.line_vertices
                    .push(Vertex2D::new(ex - nx, ey - ny, color_arr));

                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 1);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index + 3);
            }

            d += spacing_px;
        }
    }

    /// Clip a line segment to a polygon, returning visible sub-segments.
    /// Uses scanline intersection: find all intersection points of the line
    /// with polygon edges, sort them along the line, then emit inside segments.
    fn clip_line_to_polygon(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        ring: &[(f32, f32)],
    ) -> Vec<(f32, f32, f32, f32)> {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let line_len_sq = dx * dx + dy * dy;
        if line_len_sq < 1e-10 {
            return Vec::new();
        }

        // Find parametric t values where line intersects each polygon edge
        let mut t_values: Vec<f32> = Vec::new();
        let n = ring.len();
        for i in 0..n {
            let j = (i + 1) % n;
            let (ex0, ey0) = ring[i];
            let (ex1, ey1) = ring[j];

            let edx = ex1 - ex0;
            let edy = ey1 - ey0;

            let denom = dx * edy - dy * edx;
            if denom.abs() < 1e-10 {
                continue; // Parallel
            }

            let t = ((ex0 - x0) * edy - (ey0 - y0) * edx) / denom;
            let u = ((ex0 - x0) * dy - (ey0 - y0) * dx) / denom;

            if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&t) {
                t_values.push(t);
            }
        }

        if t_values.is_empty() {
            // Line might be entirely inside or outside
            let mid_x = (x0 + x1) * 0.5;
            let mid_y = (y0 + y1) * 0.5;
            if point_in_ring(mid_x, mid_y, ring) {
                return vec![(x0, y0, x1, y1)];
            }
            return Vec::new();
        }

        t_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Remove near-duplicates
        t_values.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

        // Emit segments between consecutive intersection pairs that are inside
        let mut segments = Vec::new();
        let start_inside = point_in_ring(x0, y0, ring);

        let mut prev_t = 0.0_f32;
        let mut inside = start_inside;

        for &t in &t_values {
            if inside {
                let seg_x0 = x0 + prev_t * dx;
                let seg_y0 = y0 + prev_t * dy;
                let seg_x1 = x0 + t * dx;
                let seg_y1 = y0 + t * dy;
                segments.push((seg_x0, seg_y0, seg_x1, seg_y1));
            }
            inside = !inside;
            prev_t = t;
        }

        // Handle remaining segment to end
        if inside {
            let seg_x0 = x0 + prev_t * dx;
            let seg_y0 = y0 + prev_t * dy;
            segments.push((seg_x0, seg_y0, x1, y1));
        }

        segments
    }

    /// Compute line suppression set (S-100 Part 9-11.1.9).
    /// When multiple features share the same curve geometry, only the
    /// highest-priority LineInstruction is rendered.
    fn compute_line_suppression(instructions: &[DrawingInstruction]) -> FxHashSet<usize> {
        let mut curve_max_priority: FxHashMap<u64, i32> = FxHashMap::default();
        let mut line_entries: Vec<(usize, u64, i32)> = Vec::new();
        let mut has_suppressible = false;

        for (idx, inst) in instructions.iter().enumerate() {
            if let DrawingInstruction::Line(line) = inst {
                if line.points.len() < 2 {
                    continue;
                }
                let key = Self::curve_geometry_hash(&line.points);
                let priority = line.priority.0;
                let entry = curve_max_priority.entry(key).or_insert(priority);
                if priority > *entry {
                    *entry = priority;
                }
                if line.suppressible {
                    line_entries.push((idx, key, priority));
                    has_suppressible = true;
                }
            }
        }

        if has_suppressible {
            let mut suppressed = FxHashSet::default();
            for &(idx, key, priority) in &line_entries {
                if let Some(&max_pri) = curve_max_priority.get(&key) {
                    if priority < max_pri {
                        suppressed.insert(idx);
                    }
                }
            }
            suppressed
        } else {
            FxHashSet::default()
        }
    }

    /// Compute a hash of curve geometry for S-100 line suppression.
    /// Two line instructions referencing the same spatial curve will have
    /// identical world-coordinate point sequences and thus the same hash.
    fn curve_geometry_hash(points: &[WorldPoint]) -> u64 {
        let mut hasher = rustc_hash::FxHasher::default();
        for p in points {
            p.x.to_bits().hash(&mut hasher);
            p.y.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Add line instruction
    /// Cohen-Sutherland outcode for line clipping
    #[inline]
    fn cs_outcode(x: f32, y: f32, x_min: f32, y_min: f32, x_max: f32, y_max: f32) -> u8 {
        let mut code = 0u8;
        if x < x_min {
            code |= 1;
        }
        // LEFT
        else if x > x_max {
            code |= 2;
        } // RIGHT
        if y < y_min {
            code |= 4;
        }
        // TOP
        else if y > y_max {
            code |= 8;
        } // BOTTOM
        code
    }

    /// Clip a line segment to a rectangle using Cohen-Sutherland.
    /// Returns Some((x0,y0,x1,y1)) if any portion is visible, None if fully outside.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn clip_line_segment(
        mut x0: f32,
        mut y0: f32,
        mut x1: f32,
        mut y1: f32,
        x_min: f32,
        y_min: f32,
        x_max: f32,
        y_max: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        let mut code0 = Self::cs_outcode(x0, y0, x_min, y_min, x_max, y_max);
        let mut code1 = Self::cs_outcode(x1, y1, x_min, y_min, x_max, y_max);

        loop {
            if (code0 | code1) == 0 {
                // Both inside
                return Some((x0, y0, x1, y1));
            }
            if (code0 & code1) != 0 {
                // Both on same outside side
                return None;
            }
            // Pick the point that is outside
            let code_out = if code0 != 0 { code0 } else { code1 };
            let (x, y);
            if code_out & 8 != 0 {
                // Below
                x = x0 + (x1 - x0) * (y_max - y0) / (y1 - y0);
                y = y_max;
            } else if code_out & 4 != 0 {
                // Above
                x = x0 + (x1 - x0) * (y_min - y0) / (y1 - y0);
                y = y_min;
            } else if code_out & 2 != 0 {
                // Right
                y = y0 + (y1 - y0) * (x_max - x0) / (x1 - x0);
                x = x_max;
            } else {
                // Left
                y = y0 + (y1 - y0) * (x_min - x0) / (x1 - x0);
                x = x_min;
            }
            if code_out == code0 {
                x0 = x;
                y0 = y;
                code0 = Self::cs_outcode(x0, y0, x_min, y_min, x_max, y_max);
            } else {
                x1 = x;
                y1 = y;
                code1 = Self::cs_outcode(x1, y1, x_min, y_min, x_max, y_max);
            }
        }
    }

    fn add_line(
        &mut self,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
    ) {
        let points = &line.points;
        if points.len() < 2 {
            return;
        }

        // Frustum culling: compute world AABB and skip if entirely off-screen
        {
            let mut ax = f64::MAX;
            let mut ay = f64::MAX;
            let mut bx = f64::MIN;
            let mut by = f64::MIN;
            for p in points.iter() {
                if p.x < ax {
                    ax = p.x;
                }
                if p.y < ay {
                    ay = p.y;
                }
                if p.x > bx {
                    bx = p.x;
                }
                if p.y > by {
                    by = p.y;
                }
            }
            if !self.is_aabb_visible(ax, ay, bx, by) {
                return;
            }
        }

        let color = line.style.color.to_array();
        let width = line.style.width;

        // Screen-space clip bounds with generous margin for line width
        let vw = scaler.viewport.width;
        let vh = scaler.viewport.height;
        let margin = width * 2.0 + 50.0; // extra margin for thick lines
        let clip_x_min = -margin;
        let clip_y_min = -margin;
        let clip_x_max = vw + margin;
        let clip_y_max = vh + margin;

        // Direct iteration: no Vec<ScreenPoint> allocation.
        // Transform consecutive world points to screen, clip, and emit quads inline.
        let mut prev = scaler.world_to_screen(points[0]);
        for p in &points[1..] {
            let curr = scaler.world_to_screen(*p);

            // Skip segments with NaN/Inf coordinates
            if !prev.x.is_finite()
                || !prev.y.is_finite()
                || !curr.x.is_finite()
                || !curr.y.is_finite()
            {
                prev = curr;
                continue;
            }

            // Clip line segment to screen bounds to prevent GPU precision issues
            // with extreme off-screen coordinates (ray artifacts at high zoom)
            if let Some((cx0, cy0, cx1, cy1)) = Self::clip_line_segment(
                prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max, clip_y_max,
            ) {
                let dx = cx1 - cx0;
                let dy = cy1 - cy0;
                let len = (dx * dx + dy * dy).sqrt();

                if len >= 0.001 {
                    let nx = -dy / len * width * 0.5;
                    let ny = dx / len * width * 0.5;

                    let base_index = self.line_vertices.len() as u32;

                    self.line_vertices
                        .push(Vertex2D::new(cx0 - nx, cy0 - ny, color));
                    self.line_vertices
                        .push(Vertex2D::new(cx0 + nx, cy0 + ny, color));
                    self.line_vertices
                        .push(Vertex2D::new(cx1 + nx, cy1 + ny, color));
                    self.line_vertices
                        .push(Vertex2D::new(cx1 - nx, cy1 - ny, color));

                    self.line_indices.push(base_index);
                    self.line_indices.push(base_index + 1);
                    self.line_indices.push(base_index + 2);
                    self.line_indices.push(base_index);
                    self.line_indices.push(base_index + 2);
                    self.line_indices.push(base_index + 3);
                }
            }

            prev = curr;
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

        // === STAGE 2: World-coordinate-based decluttering ===
        // Uses world coordinates divided by pixel-equivalent cell sizes for stable grids.
        // Unlike screen-space grids, world-coordinate grids produce identical results
        // regardless of pan offset, eliminating symbol pop-in/pop-out during drag.
        //
        // Cell sizes are computed as: screen_cell_size_px / scale_factor
        // This gives the same visual density as screen-space but is pan-stable.

        // Classify symbol types (using original string for pattern matching)
        let is_nav_aid = symbol_str.starts_with("LIGHTS")
            || symbol_str.starts_with("BUOY")
            || symbol_str.starts_with("BCN")
            || symbol_str.starts_with("TOPMAR");

        let is_safety_hazard = symbol_str == "ISODGR01"
            || symbol_str == "DANGER02"
            || symbol_str == "DANGER01"
            || symbol_str == "DANGER03"
            || symbol_str.starts_with("WRECKS")
            || symbol_str.starts_with("OBSTRN")
            || symbol_str.starts_with("UWTROC")
            || symbol_str.starts_with("FOULAR");
        let is_sounding = symbol_str.starts_with("SOUND");

        // Compute world-space cell sizes from screen-space pixel sizes
        let scale_x = scaler.scale_x().abs();
        let scale_y = scaler.scale_y().abs();

        // Safety hazard symbols are NEVER decluttered.
        // S-100 does not define symbol decluttering (only sounding collision via champion).
        // Hiding safety symbols (wrecks, obstructions, dangers) would violate navigation safety.
        // Scale-dependent visibility is handled by ScaleMinimum/ScaleMaximum from Lua rules.

        // Skip decluttering during animation to prevent symbols from disappearing
        // Only world-coordinate deduplication (Stage 1) applies during drag/inertia
        if !is_safety_hazard && !self.skip_screen_declutter && scale_x > 1e-10 && scale_y > 1e-10 {
            // Soundings: S-100 collision avoidance (champion = shallowest wins for safety)
            // Two-stage approach:
            // 1. sounding_exact_positions: tracks exact world positions to allow all digits of same sounding
            // 2. sounding_screen_grid: world-based grid to filter out visually nearby soundings
            // At high zoom (cell_size == 0), skip grid filtering and show all soundings
            if is_sounding && self.sounding_cell_size_px > 0.1 {
                // World-space key for exact position (all digits of one sounding share this)
                let exact_key = (world_x_key, world_y_key);

                // Check if we've already allowed a sounding at this exact world position
                if self.sounding_exact_positions.contains(&exact_key) {
                    // This is another digit of an already-allowed sounding - let it through
                    // (skip grid check)
                } else {
                    // First time seeing this exact position - check world-based grid
                    let world_cell_x = self.sounding_cell_size_px as f64 / scale_x;
                    let world_cell_y = self.sounding_cell_size_px as f64 / scale_y;
                    let sounding_grid_x = (point.position.x / world_cell_x).floor() as i32;
                    let sounding_grid_y = (point.position.y / world_cell_y).floor() as i32;
                    let sounding_grid_key = (sounding_grid_x, sounding_grid_y);

                    // Get current sounding's depth (default to MAX if not set)
                    let current_depth = point.depth().unwrap_or(f64::MAX);

                    if let Some(&(old_exact_key, old_depth)) =
                        self.sounding_screen_grid.get(&sounding_grid_key)
                    {
                        // Another sounding already claimed this cell
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
            // Non-safety, non-sounding symbols: visual declutter (non-standard optimization)
            else if !is_sounding {
                let effective_cell_size = if is_nav_aid {
                    if self.zoom_level >= 5.0 {
                        15.0
                    } else {
                        self.grid_cell_size
                    }
                } else {
                    self.grid_cell_size
                };

                let world_cell_x = effective_cell_size as f64 / scale_x;
                let world_cell_y = effective_cell_size as f64 / scale_y;
                let grid_x = (point.position.x / world_cell_x).floor() as i32;
                let grid_y = (point.position.y / world_cell_y).floor() as i32;
                let grid_key = (grid_x, grid_y);

                if self.symbol_grid.contains(&grid_key) {
                    return true;
                }
                self.symbol_grid.insert(grid_key);
            }
        }

        // Add symbol instance for rendering (uses interned SymbolId - 4 bytes vs 24+ for String)
        self.symbol_instances.push(SymbolInstance {
            symbol_id,
            screen_x: screen.x,
            screen_y: screen.y,
            scale: point.scale,
            // S-100 geographic CRS: rotation is clockwise from north (0°=up).
            // Renderer rotation matrix is clockwise from +X (east) in screen-space (Y-down).
            // Conversion: screen_angle = geo_angle - 90°
            rotation: if point.rotation != 0.0 {
                point.rotation - 90.0
            } else {
                0.0
            },
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
        let profiling = crate::profiler::is_profiling_enabled();
        let render_timer = if profiling {
            Some(ScopeTimer::new("render_total"))
        } else {
            None
        };

        let output = self.state.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let egui_timer = if profiling {
            Some(ScopeTimer::new("render_egui"))
        } else {
            None
        };
        // Begin egui frame
        self.egui.begin_frame(&self.state.window);

        // Draw egui UI
        self.egui.draw_ui(&mut self.ui_state);

        // Paint chart text labels via egui
        // Apply the same GPU pan/zoom offset so text tracks with chart geometry during drag
        if !self.text_labels.is_empty() {
            let pan_x = self.screen_pan_offset.0;
            let pan_y = self.screen_pan_offset.1;
            let zoom = self.screen_zoom_scale;
            let (pivot_x, pivot_y) = self.screen_zoom_pivot;

            let painter = self.egui.ctx.layer_painter(egui::LayerId::background());
            for label in &self.text_labels {
                let color = egui::Color32::from_rgba_unmultiplied(
                    (label.color[0] * 255.0) as u8,
                    (label.color[1] * 255.0) as u8,
                    (label.color[2] * 255.0) as u8,
                    (label.color[3] * 255.0) as u8,
                );

                // Build a LayoutJob to support bold/italic font variants
                let mut job = egui::text::LayoutJob::single_section(
                    label.text.clone(),
                    egui::TextFormat {
                        font_id: egui::FontId {
                            size: label.font_size * zoom,
                            family: egui::FontFamily::Proportional,
                        },
                        color,
                        italics: label.italic,
                        ..Default::default()
                    },
                );
                job.wrap = egui::text::TextWrapping {
                    max_rows: 1,
                    break_anywhere: false,
                    ..Default::default()
                };

                let galley = painter.layout_job(job);
                let text_width = galley.rect.width();
                let text_height = galley.rect.height();

                // Transform label position: pan, then zoom around pivot (same as GPU shader)
                let sx = label.screen_x + pan_x;
                let sy = label.screen_y + pan_y;
                let sx = (sx - pivot_x) * zoom + pivot_x;
                let sy = (sy - pivot_y) * zoom + pivot_y;

                // Apply horizontal alignment
                let x = match label.h_align {
                    ferrite_render::HAlign::Left => sx,
                    ferrite_render::HAlign::Center => sx - text_width * 0.5,
                    ferrite_render::HAlign::Right => sx - text_width,
                };

                // Apply vertical alignment
                let y = match label.v_align {
                    ferrite_render::VAlign::Top => sy,
                    ferrite_render::VAlign::Middle => sy - text_height * 0.5,
                    ferrite_render::VAlign::Bottom => sy - text_height,
                };

                painter.galley(egui::pos2(x, y), galley, color);
            }
        }

        // End egui frame and get output
        let egui_output = self.egui.end_frame(&self.state.window);
        if let Some(t) = egui_timer {
            self.cpu_profiler.record("render_egui", t.elapsed());
        }

        let mut encoder =
            self.state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("render_encoder"),
                });

        // Rebuild GPU buffers only when geometry has changed (dirty flag)
        // During GPU-only pan/zoom, we reuse the cached buffers.
        let gpu_buf_timer = if profiling {
            Some(ScopeTimer::new("gpu_buffer_create"))
        } else {
            None
        };
        if self.gpu_buffers_dirty {
            self.cached_area_vb = if !self.area_vertices.is_empty() {
                Some(
                    self.state
                        .create_vertex_buffer(&self.area_vertices, "area_vertices"),
                )
            } else {
                None
            };
            self.cached_area_ib = if !self.area_indices.is_empty() {
                self.cached_area_index_count = self.area_indices.len() as u32;
                Some(
                    self.state
                        .create_index_buffer(&self.area_indices, "area_indices"),
                )
            } else {
                self.cached_area_index_count = 0;
                None
            };

            self.cached_line_vb = if !self.line_vertices.is_empty() {
                Some(
                    self.state
                        .create_vertex_buffer(&self.line_vertices, "line_vertices"),
                )
            } else {
                None
            };
            self.cached_line_ib = if !self.line_indices.is_empty() {
                self.cached_line_index_count = self.line_indices.len() as u32;
                Some(
                    self.state
                        .create_index_buffer(&self.line_indices, "line_indices"),
                )
            } else {
                self.cached_line_index_count = 0;
                None
            };

            self.cached_pattern_vb = if !self.pattern_vertices.is_empty() {
                Some(
                    self.state
                        .create_vertex_buffer(&self.pattern_vertices, "pattern_vertices"),
                )
            } else {
                None
            };
            self.cached_pattern_ib = if !self.pattern_indices.is_empty() {
                self.cached_pattern_index_count = self.pattern_indices.len() as u32;
                Some(
                    self.state
                        .create_index_buffer(&self.pattern_indices, "pattern_indices"),
                )
            } else {
                self.cached_pattern_index_count = 0;
                None
            };

            self.gpu_buffers_dirty = false;
        }
        if let Some(t) = gpu_buf_timer {
            self.cpu_profiler.record("gpu_buffer_create", t.elapsed());
        }

        // NOTE: Symbol batches are now built per-priority in the render loop below
        // for S-101 compliant priority-based rendering

        let bg = self.background_color.to_array();

        // Use MSAA texture as render target if available, resolve to surface
        let (target_view, resolve_target) = if let Some(ref msaa_view) = self.state.msaa_view {
            (msaa_view, Some(&view))
        } else {
            (&view, None)
        };

        let render_pass_timer = if profiling {
            Some(ScopeTimer::new("render_pass"))
        } else {
            None
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
            let mut priority_set = FxHashSet::default();
            for &(plane, pri, _, _) in &self.area_priority_ranges {
                priority_set.insert((plane, pri));
            }
            for &(plane, pri, _, _) in &self.line_priority_ranges {
                priority_set.insert((plane, pri));
            }
            for &(plane, pri, _, _) in &self.symbol_priority_ranges {
                priority_set.insert((plane, pri));
            }
            for &(plane, pri, _, _, _) in &self.pattern_ranges {
                priority_set.insert((plane, pri));
            }
            let mut all_priorities: Vec<(u8, i32)> = priority_set.into_iter().collect();
            all_priorities.sort_unstable();

            // Clone symbol priority ranges to avoid borrow conflict with pack_symbol_batch_range
            let sym_priority_ranges = self.symbol_priority_ranges.to_vec();

            // Render by priority groups (display_plane, priority)
            for &(plane, priority) in &all_priorities {
                // Render areas for this priority
                if let (Some(vb), Some(ib)) = (&self.cached_area_vb, &self.cached_area_ib) {
                    for &(pl, pri, start, end) in &self.area_priority_ranges {
                        if pl == plane && pri == priority && end > start {
                            render_pass.set_pipeline(&self.pipelines.area_pipeline);
                            render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                            render_pass.set_vertex_buffer(0, vb.slice(..));
                            render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                            render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                        }
                    }
                }

                // Render pattern fills for this priority (GPU texture-repeat tiling)
                if let (Some(vb), Some(ib)) = (&self.cached_pattern_vb, &self.cached_pattern_ib) {
                    for (pl, pri, start, end, pat_key) in &self.pattern_ranges {
                        if *pl == plane && *pri == priority && end > start {
                            if let Some(pat_tex) = self.pattern_textures.get(pat_key) {
                                render_pass.set_pipeline(&self.pipelines.pattern_fill_pipeline);
                                render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                                render_pass.set_bind_group(1, &pat_tex.bind_group, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(*start as u32..*end as u32, 0, 0..1);
                            }
                        }
                    }
                }

                // Render lines for this priority
                if let (Some(vb), Some(ib)) = (&self.cached_line_vb, &self.cached_line_ib) {
                    for &(pl, pri, start, end) in &self.line_priority_ranges {
                        if pl == plane && pri == priority && end > start {
                            render_pass.set_pipeline(&self.pipelines.line_pipeline);
                            render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                            render_pass.set_vertex_buffer(0, vb.slice(..));
                            render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                            render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                        }
                    }
                }

                // Render symbols for this priority (cached packed single-buffer approach)
                for &(pl, pri, start, end) in &sym_priority_ranges {
                    if pl == plane && pri == priority && end > start {
                        // Find or build cached symbol buffer for this priority range
                        let cache_idx = self.cached_symbol_buffers.iter().position(
                            |(cp, cpr, cs, ce, _, _, _)| {
                                *cp == pl && *cpr == pri && *cs == start && *ce == end
                            },
                        );
                        let buf_idx = if let Some(idx) = cache_idx {
                            idx
                        } else {
                            // Build and cache
                            self.pack_symbol_batch_range(start, end);
                            if self.packed_symbol_indices.is_empty() {
                                continue;
                            }
                            let sym_vb = self.state.create_vertex_buffer(
                                &self.packed_symbol_vertices,
                                "symbol_packed_vb",
                            );
                            let sym_ib = self.state.create_index_buffer(
                                &self.packed_symbol_indices,
                                "symbol_packed_ib",
                            );
                            let ranges: Vec<_> = self.packed_symbol_ranges.clone();
                            self.cached_symbol_buffers
                                .push((pl, pri, start, end, sym_vb, sym_ib, ranges));
                            self.cached_symbol_buffers.len() - 1
                        };

                        let (_, _, _, _, ref sym_vb, ref sym_ib, ref ranges) =
                            self.cached_symbol_buffers[buf_idx];

                        render_pass.set_pipeline(&self.pipelines.texture_pipeline);
                        render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                        render_pass.set_vertex_buffer(0, sym_vb.slice(..));
                        render_pass.set_index_buffer(sym_ib.slice(..), wgpu::IndexFormat::Uint32);

                        for &(sym_id, idx_start, idx_count) in ranges {
                            if let Some(tex) = self.symbol_textures.get(&sym_id) {
                                render_pass.set_bind_group(1, &tex.bind_group, &[]);
                                render_pass.draw_indexed(idx_start..idx_start + idx_count, 0, 0..1);
                            }
                        }
                    }
                }
            }
        }

        if let Some(t) = render_pass_timer {
            self.cpu_profiler.record("render_pass", t.elapsed());
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

        // GPU profiler: resolve queries before submit
        if self.gpu_profiler.is_enabled() {
            self.gpu_profiler.profiler.resolve_queries(&mut encoder);
        }

        let submit_timer = if profiling {
            Some(ScopeTimer::new("queue_submit"))
        } else {
            None
        };
        self.state.queue.submit(std::iter::once(encoder.finish()));
        if let Some(t) = submit_timer {
            self.cpu_profiler.record("queue_submit", t.elapsed());
        }

        // GPU profiler: end frame and process results
        if self.gpu_profiler.is_enabled() {
            let _ = self.gpu_profiler.profiler.end_frame();
            self.gpu_profiler.process_and_log(&self.state.queue);
        }

        output.present();

        if let Some(t) = render_timer {
            self.cpu_profiler.record("render_total", t.elapsed());
        }

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

        // Pattern fill buffers (GPU texture-repeat tiling)
        let pattern_vertex_buffer = if !self.pattern_vertices.is_empty() {
            Some(
                self.state
                    .create_vertex_buffer(&self.pattern_vertices, "pattern_vertices"),
            )
        } else {
            None
        };
        let pattern_index_buffer = if !self.pattern_indices.is_empty() {
            Some(
                self.state
                    .create_index_buffer(&self.pattern_indices, "pattern_indices"),
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

            // Render pattern fills (GPU texture-repeat tiling)
            if let (Some(vb), Some(ib)) = (&pattern_vertex_buffer, &pattern_index_buffer) {
                for (_plane, _p, start, end, pat_key) in &self.pattern_ranges {
                    if end > start {
                        if let Some(pat_tex) = self.pattern_textures.get(pat_key) {
                            render_pass.set_pipeline(&self.pipelines.pattern_fill_pipeline);
                            render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                            render_pass.set_bind_group(1, &pat_tex.bind_group, &[]);
                            render_pass.set_vertex_buffer(0, vb.slice(..));
                            render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                            render_pass.draw_indexed(*start as u32..*end as u32, 0, 0..1);
                        }
                    }
                }
            }

            // Render lines
            if let (Some(vb), Some(ib)) = (&line_vertex_buffer, &line_index_buffer) {
                render_pass.set_pipeline(&self.pipelines.line_pipeline);
                render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                render_pass.set_vertex_buffer(0, vb.slice(..));
                render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..self.line_indices.len() as u32, 0, 0..1);
            }

            // Render symbols (packed single-buffer approach)
            if !self.symbol_instances.is_empty() {
                self.pack_symbol_batch_range(0, self.symbol_instances.len());

                if !self.packed_symbol_indices.is_empty() {
                    let sym_vb = self
                        .state
                        .create_vertex_buffer(&self.packed_symbol_vertices, "symbol_packed_vb");
                    let sym_ib = self
                        .state
                        .create_index_buffer(&self.packed_symbol_indices, "symbol_packed_ib");

                    render_pass.set_pipeline(&self.pipelines.texture_pipeline);
                    render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                    render_pass.set_vertex_buffer(0, sym_vb.slice(..));
                    render_pass.set_index_buffer(sym_ib.slice(..), wgpu::IndexFormat::Uint32);

                    for &(sym_id, idx_start, idx_count) in &self.packed_symbol_ranges {
                        if let Some(tex) = self.symbol_textures.get(&sym_id) {
                            render_pass.set_bind_group(1, &tex.bind_group, &[]);
                            render_pass.draw_indexed(idx_start..idx_start + idx_count, 0, 0..1);
                        }
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
