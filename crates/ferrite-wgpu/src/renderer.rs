//! Main wgpu Renderer
//!
//! Orchestrates rendering of drawing instructions to the screen.
//! Uses resvg for SVG symbol rendering via textures.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use winit::window::Window;
use winit::event::WindowEvent;

use ferrite_portrayal_catalog::ColorProfile;
use ferrite_render::{
    Color, DrawingInstruction, RenderContext, ScreenPoint,
};

use crate::{GpuState, RenderPipelines, Result, SymbolCache, Vertex2D, ViewUniforms, WgpuError};
use crate::pipeline::TextureVertex;
use crate::egui_integration::{EguiIntegration, AppUiState};

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
#[derive(Clone)]
struct SymbolInstance {
    symbol_id: String,
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
    /// GPU texture cache for symbols
    symbol_textures: HashMap<String, SymbolTexture>,
    /// Symbol instances to render
    symbol_instances: Vec<SymbolInstance>,
    /// Background color
    pub background_color: Color,
    /// Global symbol scale factor (default 1.0, reduce to make symbols smaller)
    pub symbol_scale: f32,
    /// Show sounding symbols (viewing group 33010)
    pub show_soundings: bool,
    /// Current zoom level (1.0 = default, higher = zoomed in)
    pub zoom_level: f64,
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
            symbol_scale: 0.35, // Default scale factor for symbols (reduce from 1.0 to make smaller)
            show_soundings: false, // Hide soundings by default (too dense when zoomed out)
            zoom_level: 1.0,
            symbol_grid: std::collections::HashSet::with_capacity(1000),
            sounding_screen_grid: std::collections::HashMap::with_capacity(2000),
            sounding_exact_positions: std::collections::HashSet::with_capacity(5000),
            world_dedup: std::collections::HashSet::with_capacity(5000),
            danger_grid: std::collections::HashSet::with_capacity(500),
            grid_cell_size: 30.0, // Default grid cell size in pixels
            sounding_cell_size_px: 150.0, // Fixed pixel spacing between soundings
            danger_cell_size_px: 80.0, // Danger symbols: smaller cell for higher density than soundings
            skip_screen_declutter: false,
            screen_pan_offset: (0.0, 0.0),
            egui,
            ui_state: AppUiState::default(),
        })
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

    /// Update view uniforms after resize or zoom
    fn update_view_uniforms(&self) {
        let (width, height) = self.state.viewport_size();
        let uniforms = ViewUniforms::with_pan(
            width, height, 1.0,
            self.screen_pan_offset.0, self.screen_pan_offset.1
        );
        self.state.update_view_uniforms(&self.view_buffer, &uniforms);
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

    /// Add drawing instructions from render context
    pub fn add_instructions(&mut self, context: &mut RenderContext) {
        self.add_instructions_with_symbols(context, None, None);
    }

    /// Add drawing instructions with symbol rendering support
    pub fn add_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
    ) {
        // Clone the instructions to avoid borrow issues
        let instructions: Vec<_> = context.get_sorted_instructions().to_vec();

        for instruction in &instructions {
            match instruction {
                DrawingInstruction::Area(area) => {
                    self.add_area(area, &context.scaler);
                }
                DrawingInstruction::Line(line) => {
                    self.add_line(line, &context.scaler);
                }
                DrawingInstruction::Point(point) => {
                    // Show soundings at zoom >= 3x (with world-space grid decluttering)
                    // At lower zoom levels, soundings are hidden unless explicitly enabled
                    let is_sounding = point.symbol_ref.starts_with("SOUNDG") || point.symbol_ref.starts_with("SOUNDS");
                    if is_sounding && !self.show_soundings && self.zoom_level < 3.0 {
                        continue;
                    }

                    // ISODGR01 (Isolated Danger) and DANGER02 only visible at high zoom levels (>= 5x)
                    if (point.symbol_ref == "ISODGR01" || point.symbol_ref == "DANGER02") && self.zoom_level < 5.0 {
                        continue;
                    }

                    // Try to render as symbol if cache is available
                    let rendered = if let (Some(cache), Some(profile)) = (symbol_cache.as_mut(), color_profile) {
                        self.try_add_symbol(point, &context.scaler, *cache, profile)
                    } else {
                        false
                    };

                    // Fallback to placeholder if symbol not found
                    if !rendered {
                        self.add_point_fallback(point, &context.scaler);
                    }
                }
                DrawingInstruction::Text(text) => {
                    // Text rendering requires separate handling (glyph atlas)
                    // For now, skip
                    tracing::trace!("Skipping text: {}", text.text);
                }
            }
        }
    }

    /// Add area instruction - uses earcut for proper concave polygon triangulation
    fn add_area(&mut self, area: &ferrite_render::AreaInstruction, scaler: &ferrite_render::Scaler) {
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
                self.area_vertices.push(Vertex2D::new(point.x, point.y, color));
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
        let valid_indices: Vec<usize> = indices
            .into_iter()
            .filter(|&idx| idx < max_idx)
            .collect();

        if valid_indices.len() < 3 || valid_indices.len() % 3 != 0 {
            // Fallback if indices are invalid
            let base_index = self.area_vertices.len() as u32;
            for point in &cleaned_points {
                self.area_vertices.push(Vertex2D::new(point.x, point.y, color));
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
            self.area_vertices.push(Vertex2D::new(point.x, point.y, color));
        }

        // Add triangle indices from earcut result
        for idx in valid_indices {
            self.area_indices.push(base_index + idx as u32);
        }
    }

    /// Add line instruction
    fn add_line(&mut self, line: &ferrite_render::LineInstruction, scaler: &ferrite_render::Scaler) {
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
            self.line_vertices.push(Vertex2D::new(p0.x - nx, p0.y - ny, color));
            self.line_vertices.push(Vertex2D::new(p0.x + nx, p0.y + ny, color));
            self.line_vertices.push(Vertex2D::new(p1.x + nx, p1.y + ny, color));
            self.line_vertices.push(Vertex2D::new(p1.x - nx, p1.y - ny, color));

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
        let symbol_id = &point.symbol_ref;
        if symbol_id.is_empty() {
            return false;
        }

        // Log first few symbol requests for debugging
        static SYMBOL_DEBUG_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let debug_idx = SYMBOL_DEBUG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if debug_idx < 20 {
            tracing::debug!("Symbol request [{}]: '{}'", debug_idx, symbol_id);
        }

        // Get symbol geometry from cache (this will render via resvg if not cached)
        // Use reference to avoid cloning the pixel buffer
        let geom = match symbol_cache.get_symbol(symbol_id, color_profile) {
            Some(g) => g,
            None => return false,
        };

        // Create GPU texture if not already cached
        if !self.symbol_textures.contains_key(symbol_id) {
            let (_texture, view) = self.state.create_texture_from_rgba(
                &geom.pixels,
                geom.width,
                geom.height,
                &format!("symbol_{}", symbol_id),
            );

            let bind_group = self.pipelines.create_texture_bind_group(&self.state.device, &view);

            let pivot_in_tex = geom.pivot_in_texture();
            self.symbol_textures.insert(symbol_id.clone(), SymbolTexture {
                texture: _texture,
                bind_group,
                width: geom.width,
                height: geom.height,
                pivot_in_texture: pivot_in_tex,
                render_scale: geom.render_scale,
            });

            tracing::debug!(
                "Created GPU texture for symbol '{}': {}x{}, pivot_in_tex: ({:.2}, {:.2})",
                symbol_id, geom.width, geom.height, pivot_in_tex.0, pivot_in_tex.1
            );
        }

        // Convert screen position
        let screen = scaler.world_to_screen(point.position);

        // === STAGE 1: World-coordinate deduplication ===
        // Remove exact duplicates from multiple charts at the same geographic position
        // Use high precision (6 decimal places ≈ 0.1 meter) for deduplication
        let world_x_key = (point.position.x * 1_000_000.0) as i64;
        let world_y_key = (point.position.y * 1_000_000.0) as i64;
        // Simple hash of symbol type to allow different symbol types at same position
        let symbol_hash = {
            let mut h: u64 = 0;
            for b in symbol_id.bytes().take(8) {
                h = h.wrapping_mul(31).wrapping_add(b as u64);
            }
            h
        };
        let world_key = (world_x_key, world_y_key, symbol_hash);

        if self.world_dedup.contains(&world_key) {
            // Exact duplicate from another chart - skip
            return true;
        }
        self.world_dedup.insert(world_key);

        // === STAGE 2: Screen-space decluttering ===
        // Classify symbol types
        let is_nav_aid = symbol_id.starts_with("LIGHTS")
            || symbol_id.starts_with("BUOY")
            || symbol_id.starts_with("BCN")
            || symbol_id.starts_with("TOPMAR");

        let is_low_priority = symbol_id == "ISODGR01" || symbol_id == "DANGER02";
        let is_sounding = symbol_id.starts_with("SOUND");

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
                    let current_depth = point.depth.unwrap_or(f64::MAX);

                    if let Some(&(old_exact_key, old_depth)) = self.sounding_screen_grid.get(&sounding_grid_key) {
                        // Another sounding already claimed this screen cell
                        // For SAFETY: keep the SHALLOWEST (lowest numerical depth) sounding
                        if current_depth < old_depth {
                            // This sounding is shallower - replace the old one
                            self.sounding_exact_positions.remove(&old_exact_key);
                            self.sounding_screen_grid.insert(sounding_grid_key, (exact_key, current_depth));
                            self.sounding_exact_positions.insert(exact_key);
                        } else {
                            // Existing sounding is shallower or equal - skip this one
                            return true;
                        }
                    } else {
                        // Cell is empty - this sounding claims it
                        self.sounding_screen_grid.insert(sounding_grid_key, (exact_key, current_depth));
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

        // Add symbol instance for rendering
        self.symbol_instances.push(SymbolInstance {
            symbol_id: symbol_id.clone(),
            screen_x: screen.x,
            screen_y: screen.y,
            scale: point.scale,
            rotation: point.rotation,
        });

        true
    }

    /// Fallback point rendering (small square) when symbol not available
    fn add_point_fallback(&mut self, point: &ferrite_render::PointInstruction, scaler: &ferrite_render::Scaler) {
        let screen = scaler.world_to_screen(point.position);
        let size = 4.0 * point.scale;
        let color = [1.0, 0.0, 0.0, 1.0]; // Red for missing symbols

        let base_index = self.area_vertices.len() as u32;

        // Small square
        self.area_vertices.push(Vertex2D::new(screen.x - size, screen.y - size, color));
        self.area_vertices.push(Vertex2D::new(screen.x + size, screen.y - size, color));
        self.area_vertices.push(Vertex2D::new(screen.x + size, screen.y + size, color));
        self.area_vertices.push(Vertex2D::new(screen.x - size, screen.y + size, color));

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
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Begin egui frame
        self.egui.begin_frame(&self.state.window);

        // Draw egui UI
        self.egui.draw_ui(&mut self.ui_state);

        // End egui frame and get output
        let egui_output = self.egui.end_frame(&self.state.window);

        let mut encoder = self.state.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("render_encoder"),
        });

        // Create buffers from collected vertices
        let area_vertex_buffer = if !self.area_vertices.is_empty() {
            Some(self.state.create_vertex_buffer(&self.area_vertices, "area_vertices"))
        } else {
            None
        };

        let area_index_buffer = if !self.area_indices.is_empty() {
            Some(self.state.create_index_buffer(&self.area_indices, "area_indices"))
        } else {
            None
        };

        let line_vertex_buffer = if !self.line_vertices.is_empty() {
            Some(self.state.create_vertex_buffer(&self.line_vertices, "line_vertices"))
        } else {
            None
        };

        let line_index_buffer = if !self.line_indices.is_empty() {
            Some(self.state.create_index_buffer(&self.line_indices, "line_indices"))
        } else {
            None
        };

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

            // Render areas first (they're usually background)
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

            // Render symbols as textured quads
            if !self.symbol_instances.is_empty() {
                render_pass.set_pipeline(&self.pipelines.texture_pipeline);
                render_pass.set_bind_group(0, &self.view_bind_group, &[]);

                // Iterate by index to avoid borrow conflicts with self.state
                for i in 0..self.symbol_instances.len() {
                    let instance = &self.symbol_instances[i];
                    if let Some(tex) = self.symbol_textures.get(&instance.symbol_id) {
                        // Calculate quad vertices
                        // The symbol was rendered at render_scale pixels per mm
                        // We need to display it at the correct size based on point.scale
                        let mm_to_px = 3.78; // 96 DPI
                        let display_scale = instance.scale * mm_to_px / tex.render_scale * self.symbol_scale;

                        let half_w = (tex.width as f32 * display_scale) / 2.0;
                        let half_h = (tex.height as f32 * display_scale) / 2.0;

                        // Pivot offset (in screen pixels)
                        // pivot_in_texture is in texture pixels (from top-left)
                        // display_scale converts texture pixels to screen pixels
                        let pivot_x = tex.pivot_in_texture.0 * display_scale;
                        let pivot_y = tex.pivot_in_texture.1 * display_scale;

                        // Debug: Log sounding symbol positions
                        static SOUNDING_DEBUG_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                        if instance.symbol_id.starts_with("SOUNDG") || instance.symbol_id.starts_with("SOUNDS") {
                            let debug_idx = SOUNDING_DEBUG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if debug_idx < 50 {
                                // Calculate actual screen bounds
                                let left = instance.screen_x - pivot_x;
                                let right = instance.screen_x + (tex.width as f32 * display_scale) - pivot_x;
                                tracing::info!(
                                    "Sounding [{}] '{}': screen=({:.1},{:.1}), pivot_x={:.2}, bounds=[{:.1}, {:.1}]",
                                    debug_idx, instance.symbol_id,
                                    instance.screen_x, instance.screen_y,
                                    pivot_x,
                                    left, right
                                );
                            }
                        }

                        // Rotation
                        let rotation = instance.rotation.to_radians();
                        let cos_r = rotation.cos();
                        let sin_r = rotation.sin();

                        // Transform corner positions with pivot and rotation
                        // The quad is centered at origin, but we need to shift it
                        // so that the pivot point aligns with the screen position
                        let transform = |dx: f32, dy: f32| -> (f32, f32) {
                            // Offset from top-left, then shift so pivot aligns with origin
                            let px = dx + half_w - pivot_x;
                            let py = dy + half_h - pivot_y;

                            // Rotate
                            let rx = px * cos_r - py * sin_r;
                            let ry = px * sin_r + py * cos_r;

                            // Translate to screen position
                            (instance.screen_x + rx, instance.screen_y + ry)
                        };

                        // Quad corners (before rotation, relative to center)
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

        tracing::debug!("Screenshot format: {:?}, MSAA samples: {}", screenshot_format, MSAA_SAMPLE_COUNT);

        // Calculate buffer dimensions (aligned to 256 bytes)
        let bytes_per_pixel = 4u32;
        let unpadded_bytes_per_row = width * bytes_per_pixel;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = (unpadded_bytes_per_row + align - 1) / align * align;
        let buffer_size = (padded_bytes_per_row * height) as u64;

        // Create output buffer
        let output_buffer = self.state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot_buffer"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self.state.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("screenshot_encoder"),
        });

        // Recreate buffers for rendering
        let area_vertex_buffer = if !self.area_vertices.is_empty() {
            Some(self.state.create_vertex_buffer(&self.area_vertices, "area_vertices"))
        } else {
            None
        };

        let area_index_buffer = if !self.area_indices.is_empty() {
            Some(self.state.create_index_buffer(&self.area_indices, "area_indices"))
        } else {
            None
        };

        let line_vertex_buffer = if !self.line_vertices.is_empty() {
            Some(self.state.create_vertex_buffer(&self.line_vertices, "line_vertices"))
        } else {
            None
        };

        let line_index_buffer = if !self.line_indices.is_empty() {
            Some(self.state.create_index_buffer(&self.line_indices, "line_indices"))
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
                        let mm_to_px = 3.78;
                        let display_scale = instance.scale * mm_to_px / tex.render_scale * self.symbol_scale;

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
