//! `ApplicationHandler` impl: routes winit events into `ChartApp`.
//!
//! This is the longest single block in the codebase because winit dispatches
//! every input — keyboard, mouse, scroll, resize, redraw — through one
//! `window_event` method. Splitting it further would either cost a lot of
//! state plumbing or create artificial boundaries; for now the file lives
//! as one module with internal section comments grouping by event family.

use std::sync::Arc;

#[cfg(all(windows, not(debug_assertions)))]
use crate::app::error_dialog::show_error_dialog;
use tracing::{error, info};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use ferrite_wgpu::{SelectedFeature, SymbolCache, WgpuRenderer};

use crate::app::catalogue::{
    load_feature_catalogue, load_portrayal_catalogue, validate_fc, validate_pc,
};
use crate::app::icon::load_window_icon;
use crate::app::world_map::parse_world_map_coastlines;
use crate::{ChartApp, VERSION};

impl ApplicationHandler for ChartApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Load window icon
            let window_icon = load_window_icon();

            let mut window_attrs = Window::default_attributes()
                .with_title(format!("FerriteS100 v{} - S-101 Chart Viewer", VERSION))
                .with_inner_size(winit::dpi::LogicalSize::new(1920, 1080))
                .with_maximized(true);

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
                            renderer.ui_state.version = VERSION.to_string();
                            renderer.ui_state.zoom_level = self.zoom_level;
                            renderer.ui_state.fc_status = self.fc_status.clone();
                            renderer.ui_state.pc_status = self.pc_status.clone();
                            renderer.ui_state.debug_mode = self.debug_mode;
                            renderer.set_color_profile(&self.current_profile_name);

                            // Enable profiling only in debug mode
                            if self.debug_mode {
                                renderer.set_profiling_enabled(true);
                            }

                            // Only add instructions if chart is loaded
                            if self.chart_loaded {
                                let color_profile = self
                                    .pc
                                    .color_profiles
                                    .profiles
                                    .get(&self.current_profile_name);
                                let visible_vgs = self.get_visible_viewing_groups();
                                self.render_context.zoom_to_fit(self.bounds);
                                renderer.begin_frame();
                                renderer.set_lon_wrap_pixels(
                                    360.0 * self.render_context.scaler.scale_x() as f32,
                                );
                                renderer.add_world_map_lines(&self.render_context.scaler);
                                renderer.add_instructions_with_symbols(
                                    &mut self.render_context,
                                    Some(&mut self.symbol_cache),
                                    color_profile,
                                    visible_vgs.as_ref(),
                                );
                                self.build_rendered_symbols();
                            }

                            // Load Natural Earth world map for background rendering
                            let coastlines = parse_world_map_coastlines();
                            renderer.set_world_map(coastlines);

                            // Draw world map immediately (visible even without charts)
                            if !self.chart_loaded {
                                self.render_context.zoom_to_fit(self.bounds);
                                renderer.begin_frame();
                                renderer.set_lon_wrap_pixels(
                                    360.0 * self.render_context.scaler.scale_x() as f32,
                                );
                                renderer.add_world_map_lines(&self.render_context.scaler);
                            }

                            let stats = renderer.statistics();
                            info!(
                                "GPU Renderer initialized: {} (symbols cached: {})",
                                stats,
                                self.symbol_cache.len()
                            );

                            self.renderer = Some(renderer);

                            // Auto-load chart if --chart was specified
                            if !self.pending_auto_chart.is_empty() {
                                let paths = std::mem::take(&mut self.pending_auto_chart);
                                info!("Auto-loading {} chart file(s)", paths.len());
                                if let Err(e) = self.load_charts(&paths) {
                                    error!("Failed to auto-load chart(s): {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format!(
                                "Failed to initialize GPU renderer:\n\n{}\n\n\
                                 This may be caused by missing or outdated graphics drivers.",
                                e
                            );
                            error!("{}", msg);
                            #[cfg(all(windows, not(debug_assertions)))]
                            show_error_dialog("FerriteS100 - Renderer Error", &msg);
                            event_loop.exit();
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to create window:\n\n{}", e);
                    error!("{}", msg);
                    #[cfg(all(windows, not(debug_assertions)))]
                    show_error_dialog("FerriteS100 - Window Error", &msg);
                    event_loop.exit();
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
        // Intercept Tab key before egui (egui uses Tab for focus navigation)
        if let WindowEvent::KeyboardInput {
            event:
                winit::event::KeyEvent {
                    physical_key: winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Tab),
                    state: winit::event::ElementState::Pressed,
                    repeat: false,
                    ..
                },
            ..
        } = &event
        {
            self.debug_mode = !self.debug_mode;
            if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.debug_mode = self.debug_mode;
                renderer.set_profiling_enabled(self.debug_mode);
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return;
        }

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
                // Flush profiler report before exit
                if let Some(renderer) = &mut self.renderer {
                    renderer.flush_profiler();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(physical_size) => {
                // Skip resize handling for minimized window (size 0x0)
                if physical_size.width == 0 || physical_size.height == 0 {
                    return;
                }

                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(physical_size);
                    // Reset renderer's screen pan offset (will be recalculated by update_view)
                    renderer.reset_pan_offset();

                    // Update render context viewport (preserve zoom level)
                    self.render_context
                        .set_viewport(physical_size.width as f32, physical_size.height as f32);

                    // Re-apply current view (zoom + pan) instead of resetting
                    // Must always update — world map lines need rebuild with new viewport
                    self.update_view();
                }
            }
            WindowEvent::RedrawRequested => {
                // Poll for completed async hit-test build
                self.poll_hit_test();

                // Process inertia/momentum
                let now = std::time::Instant::now();
                let dt = now.duration_since(self.last_frame_time).as_secs_f64();
                self.last_frame_time = now;

                // Frame profiling: begin frame
                let profiling_enabled = ferrite_wgpu::profiler::is_profiling_enabled();
                if profiling_enabled {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.cpu_profiler.begin_frame();
                    }
                }

                // Apply pan velocity (inertia) — iOS-style deceleration
                // Uses exponential decay: v(t) = v0 * decel^t
                // decel_rate ~0.998 per ms gives natural-feeling momentum
                let velocity_magnitude =
                    (self.pan_velocity.0.powi(2) + self.pan_velocity.1.powi(2)).sqrt();
                if velocity_magnitude > 0.0001 && !self.is_dragging {
                    // Apply velocity to pan offset (using current velocity BEFORE friction)
                    self.pan_offset.0 += self.pan_velocity.0 * dt;
                    self.pan_offset.1 += self.pan_velocity.1 * dt;

                    // Calculate screen-space velocity for GPU pan offset
                    // IMPORTANT: Must use the SAME velocity as pan_offset update (pre-friction)
                    // to keep screen offset and world offset synchronized
                    let screen_vx =
                        -self.pan_velocity.0 * self.render_context.scaler.scale_x() * dt;
                    let screen_vy = self.pan_velocity.1 * self.render_context.scaler.scale_y() * dt;

                    // Decelerate: exponential decay (frame-rate independent)
                    // 0.998^ms ≈ natural iOS-like scroll momentum
                    // At 120Hz (8.33ms): friction = 0.998^8.33 ≈ 0.9834
                    // At 60Hz (16.67ms): friction = 0.998^16.67 ≈ 0.9672
                    let dt_ms = dt * 1000.0;
                    let friction = 0.998_f64.powf(dt_ms);
                    self.pan_velocity.0 *= friction;
                    self.pan_velocity.1 *= friction;

                    // Check if velocity is now very small (stopping)
                    let new_magnitude =
                        (self.pan_velocity.0.powi(2) + self.pan_velocity.1.powi(2)).sqrt();
                    if new_magnitude < 0.00001 {
                        self.pan_velocity = (0.0, 0.0);
                        // Defer rebuild to avoid frame spike on stop frame
                        self.pan_rebuild_phase = 2; // needs phase 1
                        self.pan_rebuild_time = now;
                    } else {
                        // Still moving - use fast GPU pan path
                        if let Some(renderer) = &mut self.renderer {
                            renderer.add_pan_offset(screen_vx as f32, screen_vy as f32);
                        }
                    }
                }

                // Animated zoom: smoothly interpolate toward zoom_target
                if self.zoom_animating {
                    let ratio = self.zoom_target / self.zoom_level;
                    if ratio.abs() < 1e-6 || (ratio - 1.0).abs() < 0.001 {
                        // Close enough — snap to target
                        self.zoom_level = self.zoom_target;
                        self.zoom_animating = false;
                    } else {
                        // Exponential interpolation: lerp in log-space for uniform feel
                        // ~85% toward target per frame → reaches 99% in ~5 frames
                        let t = 1.0 - 0.15_f64.powf(dt * 60.0);
                        let log_current = self.zoom_level.ln();
                        let log_target = self.zoom_target.ln();
                        self.zoom_level = (log_current + (log_target - log_current) * t).exp();
                        self.zoom_level = self.zoom_level.clamp(0.005, 100.0);
                    }

                    // Apply GPU fast-path zoom
                    let (cursor_sx, cursor_sy) = self.zoom_cursor_screen;
                    let gpu_zoom = (self.zoom_level / self.zoom_rebuilt_level) as f32;
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_gpu_zoom(gpu_zoom, cursor_sx, cursor_sy);
                        renderer.ui_state.zoom_level = self.zoom_level;
                    }

                    // Drift-free zoom: directly compute pan_offset from anchor
                    // Step 1: Temporarily zero pan_offset to get the "unshifted" view
                    self.pan_offset = (0.0, 0.0);
                    self.recalculate_view_bounds();
                    // Step 2: Find where cursor maps to world in the unshifted view
                    let screen_pt = ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy);
                    let unshifted_world = self.render_context.scaler.screen_to_world(screen_pt);
                    // Step 3: Set pan_offset so anchor stays under cursor
                    self.pan_offset.0 = self.zoom_anchor_world.0 - unshifted_world.x;
                    self.pan_offset.1 = self.zoom_anchor_world.1 - unshifted_world.y;
                    // Step 4: Re-apply bounds with correct offset
                    self.recalculate_view_bounds();

                    // Keep debounce timer fresh while animating
                    if self.zoom_animating {
                        self.zoom_last_scroll = now;
                    }
                }

                // Zoom debounce: 2-phase rebuild when scrolling/animation stops
                // Phase 2→1 (80ms): Rebuild geometry + declutter, skip hit-test
                // Phase 1→0 (300ms): Rebuild hit-test only (geometry already correct)
                if self.zoom_rebuild_phase == 2 {
                    let elapsed = now.duration_since(self.zoom_last_scroll);
                    if elapsed.as_millis() >= 80 && !self.zoom_animating {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.reset_pan_offset();
                        }
                        self.update_view_ex(false, false);
                        self.zoom_rebuilt_level = self.zoom_level;
                        self.zoom_rebuild_phase = 1;
                    }
                } else if self.zoom_rebuild_phase == 1 {
                    let elapsed = now.duration_since(self.zoom_last_scroll);
                    if elapsed.as_millis() >= 300 {
                        // Hit-test only (geometry unchanged since Phase 1)
                        self.build_rendered_symbols();
                        self.zoom_rebuild_phase = 0;
                    }
                }

                // Deferred pan rebuild after inertia/drag stops
                // Phase 2→1: Rebuild geometry + declutter, skip hit-test
                // Phase 1→0 (150ms): Rebuild hit-test only
                if self.pan_rebuild_phase == 2 {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.update_view_ex(false, false);
                    self.pan_rebuild_phase = 1;
                } else if self.pan_rebuild_phase == 1 {
                    let elapsed = now.duration_since(self.pan_rebuild_time);
                    if elapsed.as_millis() >= 150 {
                        // Hit-test only (geometry unchanged since Phase 1)
                        self.build_rendered_symbols();
                        self.pan_rebuild_phase = 0;
                    }
                }

                // Poll for background loading completion
                if self.loading_state.is_some() {
                    self.poll_loading();
                }

                // Collect UI requests first (to avoid borrow conflicts)
                let (
                    open_file,
                    open_fc,
                    open_pc,
                    screenshot,
                    zoom_in,
                    zoom_out,
                    reset_view,
                    clear_charts,
                    color_change,
                    settings_change,
                    plugin_toggle,
                ) = {
                    if let Some(renderer) = &mut self.renderer {
                        (
                            renderer.take_open_file_request(),
                            renderer.take_open_fc_request(),
                            renderer.take_open_pc_request(),
                            renderer.take_screenshot_request(),
                            renderer.take_zoom_in_request(),
                            renderer.take_zoom_out_request(),
                            renderer.take_reset_view_request(),
                            renderer.take_clear_charts_request(),
                            renderer.take_color_profile_change(),
                            renderer.take_settings_change(),
                            renderer.take_plugin_toggle_request(),
                        )
                    } else {
                        (
                            false, false, false, false, false, false, false, false, None, None,
                            None,
                        )
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

                // Handle Feature Catalogue open request
                if open_fc {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Feature Catalogue XML", &["xml"])
                        .set_title("Open Feature Catalogue")
                        .pick_folder()
                    {
                        match load_feature_catalogue(&path) {
                            Ok(new_fc) => {
                                info!("Loaded FC: {} v{}", new_fc.product_id, new_fc.version);
                                self.fc_status = validate_fc(&new_fc, &path);
                                self.fc = Arc::new(new_fc);
                                if let Some(renderer) = &mut self.renderer {
                                    renderer.ui_state.fc_status = self.fc_status.clone();
                                }
                                // Reload charts with new FC if any are loaded
                                if self.chart_loaded {
                                    self.update_view();
                                }
                            }
                            Err(e) => error!("Failed to load Feature Catalogue: {}", e),
                        }
                    }
                }

                // Handle Portrayal Catalogue open request
                if open_pc {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Open Portrayal Catalogue Directory")
                        .pick_folder()
                    {
                        match load_portrayal_catalogue(&path) {
                            Ok(new_pc) => {
                                info!("Loaded PC: {} v{}", new_pc.product_id, new_pc.version);
                                self.pc_status = validate_pc(&new_pc, &path);

                                // Reload symbol cache with new PC
                                let symbols_path = path.join("Symbols");
                                self.symbol_cache = SymbolCache::new(&symbols_path);
                                self.pc = Arc::new(new_pc);

                                if let Some(renderer) = &mut self.renderer {
                                    renderer.ui_state.pc_status = self.pc_status.clone();
                                }
                                // Reload charts with new PC if any are loaded
                                if self.chart_loaded {
                                    self.update_view();
                                }
                            }
                            Err(e) => error!("Failed to load Portrayal Catalogue: {}", e),
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
                    self.zoom_target = self.zoom_level;
                    self.zoom_animating = false;
                    self.update_view();
                }

                if zoom_out {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = (self.zoom_level / 1.5).max(0.1);
                    self.zoom_target = self.zoom_level;
                    self.zoom_animating = false;
                    self.update_view();
                }

                if reset_view {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = 1.0;
                    self.zoom_target = 1.0;
                    self.zoom_animating = false;
                    self.pan_offset = (0.0, 0.0);
                    self.pan_velocity = (0.0, 0.0); // Stop inertia on reset
                    self.update_view();
                }

                if clear_charts {
                    self.clear_charts();
                    // Deactivate all plugins (close panels) and clear plugin data
                    self.plugin_system.deactivate_all_plugins();
                    self.plugin_system.clear_all_data();
                }

                // Handle color profile change
                if let Some(new_profile) = color_change {
                    self.set_color_profile(&new_profile);
                    // Force re-render with new colors
                    if self.chart_loaded {
                        self.update_view();
                    }
                }

                // Handle settings change (S-101 context parameters)
                if settings_change.is_some() {
                    // Settings have been updated in UI state, regenerate portrayal
                    tracing::info!("Settings changed, regenerating portrayal");
                    if self.chart_loaded {
                        self.regenerate_portrayal();
                        // Pre-compute triangulations for new instructions
                        if let Some(renderer) = &mut self.renderer {
                            renderer
                                .precompute_triangulations(self.render_context.raw_instructions());
                        }
                    }
                }

                // Handle plugin toggle request (only when chart is loaded)
                if let Some(plugin_id) = plugin_toggle {
                    if self.chart_loaded {
                        self.plugin_system.toggle_plugin(&plugin_id);
                    }
                }

                // Handle pan adjustment when panel state changes (to keep chart visually centered)
                if let Some(renderer) = &mut self.renderer {
                    if let Some(adjust_pixels) = renderer.take_pan_adjust_pixels() {
                        // Convert pixel adjustment to world coordinates
                        let world_adjust =
                            adjust_pixels as f64 / self.render_context.scaler.scale_x();
                        self.pan_offset.0 += world_adjust;
                        // Force view update with new pan offset
                        if self.chart_loaded {
                            self.update_view();
                        }
                    }
                }

                // Update plugin toolbar buttons in UI
                if let Some(renderer) = &mut self.renderer {
                    let buttons: Vec<_> = self
                        .plugin_system
                        .get_toolbar_buttons()
                        .into_iter()
                        .map(|btn| ferrite_wgpu::PluginButton {
                            plugin_id: btn.plugin_id,
                            label: btn.label,
                            tooltip: btn.tooltip,
                            active: btn.active,
                        })
                        .collect();
                    renderer.set_plugin_buttons(buttons);

                    // Update plugin UI data
                    let ui_data = self.plugin_system.get_active_plugin_ui_data();
                    renderer.set_plugin_ui_data(ui_data);

                    // Process plugin UI events
                    for (plugin_id, event_json) in renderer.take_plugin_ui_events() {
                        self.plugin_system.send_ui_event(&plugin_id, &event_json);
                        // Update view after UI event
                        if self.chart_loaded {
                            self.update_view();
                        }
                    }
                }

                // Update debug stats
                if self.debug_mode {
                    if let Some(renderer) = &mut self.renderer {
                        // FPS calculation (always update for accurate measurement)
                        self.frame_times.push_back(now);
                        while self.frame_times.len() > 60 {
                            self.frame_times.pop_front();
                        }

                        // Throttle other stats to update every 0.5 seconds
                        let debug_update_interval = std::time::Duration::from_millis(500);
                        let should_update_stats =
                            now.duration_since(self.last_debug_update) >= debug_update_interval;

                        if should_update_stats {
                            self.last_debug_update = now;

                            // Calculate FPS from accumulated frame times
                            if self.frame_times.len() >= 2 {
                                let oldest = self.frame_times.front().unwrap();
                                let elapsed = now.duration_since(*oldest).as_secs_f32();
                                renderer.ui_state.debug_fps =
                                    (self.frame_times.len() - 1) as f32 / elapsed;
                            }

                            // Memory and CPU usage (Windows only)
                            #[cfg(windows)]
                            {
                                use std::mem::MaybeUninit;
                                use windows_sys::Win32::Foundation::FILETIME;
                                use windows_sys::Win32::System::ProcessStatus::{
                                    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
                                };
                                use windows_sys::Win32::System::Threading::{
                                    GetCurrentProcess, GetProcessTimes,
                                };

                                unsafe {
                                    let process = GetCurrentProcess();

                                    // Memory usage (Working Set - physical memory used)
                                    let mut pmc = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
                                    if GetProcessMemoryInfo(
                                        process,
                                        pmc.as_mut_ptr(),
                                        std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                                    ) != 0
                                    {
                                        let pmc = pmc.assume_init();
                                        renderer.ui_state.debug_memory_mb =
                                            pmc.WorkingSetSize as f32 / (1024.0 * 1024.0);
                                    }

                                    // CPU usage calculation
                                    let mut creation_time = MaybeUninit::<FILETIME>::zeroed();
                                    let mut exit_time = MaybeUninit::<FILETIME>::zeroed();
                                    let mut kernel_time = MaybeUninit::<FILETIME>::zeroed();
                                    let mut user_time = MaybeUninit::<FILETIME>::zeroed();

                                    if GetProcessTimes(
                                        process,
                                        creation_time.as_mut_ptr(),
                                        exit_time.as_mut_ptr(),
                                        kernel_time.as_mut_ptr(),
                                        user_time.as_mut_ptr(),
                                    ) != 0
                                    {
                                        let kernel = kernel_time.assume_init();
                                        let user = user_time.assume_init();

                                        // Convert FILETIME to u64 (100-nanosecond intervals)
                                        let kernel_100ns = ((kernel.dwHighDateTime as u64) << 32)
                                            | (kernel.dwLowDateTime as u64);
                                        let user_100ns = ((user.dwHighDateTime as u64) << 32)
                                            | (user.dwLowDateTime as u64);

                                        if let Some((prev_kernel, prev_user, prev_time)) =
                                            self.prev_cpu_times
                                        {
                                            let wall_elapsed =
                                                now.duration_since(prev_time).as_nanos() as u64
                                                    / 100;
                                            if wall_elapsed > 0 {
                                                let cpu_elapsed = (kernel_100ns - prev_kernel)
                                                    + (user_100ns - prev_user);
                                                let num_cpus = std::thread::available_parallelism()
                                                    .map(|n| n.get())
                                                    .unwrap_or(1)
                                                    as f32;
                                                renderer.ui_state.debug_cpu_usage =
                                                    (cpu_elapsed as f32 / wall_elapsed as f32)
                                                        * 100.0
                                                        / num_cpus;
                                            }
                                        }

                                        self.prev_cpu_times = Some((kernel_100ns, user_100ns, now));
                                    }
                                }
                            }

                            // Instruction and symbol counts
                            renderer.ui_state.debug_instruction_count =
                                self.render_context.instruction_count();
                            renderer.ui_state.debug_symbol_count = self.rendered_symbols.len();
                        }
                    }
                }

                // Render
                if let Some(renderer) = &mut self.renderer {
                    if let Err(e) = renderer.render() {
                        error!("Render error: {}", e);
                    }

                    // Frame profiling: end frame (logs periodic report)
                    if profiling_enabled {
                        renderer.cpu_profiler.end_frame();
                    }
                }

                // Auto-screenshot: wait a few frames after load for rendering to stabilize
                if let Some(count) = &mut self.frames_since_loaded {
                    *count += 1;
                    if *count >= 5 {
                        if let Some(path) = self.auto_screenshot.take() {
                            info!("Auto-screenshot: saving to {}", path.display());
                            if let Some(renderer) = &mut self.renderer {
                                match renderer.save_screenshot(&path) {
                                    Ok(_) => info!("Screenshot saved successfully"),
                                    Err(e) => error!("Screenshot failed: {}", e),
                                }
                            }
                            self.frames_since_loaded = None;
                            // Exit after screenshot
                            event_loop.exit();
                            return;
                        }
                    }
                }

                // Request next frame only when needed (on-demand rendering)
                // During drag/inertia: keep requesting frames at VSync rate for smooth motion
                let needs_redraw = {
                    let has_inertia =
                        self.pan_velocity.0.abs() > 0.00001 || self.pan_velocity.1.abs() > 0.00001;
                    let has_loading = self.loading_state.is_some();
                    let has_screenshot_pending = self.frames_since_loaded.is_some();
                    let has_zoom_pending = self.zoom_rebuild_phase > 0;
                    let egui_needs = self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.egui_needs_repaint());
                    let has_pan_rebuild = self.pan_rebuild_phase > 0;
                    self.is_dragging
                        || self.zoom_animating
                        || has_inertia
                        || has_loading
                        || has_screenshot_pending
                        || has_zoom_pending
                        || has_pan_rebuild
                        || egui_needs
                };
                if needs_redraw {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
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
                if !egui_consumed && self.is_dragging {
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
                    self.pan_rebuild_phase = 0;

                    // FAST PATH: Use GPU pan offset instead of rebuilding vertices
                    // This is much faster than update_view_ex which rebuilds all geometry
                    if let Some(renderer) = &mut self.renderer {
                        renderer.add_pan_offset(dx as f32, dy as f32);
                    }
                }

                self.mouse_pos = new_pos;

                // Request redraw for cursor updates and drag rendering
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !egui_consumed => {
                let scroll_amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(pos) => pos.y / 50.0,
                };

                // Accumulate into zoom target (animated zoom will interpolate toward it)
                let zoom_factor = 1.0 + scroll_amount * 0.15;
                if !self.zoom_animating {
                    self.zoom_target = self.zoom_level;
                }
                self.zoom_target = (self.zoom_target * zoom_factor).clamp(0.005, 100.0);
                self.zoom_animating = true;

                // Record cursor as zoom pivot + anchor world point
                let cursor_sx = self.mouse_pos.0 as f32;
                let cursor_sy = self.mouse_pos.1 as f32;
                self.zoom_cursor_screen = (cursor_sx, cursor_sy);
                // Capture the world point under cursor — used for drift-free zoom
                let anchor = self
                    .render_context
                    .scaler
                    .screen_to_world(ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy));
                self.zoom_anchor_world = (anchor.x, anchor.y);

                // Mark rebuild pending — will execute when animation stops
                self.zoom_last_scroll = std::time::Instant::now();
                self.zoom_rebuild_phase = 2;

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if !egui_consumed => {
                match state {
                    ElementState::Pressed => {
                        // If inertia was active, stop it and rebuild view immediately
                        // so the scaler reflects the actual pan offset (not the stale GPU offset)
                        let had_inertia =
                            self.pan_velocity.0.abs() > 0.001 || self.pan_velocity.1.abs() > 0.001;
                        self.is_dragging = true;
                        self.drag_start = self.mouse_pos;
                        // Clear recent positions and velocity on new drag
                        self.recent_positions.clear();
                        self.pan_velocity = (0.0, 0.0);
                        if had_inertia {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.reset_pan_offset();
                            }
                            self.update_view();
                            self.pan_rebuild_phase = 0;
                        }
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
                                    // 0.7 gives good momentum while preventing overshoot
                                    self.pan_velocity = (world_vx * 0.7, world_vy * 0.7);
                                    inertia_applied = true;
                                }
                            }
                            self.recent_positions.clear();
                        }

                        // If drag ended without inertia, defer rebuild
                        if was_dragging && !inertia_applied {
                            self.pan_rebuild_phase = 2;
                            self.pan_rebuild_time = std::time::Instant::now();
                        }

                        // Suppress click during inertia — coordinates are stale while map is moving
                        let has_inertia =
                            self.pan_velocity.0.abs() > 0.001 || self.pan_velocity.1.abs() > 0.001;

                        if (!was_dragging || drag_dist < 5.0) && !has_inertia {
                            // Check if egui wants the pointer (click is on UI)
                            let egui_wants = self
                                .renderer
                                .as_ref()
                                .is_some_and(|r| r.egui_wants_pointer());

                            // Skip chart/plugin handling if click was on UI or no chart loaded
                            if !egui_wants && self.chart_loaded {
                                let (x, y) = self.mouse_pos;
                                info!("Click: screen=({:.1}, {:.1})", x, y);
                                let screen_pt =
                                    ferrite_render::ScreenPoint::new(x as f32, y as f32);
                                let world = self.render_context.scaler.screen_to_world(screen_pt);
                                info!("Click: world=({:.6}, {:.6})", world.x, world.y);

                                // Route click to plugins first
                                let plugin_consumed = self.plugin_system.handle_click(
                                    world.x,
                                    world.y,
                                    ferrite_plugin_api::MouseButton::Left,
                                    false,
                                );

                                // If plugin consumed the event, skip default handling but update view
                                if plugin_consumed {
                                    info!(
                                        "Click consumed by plugin at world ({:.6}, {:.6})",
                                        world.x, world.y
                                    );
                                    // Update view to render plugin's new drawing instructions
                                    self.update_view();
                                } else {
                                    // Hit testing (default behavior)
                                    // Find nearby symbols (sorted by priority then distance)
                                    let nearby = self.find_symbols_at(x, y, 20.0);

                                    let selected = nearby.first().map(|sym| {
                                        // Use cell_index to look up feature in the correct cell
                                        // This fixes the bug where multiple cells have the same feature_id
                                        // but different feature types
                                        let feature = if let Some(cell_idx) = sym.cell_index {
                                            // Look up in the specific cell the symbol came from
                                            self.cells
                                                .get(cell_idx as usize)
                                                .and_then(|cell| cell.features.get(&sym.feature_id))
                                        } else {
                                            // Fallback: search all cells (old behavior)
                                            self.cells
                                                .iter()
                                                .find_map(|cell| cell.features.get(&sym.feature_id))
                                        };

                                        let (feature_code, definition) = feature
                                            .map(|f| {
                                                let code = f
                                                    .feature_code
                                                    .as_deref()
                                                    .unwrap_or(&sym.symbol_ref);
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
                        } // end if !egui_wants
                    }
                }

                // Request redraw after click/release events
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } if !egui_consumed => {
                // Check if egui wants the pointer (click is on UI)
                let egui_wants = self
                    .renderer
                    .as_ref()
                    .is_some_and(|r| r.egui_wants_pointer());

                // Skip if click was on UI
                if !egui_wants {
                    // Route right-click to plugins first (only when chart is loaded)
                    let plugin_consumed = if self.chart_loaded {
                        let screen_pt = ferrite_render::ScreenPoint::new(
                            self.mouse_pos.0 as f32,
                            self.mouse_pos.1 as f32,
                        );
                        let world = self.render_context.scaler.screen_to_world(screen_pt);
                        let consumed = self.plugin_system.handle_click(
                            world.x,
                            world.y,
                            ferrite_plugin_api::MouseButton::Right,
                            false,
                        );
                        if consumed {
                            info!(
                                "Right-click consumed by plugin at ({:.4}, {:.4})",
                                world.x, world.y
                            );
                            self.update_view();
                        }
                        consumed
                    } else {
                        false
                    };

                    if !plugin_consumed {
                        // Plugin didn't consume, do default behavior (reset view)
                        if let Some(renderer) = &mut self.renderer {
                            renderer.reset_pan_offset();
                        }
                        self.zoom_level = 1.0;
                        self.zoom_target = 1.0;
                        self.zoom_animating = false;
                        self.pan_offset = (0.0, 0.0);
                        self.pan_velocity = (0.0, 0.0); // Stop inertia on reset
                        self.update_view();
                    }
                } // end if !egui_wants

                // Request redraw after right-click
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {
                // For any other window event (keyboard, etc.), request redraw
                // to ensure egui UI updates are rendered
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }
}
