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
pub(super) const SCREEN_PX_PER_MM: f32 = 96.0 / 25.4;

// Note: S-100 symbol sizing works as follows:
// 1. SVG symbols have mm dimensions (e.g., ACHBRT07 = 5.38mm wide)
// 2. usvg converts mm → user units (px at 96 DPI): 5.38mm → 20.3 user units
// 3. Texture is rendered at tree_size × render_scale (7.56) → ~154px
// 4. To display at correct mm size: display_scale = 1.0 / render_scale
//    (which recovers the original user-unit size = physical mm size at 96 DPI)
// The formula is: display_scale = instance.scale / tex.render_scale * self.symbol_scale

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use winit::window::Window;

use crate::profiler::{CpuProfiler, GpuProfilerWrapper, ScopeTimer};
use ferrite_render::{Color, SymbolId};

use crate::egui_integration::{AppUiState, EguiIntegration};
use crate::pipeline::{PatternVertex, TextureVertex};
use crate::renderer_internals::{
    CachedTriangulation, PatternTexture, SymbolInstance, SymbolTexture, TextCollisionGrid,
    TextLabel,
};
use crate::{GpuState, RenderPipelines, Result, Vertex2D, ViewUniforms, WgpuError};

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
    /// Cached symbol classification flags (computed once per unique SymbolId, never cleared)
    symbol_class_cache: FxHashMap<SymbolId, u8>,
    /// Set of SymbolIds that could not be rendered (missing SVG, color profile, etc).
    /// Used to log each missing symbol exactly once instead of every frame, and to
    /// surface a "N symbols missing" count to the debug HUD/logs.
    missing_symbol_ids: FxHashSet<SymbolId>,
    /// Count of point instructions that arrived with an empty symbol_ref. Indicates a
    /// portrayal-rules bug (Lua emitted a Point without a symbol). Tracked but not
    /// rendered — silently swallowing this would hide chart-data quality issues.
    empty_symbol_ref_count: u32,
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
    // === BACKGROUND WORLD MAP (Natural Earth) ===
    /// Pre-parsed coastline segments from Natural Earth 110m GeoJSON
    /// Each inner Vec is a line string: list of [longitude, latitude] pairs
    world_map_coastlines: Vec<Vec<[f64; 2]>>,
    /// Bounding boxes of loaded chart cells (world coords).
    /// Used to draw opaque background rectangles that mask world map under charts.
    world_map_chart_boxes: Vec<(f64, f64, f64, f64)>,
    /// World map line vertices (separate from chart line_vertices)
    world_map_line_vertices: Vec<Vertex2D>,
    world_map_line_indices: Vec<u32>,
    /// Opaque background rectangles over chart bboxes (mask world map under charts)
    world_map_mask_vertices: Vec<Vertex2D>,
    world_map_mask_indices: Vec<u32>,
    /// Cached GPU buffers for world map
    cached_wm_line_vb: Option<wgpu::Buffer>,
    cached_wm_line_ib: Option<wgpu::Buffer>,
    cached_wm_mask_vb: Option<wgpu::Buffer>,
    cached_wm_mask_ib: Option<wgpu::Buffer>,
    // === LONGITUDE WRAPPING (infinite horizontal panning) ===
    /// Screen pixels corresponding to 360° of longitude (0 = wrapping disabled)
    lon_wrap_screen_px: f32,
    /// View uniform buffer for left (-360°) wrapping copy
    view_buffer_left: wgpu::Buffer,
    /// View uniform buffer for right (+360°) wrapping copy
    view_buffer_right: wgpu::Buffer,
    /// Bind group for left wrapping view
    view_bind_group_left: wgpu::BindGroup,
    /// Bind group for right wrapping view
    view_bind_group_right: wgpu::BindGroup,
}

mod drawing;
mod frame;
mod input;
mod visibility;
mod world_map;

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

        // Create wrapping view buffers for ±360° longitude copies
        let view_buffer_left = state.create_uniform_buffer(&uniforms, "view_uniforms_left");
        let view_bind_group_left =
            pipelines.create_view_bind_group(&state.device, &view_buffer_left);
        let view_buffer_right = state.create_uniform_buffer(&uniforms, "view_uniforms_right");
        let view_bind_group_right =
            pipelines.create_view_bind_group(&state.device, &view_buffer_right);

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
            background_color: Color::from_u8(201, 237, 255, 255), // DEPDW (deep water) — matches S-101 default
            symbol_scale: 1.0, // S-100 standard: 1.0 = nominal symbol size at 0.3mm/pixel
            show_soundings: true, // Visibility controlled by S-101 viewing groups
            zoom_level: 1.0,
            compilation_scale: 22000, // Default compilation scale (1:22000)
            symbol_grid: FxHashSet::with_capacity_and_hasher(1000, Default::default()),
            sounding_screen_grid: FxHashMap::with_capacity_and_hasher(2000, Default::default()),
            sounding_exact_positions: FxHashSet::with_capacity_and_hasher(5000, Default::default()),
            world_dedup: FxHashSet::with_capacity_and_hasher(5000, Default::default()),
            symbol_class_cache: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            missing_symbol_ids: FxHashSet::with_capacity_and_hasher(32, Default::default()),
            empty_symbol_ref_count: 0,

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
            // Background world map (empty until set_world_map is called)
            world_map_coastlines: Vec::new(),
            world_map_chart_boxes: Vec::new(),
            world_map_line_vertices: Vec::with_capacity(2000),
            world_map_line_indices: Vec::with_capacity(6000),
            world_map_mask_vertices: Vec::new(),
            world_map_mask_indices: Vec::new(),
            cached_wm_line_vb: None,
            cached_wm_line_ib: None,
            cached_wm_mask_vb: None,
            cached_wm_mask_ib: None,
            // Longitude wrapping
            lon_wrap_screen_px: 0.0,
            view_buffer_left,
            view_buffer_right,
            view_bind_group_left,
            view_bind_group_right,
        })
    }

    /// Render the frame
    pub fn render(&mut self) -> Result<()> {
        let profiling = crate::profiler::is_profiling_enabled();

        // get_current_texture includes VSync wait — measure separately
        let get_tex_timer = if profiling {
            Some(ScopeTimer::new("get_texture"))
        } else {
            None
        };
        let output = self.state.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        if let Some(t) = get_tex_timer {
            self.cpu_profiler.record("get_texture", t.elapsed());
        }

        // render_total starts AFTER VSync wait for accurate CPU render cost
        let render_timer = if profiling {
            Some(ScopeTimer::new("render_total"))
        } else {
            None
        };

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

            // World map separate GPU buffers
            self.cached_wm_line_vb = if !self.world_map_line_vertices.is_empty() {
                Some(
                    self.state
                        .create_vertex_buffer(&self.world_map_line_vertices, "wm_line_vb"),
                )
            } else {
                None
            };
            self.cached_wm_line_ib = if !self.world_map_line_indices.is_empty() {
                Some(
                    self.state
                        .create_index_buffer(&self.world_map_line_indices, "wm_line_ib"),
                )
            } else {
                None
            };
            self.cached_wm_mask_vb = if !self.world_map_mask_vertices.is_empty() {
                Some(
                    self.state
                        .create_vertex_buffer(&self.world_map_mask_vertices, "wm_mask_vb"),
                )
            } else {
                None
            };
            self.cached_wm_mask_ib = if !self.world_map_mask_indices.is_empty() {
                Some(
                    self.state
                        .create_index_buffer(&self.world_map_mask_indices, "wm_mask_ib"),
                )
            } else {
                None
            };

            self.gpu_buffers_dirty = false;
        }
        if let Some(t) = gpu_buf_timer {
            self.cpu_profiler.record("gpu_buffer_create", t.elapsed());
        }

        // Pre-build all symbol GPU buffers before the render pass.
        // This avoids mutable self borrows inside the render pass where view bind groups
        // are held as immutable references (for longitude wrapping multi-pass rendering).
        {
            let sym_ranges = self.symbol_priority_ranges.to_vec();
            for &(pl, pri, start, end) in &sym_ranges {
                if end <= start {
                    continue;
                }
                let already_cached =
                    self.cached_symbol_buffers
                        .iter()
                        .any(|(cp, cpr, cs, ce, _, _, _)| {
                            *cp == pl && *cpr == pri && *cs == start && *ce == end
                        });
                if already_cached {
                    continue;
                }
                self.pack_symbol_batch_range(start, end);
                if self.packed_symbol_indices.is_empty() {
                    continue;
                }
                let sym_vb = self
                    .state
                    .create_vertex_buffer(&self.packed_symbol_vertices, "symbol_packed_vb");
                let sym_ib = self
                    .state
                    .create_index_buffer(&self.packed_symbol_indices, "symbol_packed_ib");
                let ranges: Vec<_> = self.packed_symbol_ranges.clone();
                self.cached_symbol_buffers
                    .push((pl, pri, start, end, sym_vb, sym_ib, ranges));
            }
        }

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

            // === LAYER 1: World map coastlines (lowest layer) ===
            if let (Some(vb), Some(ib)) = (&self.cached_wm_line_vb, &self.cached_wm_line_ib) {
                let idx_count = self.world_map_line_indices.len() as u32;
                if idx_count > 0 {
                    render_pass.set_pipeline(&self.pipelines.line_pipeline);
                    render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                    render_pass.set_vertex_buffer(0, vb.slice(..));
                    render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..idx_count, 0, 0..1);
                }
            }

            // === LAYER 2: Opaque background rectangles over chart bboxes (mask coastlines) ===
            if let (Some(vb), Some(ib)) = (&self.cached_wm_mask_vb, &self.cached_wm_mask_ib) {
                let idx_count = self.world_map_mask_indices.len() as u32;
                if idx_count > 0 {
                    render_pass.set_pipeline(&self.pipelines.area_pipeline);
                    render_pass.set_bind_group(0, &self.view_bind_group, &[]);
                    render_pass.set_vertex_buffer(0, vb.slice(..));
                    render_pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..idx_count, 0, 0..1);
                }
            }

            // === LAYER 3: Chart data (S-101 priority-based rendering) ===
            // Drawn at center, left (-360°), and right (+360°) offsets for wrapping.
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

            // Number of wrapping passes: center (always) + left/right if wrapping
            let wrap_pass_count: u8 = if self.lon_wrap_screen_px > 0.0 { 3 } else { 1 };

            for wrap_pass in 0..wrap_pass_count {
                // Select view bind group for this pass: 0=center, 1=left, 2=right
                let view_bg = match wrap_pass {
                    1 => &self.view_bind_group_left,
                    2 => &self.view_bind_group_right,
                    _ => &self.view_bind_group,
                };

                // Render by priority groups (display_plane, priority)
                for &(plane, priority) in &all_priorities {
                    // Render areas for this priority
                    if let (Some(vb), Some(ib)) = (&self.cached_area_vb, &self.cached_area_ib) {
                        for &(pl, pri, start, end) in &self.area_priority_ranges {
                            if pl == plane && pri == priority && end > start {
                                render_pass.set_pipeline(&self.pipelines.area_pipeline);
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                            }
                        }
                    }

                    // Render pattern fills for this priority
                    if let (Some(vb), Some(ib)) = (&self.cached_pattern_vb, &self.cached_pattern_ib)
                    {
                        for (pl, pri, start, end, pat_key) in &self.pattern_ranges {
                            if *pl == plane && *pri == priority && end > start {
                                if let Some(pat_tex) = self.pattern_textures.get(pat_key) {
                                    render_pass.set_pipeline(&self.pipelines.pattern_fill_pipeline);
                                    render_pass.set_bind_group(0, view_bg, &[]);
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
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, vb.slice(..));
                                render_pass
                                    .set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                                render_pass.draw_indexed(start as u32..end as u32, 0, 0..1);
                            }
                        }
                    }

                    // Render symbols for this priority (pre-built GPU buffers)
                    for &(pl, pri, start, end) in &sym_priority_ranges {
                        if pl == plane && pri == priority && end > start {
                            // Find pre-built buffer (built before render pass)
                            let cache_idx = self.cached_symbol_buffers.iter().position(
                                |(cp, cpr, cs, ce, _, _, _)| {
                                    *cp == pl && *cpr == pri && *cs == start && *ce == end
                                },
                            );
                            if let Some(buf_idx) = cache_idx {
                                let (_, _, _, _, ref sym_vb, ref sym_ib, ref ranges) =
                                    self.cached_symbol_buffers[buf_idx];

                                render_pass.set_pipeline(&self.pipelines.texture_pipeline);
                                render_pass.set_bind_group(0, view_bg, &[]);
                                render_pass.set_vertex_buffer(0, sym_vb.slice(..));
                                render_pass
                                    .set_index_buffer(sym_ib.slice(..), wgpu::IndexFormat::Uint32);

                                for &(sym_id, idx_start, idx_count) in ranges {
                                    if let Some(tex) = self.symbol_textures.get(&sym_id) {
                                        render_pass.set_bind_group(1, &tex.bind_group, &[]);
                                        render_pass.draw_indexed(
                                            idx_start..idx_start + idx_count,
                                            0,
                                            0..1,
                                        );
                                    }
                                }
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

        let egui_render_timer = if profiling {
            Some(ScopeTimer::new("egui_gpu_render"))
        } else {
            None
        };
        self.egui.render(
            &self.state.device,
            &self.state.queue,
            &mut encoder,
            &view,
            screen_descriptor,
            egui_output,
        );
        if let Some(t) = egui_render_timer {
            self.cpu_profiler.record("egui_gpu_render", t.elapsed());
        }

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

        let present_timer = if profiling {
            Some(ScopeTimer::new("present"))
        } else {
            None
        };
        output.present();
        if let Some(t) = present_timer {
            self.cpu_profiler.record("present", t.elapsed());
        }

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

        // Create MSAA texture for rendering (only if MSAA is enabled)
        let msaa_texture = if MSAA_SAMPLE_COUNT > 1 {
            Some(self.state.device.create_texture(&wgpu::TextureDescriptor {
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
            }))
        } else {
            None
        };
        let msaa_view = msaa_texture
            .as_ref()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));

        // Create resolve/output texture (non-MSAA, COPY_SRC for screenshot)
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

        // Render to MSAA texture (resolve to output) or directly to output texture
        {
            let bg = self.background_color.to_array();
            let (target_view, resolve_target) = if let Some(ref mv) = msaa_view {
                (mv, Some(&resolve_view))
            } else {
                (&resolve_view, None)
            };
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("screenshot_render_pass"),
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
