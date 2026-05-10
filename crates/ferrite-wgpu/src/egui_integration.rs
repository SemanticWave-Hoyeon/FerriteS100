//! egui Integration for wgpu Renderer
//!
//! Provides egui GUI overlay for the chart viewer.

use std::sync::Arc;
use winit::event::WindowEvent;
use winit::window::Window;

/// S-101 Display Mode following IHO standard ViewingGroupLayers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    /// Base display - essential navigation information only
    Base,
    /// Standard display - default operational view
    #[default]
    Standard,
    /// All - display all available information
    All,
}

impl DisplayMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisplayMode::Base => "Base",
            DisplayMode::Standard => "Standard",
            DisplayMode::All => "All",
        }
    }
}

/// S-101 Context Parameters settings
/// Following IHO S-101 standard for mariner-settable parameters
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    /// Safety depth in meters (for danger highlighting)
    pub safety_depth: f64,
    /// Safety contour in meters (primary safety boundary)
    pub safety_contour: f64,
    /// Shallow contour in meters
    pub shallow_contour: f64,
    /// Deep contour in meters
    pub deep_contour: f64,
    /// Two shades depth display (simplified)
    pub two_shades: bool,
    /// Use simplified point symbols (vs paper chart symbols)
    pub simplified_symbols: bool,
    /// Display isolated dangers in shallow water
    pub isolated_dangers: bool,
    /// Show full light sectors
    pub full_light_sectors: bool,
    /// Ignore scale minimum attribute
    pub ignore_scale_minimum: bool,
    /// Use plain boundaries (vs symbolized)
    pub plain_boundaries: bool,
    /// Display mode (Base/Standard/All)
    pub display_mode: DisplayMode,
    /// Show shallow water pattern overlay (DIAMOND1 - areas less than safety contour)
    pub show_shallow_pattern: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            safety_depth: 30.0,
            safety_contour: 30.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            two_shades: false,
            simplified_symbols: false,
            isolated_dangers: true,
            full_light_sectors: true,
            ignore_scale_minimum: false,
            plain_boundaries: false,
            display_mode: DisplayMode::Standard,
            show_shallow_pattern: true,
        }
    }
}

/// Feature Catalogue status information
#[derive(Debug, Clone, Default)]
pub struct CatalogueStatus {
    /// Whether the catalogue is loaded and valid
    pub loaded: bool,
    /// Product ID (e.g., "S-101")
    pub product_id: String,
    /// Version string
    pub version: String,
    /// File path
    pub path: String,
    /// Number of items (feature types for FC, symbols for PC)
    pub item_count: usize,
    /// Validation status message
    pub validation_message: Option<String>,
}

/// Plugin toolbar button state
#[derive(Debug, Clone, Default)]
pub struct PluginButton {
    pub plugin_id: String,
    pub label: String,
    pub tooltip: Option<String>,
    pub active: bool,
}

/// Application state shared between egui UI and main app
#[derive(Debug, Clone, Default)]
pub struct AppUiState {
    /// Application version
    pub version: String,
    /// Current cursor position in world coordinates (lon, lat)
    pub cursor_world: (f64, f64),
    /// Current cursor position in screen coordinates
    pub cursor_screen: (f32, f32),
    /// Current zoom level
    pub zoom_level: f64,
    /// Currently loaded chart file name or count
    pub loaded_chart: Option<String>,
    /// Total number of features
    pub feature_count: usize,
    /// Number of loaded chart cells
    pub chart_count: usize,
    /// Selected feature info
    pub selected_feature: Option<SelectedFeature>,
    /// Request to open file dialog (chart files)
    pub open_file_requested: bool,
    /// Request to open Feature Catalogue
    pub open_fc_requested: bool,
    /// Request to open Portrayal Catalogue
    pub open_pc_requested: bool,
    /// Request to save screenshot
    pub screenshot_requested: bool,
    /// Request to zoom in
    pub zoom_in_requested: bool,
    /// Request to zoom out
    pub zoom_out_requested: bool,
    /// Request to reset view
    pub reset_view_requested: bool,
    /// Request to clear all loaded charts
    pub clear_charts_requested: bool,
    /// Show about dialog
    pub show_about: bool,
    /// Show catalogues dialog
    pub show_catalogues: bool,
    /// Current color profile name (Day, Dusk, Night)
    pub color_profile: String,
    /// Color profile was changed
    pub color_profile_changed: bool,
    /// Feature Catalogue status
    pub fc_status: CatalogueStatus,
    /// Portrayal Catalogue status
    pub pc_status: CatalogueStatus,
    /// Loading in progress (total files, loaded count)
    pub loading_progress: Option<(usize, usize)>,
    /// Show settings dialog
    pub show_settings: bool,
    /// Current settings state
    pub settings: SettingsState,
    /// Settings were changed (triggers Lua re-run + re-render)
    pub settings_changed: bool,
    /// Settings are dirty (waiting for pointer release to apply)
    /// Pending settings (edited but not yet applied)
    pub pending_settings: Option<SettingsState>,
    /// Actual chart area after UI panels (x, y, width, height)
    /// Used for proper viewport calculation excluding UI panels
    pub chart_area: (f32, f32, f32, f32),
    /// Plugin toolbar buttons
    pub plugin_buttons: Vec<PluginButton>,
    /// Plugin toggle request (plugin_id)
    pub plugin_toggle_requested: Option<String>,
    /// Active plugin UI data (plugin_id, json_data)
    pub plugin_ui_data: Vec<(String, String)>,
    /// Plugin UI event queue (plugin_id, event_json)
    pub plugin_ui_events: Vec<(String, String)>,
    /// Previous chart area x offset (for detecting panel changes)
    prev_chart_x: f32,
    /// Pan offset adjustment needed due to panel changes (in pixels)
    pub pan_adjust_pixels: Option<f32>,
    /// Route being edited (route_id, current_edit_text)
    route_editing: Option<(u32, String)>,
    /// Waypoint being edited (waypoint_id, current_edit_text)
    waypoint_editing: Option<(u32, String)>,
    /// Debug mode enabled (--debug flag)
    pub debug_mode: bool,
    /// Debug stats: FPS
    pub debug_fps: f32,
    /// Debug stats: CPU usage percentage
    pub debug_cpu_usage: f32,
    /// Debug stats: Memory usage in MB
    pub debug_memory_mb: f32,
    /// Debug stats: Render instruction count
    pub debug_instruction_count: usize,
    /// Debug stats: Symbol count
    pub debug_symbol_count: usize,
    /// S-101 explorer panel: in-progress catalogue search term.
    pub s101_explorer_search: Option<String>,
    /// S-101 explorer panel: in-progress feature lookup id (raw text).
    pub s101_explorer_feature_id: Option<String>,
    /// S-101 explorer panel: in-progress nearby radius (raw text, metres).
    pub s101_explorer_radius_m: Option<String>,
}

/// Information about a selected feature
#[derive(Debug, Clone)]
pub struct SelectedFeature {
    pub feature_type: String,
    pub feature_id: i64,
    pub primitive_type: String,
    pub attributes: Vec<(String, String)>,
    pub world_pos: (f64, f64),
    /// Definition from Feature Catalogue
    pub definition: Option<String>,
    /// Symbol name (e.g., "ISODGR01", "SOUNDG10")
    pub symbol_name: Option<String>,
}

/// Convert decimal degrees to degrees, minutes, seconds format
/// Latitude uses 2 digits (00-90), Longitude uses 3 digits (000-180)
fn format_dms(decimal_degrees: f64, is_lat: bool) -> String {
    let abs_deg = decimal_degrees.abs();
    let degrees = abs_deg.floor() as i32;
    let minutes_full = (abs_deg - degrees as f64) * 60.0;
    let minutes = minutes_full.floor() as i32;
    let seconds = (minutes_full - minutes as f64) * 60.0;

    let dir = if is_lat {
        if decimal_degrees >= 0.0 {
            "N"
        } else {
            "S"
        }
    } else if decimal_degrees >= 0.0 {
        "E"
    } else {
        "W"
    };

    if is_lat {
        // Latitude: 2 digits for degrees (00-90)
        format!("{:02}°{:02}'{:05.2}\"{}", degrees, minutes, seconds, dir)
    } else {
        // Longitude: 3 digits for degrees (000-180)
        format!("{:03}°{:02}'{:05.2}\"{}", degrees, minutes, seconds, dir)
    }
}

/// egui integration wrapper
pub struct EguiIntegration {
    pub ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

impl EguiIntegration {
    /// Create new egui integration for the given window and wgpu state
    pub fn new(
        device: &wgpu::Device,
        output_format: wgpu::TextureFormat,
        msaa_samples: u32,
        window: Arc<Window>,
    ) -> Self {
        let ctx = egui::Context::default();

        // Configure egui style for dark theme
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        ctx.set_style(style);

        let viewport_id = ctx.viewport_id();
        let state = egui_winit::State::new(
            ctx.clone(),
            viewport_id,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        let renderer = egui_wgpu::Renderer::new(device, output_format, None, msaa_samples, false);

        EguiIntegration {
            ctx,
            state,
            renderer,
        }
    }

    /// Handle winit window event, returns true if egui consumed the event
    pub fn handle_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.state.on_window_event(window, event);
        response.consumed
    }

    /// Begin egui frame
    pub fn begin_frame(&mut self, window: &Window) {
        let raw_input = self.state.take_egui_input(window);
        self.ctx.begin_pass(raw_input);
    }

    /// End egui frame and get render output
    pub fn end_frame(&mut self, window: &Window) -> egui::FullOutput {
        let output = self.ctx.end_pass();
        self.state
            .handle_platform_output(window, output.platform_output.clone());
        output
    }

    /// Check if egui wants pointer input (mouse is over UI element)
    pub fn wants_pointer_input(&self) -> bool {
        self.ctx.wants_pointer_input()
    }

    /// Render egui UI
    ///
    /// This method handles the egui rendering with proper lifetime management.
    /// The render pass lifetime issue is handled by using unsafe transmute,
    /// which is safe because we ensure the render pass is dropped before
    /// the encoder is used again.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        screen_descriptor: egui_wgpu::ScreenDescriptor,
        full_output: egui::FullOutput,
    ) {
        // Process texture deltas
        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }

        // Tessellate shapes
        let clipped_primitives = self
            .ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);

        // Update buffers
        self.renderer.update_buffers(
            device,
            queue,
            encoder,
            &clipped_primitives,
            &screen_descriptor,
        );

        // Render using scoped render pass
        // SAFETY: The render pass is created and dropped within this scope,
        // so the encoder reference remains valid throughout the render pass lifetime.
        // The transmute extends the render pass lifetime to 'static only for the
        // egui_wgpu API, but we ensure it doesn't actually outlive the encoder.
        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui_render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load, // Don't clear, overlay on top
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // SAFETY: Transmuting render_pass lifetime to 'static for egui_wgpu API compatibility.
            //
            // Security consideration: This is a KNOWN LIMITATION that should be monitored.
            // If egui_wgpu API changes to accept non-'static lifetimes, this should be removed.
            //
            // This transmute is safe because:
            // 1. render_pass is created from encoder.begin_render_pass() above
            // 2. render_pass is used ONLY within this block and dropped at line ~182
            // 3. encoder outlives render_pass (encoder is used again at line ~183+)
            // 4. No references to render_pass escape this lexical scope
            // 5. The transmute only affects the borrow checker, not the actual data layout
            //
            // Invariant: render_pass MUST be dropped before encoder is accessed again.
            #[allow(clippy::transmute_ptr_to_ref)]
            let mut render_pass: wgpu::RenderPass<'static> =
                unsafe { std::mem::transmute(render_pass) };

            self.renderer
                .render(&mut render_pass, &clipped_primitives, &screen_descriptor);

            // render_pass is dropped here, before encoder is used again
        }

        // Free textures
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }

    /// Draw the UI and return the app state changes
    pub fn draw_ui(&self, ui_state: &mut AppUiState) {
        // Combined toolbar with menu and buttons
        egui::TopBottomPanel::top("toolbar")
            .min_height(32.0)
            .show(&self.ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    // File menu
                    ui.menu_button("File", |ui| {
                        if ui.button("Open Chart...").clicked() {
                            ui_state.open_file_requested = true;
                            ui.close_menu();
                        }
                        if ui_state.chart_count > 0 && ui.button("Clear All Charts").clicked() {
                            ui_state.clear_charts_requested = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Open Feature Catalogue...").clicked() {
                            ui_state.open_fc_requested = true;
                            ui.close_menu();
                        }
                        if ui.button("Open Portrayal Catalogue...").clicked() {
                            ui_state.open_pc_requested = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Save Screenshot...").clicked() {
                            ui_state.screenshot_requested = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Exit").clicked() {
                            std::process::exit(0);
                        }
                    });

                    // View menu
                    ui.menu_button("View", |ui| {
                        if ui.button("Zoom In").clicked() {
                            ui_state.zoom_in_requested = true;
                            ui.close_menu();
                        }
                        if ui.button("Zoom Out").clicked() {
                            ui_state.zoom_out_requested = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Fit to Chart").clicked() {
                            ui_state.reset_view_requested = true;
                            ui.close_menu();
                        }
                    });

                    // Settings menu (independent top-level menu)
                    ui.menu_button("Settings", |ui| {
                        if ui.button("Display Settings...").clicked() {
                            ui_state.show_settings = true;
                            ui.close_menu();
                        }
                    });

                    // Help menu
                    ui.menu_button("Help", |ui| {
                        if ui.button("Catalogues...").clicked() {
                            ui_state.show_catalogues = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("About").clicked() {
                            ui_state.show_about = true;
                            ui.close_menu();
                        }
                    });

                    ui.separator();

                    // Zoom controls
                    if ui.button("+").on_hover_text("Zoom In").clicked() {
                        ui_state.zoom_in_requested = true;
                    }
                    if ui.button("-").on_hover_text("Zoom Out").clicked() {
                        ui_state.zoom_out_requested = true;
                    }
                    if ui.button("Fit").on_hover_text("Reset View").clicked() {
                        ui_state.reset_view_requested = true;
                    }

                    ui.separator();

                    // Color profile selector (Day/Dusk/Night)
                    ui.label("Mode:");
                    let profiles = ["Day", "Dusk", "Night"];
                    let current = if ui_state.color_profile.is_empty() {
                        "Day".to_string()
                    } else {
                        ui_state.color_profile.clone()
                    };
                    egui::ComboBox::from_id_salt("color_profile")
                        .selected_text(&current)
                        .show_ui(ui, |ui| {
                            for profile in profiles {
                                if ui.selectable_label(current == profile, profile).clicked()
                                    && current != profile
                                {
                                    ui_state.color_profile = profile.to_string();
                                    ui_state.color_profile_changed = true;
                                }
                            }
                        });

                    // Plugin toolbar buttons (disabled when no chart loaded)
                    if !ui_state.plugin_buttons.is_empty() {
                        ui.separator();
                        let chart_loaded = ui_state.chart_count > 0;
                        for btn in &ui_state.plugin_buttons {
                            let button = egui::Button::new(&btn.label).selected(btn.active);
                            let tooltip_text = if chart_loaded {
                                btn.tooltip.clone()
                            } else {
                                Some("Load a chart first".to_string())
                            };
                            let response = ui.add_enabled(chart_loaded || btn.active, button);
                            let response = if let Some(ref tooltip) = tooltip_text {
                                response.on_hover_text(tooltip)
                            } else {
                                response
                            };
                            // Only allow toggling if chart is loaded (or deactivating an active plugin)
                            if response.clicked() && (chart_loaded || btn.active) {
                                ui_state.plugin_toggle_requested = Some(btn.plugin_id.clone());
                            }
                        }
                    }
                });
            });

        // Route panel (left side) - shown when route plugin is active
        Self::draw_route_panel(&self.ctx, ui_state);

        // S-101 explorer panel — in-process index browser exposed via HostApi
        // chart-data queries. Shown when its plugin button is active.
        Self::draw_s101_explorer_panel(&self.ctx, ui_state);

        // Status bar
        egui::TopBottomPanel::bottom("status_bar")
            .min_height(28.0)
            .show(&self.ctx, |ui| {
                ui.horizontal(|ui| {
                    // Coordinate display (only show valid coords when chart is loaded)
                    if ui_state.chart_count > 0 {
                        let (lon, lat) = ui_state.cursor_world;
                        let lat_dms = format_dms(lat, true);
                        let lon_dms = format_dms(lon, false);
                        ui.label(
                            egui::RichText::new(format!("{} | {}", lat_dms, lon_dms)).size(14.0),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("--°--'--.--\"- | ---°--'--.--\"-").size(14.0),
                        );
                    }

                    ui.separator();

                    // Zoom level
                    ui.label(
                        egui::RichText::new(format!("Zoom: {:.1}x", ui_state.zoom_level))
                            .size(14.0),
                    );

                    ui.separator();

                    // Loading indicator or feature count
                    if let Some((total, loaded)) = ui_state.loading_progress {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new(format!("Loading... ({}/{})", loaded, total))
                                .size(14.0)
                                .color(egui::Color32::from_rgb(100, 180, 255)),
                        );
                    } else if ui_state.chart_count > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "Charts: {} | Features: {}",
                                ui_state.chart_count, ui_state.feature_count
                            ))
                            .size(14.0),
                        );

                        // Loaded chart name(s)
                        if let Some(ref chart) = ui_state.loaded_chart {
                            ui.separator();
                            ui.label(egui::RichText::new(chart).size(14.0));
                        }
                    } else {
                        ui.label(egui::RichText::new("No chart loaded").size(14.0));
                    }
                });
            });

        // Feature info panel (right side)
        egui::SidePanel::right("feature_panel")
            .default_width(320.0)
            .resizable(true)
            .show(&self.ctx, |ui| {
                // Panel title with larger font
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("Feature Info").size(18.0).strong());
                });
                ui.separator();

                if let Some(ref feature) = ui_state.selected_feature {
                    // Feature type - prominent display
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Type:").size(14.0).strong());
                        ui.label(
                            egui::RichText::new(&feature.feature_type)
                                .size(14.0)
                                .color(egui::Color32::from_rgb(100, 149, 237)),
                        );
                    });

                    // Symbol name (if available)
                    if let Some(ref symbol) = feature.symbol_name {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Symbol:").size(13.0).strong());
                            ui.label(
                                egui::RichText::new(symbol)
                                    .size(13.0)
                                    .color(egui::Color32::from_rgb(144, 238, 144)),
                            );
                        });
                    }

                    // Definition (if available) - displayed in a styled box for better readability
                    if let Some(ref definition) = feature.definition {
                        ui.add_space(4.0);
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(45, 55, 72))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(definition)
                                        .size(12.5)
                                        .color(egui::Color32::from_rgb(200, 210, 225)),
                                );
                            });
                    }

                    ui.add_space(4.0);

                    // Feature ID
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("ID:").size(13.0).strong());
                        ui.label(egui::RichText::new(format!("{}", feature.feature_id)).size(13.0));
                    });

                    // Primitive type
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Primitive:").size(13.0).strong());
                        ui.label(egui::RichText::new(&feature.primitive_type).size(13.0));
                    });

                    ui.add_space(4.0);

                    // Position with better formatting (DMS)
                    ui.group(|ui| {
                        ui.label(egui::RichText::new("Position").size(13.0).strong());
                        let (lon, lat) = feature.world_pos;
                        ui.label(
                            egui::RichText::new(format!("  LAT: {}", format_dms(lat, true)))
                                .size(12.0),
                        );
                        ui.label(
                            egui::RichText::new(format!("  LON: {}", format_dms(lon, false)))
                                .size(12.0),
                        );
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Attributes section
                    ui.label(egui::RichText::new("Attributes").size(14.0).strong());

                    if feature.attributes.is_empty() {
                        ui.label(
                            egui::RichText::new("  (No attributes)")
                                .size(12.0)
                                .italics(),
                        );
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                for (key, value) in &feature.attributes {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{}:", key))
                                                .size(12.0)
                                                .strong(),
                                        );
                                        ui.label(egui::RichText::new(value).size(12.0));
                                    });
                                }
                            });
                    }
                } else {
                    // No feature selected - show help
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            egui::RichText::new("Click on a feature to see details")
                                .size(13.0)
                                .italics(),
                        );
                    });

                    ui.add_space(30.0);
                    ui.separator();
                    ui.add_space(10.0);

                    // Controls section
                    ui.label(egui::RichText::new("Controls").size(14.0).strong());
                    ui.add_space(8.0);

                    egui::Grid::new("controls_grid")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Mouse wheel").size(12.0));
                            ui.label(egui::RichText::new("Zoom").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Left drag").size(12.0));
                            ui.label(egui::RichText::new("Pan").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Left click").size(12.0));
                            ui.label(egui::RichText::new("Select").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Right click").size(12.0));
                            ui.label(egui::RichText::new("Reset view").size(12.0));
                            ui.end_row();
                        });
                }
            });

        // About dialog
        if ui_state.show_about {
            egui::Window::new("About")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&self.ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(10.0);
                        ui.heading("FerriteS100");
                        ui.label("S-101 Electronic Navigational Chart Viewer");
                        ui.add_space(10.0);
                        ui.label(format!("Version {}", ui_state.version));
                        ui.add_space(5.0);
                        ui.hyperlink_to("GitHub", "https://github.com/hoyeonchoKMOU/FerriteS100");
                        ui.add_space(15.0);
                        if ui.button("Close").clicked() {
                            ui_state.show_about = false;
                        }
                    });
                });
        }

        // Catalogues dialog
        if ui_state.show_catalogues {
            egui::Window::new("S-101 Catalogues")
                .collapsible(false)
                .resizable(true)
                .min_width(450.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&self.ctx, |ui| {
                    // Feature Catalogue section
                    ui.heading("Feature Catalogue (FC)");
                    ui.add_space(4.0);

                    Self::draw_catalogue_status(ui, &ui_state.fc_status, "Feature Types");

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // Portrayal Catalogue section
                    ui.heading("Portrayal Catalogue (PC)");
                    ui.add_space(4.0);

                    Self::draw_catalogue_status(ui, &ui_state.pc_status, "Symbols/Styles");

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.vertical_centered(|ui| {
                        if ui.button("Close").clicked() {
                            ui_state.show_catalogues = false;
                        }
                    });
                });
        }

        // Settings dialog
        if ui_state.show_settings {
            Self::draw_settings_dialog(&self.ctx, ui_state);
        }

        // Calculate actual chart area from screen size and known panel dimensions
        // This avoids using CentralPanel which would consume mouse events
        let screen_rect = self.ctx.screen_rect();
        let top_panel_height = 32.0; // toolbar min_height
        let bottom_panel_height = 28.0; // status bar min_height
        let right_panel_width = 320.0; // feature panel default_width

        // Check if route panel is active (adds left panel width)
        let route_active = ui_state
            .plugin_buttons
            .iter()
            .any(|b| b.plugin_id.contains("route") && b.active);
        let left_panel_width = if route_active { 280.0 } else { 0.0 }; // route panel default_width

        let chart_x = left_panel_width;
        let chart_y = top_panel_height;
        let chart_width = (screen_rect.width() - left_panel_width - right_panel_width).max(100.0);
        let chart_height =
            (screen_rect.height() - top_panel_height - bottom_panel_height).max(100.0);

        // Detect chart_x change (panel opened/closed) and calculate pan adjustment
        // to keep the visual center in the same position
        if (chart_x - ui_state.prev_chart_x).abs() > 1.0 {
            // Half the shift to maintain visual center
            let adjust = (chart_x - ui_state.prev_chart_x) / 2.0;
            ui_state.pan_adjust_pixels = Some(adjust);
            ui_state.prev_chart_x = chart_x;
        }

        ui_state.chart_area = (chart_x, chart_y, chart_width, chart_height);

        // Debug overlay (only in debug mode)
        if ui_state.debug_mode {
            Self::draw_debug_overlay(&self.ctx, ui_state, chart_x, chart_y);
        }
    }

    /// Draw debug overlay on chart (top-left corner, green text)
    fn draw_debug_overlay(ctx: &egui::Context, ui_state: &AppUiState, chart_x: f32, chart_y: f32) {
        egui::Area::new(egui::Id::new("debug_overlay"))
            .fixed_pos(egui::pos2(chart_x + 10.0, chart_y + 10.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.style_mut().visuals.override_text_color =
                    Some(egui::Color32::from_rgb(0, 255, 0));

                let frame_response = egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180))
                    .inner_margin(egui::Margin::same(8))
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        ui.set_min_width(200.0);
                        ui.label(egui::RichText::new("DEBUG MODE").size(14.0).strong());

                        ui.separator();

                        // Performance stats
                        ui.label(format!("FPS: {:.1}", ui_state.debug_fps));
                        ui.label(format!("CPU: {:.1}%", ui_state.debug_cpu_usage));
                        ui.label(format!("RAM: {:.1} MB", ui_state.debug_memory_mb));

                        ui.separator();

                        // Render stats
                        ui.label(format!(
                            "Instructions: {}",
                            ui_state.debug_instruction_count
                        ));
                        ui.label(format!("Symbols: {}", ui_state.debug_symbol_count));
                        ui.label(format!("Charts: {}", ui_state.chart_count));
                        ui.label(format!("Features: {}", ui_state.feature_count));

                        ui.separator();

                        // View stats
                        ui.label(format!("Zoom: {:.2}x", ui_state.zoom_level));
                        if ui_state.chart_count > 0 {
                            ui.label(format!(
                                "Pos: {:.4}, {:.4}",
                                ui_state.cursor_world.1, ui_state.cursor_world.0
                            ));
                        } else {
                            ui.label("Pos: (no chart)");
                        }
                    });
                // Consume mouse events on the frame area to prevent chart from moving when dragging on overlay
                ui.interact(
                    frame_response.response.rect,
                    egui::Id::new("debug_overlay_blocker"),
                    egui::Sense::click_and_drag(),
                );
            });
    }

    /// Draw the Route panel (for route plugin)
    fn draw_route_panel(ctx: &egui::Context, ui_state: &mut AppUiState) {
        // Check if route plugin is active
        let route_active = ui_state
            .plugin_buttons
            .iter()
            .any(|b| b.plugin_id.contains("route") && b.active);
        if !route_active {
            return;
        }

        // Find route plugin UI data
        let route_data = ui_state
            .plugin_ui_data
            .iter()
            .find(|(id, _)| id.contains("route"))
            .map(|(_, data)| data.clone());

        egui::SidePanel::left("route_panel")
            .default_width(280.0)
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(data) = &route_data {
                    if let Ok(route_ui) = serde_json::from_str::<serde_json::Value>(data) {
                        let editing = route_ui.get("editing").and_then(|v| v.as_bool()).unwrap_or(false);
                        let title = route_ui.get("title").and_then(|v| v.as_str()).unwrap_or("Route Plan");

                        // Header with editing indicator
                        ui.vertical_centered(|ui| {
                            ui.heading(egui::RichText::new(title).size(18.0).strong());
                            if editing {
                                ui.label(egui::RichText::new("Click on chart to add waypoints")
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(100, 200, 100)));
                            }
                        });
                        ui.separator();

                        // Rendering toggle at top of panel
                        let rendering_enabled = route_ui.get("rendering_enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                        ui.horizontal(|ui| {
                            ui.label("Route Display:");
                            let btn_text = if rendering_enabled { "ON" } else { "OFF" };
                            let btn_color = if rendering_enabled {
                                egui::Color32::from_rgb(100, 200, 100)
                            } else {
                                egui::Color32::from_rgb(200, 100, 100)
                            };
                            if ui.add(egui::Button::new(egui::RichText::new(btn_text).color(btn_color))).clicked() {
                                ui_state.plugin_ui_events.push((
                                    "com.ferrite.route-planner".to_string(),
                                    r#"{"type":"ToggleRendering"}"#.to_string(),
                                ));
                            }
                        });
                        ui.separator();

                        // Route list section
                        if let Some(routes) = route_ui.get("routes").and_then(|v| v.as_array()) {
                            if !routes.is_empty() {
                                egui::CollapsingHeader::new(egui::RichText::new(format!("Routes ({})", routes.len())).strong())
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        let available_width = ui.available_width();
                                        for (idx, route) in routes.iter().enumerate() {
                                            let route_id = route.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                            let name = route.get("name").and_then(|v| v.as_str()).unwrap_or("Route");
                                            let wp_count = route.get("waypoint_count").and_then(|v| v.as_u64()).unwrap_or(0);
                                            let dist = route.get("total_distance").and_then(|v| v.as_str()).unwrap_or("0 NM");
                                            let is_active = route.get("active").and_then(|v| v.as_bool()).unwrap_or(false);

                                            let bg_color = if is_active {
                                                egui::Color32::from_rgb(60, 80, 100)
                                            } else {
                                                egui::Color32::from_rgb(45, 55, 72)
                                            };

                                            // Check if this route is being edited
                                            let is_editing = ui_state.route_editing.as_ref().is_some_and(|(id, _)| *id == route_id);

                                            // Track button clicks to prevent frame from consuming them
                                            let mut button_clicked = false;

                                            let frame_response = egui::Frame::new()
                                                .fill(bg_color)
                                                .corner_radius(4.0)
                                                .inner_margin(egui::Margin::symmetric(8, 4))
                                                .show(ui, |ui| {
                                                    ui.set_min_width(available_width - 16.0);
                                                    ui.horizontal(|ui| {
                                                        // Route info (left side)
                                                        ui.vertical(|ui| {
                                                            if is_editing {
                                                                // Inline text edit
                                                                if let Some((_, ref mut edit_text)) = ui_state.route_editing {
                                                                    let response = ui.add(
                                                                        egui::TextEdit::singleline(edit_text)
                                                                            .desired_width(available_width - 80.0)
                                                                            .font(egui::TextStyle::Body)
                                                                    );
                                                                    // Auto-focus on the text field
                                                                    response.request_focus();
                                                                    // Confirm on Enter or focus lost
                                                                    if response.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                                                        let new_name = edit_text.clone();
                                                                        let event = format!(
                                                                            r#"{{"type":"RenameRoute","id":{},"name":"{}"}}"#,
                                                                            route_id,
                                                                            new_name.replace('\\', "\\\\").replace('"', "\\\"")
                                                                        );
                                                                        ui_state.plugin_ui_events.push((
                                                                            "com.ferrite.route-planner".to_string(),
                                                                            event
                                                                        ));
                                                                        ui_state.route_editing = None;
                                                                    }
                                                                    // Cancel on Escape
                                                                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                                                        ui_state.route_editing = None;
                                                                    }
                                                                }
                                                            } else {
                                                                ui.label(egui::RichText::new(name).strong());
                                                            }
                                                            ui.label(egui::RichText::new(format!("{} WP • {}", wp_count, dist))
                                                                .size(11.0)
                                                                .color(egui::Color32::LIGHT_GRAY));
                                                        });

                                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                            // Delete route button - use sense to capture click properly
                                                            let delete_btn = ui.add(egui::Button::new("X").small().sense(egui::Sense::click()));
                                                            if delete_btn.on_hover_text("Delete route").clicked() {
                                                                button_clicked = true;
                                                                let event = format!(r#"{{"type":"DeleteRoute","id":{}}}"#, route_id);
                                                                ui_state.plugin_ui_events.push((
                                                                    "com.ferrite.route-planner".to_string(),
                                                                    event
                                                                ));
                                                            }
                                                            // Edit route name button
                                                            if !is_editing {
                                                                let edit_btn = ui.add(egui::Button::new("E").small().sense(egui::Sense::click()));
                                                                if edit_btn.on_hover_text("Rename route").clicked() {
                                                                    button_clicked = true;
                                                                    ui_state.route_editing = Some((route_id, name.to_string()));
                                                                }
                                                            }
                                                        });
                                                    });
                                                });

                                            // Make entire frame clickable for selection (except when editing or clicking buttons)
                                            if !is_editing && !button_clicked && frame_response.response.clicked() && !is_active {
                                                let event = format!(r#"{{"type":"SelectRoute","index":{}}}"#, idx);
                                                ui_state.plugin_ui_events.push((
                                                    "com.ferrite.route-planner".to_string(),
                                                    event
                                                ));
                                            }
                                            ui.add_space(2.0);
                                        }
                                    });
                                ui.add_space(4.0);
                            }
                        }

                        // Active route waypoints
                        if let Some(_active_idx) = route_ui.get("active_route_index").and_then(|v| v.as_u64()) {
                            ui.separator();

                            // Waypoint count and total distance for active route
                            if let Some(count) = route_ui.get("waypoint_count").and_then(|v| v.as_u64()) {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Waypoints:").strong());
                                    ui.label(format!("{}", count));
                                });
                            }
                            if let Some(dist) = route_ui.get("total_distance").and_then(|v| v.as_str()) {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Total:").strong());
                                    ui.label(dist);
                                });
                            }
                            ui.add_space(8.0);

                            // Waypoint list with delete and edit buttons
                            if let Some(waypoints) = route_ui.get("waypoints").and_then(|v| v.as_array()) {
                                egui::ScrollArea::vertical().max_height(250.0).show(ui, |ui| {
                                    let available_width = ui.available_width();

                                    for wp in waypoints {
                                        let wp_id = wp.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                        let name = wp.get("name").and_then(|v| v.as_str()).unwrap_or("WP");
                                        let pos = wp.get("position").and_then(|v| v.as_str()).unwrap_or("");
                                        let leg = wp.get("leg_distance").and_then(|v| v.as_str());

                                        // Check if this waypoint is being edited
                                        let is_wp_editing = ui_state.waypoint_editing.as_ref().is_some_and(|(id, _)| *id == wp_id);

                                        egui::Frame::new()
                                            .fill(egui::Color32::from_rgb(45, 55, 72))
                                            .corner_radius(4.0)
                                            .inner_margin(egui::Margin::symmetric(8, 4))
                                            .show(ui, |ui| {
                                                ui.set_min_width(available_width - 16.0);
                                                ui.horizontal(|ui| {
                                                    ui.vertical(|ui| {
                                                        if is_wp_editing {
                                                            // Inline text edit for waypoint name
                                                            if let Some((_, ref mut edit_text)) = ui_state.waypoint_editing {
                                                                let response = ui.add(
                                                                    egui::TextEdit::singleline(edit_text)
                                                                        .desired_width(available_width - 80.0)
                                                                        .font(egui::TextStyle::Body)
                                                                );
                                                                response.request_focus();

                                                                // Confirm on Enter
                                                                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                                                    let event = format!(
                                                                        r#"{{"type":"RenameWaypoint","id":{},"name":"{}"}}"#,
                                                                        wp_id,
                                                                        edit_text.replace('"', "\\\"")
                                                                    );
                                                                    ui_state.plugin_ui_events.push((
                                                                        "com.ferrite.route-planner".to_string(),
                                                                        event
                                                                    ));
                                                                    ui_state.waypoint_editing = None;
                                                                }
                                                                // Cancel on Escape
                                                                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                                                    ui_state.waypoint_editing = None;
                                                                }
                                                            }
                                                        } else {
                                                            ui.label(egui::RichText::new(name).strong());
                                                        }
                                                        ui.label(egui::RichText::new(pos).size(11.0).color(egui::Color32::LIGHT_GRAY));
                                                        if let Some(d) = leg {
                                                            ui.label(egui::RichText::new(format!("Leg: {}", d)).size(11.0).color(egui::Color32::from_rgb(100, 180, 255)));
                                                        }
                                                    });

                                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                        // Delete button - use sense to capture click properly
                                                        let delete_btn = ui.add(egui::Button::new("X").small().sense(egui::Sense::click()));
                                                        if delete_btn.on_hover_text("Delete waypoint").clicked() {
                                                            let event = format!(r#"{{"type":"DeleteWaypoint","id":{}}}"#, wp_id);
                                                            ui_state.plugin_ui_events.push((
                                                                "com.ferrite.route-planner".to_string(),
                                                                event
                                                            ));
                                                        }
                                                        // Edit button
                                                        if !is_wp_editing {
                                                            let edit_btn = ui.add(egui::Button::new("E").small().sense(egui::Sense::click()));
                                                            if edit_btn.on_hover_text("Rename waypoint").clicked() {
                                                                ui_state.waypoint_editing = Some((wp_id, name.to_string()));
                                                            }
                                                        }
                                                    });
                                                });
                                            });
                                        ui.add_space(2.0);
                                    }
                                });
                            }
                        } else if route_ui.get("routes").and_then(|v| v.as_array()).is_some_and(|r| r.is_empty()) {
                            ui.label(egui::RichText::new("No routes. Click 'New' to create one.").size(12.0).color(egui::Color32::GRAY));
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);

                        // Action buttons
                        if let Some(actions) = route_ui.get("actions").and_then(|v| v.as_array()) {
                            ui.horizontal_wrapped(|ui| {
                                for action in actions {
                                    let id = action.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                    let label = action.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                                    let enabled = action.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);

                                    if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                                        let event = match id {
                                            "new" => r#"{"type":"New"}"#,
                                            "finish" => r#"{"type":"Finish"}"#,
                                            "clear" => r#"{"type":"Clear"}"#,
                                            "export" => r#"{"type":"Export"}"#,
                                            "import" => r#"{"type":"Import"}"#,
                                            _ => continue,
                                        };
                                        ui_state.plugin_ui_events.push((
                                            "com.ferrite.route-planner".to_string(),
                                            event.to_string(),
                                        ));
                                    }
                                }
                            });
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);

                        // S-421 Catalogue Settings
                        egui::CollapsingHeader::new(egui::RichText::new("S-421 Catalogues").strong())
                            .default_open(false)
                            .show(ui, |ui| {
                                // FC Status
                                ui.horizontal(|ui| {
                                    ui.label("FC:");
                                    if let Some(fc) = route_ui.get("fc_status") {
                                        let loaded = fc.get("loaded").and_then(|v| v.as_bool()).unwrap_or(false);
                                        let message = fc.get("message").and_then(|v| v.as_str()).unwrap_or("Unknown");
                                        let color = if loaded {
                                            egui::Color32::from_rgb(100, 200, 100)
                                        } else {
                                            egui::Color32::from_rgb(200, 100, 100)
                                        };
                                        ui.label(egui::RichText::new(message).color(color).size(11.0));
                                    } else {
                                        ui.label(egui::RichText::new("Not loaded").color(egui::Color32::GRAY).size(11.0));
                                    }
                                });

                                // PC Status
                                ui.horizontal(|ui| {
                                    ui.label("PC:");
                                    if let Some(pc) = route_ui.get("pc_status") {
                                        let loaded = pc.get("loaded").and_then(|v| v.as_bool()).unwrap_or(false);
                                        let message = pc.get("message").and_then(|v| v.as_str()).unwrap_or("Unknown");
                                        let color = if loaded {
                                            egui::Color32::from_rgb(100, 200, 100)
                                        } else {
                                            egui::Color32::from_rgb(200, 100, 100)
                                        };
                                        ui.label(egui::RichText::new(message).color(color).size(11.0));
                                    } else {
                                        ui.label(egui::RichText::new("Not loaded").color(egui::Color32::GRAY).size(11.0));
                                    }
                                });

                                ui.add_space(4.0);
                                ui.label(egui::RichText::new("Catalogue path: ./Catalogues/*/S-421/").size(10.0).color(egui::Color32::GRAY));
                            });

                        ui.add_space(4.0);

                        // Instructions based on mode
                        if editing {
                            ui.label(egui::RichText::new("Editing Mode:").size(12.0).strong());
                            ui.label(egui::RichText::new("• Left click: Add waypoint").size(11.0));
                            ui.label(egui::RichText::new("• Right click: Remove last").size(11.0));
                            ui.label(egui::RichText::new("• Click 'Finish' when done").size(11.0));
                        } else if route_ui.get("active_route_index").is_some() {
                            ui.label(egui::RichText::new("Click 'New' to add another route").size(12.0).color(egui::Color32::GRAY));
                        } else {
                            ui.label(egui::RichText::new("Click 'New' to start a route").size(12.0).color(egui::Color32::GRAY));
                        }
                    }
                } else {
                    ui.vertical_centered(|ui| {
                        ui.heading(egui::RichText::new("Route Plan").size(18.0).strong());
                    });
                    ui.separator();
                    ui.label("Loading plugin...");
                }
            });
    }

    /// Draw the S-101 Explorer side panel.
    ///
    /// The plugin emits `PanelData` JSON via `get_ui_data`; the host renders
    /// it here as a tabbed panel (Catalogue search / Feature lookup / Bbox
    /// query / Nearby query / Dataset metadata). Each tab posts a UI event
    /// back to the plugin describing what was clicked + any inputs; the
    /// plugin then calls `HostApi::chart_*` and returns the result on the
    /// next `get_ui_data` cycle.
    fn draw_s101_explorer_panel(ctx: &egui::Context, ui_state: &mut AppUiState) {
        let active = ui_state
            .plugin_buttons
            .iter()
            .any(|b| b.plugin_id.contains("s101-explorer") && b.active);
        if !active {
            return;
        }
        let panel_data = ui_state
            .plugin_ui_data
            .iter()
            .find(|(id, _)| id.contains("s101-explorer"))
            .map(|(_, data)| data.clone());

        egui::SidePanel::left("s101_explorer_panel")
            .default_width(360.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("S-101 Explorer");
                ui.label(
                    egui::RichText::new("In-process index browser (read-only)")
                        .size(11.0)
                        .color(egui::Color32::from_rgb(150, 150, 160)),
                );
                ui.separator();

                let Some(data) = panel_data else {
                    ui.label("Plugin loading...");
                    return;
                };
                let Ok(panel) = serde_json::from_str::<serde_json::Value>(&data) else {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 100, 100),
                        "Plugin sent malformed UI data",
                    );
                    return;
                };

                let chart_loaded = panel
                    .get("chart_loaded")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !chart_loaded {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 160, 80),
                        "Load a chart first to enable explorer queries.",
                    );
                    return;
                }

                if let Some(meta) = panel.get("metadata_summary").and_then(|v| v.as_str()) {
                    ui.label(meta);
                    ui.separator();
                }

                // Search input
                let search_term_default = panel
                    .get("input_search")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                ui.label(egui::RichText::new("Catalogue search").strong());
                let mut search_term = ui_state
                    .s101_explorer_search
                    .clone()
                    .unwrap_or_else(|| search_term_default.to_string());
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut search_term)
                        .desired_width(f32::INFINITY)
                        .hint_text("e.g. buoy, anchor, depth"),
                );
                ui_state.s101_explorer_search = Some(search_term.clone());
                let do_search = ui.button("Search").clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if do_search {
                    ui_state.plugin_ui_events.push((
                        "com.ferrite.s101-explorer".to_string(),
                        serde_json::json!({
                            "kind": "catalogue_search",
                            "term": search_term
                        })
                        .to_string(),
                    ));
                }
                ui.separator();

                // Feature lookup
                ui.label(egui::RichText::new("Feature lookup").strong());
                let mut id_str = ui_state
                    .s101_explorer_feature_id
                    .clone()
                    .unwrap_or_default();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut id_str)
                        .desired_width(f32::INFINITY)
                        .hint_text("Feature ID (numeric)"),
                );
                ui_state.s101_explorer_feature_id = Some(id_str.clone());
                let do_lookup = ui.button("Lookup").clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if do_lookup {
                    if let Ok(id) = id_str.trim().parse::<i64>() {
                        ui_state.plugin_ui_events.push((
                            "com.ferrite.s101-explorer".to_string(),
                            serde_json::json!({"kind": "feature_get", "id": id}).to_string(),
                        ));
                    }
                }
                ui.separator();

                // Nearby (uses last clicked world position)
                ui.label(egui::RichText::new("Nearby (uses cursor position)").strong());
                let mut radius_str = ui_state
                    .s101_explorer_radius_m
                    .clone()
                    .unwrap_or_else(|| "500".to_string());
                ui.horizontal(|ui| {
                    ui.label("Radius (m):");
                    ui.add(egui::TextEdit::singleline(&mut radius_str).desired_width(80.0));
                });
                ui_state.s101_explorer_radius_m = Some(radius_str.clone());
                if ui.button("Find nearby (cursor)").clicked() {
                    if let Ok(radius) = radius_str.trim().parse::<f64>() {
                        let (lon, lat) = ui_state.cursor_world;
                        ui_state.plugin_ui_events.push((
                            "com.ferrite.s101-explorer".to_string(),
                            serde_json::json!({
                                "kind": "feature_nearby",
                                "lat": lat,
                                "lon": lon,
                                "radius_m": radius
                            })
                            .to_string(),
                        ));
                    }
                }
                ui.separator();

                // Result area
                ui.label(egui::RichText::new("Result").strong());
                if let Some(result) = panel.get("last_result").and_then(|v| v.as_str()) {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(280.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut result.to_string())
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(14)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                } else {
                    ui.label("(no query run yet)");
                }

                ui.separator();

                // MCP server connection — for registering the same tools
                // with an external LLM client (Claude Desktop / ChatGPT / …).
                if let Some(conn) = panel.get("connection_info") {
                    egui::CollapsingHeader::new(
                        egui::RichText::new("MCP server registration").strong(),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        let binary_path = conn
                            .get("binary_path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let binary_found = conn
                            .get("binary_found")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let chart_path = conn
                            .get("chart_path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let catalogue_path = conn
                            .get("catalogue_path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let command_line = conn
                            .get("command_line")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let snippet = conn
                            .get("config_snippet")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        if !binary_found {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 160, 70),
                                format!(
                                    "s101-mcp not found at {} — build with `cargo build --release -p s101-mcp` first.",
                                    binary_path
                                ),
                            );
                        }
                        ui.label(format!("Binary:    {}", binary_path));
                        ui.label(format!("Chart:     {}", chart_path));
                        ui.label(format!("Catalogue: {}", catalogue_path));

                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("Shell command")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(160, 170, 200)),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(command_line.to_string());
                            }
                        });
                        ui.add(
                            egui::TextEdit::multiline(&mut command_line.to_string())
                                .desired_width(f32::INFINITY)
                                .desired_rows(2)
                                .font(egui::TextStyle::Monospace),
                        );

                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("claude_desktop_config.json snippet")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(160, 170, 200)),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(snippet.to_string());
                            }
                            ui.label(
                                egui::RichText::new(
                                    "Same JSON works in any MCP-aware client (ChatGPT, OpenRouter, etc).",
                                )
                                .size(10.0)
                                .color(egui::Color32::GRAY),
                            );
                        });
                        egui::ScrollArea::vertical()
                            .max_height(200.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut snippet.to_string())
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(10)
                                        .font(egui::TextStyle::Monospace),
                                );
                            });
                    });
                }
            });
    }

    /// Draw the Settings dialog
    fn draw_settings_dialog(ctx: &egui::Context, ui_state: &mut AppUiState) {
        // Initialize pending settings when dialog opens
        if ui_state.pending_settings.is_none() {
            ui_state.pending_settings = Some(ui_state.settings.clone());
        }

        let mut open = ui_state.show_settings;
        egui::Window::new("S-101 Display Settings")
            .collapsible(false)
            .resizable(false)
            .min_width(400.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // Get mutable reference to pending settings
                let pending = ui_state.pending_settings.as_mut().unwrap();

                ui.add_space(4.0);

                // Display Mode section
                ui.heading("Display Mode");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Mode:");
                    ui.add_space(8.0);
                    let modes = [DisplayMode::Base, DisplayMode::Standard, DisplayMode::All];
                    for mode in modes {
                        if ui
                            .selectable_label(pending.display_mode == mode, mode.as_str())
                            .clicked()
                        {
                            pending.display_mode = mode;
                        }
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(match pending.display_mode {
                        DisplayMode::Base => "Essential navigation information only",
                        DisplayMode::Standard => "Default operational display",
                        DisplayMode::All => "All available chart information",
                    })
                    .size(11.0)
                    .color(egui::Color32::GRAY),
                );

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                // Depth Contours section
                ui.heading("Depth Contours (meters)");
                ui.add_space(4.0);

                egui::Grid::new("depth_contours_grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        // Safety Depth
                        ui.label("Safety Depth:");
                        ui.add(
                            egui::DragValue::new(&mut pending.safety_depth)
                                .speed(0.5)
                                .range(0.0..=1000.0)
                                .suffix(" m"),
                        );
                        ui.end_row();

                        // Safety Contour
                        ui.label("Safety Contour:");
                        ui.add(
                            egui::DragValue::new(&mut pending.safety_contour)
                                .speed(0.5)
                                .range(0.0..=1000.0)
                                .suffix(" m"),
                        );
                        ui.end_row();

                        // Shallow Contour
                        ui.label("Shallow Contour:");
                        ui.add(
                            egui::DragValue::new(&mut pending.shallow_contour)
                                .speed(0.5)
                                .range(0.0..=1000.0)
                                .suffix(" m"),
                        );
                        ui.end_row();

                        // Deep Contour
                        ui.label("Deep Contour:");
                        ui.add(
                            egui::DragValue::new(&mut pending.deep_contour)
                                .speed(0.5)
                                .range(0.0..=1000.0)
                                .suffix(" m"),
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                // Display Options section
                ui.heading("Display Options");
                ui.add_space(4.0);

                egui::Grid::new("display_options_grid")
                    .num_columns(2)
                    .spacing([20.0, 6.0])
                    .show(ui, |ui| {
                        // Shallow Water Pattern
                        ui.checkbox(&mut pending.show_shallow_pattern, "Shallow Pattern")
                            .on_hover_text("Show diamond pattern overlay on areas shallower than safety contour");

                        // Two Shades
                        ui.checkbox(&mut pending.two_shades, "Two Shades")
                            .on_hover_text("Simplified two-color depth shading");
                        ui.end_row();

                        // Simplified Symbols
                        ui.checkbox(&mut pending.simplified_symbols, "Simplified Symbols")
                            .on_hover_text(
                                "Use simplified point symbols instead of paper chart symbols",
                            );

                        // Isolated Dangers
                        ui.checkbox(&mut pending.isolated_dangers, "Isolated Dangers")
                            .on_hover_text("Highlight isolated dangers in shallow water");
                        ui.end_row();

                        // Full Light Sectors
                        ui.checkbox(&mut pending.full_light_sectors, "Full Light Sectors")
                            .on_hover_text("Show complete light sector arcs");

                        // Ignore Scale Minimum
                        ui.checkbox(&mut pending.ignore_scale_minimum, "Ignore Scale Min")
                            .on_hover_text(
                                "Display features regardless of scale minimum attribute",
                            );
                        ui.end_row();

                        // Plain Boundaries
                        ui.checkbox(&mut pending.plain_boundaries, "Plain Boundaries")
                            .on_hover_text("Use plain lines instead of symbolized boundaries");
                        ui.end_row();
                    });

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                // Non-Lua settings (show_shallow_pattern) apply immediately for responsiveness
                if let Some(pending) = ui_state.pending_settings.as_ref() {
                    if pending.show_shallow_pattern != ui_state.settings.show_shallow_pattern {
                        ui_state.settings.show_shallow_pattern = pending.show_shallow_pattern;
                    }
                }

                // Check if pending settings differ from applied (for Apply button highlight)
                let has_pending_changes = ui_state
                    .pending_settings
                    .as_ref()
                    .is_some_and(|p| *p != ui_state.settings);

                // Buttons
                ui.horizontal(|ui| {
                    if ui.button("Reset to Defaults").clicked() {
                        ui_state.pending_settings = Some(SettingsState::default());
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            ui_state.pending_settings = None;
                            ui_state.show_settings = false;
                        }

                        // Apply button: commits pending settings and triggers Lua regeneration
                        let apply_btn = egui::Button::new("Apply");
                        let apply_resp = if has_pending_changes {
                            ui.add(apply_btn)
                        } else {
                            ui.add_enabled(false, apply_btn)
                        };
                        if apply_resp.clicked() {
                            if let Some(pending) = ui_state.pending_settings.as_ref() {
                                ui_state.settings = pending.clone();
                                ui_state.settings_changed = true;
                            }
                        }
                    });
                });
            });

        // Handle window close via X button
        if !open {
            ui_state.pending_settings = None;
            ui_state.show_settings = false;
        }
    }

    /// Draw catalogue status information
    fn draw_catalogue_status(ui: &mut egui::Ui, status: &CatalogueStatus, item_label: &str) {
        egui::Grid::new(format!("catalogue_grid_{}", item_label))
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                // Status indicator
                ui.label(egui::RichText::new("Status:").strong());
                if status.loaded {
                    ui.label(
                        egui::RichText::new("Loaded").color(egui::Color32::from_rgb(100, 200, 100)),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("Not Loaded")
                            .color(egui::Color32::from_rgb(200, 100, 100)),
                    );
                }
                ui.end_row();

                if status.loaded {
                    // Product ID
                    ui.label(egui::RichText::new("Product:").strong());
                    ui.label(&status.product_id);
                    ui.end_row();

                    // Version
                    ui.label(egui::RichText::new("Version:").strong());
                    ui.label(&status.version);
                    ui.end_row();

                    // Item count
                    ui.label(egui::RichText::new(format!("{}:", item_label)).strong());
                    ui.label(format!("{}", status.item_count));
                    ui.end_row();

                    // Path
                    ui.label(egui::RichText::new("Path:").strong());
                    ui.label(
                        egui::RichText::new(&status.path)
                            .size(11.0)
                            .color(egui::Color32::GRAY),
                    );
                    ui.end_row();

                    // Validation status
                    if let Some(ref msg) = status.validation_message {
                        ui.label(egui::RichText::new("Validation:").strong());
                        let color = if msg.starts_with("Valid") {
                            egui::Color32::from_rgb(100, 200, 100)
                        } else {
                            egui::Color32::from_rgb(255, 200, 100)
                        };
                        ui.label(egui::RichText::new(msg).color(color));
                        ui.end_row();
                    }
                }
            });
    }
}
