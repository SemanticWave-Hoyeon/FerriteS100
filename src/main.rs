// Hide console window in release mode on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! FerriteS100 - Rust implementation of S-100/S-101 ENC viewer
//!
//! This application loads and parses S-101 Electronic Navigational Charts
//! using dynamically loaded Feature Catalogue (FC) and Portrayal Catalogue (PC).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use walkdir::WalkDir;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Icon, Window, WindowId},
};

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{
    ContextParameters as LuaContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue,
};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, GeoBounds, LineInstruction, LineStyle, PointInstruction, RenderContext,
    Viewport, WorldPoint,
};
use ferrite_s100_core::{S101Cell, SpatialPrimitiveType};
use ferrite_wgpu::{SelectedFeature, SymbolCache, WgpuRenderer};

/// Application configuration
struct AppConfig {
    /// Path to Feature Catalogue XML
    fc_path: PathBuf,
    /// Path to Portrayal Catalogue directory
    pc_path: PathBuf,
    /// Path to log directory
    log_path: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            fc_path: PathBuf::from("./catalogues/FC/101_Feature_Catalogue_2.0.0.xml"),
            pc_path: PathBuf::from("./catalogues/PC/PortrayalCatalog"),
            log_path: PathBuf::from("./logs"),
        }
    }
}

/// Rendered symbol info for hit testing
/// Fields ordered by size (largest first) for optimal memory layout
#[derive(Clone, Debug)]
struct RenderedSymbol {
    world_x: f64,
    world_y: f64,
    feature_id: i64,
    screen_x: f32,
    screen_y: f32,
    /// Drawing priority (higher = drawn on top, should be selected first)
    priority: i32,
    symbol_ref: String,
    /// Cell index this symbol belongs to (for correct feature lookup in multi-cell scenarios)
    cell_index: Option<usize>,
}

/// Chart viewer application for winit
struct ChartApp {
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    render_context: RenderContext,
    bounds: GeoBounds,
    /// Symbol cache for SVG symbol rendering
    symbol_cache: SymbolCache,
    /// Color profile from PC for symbol colors
    color_profile: ferrite_portrayal_catalog::ColorProfile,
    /// Current mouse position
    mouse_pos: (f64, f64),
    /// Rendered symbols for hit testing
    rendered_symbols: Vec<RenderedSymbol>,
    /// Is mouse being dragged for panning
    is_dragging: bool,
    /// Last drag position
    drag_start: (f64, f64),
    /// Current zoom level (1.0 = fit to window)
    zoom_level: f64,
    /// Pan offset in world coordinates
    pan_offset: (f64, f64),
    /// Pan velocity for inertia (world coordinates per second)
    pan_velocity: (f64, f64),
    /// Last frame time for velocity calculation
    last_frame_time: std::time::Instant,
    /// Recent mouse positions for velocity calculation (screen coords, time)
    recent_positions: Vec<((f64, f64), std::time::Instant)>,
    /// Feature Catalogue reference for attribute lookup
    fc: Arc<FeatureCatalogue>,
    /// Portrayal Catalogue reference
    pc: Arc<PortrayalCatalogue>,
    /// All loaded S101 cells
    cells: Vec<S101Cell>,
    /// Whether chart data is loaded
    chart_loaded: bool,
    /// Paths of already loaded chart files (to prevent duplicates)
    loaded_paths: std::collections::HashSet<PathBuf>,
}

impl ChartApp {
    fn new(
        symbol_cache: SymbolCache,
        color_profile: ferrite_portrayal_catalog::ColorProfile,
        fc: Arc<FeatureCatalogue>,
        pc: Arc<PortrayalCatalogue>,
    ) -> Self {
        ChartApp {
            window: None,
            renderer: None,
            render_context: RenderContext::new(Viewport::new(1920.0, 1080.0)),
            bounds: GeoBounds::default(),
            symbol_cache,
            color_profile,
            mouse_pos: (0.0, 0.0),
            rendered_symbols: Vec::new(),
            is_dragging: false,
            drag_start: (0.0, 0.0),
            zoom_level: 1.0,
            pan_offset: (0.0, 0.0),
            pan_velocity: (0.0, 0.0),
            last_frame_time: std::time::Instant::now(),
            recent_positions: Vec::new(),
            fc,
            pc,
            cells: Vec::new(),
            chart_loaded: false,
            loaded_paths: std::collections::HashSet::new(),
        }
    }

    /// Load chart files (appends to existing cells)
    fn load_charts(&mut self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        // Filter out already loaded files
        let new_paths: Vec<_> = paths
            .iter()
            .filter(|p| {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                !self.loaded_paths.contains(&canonical)
            })
            .collect();

        if new_paths.is_empty() {
            #[cfg(debug_assertions)]
            info!("All selected files are already loaded");
            return Ok(());
        }

        #[cfg(debug_assertions)]
        info!(
            "Loading {} new chart file(s) ({} skipped as duplicates)",
            new_paths.len(),
            paths.len() - new_paths.len()
        );

        let fc_feature_codes = self.fc.feature_type_codes();
        let mut loaded_names = Vec::new();
        #[cfg(debug_assertions)]
        let mut total_features = 0;
        #[cfg(debug_assertions)]
        let new_paths_count = new_paths.len();

        for path in new_paths {
            #[cfg(debug_assertions)]
            info!("Loading chart: {}", path.display());

            // Load cell
            let mut cell = S101Cell::load(path)
                .with_context(|| format!("Failed to load chart: {}", path.display()))?;

            // Normalize feature codes
            cell.normalize_feature_codes(&fc_feature_codes);

            #[cfg(debug_assertions)]
            {
                let stats = cell.statistics();
                info!(
                    "Loaded: {} features, {} points, {} curves, {} surfaces",
                    stats.features, stats.points, stats.curves, stats.surfaces
                );
                total_features += stats.features;
            }

            // Expand bounds to include this cell
            for point in cell.points.values() {
                let wp = WorldPoint::new(point.position.x, point.position.y);
                self.bounds.expand(wp);
            }
            for curve in cell.curves.values() {
                for pos in curve.all_positions() {
                    let wp = WorldPoint::new(pos.x, pos.y);
                    self.bounds.expand(wp);
                }
            }

            // Track loaded file path (canonical to handle symlinks/relative paths)
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            self.loaded_paths.insert(canonical);

            // Track loaded file name
            if let Some(name) = path.file_name() {
                loaded_names.push(name.to_string_lossy().to_string());
            }

            // Append cell to existing cells
            self.cells.push(cell);
        }

        // Expand bounds by 10%
        self.bounds.expand_by_percent(0.1);
        self.chart_loaded = true;

        // Generate drawing instructions for all cells
        self.regenerate_instructions()?;

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            // Show count of loaded charts or single chart name
            let chart_info = if self.cells.len() == 1 {
                loaded_names
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "Chart".to_string())
            } else {
                format!("{} charts loaded", self.cells.len())
            };
            renderer.ui_state.loaded_chart = Some(chart_info);

            // Update total feature count across all cells
            let total_count: usize = self.cells.iter().map(|c| c.statistics().features).sum();
            renderer.ui_state.feature_count = total_count;
            renderer.ui_state.chart_count = self.cells.len();
        }

        #[cfg(debug_assertions)]
        info!(
            "Loaded {} new charts ({} total features)",
            new_paths_count, total_features
        );
        Ok(())
    }

    /// Clear all loaded charts
    fn clear_charts(&mut self) {
        #[cfg(debug_assertions)]
        info!("Clearing all charts");

        self.cells.clear();
        self.bounds = GeoBounds::default();
        self.chart_loaded = false;
        self.zoom_level = 1.0;
        self.pan_offset = (0.0, 0.0);
        self.rendered_symbols.clear();
        self.loaded_paths.clear();

        // Clear render context
        if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            self.render_context =
                RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        }

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loaded_chart = None;
            renderer.ui_state.feature_count = 0;
            renderer.ui_state.chart_count = 0;
            renderer.ui_state.selected_feature = None;

            // Clear renderer frame
            renderer.begin_frame();
        }

        info!("All charts cleared");
    }

    /// Regenerate drawing instructions from loaded cells
    fn regenerate_instructions(&mut self) -> Result<()> {
        if self.cells.is_empty() {
            return Ok(());
        }

        // Get current viewport size from renderer/window
        let (width, height) = if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            (size.width as f32, size.height as f32)
        } else {
            (1920.0, 1080.0)
        };

        // Clear existing instructions
        self.render_context = RenderContext::new(Viewport::new(width, height));
        self.render_context.set_bounds(self.bounds);

        // Try Lua portrayal
        let lua_result =
            try_lua_portrayal(&self.cells, &self.fc, &self.pc, &mut self.render_context);

        if let Err(e) = lua_result {
            warn!("Lua portrayal failed: {}. Using default instructions.", e);
            for cell in &self.cells {
                generate_default_instructions(cell, &mut self.render_context, &self.pc);
            }
        }

        // Update renderer
        if let Some(renderer) = &mut self.renderer {
            let size = renderer.window().inner_size();
            self.render_context
                .set_viewport(size.width as f32, size.height as f32);
            self.render_context.zoom_to_fit(self.bounds);

            renderer.begin_frame();
            renderer.add_instructions_with_symbols(
                &mut self.render_context,
                Some(&mut self.symbol_cache),
                Some(&self.color_profile),
            );

            // Rebuild hit testing
            self.build_rendered_symbols();
        }

        Ok(())
    }

    /// Build rendered symbols list for hit testing
    fn build_rendered_symbols(&mut self) {
        self.rendered_symbols.clear();

        // Clone instructions to avoid borrow issues
        let instructions: Vec<_> = self.render_context.get_sorted_instructions().to_vec();

        for instr in &instructions {
            if let ferrite_render::DrawingInstruction::Point(point) = instr {
                let screen = self.render_context.scaler.world_to_screen(point.position);
                self.rendered_symbols.push(RenderedSymbol {
                    symbol_ref: point.symbol_ref.clone(),
                    feature_id: point.feature_id.unwrap_or(0),
                    screen_x: screen.x,
                    screen_y: screen.y,
                    world_x: point.position.x,
                    world_y: point.position.y,
                    priority: point.priority.0,
                    cell_index: point.cell_index,
                });
            }
        }

        info!(
            "Built {} rendered symbols for hit testing",
            self.rendered_symbols.len()
        );
    }

    /// Find symbols near the click position
    /// Sorted by priority (highest first = topmost visible), then by distance (closest first)
    fn find_symbols_at(&self, x: f64, y: f64, radius: f32) -> Vec<&RenderedSymbol> {
        let mut nearby: Vec<_> = self
            .rendered_symbols
            .iter()
            .filter_map(|s| {
                let dx = s.screen_x - x as f32;
                let dy = s.screen_y - y as f32;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq <= radius * radius {
                    Some((s, dist_sq))
                } else {
                    None
                }
            })
            .collect();

        // Sort by priority (highest first = topmost), then by distance (closest first)
        nearby.sort_by(|a, b| {
            // First compare by priority (higher priority = drawn on top)
            match b.0.priority.cmp(&a.0.priority) {
                std::cmp::Ordering::Equal => {
                    // Same priority: prefer closer symbol
                    a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                }
                other => other,
            }
        });

        nearby.into_iter().map(|(s, _)| s).collect()
    }

    /// Update the view based on current zoom and pan
    /// - `rebuild_hit_test`: if false, skip rebuilding the hit-test symbol list
    /// - `preserve_declutter`: if true, preserve symbol declutter grids to avoid flickering
    fn update_view_ex(&mut self, rebuild_hit_test: bool, preserve_declutter: bool) {
        if !self.chart_loaded {
            return;
        }

        // Calculate the zoomed and panned bounds
        let base_width = self.bounds.max_x - self.bounds.min_x;
        let base_height = self.bounds.max_y - self.bounds.min_y;
        let center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0 + self.pan_offset.0;
        let center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0 + self.pan_offset.1;

        let zoomed_width = base_width / self.zoom_level;
        let zoomed_height = base_height / self.zoom_level;

        let new_bounds = GeoBounds {
            min_x: center_x - zoomed_width / 2.0,
            max_x: center_x + zoomed_width / 2.0,
            min_y: center_y - zoomed_height / 2.0,
            max_y: center_y + zoomed_height / 2.0,
        };

        self.render_context.zoom_to_fit(new_bounds);

        // Re-render with new view
        if let Some(renderer) = &mut self.renderer {
            // Update zoom level for symbol decluttering and UI
            renderer.set_zoom_level(self.zoom_level);
            renderer.ui_state.zoom_level = self.zoom_level;
            // During animation, preserve declutter state to avoid flickering
            renderer.begin_frame_ex(preserve_declutter);
            renderer.add_instructions_with_symbols(
                &mut self.render_context,
                Some(&mut self.symbol_cache),
                Some(&self.color_profile),
            );
        }

        // Rebuild symbols for hit testing (skip during animation for performance)
        if rebuild_hit_test {
            self.build_rendered_symbols();
        }
    }

    /// Update the view (rebuilds hit-test symbols, clears declutter grids)
    fn update_view(&mut self) {
        self.update_view_ex(true, false);
    }
}

impl ApplicationHandler for ChartApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Load window icon
            let window_icon = load_window_icon();

            let mut window_attrs = Window::default_attributes()
                .with_title("FerriteS100 - S-101 Chart Viewer")
                .with_inner_size(winit::dpi::LogicalSize::new(1920, 1080));

            if let Some(icon) = window_icon {
                window_attrs = window_attrs.with_window_icon(Some(icon));
            }

            match event_loop.create_window(window_attrs) {
                Ok(window) => {
                    let window = Arc::new(window);
                    self.window = Some(window.clone());

                    // Create renderer asynchronously
                    match pollster::block_on(WgpuRenderer::new(window.clone())) {
                        Ok(mut renderer) => {
                            // Update render context viewport
                            let size = window.inner_size();
                            self.render_context
                                .set_viewport(size.width as f32, size.height as f32);

                            // Initialize UI state
                            renderer.ui_state.zoom_level = self.zoom_level;

                            // Only add instructions if chart is loaded
                            if self.chart_loaded {
                                self.render_context.zoom_to_fit(self.bounds);
                                renderer.begin_frame();
                                renderer.add_instructions_with_symbols(
                                    &mut self.render_context,
                                    Some(&mut self.symbol_cache),
                                    Some(&self.color_profile),
                                );
                                self.build_rendered_symbols();
                            }

                            let stats = renderer.statistics();
                            info!(
                                "GPU Renderer initialized: {} (symbols cached: {})",
                                stats,
                                self.symbol_cache.len()
                            );

                            self.renderer = Some(renderer);
                        }
                        Err(e) => {
                            error!("Failed to create renderer: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to create window: {}", e);
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Forward events to egui first
        let egui_consumed = if let Some(renderer) = &mut self.renderer {
            renderer.handle_egui_event(&event)
        } else {
            false
        };

        match event {
            WindowEvent::CloseRequested => {
                #[cfg(debug_assertions)]
                info!("Window close requested");
                event_loop.exit();
            }
            WindowEvent::Resized(physical_size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(physical_size);

                    // Update render context and re-add instructions
                    self.render_context
                        .set_viewport(physical_size.width as f32, physical_size.height as f32);

                    if self.chart_loaded {
                        self.render_context.zoom_to_fit(self.bounds);
                        renderer.begin_frame();
                        renderer.add_instructions_with_symbols(
                            &mut self.render_context,
                            Some(&mut self.symbol_cache),
                            Some(&self.color_profile),
                        );
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                // Process inertia/momentum
                let now = std::time::Instant::now();
                let dt = now.duration_since(self.last_frame_time).as_secs_f64();
                self.last_frame_time = now;

                // Apply pan velocity (inertia)
                let velocity_magnitude =
                    (self.pan_velocity.0.powi(2) + self.pan_velocity.1.powi(2)).sqrt();
                if velocity_magnitude > 0.0001 && self.chart_loaded && !self.is_dragging {
                    // Apply velocity to pan offset
                    self.pan_offset.0 += self.pan_velocity.0 * dt;
                    self.pan_offset.1 += self.pan_velocity.1 * dt;

                    // Decelerate (friction) - exponential decay for smooth stop
                    let friction = 0.95_f64.powf(dt * 60.0); // ~5% decay per frame at 60fps
                    self.pan_velocity.0 *= friction;
                    self.pan_velocity.1 *= friction;

                    // Calculate screen-space velocity for GPU pan offset
                    let screen_vx =
                        -self.pan_velocity.0 * self.render_context.scaler.scale_x() * dt;
                    let screen_vy = self.pan_velocity.1 * self.render_context.scaler.scale_y() * dt;

                    // Check if velocity is now very small (stopping)
                    let new_magnitude =
                        (self.pan_velocity.0.powi(2) + self.pan_velocity.1.powi(2)).sqrt();
                    if new_magnitude < 0.00001 {
                        self.pan_velocity = (0.0, 0.0);
                        // Motion stopped - reset pan offset and full rebuild
                        if let Some(renderer) = &mut self.renderer {
                            renderer.reset_pan_offset();
                        }
                        self.update_view();
                    } else {
                        // Still moving - use fast GPU pan path
                        if let Some(renderer) = &mut self.renderer {
                            renderer.add_pan_offset(screen_vx as f32, screen_vy as f32);
                        }
                    }
                }

                // Collect UI requests first (to avoid borrow conflicts)
                let (open_file, screenshot, zoom_in, zoom_out, reset_view, clear_charts) = {
                    if let Some(renderer) = &mut self.renderer {
                        (
                            renderer.take_open_file_request(),
                            renderer.take_screenshot_request(),
                            renderer.take_zoom_in_request(),
                            renderer.take_zoom_out_request(),
                            renderer.take_reset_view_request(),
                            renderer.take_clear_charts_request(),
                        )
                    } else {
                        (false, false, false, false, false, false)
                    }
                };

                // Process UI requests
                if open_file {
                    let paths = rfd::FileDialog::new()
                        .add_filter("S-101 Chart", &["000"])
                        .set_title("Open S-101 Chart(s) - Hold Ctrl/Shift to select multiple")
                        .pick_files()
                        .unwrap_or_default();

                    #[cfg(debug_assertions)]
                    {
                        info!("File dialog returned {} files", paths.len());
                        for (i, p) in paths.iter().enumerate() {
                            info!("  [{}] {}", i, p.display());
                        }
                    }

                    if !paths.is_empty() {
                        if let Err(e) = self.load_charts(&paths) {
                            error!("Failed to load chart(s): {}", e);
                        }
                    }
                }

                if screenshot {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("PNG Image", &["png"])
                        .set_title("Save Screenshot")
                        .save_file()
                    {
                        if let Some(renderer) = &mut self.renderer {
                            match renderer.save_screenshot(&path) {
                                Ok(_) => info!("Screenshot saved to: {}", path.display()),
                                Err(e) => error!("Failed to save screenshot: {}", e),
                            }
                        }
                    }
                }

                if zoom_in {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = (self.zoom_level * 1.5).min(50.0);
                    self.update_view();
                }

                if zoom_out {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = (self.zoom_level / 1.5).max(0.1);
                    self.update_view();
                }

                if reset_view {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = 1.0;
                    self.pan_offset = (0.0, 0.0);
                    self.pan_velocity = (0.0, 0.0); // Stop inertia on reset
                    self.update_view();
                }

                if clear_charts {
                    self.clear_charts();
                }

                // Render
                if let Some(renderer) = &mut self.renderer {
                    if let Err(e) = renderer.render() {
                        error!("Render error: {}", e);
                    }
                }

                // Request next frame
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let new_pos = (position.x, position.y);
                let now = std::time::Instant::now();

                // Update UI state with cursor position
                if let Some(renderer) = &mut self.renderer {
                    let screen_pt =
                        ferrite_render::ScreenPoint::new(new_pos.0 as f32, new_pos.1 as f32);
                    let world = self.render_context.scaler.screen_to_world(screen_pt);
                    renderer.set_cursor_world(world.x, world.y);
                    renderer.set_cursor_screen(new_pos.0 as f32, new_pos.1 as f32);
                }

                // Handle panning when dragging (only if egui didn't consume)
                if !egui_consumed && self.is_dragging && self.chart_loaded {
                    let dx = new_pos.0 - self.mouse_pos.0;
                    let dy = new_pos.1 - self.mouse_pos.1;

                    // Track world-space pan offset for final calculation
                    let world_dx = -dx / self.render_context.scaler.scale_x();
                    let world_dy = dy / self.render_context.scaler.scale_y();
                    self.pan_offset.0 += world_dx;
                    self.pan_offset.1 += world_dy;

                    // Track recent positions for velocity calculation (keep last 100ms worth)
                    self.recent_positions.push((new_pos, now));
                    self.recent_positions
                        .retain(|(_, t)| now.duration_since(*t).as_millis() < 100);

                    // Stop any existing inertia when actively dragging
                    self.pan_velocity = (0.0, 0.0);

                    // FAST PATH: Use GPU pan offset instead of rebuilding vertices
                    // This is much faster than update_view_ex which rebuilds all geometry
                    if let Some(renderer) = &mut self.renderer {
                        renderer.add_pan_offset(dx as f32, dy as f32);
                    }
                }

                self.mouse_pos = new_pos;
            }
            WindowEvent::MouseWheel { delta, .. } if !egui_consumed && self.chart_loaded => {
                let scroll_amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(pos) => pos.y / 50.0,
                };

                // Reset GPU pan offset before zoom (we'll rebuild vertices)
                if let Some(renderer) = &mut self.renderer {
                    renderer.reset_pan_offset();
                }

                let zoom_factor = 1.0 + scroll_amount * 0.1;
                let screen_pt = ferrite_render::ScreenPoint::new(
                    self.mouse_pos.0 as f32,
                    self.mouse_pos.1 as f32,
                );
                let world_before = self.render_context.scaler.screen_to_world(screen_pt);

                let new_zoom = (self.zoom_level * zoom_factor).clamp(0.1, 50.0);
                self.zoom_level = new_zoom;

                self.update_view();

                let world_after = self.render_context.scaler.screen_to_world(screen_pt);
                self.pan_offset.0 += world_before.x - world_after.x;
                self.pan_offset.1 += world_before.y - world_after.y;

                self.update_view();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if !egui_consumed => {
                match state {
                    ElementState::Pressed => {
                        self.is_dragging = true;
                        self.drag_start = self.mouse_pos;
                        // Clear recent positions and velocity on new drag
                        self.recent_positions.clear();
                        self.pan_velocity = (0.0, 0.0);
                    }
                    ElementState::Released => {
                        let was_dragging = self.is_dragging;
                        self.is_dragging = false;

                        let drag_dist = ((self.mouse_pos.0 - self.drag_start.0).powi(2)
                            + (self.mouse_pos.1 - self.drag_start.1).powi(2))
                        .sqrt();

                        // Calculate velocity for inertia from recent positions
                        let mut inertia_applied = false;
                        if was_dragging && drag_dist >= 5.0 && self.recent_positions.len() >= 2 {
                            // Use positions from recent history to calculate velocity
                            if let (Some(first), Some(last)) =
                                (self.recent_positions.first(), self.recent_positions.last())
                            {
                                let dt = last.1.duration_since(first.1).as_secs_f64();
                                if dt > 0.001 {
                                    // Screen-space velocity
                                    let vx = (last.0 .0 - first.0 .0) / dt;
                                    let vy = (last.0 .1 - first.0 .1) / dt;

                                    // Convert to world-space velocity
                                    let world_vx = -vx / self.render_context.scaler.scale_x();
                                    let world_vy = vy / self.render_context.scaler.scale_y();

                                    // Apply velocity with damping factor for natural feel
                                    self.pan_velocity = (world_vx * 0.5, world_vy * 0.5);
                                    inertia_applied = true;
                                }
                            }
                            self.recent_positions.clear();
                        }

                        // If drag ended without inertia, reset pan offset and do full rebuild
                        if was_dragging && !inertia_applied && self.chart_loaded {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.reset_pan_offset();
                            }
                            self.update_view();
                        }

                        if (!was_dragging || drag_dist < 5.0) && self.chart_loaded {
                            // Hit testing
                            let (x, y) = self.mouse_pos;
                            let screen_pt = ferrite_render::ScreenPoint::new(x as f32, y as f32);
                            let world = self.render_context.scaler.screen_to_world(screen_pt);

                            // Find nearby symbols (sorted by priority then distance)
                            let nearby = self.find_symbols_at(x, y, 20.0);

                            let selected = nearby.first().map(|sym| {
                                // Use cell_index to look up feature in the correct cell
                                // This fixes the bug where multiple cells have the same feature_id
                                // but different feature types
                                let feature = if let Some(cell_idx) = sym.cell_index {
                                    // Look up in the specific cell the symbol came from
                                    self.cells
                                        .get(cell_idx)
                                        .and_then(|cell| cell.features.get(&sym.feature_id))
                                } else {
                                    // Fallback: search all cells (old behavior)
                                    self.cells
                                        .iter()
                                        .find_map(|cell| cell.features.get(&sym.feature_id))
                                };

                                let (feature_code, definition) = feature
                                    .map(|f| {
                                        let code =
                                            f.feature_code.as_deref().unwrap_or(&sym.symbol_ref);
                                        // Look up definition from FC
                                        let def = self
                                            .fc
                                            .feature_types
                                            .get(code)
                                            .and_then(|ft| ft.definition.clone());
                                        (code.to_string(), def)
                                    })
                                    .unwrap_or_else(|| (sym.symbol_ref.clone(), None));

                                SelectedFeature {
                                    feature_type: feature_code,
                                    feature_id: sym.feature_id,
                                    primitive_type: "Point".to_string(),
                                    attributes: vec![],
                                    world_pos: (sym.world_x, sym.world_y),
                                    definition,
                                    symbol_name: Some(sym.symbol_ref.clone()),
                                }
                            });
                            let nearby_count = nearby.len();
                            drop(nearby); // Release borrow on self.rendered_symbols

                            // Update selected feature in UI
                            if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.selected_feature = selected;
                            }

                            info!(
                                "Click at ({:.4}, {:.4}): {} symbols found",
                                world.x, world.y, nearby_count
                            );
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } if !egui_consumed => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.reset_pan_offset();
                }
                self.zoom_level = 1.0;
                self.pan_offset = (0.0, 0.0);
                self.pan_velocity = (0.0, 0.0); // Stop inertia on reset
                self.update_view();
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let config = AppConfig::default();

    // Initialize logging (only in debug mode)
    #[cfg(debug_assertions)]
    init_logging(&config.log_path)?;

    #[cfg(debug_assertions)]
    {
        info!("========================================");
        info!("FerriteS100 Starting...");
        info!("========================================");
        info!("");
        info!("=== Loading Catalogues ===");
    }

    let fc = Arc::new(load_feature_catalogue(&config.fc_path)?);
    let pc = Arc::new(load_portrayal_catalogue(&config.pc_path)?);

    #[cfg(debug_assertions)]
    {
        info!("");
        info!("Feature Catalogue:");
        info!("  - {} feature types", fc.feature_types.len());
        info!("  - {} simple attributes", fc.simple_attributes.len());
        info!("  - {} complex attributes", fc.complex_attributes.len());
        info!("");
        info!("Portrayal Catalogue:");
        info!("  - {} color profiles", pc.color_profiles.profiles.len());
        info!("  - {} symbols", pc.symbols.symbols.len());
        info!("  - {} line styles", pc.line_styles.len());
        info!("  - {} area fills", pc.area_fills.len());
        info!("");
        info!("========================================");
        info!("Starting GUI - Use File > Open to load chart");
        info!("========================================");
    }

    // Create symbol cache for SVG rendering
    let symbols_path = config.pc_path.join("Symbols");
    let symbol_cache = SymbolCache::new(&symbols_path);
    #[cfg(debug_assertions)]
    info!("Symbol cache initialized: {}", symbols_path.display());

    // Get default color profile from PC
    let color_profile = pc
        .color_profiles
        .default_profile
        .as_ref()
        .and_then(|name| pc.color_profiles.profiles.get(name))
        .or_else(|| pc.color_profiles.profiles.values().next())
        .cloned()
        .unwrap_or_else(|| {
            #[cfg(debug_assertions)]
            warn!("No color profile found in PC, using empty profile");
            ferrite_portrayal_catalog::ColorProfile::new(
                "default".to_string(),
                "Default".to_string(),
            )
        });
    #[cfg(debug_assertions)]
    info!(
        "Using color profile: {} ({} colors)",
        color_profile.name,
        color_profile.colors.len()
    );

    let event_loop = EventLoop::new().context("Failed to create event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll); // Use Poll for smooth UI updates

    let mut app = ChartApp::new(symbol_cache, color_profile, fc, pc);

    event_loop.run_app(&mut app).context("Event loop error")?;

    Ok(())
}

/// Try to execute Lua portrayal rules and convert to drawing instructions
/// Context parameters are loaded dynamically from PC XML (no hardcoding)
/// Processes each cell separately to avoid feature ID collisions across cells
fn try_lua_portrayal(
    cells: &[S101Cell],
    fc: &FeatureCatalogue,
    pc: &PortrayalCatalogue,
    render_context: &mut RenderContext,
) -> Result<()> {
    let rules_path = pc.root_path.join("Rules");

    if !rules_path.exists() {
        return Err(anyhow::anyhow!(
            "Rules directory not found: {}",
            rules_path.display()
        ));
    }

    info!("Initializing Lua portrayal engine...");

    // Create portrayal engine
    let mut engine =
        PortrayalEngine::new(&rules_path).context("Failed to create portrayal engine")?;

    // Set type catalogue from FC
    let type_catalogue = TypeCatalogue::from_feature_catalogue(fc);
    engine.set_type_catalogue(type_catalogue);
    info!(
        "  Type catalogue loaded: {} feature types, {} attributes",
        fc.feature_types.len(),
        fc.simple_attributes.len()
    );

    // Initialize engine (load main.lua)
    engine
        .initialize()
        .context("Failed to initialize portrayal engine")?;
    info!("  Lua engine initialized");

    // Load context parameters from PC XML (dynamically, no hardcoding)
    let pc_context_params = pc.get_context_parameters();
    let context = LuaContextParameters::from_pc_context(pc_context_params);
    info!(
        "  Context parameters loaded from PC XML: {} parameters",
        pc_context_params.len()
    );

    let mut total_results = 0;

    // Process each cell separately to avoid feature ID collisions
    for (cell_index, cell) in cells.iter().enumerate() {
        debug!(
            "Processing cell {}: {}",
            cell_index,
            cell.file_path.display()
        );

        // Create portrayal context for this cell
        let portrayal_context = PortrayalContext::from_cell(cell, context.clone());
        let cell_data_arc = portrayal_context.cell_data();
        let cell_data_guard = cell_data_arc.read().unwrap();

        // Process cell through Lua
        match engine.process_cell(&cell_data_guard, context.clone()) {
            Ok(results) => {
                debug!(
                    "  Cell {} produced {} portrayal results",
                    cell_index,
                    results.len()
                );
                total_results += results.len();

                // Convert THIS cell's Lua results using ONLY this cell's data
                // Pass cell_index so symbols can be looked up in the correct cell
                convert_lua_results_for_cell(&results, cell, pc, render_context, cell_index);
            }
            Err(e) => {
                warn!("  Cell {} portrayal failed: {}", cell_index, e);
            }
        }
    }

    info!("Lua portrayal complete: {} total results", total_results);
    Ok(())
}

/// Convert Lua portrayal results to drawing instructions for a single cell
/// Uses only this cell's data to avoid feature ID collisions across cells
fn convert_lua_results_for_cell(
    results: &[ferrite_lua::PortrayalResult],
    cell: &S101Cell,
    pc: &PortrayalCatalogue,
    context: &mut RenderContext,
    cell_index: usize,
) {
    use ferrite_lua::DrawingCommand;

    // Helper to lookup color from token (from PC colorProfile.xml)
    let lookup_color = |token: &str| -> Color { lookup_pc_color(pc, token) };

    let mut area_count = 0;
    let mut area_rendered = 0;
    let mut line_count = 0;
    let mut point_count = 0;

    // Count total LandArea features in cell (before Lua processing)
    let total_land_area_in_cell: usize = cell
        .features
        .values()
        .filter(|f| f.feature_code.as_deref() == Some("LandArea"))
        .count();
    let total_land_area_surface_in_cell: usize = cell
        .features
        .values()
        .filter(|f| {
            f.feature_code.as_deref() == Some("LandArea")
                && f.primitive_type == SpatialPrimitiveType::Surface
        })
        .count();

    // Count all Surface-type features by feature code
    let mut surface_feature_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for f in cell.features.values() {
        if f.primitive_type == SpatialPrimitiveType::Surface {
            let code = f.feature_code.as_deref().unwrap_or("UNKNOWN").to_string();
            *surface_feature_counts.entry(code).or_insert(0) += 1;
        }
    }
    info!(
        "Cell total: {} LandArea features ({} with Surface primitive)",
        total_land_area_in_cell, total_land_area_surface_in_cell
    );
    info!("All Surface features by type: {:?}", surface_feature_counts);

    // Log LandArea results from Lua (feature_id is numeric, lookup feature code)
    let mut land_area_count = 0;
    let mut land_area_surface_count = 0;
    let mut land_area_curve_count = 0;
    for r in results.iter() {
        if let Ok(fid) = r.feature_id.parse::<i64>() {
            if let Some(feature) = cell.features.get(&fid) {
                if feature.feature_code.as_deref() == Some("LandArea") {
                    land_area_count += 1;

                    // Count by primitive type
                    match feature.primitive_type {
                        SpatialPrimitiveType::Surface => land_area_surface_count += 1,
                        SpatialPrimitiveType::Curve | SpatialPrimitiveType::CompositeCurve => {
                            land_area_curve_count += 1
                        }
                        _ => {}
                    }

                    if land_area_count <= 5 {
                        // Log feature PrimitiveType and spatial associations
                        let spas_types: Vec<_> = feature
                            .spatial_associations
                            .iter()
                            .map(|sa| format!("RCNM{}", sa.spatial_id.rcnm))
                            .collect();
                        info!(
                            "LandArea[{}] id={} primitive={:?} spas={:?}: {} instructions",
                            land_area_count,
                            fid,
                            feature.primitive_type,
                            spas_types,
                            r.instructions.len()
                        );
                        for inst in &r.instructions {
                            for cmd in &inst.commands {
                                info!("    cmd: {:?}", cmd);
                            }
                        }
                    }
                }
            }
        }
    }
    if land_area_count > 0 {
        info!(
            "Cell has {} LandArea results from Lua total ({} Surface, {} Curve)",
            land_area_count, land_area_surface_count, land_area_curve_count
        );
    }

    for result in results {
        // Parse feature ID from the result (format: "type|id")
        let feature_id: Option<i64> = result
            .feature_id
            .split('|')
            .next_back()
            .and_then(|s| s.parse().ok());

        // Get feature from THIS cell only (no collision with other cells)
        let feature = feature_id.and_then(|id| cell.features.get(&id));

        for instruction in &result.instructions {
            for cmd in &instruction.commands {
                match cmd {
                    DrawingCommand::PointInstruction {
                        symbol_ref,
                        rotation,
                        scale,
                        position,
                    } => {
                        point_count += 1;

                        // If explicit position is available (from AugmentedPoint), use it
                        // This is used for Sounding features where each point has specific coordinates
                        if let Some((x, y)) = position {
                            // For soundings: look up depth from multi_points spatial data
                            // The depth is used for decluttering (keep shallowest for safety)
                            let depth = feature.and_then(|f| {
                                // Find the MultiPoint spatial association
                                for spas in &f.spatial_associations {
                                    if let Some(mp) = cell.multi_points.get(&spas.spatial_id.key())
                                    {
                                        // Find the position with matching coordinates
                                        for coord in &mp.positions {
                                            // Use small epsilon for floating point comparison
                                            if (coord.x - x).abs() < 1e-9
                                                && (coord.y - y).abs() < 1e-9
                                            {
                                                return coord.z;
                                            }
                                        }
                                    }
                                }
                                None
                            });

                            let mut point_inst =
                                PointInstruction::new(symbol_ref.clone(), WorldPoint::new(*x, *y))
                                    .with_rotation(*rotation)
                                    .with_scale(*scale)
                                    .with_priority(instruction.drawing_priority)
                                    .with_feature_id(feature_id.unwrap_or(0))
                                    .with_cell_index(cell_index);

                            // Add depth for sounding decluttering (shallowest wins for safety)
                            if let Some(d) = depth {
                                point_inst = point_inst.with_depth(d);
                            }

                            context.add_instruction(ferrite_render::DrawingInstruction::Point(
                                point_inst,
                            ));
                        } else if let Some(feature) = feature {
                            // Get coordinates from feature's spatial associations
                            for spas in &feature.spatial_associations {
                                if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                                    let point_inst = PointInstruction::new(
                                        symbol_ref.clone(),
                                        WorldPoint::new(point.position.x, point.position.y),
                                    )
                                    .with_rotation(*rotation)
                                    .with_scale(*scale)
                                    .with_priority(instruction.drawing_priority)
                                    .with_feature_id(feature_id.unwrap_or(0))
                                    .with_cell_index(cell_index);

                                    context.add_instruction(
                                        ferrite_render::DrawingInstruction::Point(point_inst),
                                    );
                                }
                            }
                        }
                    }
                    DrawingCommand::AugmentedPoint { .. } => {
                        // AugmentedPoint is handled during parsing, position is passed to PointInstruction
                    }
                    DrawingCommand::LineInstruction {
                        style_ref,
                        simple_style,
                    } => {
                        line_count += 1;
                        // Determine line color and width from PC (no hardcoding)
                        let (color, width) = if let Some((w, token)) = simple_style {
                            (lookup_color(token), *w)
                        } else if let Some(ref_name) = style_ref {
                            // Look up from PC line styles
                            if let Some(style) = pc.line_styles.get(ref_name) {
                                match style {
                                    ferrite_portrayal_catalog::LineStyle::Simple(s) => {
                                        (lookup_color(&s.pen.color_token), s.pen.width as f32)
                                    }
                                    ferrite_portrayal_catalog::LineStyle::Complex(c) => {
                                        if let Some(s) = c.strokes.first() {
                                            (lookup_color(&s.pen.color_token), s.pen.width as f32)
                                        } else {
                                            (lookup_color("CSTLN"), 1.0)
                                        }
                                    }
                                    ferrite_portrayal_catalog::LineStyle::Composite(c) => {
                                        if let Some(s) = c.components.first() {
                                            (lookup_color(&s.pen.color_token), s.pen.width as f32)
                                        } else {
                                            (lookup_color("CSTLN"), 1.0)
                                        }
                                    }
                                }
                            } else {
                                (lookup_color("CSTLN"), 1.0)
                            }
                        } else {
                            (lookup_color("CSTLN"), 1.0)
                        };

                        // Get coordinates from feature's spatial associations
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if let Some(curve) = cell.curves.get(&spas.spatial_id.key()) {
                                    let points: Vec<WorldPoint> = curve
                                        .all_positions()
                                        .iter()
                                        .map(|c| WorldPoint::new(c.x, c.y))
                                        .collect();

                                    if points.len() >= 2 {
                                        let line_inst = LineInstruction::new(points)
                                            .with_style(LineStyle::solid(color, width))
                                            .with_priority(instruction.drawing_priority)
                                            .with_feature_id(feature_id.unwrap_or(0));

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Line(line_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::AreaInstruction {
                        fill_ref,
                        color_fill,
                    } => {
                        area_count += 1;

                        // Log area assignments by feature type
                        // feature here is FeatureRecord from S101Cell
                        let feature_code_str = feature
                            .and_then(|f| f.feature_code.as_deref())
                            .unwrap_or("unknown");
                        if feature_code_str == "LandArea" {
                            static LAND_LOG: std::sync::atomic::AtomicUsize =
                                std::sync::atomic::AtomicUsize::new(0);
                            if LAND_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 10 {
                                info!(
                                    "LandArea area: color_fill={:?} fill_ref={:?} priority={}",
                                    color_fill, fill_ref, instruction.drawing_priority
                                );
                            }
                        }
                        if feature_code_str == "DepthArea" {
                            static DEPTH_LOG: std::sync::atomic::AtomicUsize =
                                std::sync::atomic::AtomicUsize::new(0);
                            if DEPTH_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 10 {
                                info!(
                                    "DepthArea area: color_fill={:?} fill_ref={:?} priority={}",
                                    color_fill, fill_ref, instruction.drawing_priority
                                );
                            }
                        }

                        // Determine area color from PC (no hardcoding)
                        let color = if let Some(token) = color_fill {
                            lookup_color(token)
                        } else if let Some(ref_name) = fill_ref {
                            // Look up from PC area fills
                            if let Some(fill) = pc.area_fills.get(ref_name) {
                                match &fill.fill_type {
                                    ferrite_portrayal_catalog::AreaFillType::Color(c) => {
                                        lookup_color(&c.color_token)
                                    }
                                    ferrite_portrayal_catalog::AreaFillType::Symbol(_)
                                    | ferrite_portrayal_catalog::AreaFillType::Pattern(_)
                                    | ferrite_portrayal_catalog::AreaFillType::Pixmap(_) => {
                                        lookup_color("DEPVS")
                                    }
                                    ferrite_portrayal_catalog::AreaFillType::Hatch(h) => {
                                        lookup_color(&h.line_color)
                                    }
                                }
                            } else {
                                lookup_color("NODTA")
                            }
                        } else {
                            lookup_color("NODTA")
                        };

                        // Get area from feature's spatial associations (surfaces)
                        if let Some(feature) = feature {
                            // Debug: track LandArea surface processing
                            let is_land_area = feature.feature_code.as_deref() == Some("LandArea");
                            static LAND_DEBUG: std::sync::atomic::AtomicUsize =
                                std::sync::atomic::AtomicUsize::new(0);
                            let land_debug_idx = if is_land_area {
                                LAND_DEBUG.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            } else {
                                999
                            };

                            if is_land_area && land_debug_idx < 5 {
                                debug!(
                                    "LandArea[{}] fid={:?} spas_count={}",
                                    land_debug_idx,
                                    feature_id,
                                    feature.spatial_associations.len()
                                );
                            }

                            for spas in &feature.spatial_associations {
                                // Only process surfaces (RCNM=130)
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }

                                let surface_key = spas.spatial_id.key();
                                if is_land_area && land_debug_idx < 5 {
                                    debug!(
                                        "  LandArea[{}] looking for surface key={}",
                                        land_debug_idx, surface_key
                                    );
                                    debug!(
                                        "  cell.surfaces has {} entries, keys sample: {:?}",
                                        cell.surfaces.len(),
                                        cell.surfaces.keys().take(5).collect::<Vec<_>>()
                                    );
                                }

                                if let Some(surface) = cell.surfaces.get(&surface_key) {
                                    if is_land_area && land_debug_idx < 5 {
                                        debug!("  LandArea[{}] found surface, exterior_ring has {} curves", land_debug_idx, surface.exterior_ring.len());
                                    }
                                    let mut exterior_points = Vec::new();

                                    // Collect points from exterior ring curves
                                    for oriented_curve in &surface.exterior_ring {
                                        let curve_key = oriented_curve.curve_id.key();

                                        if let Some(curve) = cell.curves.get(&curve_key) {
                                            let positions = curve.all_positions();
                                            if oriented_curve.orientation {
                                                for pos in positions {
                                                    exterior_points
                                                        .push(WorldPoint::new(pos.x, pos.y));
                                                }
                                            } else {
                                                for pos in positions.into_iter().rev() {
                                                    exterior_points
                                                        .push(WorldPoint::new(pos.x, pos.y));
                                                }
                                            }
                                        } else if let Some(composite) =
                                            cell.composite_curves.get(&curve_key)
                                        {
                                            for sub_curve in &composite.curves {
                                                let sub_key = sub_curve.curve_id.key();
                                                if let Some(curve) = cell.curves.get(&sub_key) {
                                                    let positions = curve.all_positions();
                                                    let forward = oriented_curve.orientation
                                                        == sub_curve.orientation;
                                                    if forward {
                                                        for pos in positions {
                                                            exterior_points.push(WorldPoint::new(
                                                                pos.x, pos.y,
                                                            ));
                                                        }
                                                    } else {
                                                        for pos in positions.into_iter().rev() {
                                                            exterior_points.push(WorldPoint::new(
                                                                pos.x, pos.y,
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Remove duplicate consecutive points (curves share endpoints)
                                    // This prevents triangulation issues
                                    let mut cleaned_points =
                                        Vec::with_capacity(exterior_points.len());
                                    for point in exterior_points {
                                        if cleaned_points.is_empty() {
                                            cleaned_points.push(point);
                                        } else {
                                            let last = cleaned_points.last().unwrap();
                                            // Skip if same as previous point (within tolerance)
                                            let dx = (point.x - last.x).abs();
                                            let dy = (point.y - last.y).abs();
                                            if dx > 1e-9 || dy > 1e-9 {
                                                cleaned_points.push(point);
                                            }
                                        }
                                    }

                                    // Also check if first and last points are the same (closed ring)
                                    // and remove the duplicate closing point
                                    if cleaned_points.len() > 3 {
                                        let first = cleaned_points.first().unwrap();
                                        let last = cleaned_points.last().unwrap();
                                        let dx = (first.x - last.x).abs();
                                        let dy = (first.y - last.y).abs();
                                        if dx < 1e-9 && dy < 1e-9 {
                                            cleaned_points.pop();
                                        }
                                    }

                                    if cleaned_points.len() >= 3 {
                                        area_rendered += 1;
                                        // Adjust priority: LandArea should be on top of DepthArea
                                        // S-52 standard: land is always above water
                                        let adjusted_priority = if feature_code_str == "LandArea" {
                                            instruction.drawing_priority.max(4)
                                        // Ensure land is above depth (priority 3)
                                        } else {
                                            instruction.drawing_priority
                                        };
                                        let area_inst = AreaInstruction::new(cleaned_points)
                                            .with_solid_fill(color)
                                            .with_priority(adjusted_priority)
                                            .with_feature_id(feature_id.unwrap_or(0));

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::TextInstruction {
                        text,
                        font_size: _,
                        color_token,
                    } => {
                        let _color = lookup_color(color_token);
                        debug!("Text instruction (not rendered yet): {}", text);
                    }
                    DrawingCommand::SpatialReference { .. } | DrawingCommand::Dash { .. } => {
                        // These modify other instructions, handled separately
                    }
                }
            }
        }
    }

    debug!(
        "Cell conversion: {} points, {} lines, {} areas ({} rendered)",
        point_count, line_count, area_count, area_rendered
    );
}

/// Load window icon from icon.ico file
fn load_window_icon() -> Option<Icon> {
    let icon_path = PathBuf::from("./icon.ico");

    if !icon_path.exists() {
        warn!("Icon file not found: {}", icon_path.display());
        return None;
    }

    // Read the ICO file
    match fs::read(&icon_path) {
        Ok(data) => {
            // Parse ICO file to get RGBA data
            // ICO files have a directory structure, we need to extract the image
            match parse_ico_to_rgba(&data) {
                Some((rgba, width, height)) => match Icon::from_rgba(rgba, width, height) {
                    Ok(icon) => {
                        info!("Window icon loaded: {}x{}", width, height);
                        Some(icon)
                    }
                    Err(e) => {
                        warn!("Failed to create icon: {}", e);
                        None
                    }
                },
                None => {
                    warn!("Failed to parse ICO file");
                    None
                }
            }
        }
        Err(e) => {
            warn!("Failed to read icon file: {}", e);
            None
        }
    }
}

/// Parse ICO file and extract RGBA pixel data
fn parse_ico_to_rgba(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    // ICO file structure:
    // - Header (6 bytes): reserved, type, image count
    // - Directory entries (16 bytes each): width, height, colors, reserved, planes, bpp, size, offset
    // - Image data (BMP or PNG format)

    if data.len() < 6 {
        return None;
    }

    // Check ICO header
    let _reserved = u16::from_le_bytes([data[0], data[1]]);
    let image_type = u16::from_le_bytes([data[2], data[3]]);
    let image_count = u16::from_le_bytes([data[4], data[5]]);

    if image_type != 1 || image_count == 0 {
        return None;
    }

    // Find the best (largest) icon
    let mut best_entry: Option<(usize, u32, u32, u32, u32)> = None;

    for i in 0..image_count as usize {
        let entry_offset = 6 + i * 16;
        if entry_offset + 16 > data.len() {
            break;
        }

        // Width and height (0 means 256)
        let width = if data[entry_offset] == 0 {
            256u32
        } else {
            data[entry_offset] as u32
        };
        let height = if data[entry_offset + 1] == 0 {
            256u32
        } else {
            data[entry_offset + 1] as u32
        };
        let size = u32::from_le_bytes([
            data[entry_offset + 8],
            data[entry_offset + 9],
            data[entry_offset + 10],
            data[entry_offset + 11],
        ]);
        let offset = u32::from_le_bytes([
            data[entry_offset + 12],
            data[entry_offset + 13],
            data[entry_offset + 14],
            data[entry_offset + 15],
        ]);

        // Prefer larger icons, but not too large (32x32 or 48x48 is ideal for window icons)
        let score = width * height;
        if best_entry.is_none() || score <= 48 * 48 {
            best_entry = Some((i, width, height, size, offset));
        }
    }

    let (_, _width, _height, size, offset) = best_entry?;
    let offset = offset as usize;
    let size = size as usize;

    if offset + size > data.len() {
        return None;
    }

    let image_data = &data[offset..offset + size];

    // Check if it's PNG (starts with PNG signature)
    if image_data.len() >= 8 && &image_data[0..8] == b"\x89PNG\r\n\x1a\n" {
        // PNG format - use image crate to decode
        use image::GenericImageView;
        match image::load_from_memory(image_data) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = img.dimensions();
                Some((rgba.into_raw(), w, h))
            }
            Err(_) => None,
        }
    } else {
        // BMP format (DIB) - more complex parsing needed
        // For simplicity, try using image crate with BMP header reconstruction
        // Or just decode the DIB directly

        // DIB header starts directly (no BMP file header)
        if image_data.len() < 40 {
            return None;
        }

        let header_size =
            u32::from_le_bytes([image_data[0], image_data[1], image_data[2], image_data[3]]);

        if header_size < 40 {
            return None;
        }

        let dib_width =
            i32::from_le_bytes([image_data[4], image_data[5], image_data[6], image_data[7]]) as u32;

        // Height in DIB is doubled (includes mask)
        let dib_height =
            i32::from_le_bytes([image_data[8], image_data[9], image_data[10], image_data[11]])
                .unsigned_abs()
                / 2;

        let bpp = u16::from_le_bytes([image_data[14], image_data[15]]);

        // Only handle 32-bit BGRA
        if bpp != 32 {
            return None;
        }

        let pixel_offset = header_size as usize;
        let row_size = (dib_width * 4) as usize;
        let pixel_data_size = row_size * dib_height as usize;

        if pixel_offset + pixel_data_size > image_data.len() {
            return None;
        }

        // Convert BGRA to RGBA, and flip vertically (DIB is bottom-up)
        let mut rgba = vec![0u8; (dib_width * dib_height * 4) as usize];

        for y in 0..dib_height {
            let src_y = (dib_height - 1 - y) as usize;
            let src_offset = pixel_offset + src_y * row_size;
            let dst_offset = (y * dib_width * 4) as usize;

            for x in 0..dib_width {
                let src_px = src_offset + (x as usize) * 4;
                let dst_px = dst_offset + (x as usize) * 4;

                if src_px + 4 <= image_data.len() {
                    // BGRA -> RGBA
                    rgba[dst_px] = image_data[src_px + 2]; // R
                    rgba[dst_px + 1] = image_data[src_px + 1]; // G
                    rgba[dst_px + 2] = image_data[src_px]; // B
                    rgba[dst_px + 3] = image_data[src_px + 3]; // A
                }
            }
        }

        Some((rgba, dib_width, dib_height))
    }
}

/// Initialize logging with file and console output
#[allow(dead_code)]
fn init_logging(log_path: &Path) -> Result<()> {
    // Create log directory
    fs::create_dir_all(log_path)
        .with_context(|| format!("Failed to create log directory: {}", log_path.display()))?;

    // File appender
    let file_appender = RollingFileAppender::new(Rotation::NEVER, log_path, "ferrite_debug.log");

    // Console layer
    let console_layer = fmt::layer().with_target(false).with_level(true);

    // File layer
    let file_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_writer(file_appender);

    // Environment filter - default to info, debug for core modules
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ferrite_s100_core=debug"));

    // Initialize subscriber
    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    info!(
        "Logging initialized: {}/ferrite_debug.log",
        log_path.display()
    );

    Ok(())
}

/// Load Feature Catalogue from XML file
fn load_feature_catalogue(path: &Path) -> Result<FeatureCatalogue> {
    info!("Loading Feature Catalogue: {}", path.display());

    if !path.exists() {
        warn!("Feature Catalogue not found at: {}", path.display());
        warn!("Creating empty catalogue...");
        return Ok(FeatureCatalogue {
            name: String::new(),
            scope: String::new(),
            version: String::new(),
            version_date: String::new(),
            product_id: String::new(),
            simple_attributes: Default::default(),
            complex_attributes: Default::default(),
            feature_types: Default::default(),
            information_types: Default::default(),
        });
    }

    let fc = FeatureCatalogue::load(path)
        .with_context(|| format!("Failed to load Feature Catalogue: {}", path.display()))?;

    info!("FC loaded successfully");
    debug!("  Product: {}", fc.product_id);
    debug!("  Version: {}", fc.version);
    debug!("  Feature types: {}", fc.feature_types.len());
    debug!("  Simple attributes: {}", fc.simple_attributes.len());
    debug!("  Complex attributes: {}", fc.complex_attributes.len());
    debug!("  Information types: {}", fc.information_types.len());

    Ok(fc)
}

/// Load Portrayal Catalogue from directory
fn load_portrayal_catalogue(path: &Path) -> Result<PortrayalCatalogue> {
    info!("Loading Portrayal Catalogue: {}", path.display());

    if !path.exists() {
        warn!("Portrayal Catalogue not found at: {}", path.display());
        warn!("Creating empty catalogue...");
        return Ok(PortrayalCatalogue {
            root_path: path.to_path_buf(),
            product_id: String::new(),
            version: String::new(),
            color_profiles: Default::default(),
            symbols: ferrite_portrayal_catalog::Symbols::new(path.join("Symbols")),
            line_styles: Default::default(),
            area_fills: Default::default(),
            viewing_groups: Default::default(),
            viewing_group_layers: Default::default(),
            display_modes: Default::default(),
            rules: ferrite_portrayal_catalog::PortrayalRules::new(path.join("Rules")),
        });
    }

    let pc = PortrayalCatalogue::load(path)
        .with_context(|| format!("Failed to load Portrayal Catalogue: {}", path.display()))?;

    info!("PC loaded successfully");
    debug!("  Product: {}", pc.product_id);
    debug!("  Version: {}", pc.version);
    debug!("  Color profiles: {}", pc.color_profiles.profiles.len());
    for (profile_id, profile) in &pc.color_profiles.profiles {
        info!(
            "  Color profile '{}' (name: '{}'): {} colors",
            profile_id,
            profile.name,
            profile.colors.len()
        );
        // Log some sample colors
        for token in ["DEPVS", "DEPMS", "DEPMD", "DEPDW", "DEPIT", "LANDA"] {
            if let Some(color) = profile.get_srgb(token) {
                debug!("    {}: RGB({}, {}, {})", token, color.r, color.g, color.b);
            }
        }
    }
    debug!("  Symbols: {}", pc.symbols.symbols.len());
    debug!("  Line styles: {}", pc.line_styles.len());
    debug!("  Area fills: {}", pc.area_fills.len());

    Ok(pc)
}

/// Load all chart data from directory
#[allow(dead_code)]
fn load_chart_data(path: &Path) -> Result<Vec<S101Cell>> {
    info!("Scanning ChartData folder: {}", path.display());

    if !path.exists() {
        warn!("ChartData directory not found: {}", path.display());
        warn!("Creating directory...");
        fs::create_dir_all(path)
            .with_context(|| format!("Failed to create ChartData directory: {}", path.display()))?;
        return Ok(Vec::new());
    }

    // Find all .000 files (filter to specific file for testing)
    let chart_files: Vec<PathBuf> = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "000")
                // Filter to specific file for testing
                && e.path().file_name().is_some_and(|name| name == "101GB00GB302045.000")
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    info!("Found {} chart files", chart_files.len());

    // Load each chart
    let mut cells = Vec::new();

    for chart_path in &chart_files {
        info!("Loading: {}", chart_path.display());

        match S101Cell::load(chart_path) {
            Ok(cell) => {
                let stats = cell.statistics();
                debug!("  -> {}", stats);
                cells.push(cell);
            }
            Err(e) => {
                error!("Failed to load {}: {}", chart_path.display(), e);
            }
        }
    }

    info!("Loaded {} cells successfully", cells.len());

    Ok(cells)
}

/// Generate default drawing instructions for a cell (simplified portrayal)
/// Uses PC color lookup - no hardcoded colors
fn generate_default_instructions(
    cell: &S101Cell,
    context: &mut RenderContext,
    pc: &PortrayalCatalogue,
) {
    // Process each feature and generate default instructions based on type
    for (key, feature) in &cell.features {
        let feature_code = feature.feature_code.as_deref().unwrap_or("UNKNOWN");

        // Get color token and priority based on feature type, then look up color from PC
        let (color_token, priority) = get_feature_color_token(feature_code);
        let color = lookup_pc_color(pc, color_token);

        match feature.primitive_type {
            SpatialPrimitiveType::Point => {
                // Get point coordinates from spatial associations
                for spas in &feature.spatial_associations {
                    if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                        let instruction = PointInstruction::new(
                            feature_code.to_string(),
                            WorldPoint::new(point.position.x, point.position.y),
                        )
                        .with_priority(priority)
                        .with_feature_id(*key);

                        context.add_instruction(ferrite_render::DrawingInstruction::Point(
                            instruction,
                        ));
                    }
                }
            }
            SpatialPrimitiveType::Curve | SpatialPrimitiveType::CompositeCurve => {
                // Get curve coordinates
                for spas in &feature.spatial_associations {
                    if let Some(curve) = cell.curves.get(&spas.spatial_id.key()) {
                        let points: Vec<WorldPoint> = curve
                            .all_positions()
                            .iter()
                            .map(|c| WorldPoint::new(c.x, c.y))
                            .collect();

                        if points.len() >= 2 {
                            let instruction = LineInstruction::new(points)
                                .with_style(LineStyle::solid(color, 1.0))
                                .with_priority(priority)
                                .with_feature_id(*key);

                            context.add_instruction(ferrite_render::DrawingInstruction::Line(
                                instruction,
                            ));
                        }
                    }
                }
            }
            SpatialPrimitiveType::Surface => {
                // Get surface boundary from surface records
                for spas in &feature.spatial_associations {
                    // Get the surface record
                    let surface_key = spas.spatial_id.key();
                    if let Some(surface) = cell.surfaces.get(&surface_key) {
                        let mut exterior_points = Vec::new();

                        // Collect points from exterior ring curves
                        for oriented_curve in &surface.exterior_ring {
                            let curve_key = oriented_curve.curve_id.key();

                            // Try to get curve from curves collection
                            if let Some(curve) = cell.curves.get(&curve_key) {
                                let positions = curve.all_positions();
                                if oriented_curve.orientation {
                                    for pos in positions {
                                        exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                    }
                                } else {
                                    // Reverse orientation
                                    for pos in positions.into_iter().rev() {
                                        exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                    }
                                }
                            }
                            // Also check composite curves
                            else if let Some(composite) = cell.composite_curves.get(&curve_key) {
                                for sub_curve in &composite.curves {
                                    if let Some(curve) = cell.curves.get(&sub_curve.curve_id.key())
                                    {
                                        let positions = curve.all_positions();
                                        let forward =
                                            oriented_curve.orientation == sub_curve.orientation;
                                        if forward {
                                            for pos in positions {
                                                exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                            }
                                        } else {
                                            for pos in positions.into_iter().rev() {
                                                exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Remove duplicate consecutive points (curves share endpoints)
                        let mut cleaned_points = Vec::with_capacity(exterior_points.len());
                        for point in exterior_points {
                            if cleaned_points.is_empty() {
                                cleaned_points.push(point);
                            } else {
                                let last = cleaned_points.last().unwrap();
                                let dx = (point.x - last.x).abs();
                                let dy = (point.y - last.y).abs();
                                if dx > 1e-9 || dy > 1e-9 {
                                    cleaned_points.push(point);
                                }
                            }
                        }

                        // Remove duplicate closing point if present
                        if cleaned_points.len() > 3 {
                            let first = cleaned_points.first().unwrap();
                            let last = cleaned_points.last().unwrap();
                            let dx = (first.x - last.x).abs();
                            let dy = (first.y - last.y).abs();
                            if dx < 1e-9 && dy < 1e-9 {
                                cleaned_points.pop();
                            }
                        }

                        if cleaned_points.len() >= 3 {
                            let fill_color = color.with_alpha(0.3);
                            let instruction = AreaInstruction::new(cleaned_points)
                                .with_solid_fill(fill_color)
                                .with_outline(LineStyle::solid(color, 0.5))
                                .with_priority(priority)
                                .with_feature_id(*key);

                            context.add_instruction(ferrite_render::DrawingInstruction::Area(
                                instruction,
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Get color token and priority for feature type (maps feature code to PC color token)
/// Returns (color_token, priority) - color_token should be looked up from PC color profile
fn get_feature_color_token(feature_code: &str) -> (&'static str, i32) {
    match feature_code {
        // Land features - use PC tokens
        "LandArea" => ("LANDA", 1),
        "BuiltUpArea" => ("CHBRN", 2),

        // Depth features - use PC tokens
        "DepthArea" => ("DEPVS", 1),     // Very shallow water
        "DepthContour" => ("DEPCN", 10), // Depth contour
        "DredgedArea" => ("DEPMD", 3),   // Medium depth

        // Coastline
        "Coastline" => ("CSTLN", 15),

        // Navigation features
        "Light" | "LightAllAround" | "LightSectored" => ("LITRD", 20),
        "Buoy" | "LateralBuoy" | "CardinalBuoy" | "IsolatedDangerBuoy" => ("LITRD", 18),
        "Beacon" | "LateralBeacon" | "CardinalBeacon" => ("LITRD", 18),

        // Obstructions and dangers
        "Wreck" => ("DEPVS", 25),
        "Obstruction" => ("CHGRD", 25),
        "Rock" | "UnderwaterRock" => ("CHGRD", 22),

        // Anchorage
        "AnchorageArea" => ("CHMGD", 8),
        "AnchorBerth" => ("CHMGD", 12),

        // Traffic
        "TrafficSeparationScheme" | "TrafficSeparationZone" => ("TRFCD", 5),

        // Default - use CHGRD (chart grid color)
        _ => ("CHGRD", 5),
    }
}

/// Look up color from PC color profile by token
fn lookup_pc_color(pc: &PortrayalCatalogue, token: &str) -> Color {
    let profile = pc
        .color_profiles
        .default_profile
        .as_ref()
        .and_then(|name| pc.color_profiles.profiles.get(name))
        .or_else(|| pc.color_profiles.profiles.values().next());

    if let Some(profile) = profile {
        if let Some(srgb) = profile.get_srgb(token) {
            return Color::rgb(
                srgb.r as f32 / 255.0,
                srgb.g as f32 / 255.0,
                srgb.b as f32 / 255.0,
            );
        }
    }

    // Color not found in PC - log warning and return gray
    tracing::warn!("Color token '{}' not found in PC color profile", token);
    Color::rgb(0.5, 0.5, 0.5)
}
