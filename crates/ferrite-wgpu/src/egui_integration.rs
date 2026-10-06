//! egui Integration for wgpu Renderer
//!
//! Provides egui GUI overlay for the chart viewer.

use crate::object_details::{detail_section, draw_selected_object_details, ObjectDetailSections};
use crate::ui_chrome::{icon_button, selection_emphasis, Icon, Theme};
use ferrite_render::{TemporalView, TemporalViewMode};
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
    /// Additional catalogue layer IDs, independent of display modes.
    pub viewing_layers: std::collections::BTreeSet<String>,
    pub interoperability_enabled: bool,
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
            viewing_layers: Default::default(),
            interoperability_enabled: true,
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
    pub reduced_motion: bool,
    pub globe_preview: bool,
    pub view_mode_changed: bool,
    pub globe_summary: String,
    pub globe_tilt_deg: f64,
    pub globe_range_factor: f64,
    pub globe_pose: Option<ferrite_kernel::globe_navigation::GlobePose>,
    pub fit_globe_requested: bool,
    pub object_detail_sections: ObjectDetailSections,
    pub object_attribute_query: String,
    pub open_exchange_requested: bool,
    pub close_requested: bool,
    pub notice: Option<String>,
    selection_ui_identity: Option<(Option<u32>, i64)>,
    selection_ui_started: f64,
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
    pub selection_candidates: Vec<SelectedFeature>,
    pub selection_requested: Option<usize>,
    pub bathymetry_count: usize,
    pub coverage_info: Option<String>,
    pub security_status: String,
    /// Application load policy, separate from portrayal settings. Default OFF.
    pub verify_dataset_signatures: bool,
    pub interoperability_status: String,
    pub interoperability_available: bool,
    pub interoperability_active: bool,
    pub security_details: String,
    /// Request to open file dialog (chart files)
    pub open_file_requested: bool,
    /// Request to open Feature Catalogue
    pub open_fc_requested: bool,
    /// Request to open Portrayal Catalogue
    pub open_pc_requested: bool,
    /// Request to switch an FC/PC pair together.
    pub open_catalogue_set_requested: bool,
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
    pub show_temporal: bool,
    pub temporal_view: TemporalView,
    pub pending_temporal_view: Option<TemporalView>,
    pub temporal_changed: bool,
    /// Current settings state
    pub settings: SettingsState,
    /// Installed catalogue optional layer IDs and user-facing names.
    pub optional_viewing_layers: Vec<(String, String)>,
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
    /// Debug overlay and profiling enabled (toolbar, F12, or --debug).
    pub debug_mode: bool,
    /// Debug stats: FPS
    pub debug_fps: f32,
    /// Debug stats: CPU usage percentage
    pub debug_cpu_usage: Option<f32>,
    /// Debug stats: Memory usage in MB
    pub debug_memory_mb: Option<f32>,
    /// Debug stats: Render instruction count
    pub debug_instruction_count: usize,
    /// Debug stats: Symbol count
    pub debug_symbol_count: usize,
}

/// Information about a selected feature
#[derive(Debug, Clone)]
pub struct SelectedFeature {
    pub feature_type: String,
    pub feature_id: i64,
    pub foid: Option<String>,
    pub cell_index: Option<u32>,
    pub primitive_type: String,
    pub source: Option<String>,
    pub attributes: Vec<(String, String)>,
    pub world_pos: (f64, f64),
    /// Display copy; source coordinates and identity remain unchanged.
    pub longitude_shift: f64,
    /// Definition from Feature Catalogue
    pub definition: Option<String>,
    /// Symbol name (e.g., "ISODGR01", "SOUNDG10")
    pub symbol_name: Option<String>,
}

/// Convert decimal degrees to degrees, minutes, seconds format
/// Latitude uses 2 digits (00-90), Longitude uses 3 digits (000-180)
pub(crate) fn format_dms(decimal_degrees: f64, is_lat: bool) -> String {
    let limit = if is_lat { 90. } else { 180. };
    if !decimal_degrees.is_finite() || decimal_degrees.abs() > limit {
        return if is_lat {
            "Invalid WGS84 latitude"
        } else {
            "Invalid WGS84 longitude"
        }
        .into();
    }
    // Round once in integer hundredths of an arc-second. Carry propagates into
    // minutes and degrees, so a binary value just below a minute never prints 60s.
    let total = (decimal_degrees.abs() * 360_000.).round() as u64;
    let degrees = total / 360_000;
    let minutes = (total / 6_000) % 60;
    let seconds = (total % 6_000) as f64 / 100.;
    let direction = match (is_lat, decimal_degrees >= 0.) {
        (true, true) => "N",
        (true, false) => "S",
        (false, true) => "E",
        (false, false) => "W",
    };
    if is_lat {
        format!("{degrees:02}°{minutes:02}'{seconds:05.2}\"{direction}")
    } else {
        format!("{degrees:03}°{minutes:02}'{seconds:05.2}\"{direction}")
    }
}

#[cfg(test)]
mod coordinate_format_tests {
    use super::format_dms;
    #[test]
    fn seconds_round_with_carry_and_invalid_positions_are_disclosed() {
        assert_eq!(format_dms(50.8, true), "50°48'00.00\"N");
        assert_eq!(format_dms(-1.1, false), "001°06'00.00\"W");
        assert_eq!(format_dms(-0.5, true), "00°30'00.00\"S");
        assert_eq!(format_dms(89.999_999_999, true), "90°00'00.00\"N");
        assert_eq!(format_dms(-179.999_999_999, false), "180°00'00.00\"W");
        assert!(format_dms(f64::NAN, true).starts_with("Invalid"));
        assert!(format_dms(91., true).starts_with("Invalid"));
        assert!(format_dms(181., false).starts_with("Invalid"));
    }
}

/// egui integration wrapper
pub struct EguiIntegration {
    pub ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    font_metrics_ready: bool,
    pending_font_textures: egui::TexturesDelta,
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
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "ChartBold".into(),
            egui::FontData::from_static(include_bytes!(
                "../../../Catalogues/PC/S-421/Fonts/OpenSans-Bold.ttf"
            ))
            .into(),
        );
        let mut bold_fallbacks = fonts.families[&egui::FontFamily::Proportional].clone();
        bold_fallbacks.insert(0, "ChartBold".into());
        fonts
            .families
            .insert(egui::FontFamily::Name("ChartBold".into()), bold_fallbacks);
        ctx.set_fonts(fonts);

        // Configure egui style for dark theme
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        ctx.set_style(style);
        Theme::for_profile("Day").apply(&ctx, false);

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
            font_metrics_ready: false,
            pending_font_textures: Default::default(),
        }
    }

    /// Initialize font metrics for dependency evaluation before the first GUI
    /// frame. Retain atlas uploads; do not consume queued window/input events.
    pub fn ensure_font_metrics(&mut self, window: &Window) {
        if self.font_metrics_ready {
            return;
        }
        let density = window.scale_factor() as f32;
        let size = window.inner_size();
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.width as f32 / density, size.height as f32 / density),
            )),
            ..Default::default()
        };
        raw.viewports
            .entry(self.ctx.viewport_id())
            .or_default()
            .native_pixels_per_point = Some(density);
        let output = self.ctx.run(raw, |_| {});
        self.pending_font_textures.append(output.textures_delta);
        self.font_metrics_ready = true;
    }

    /// Handle winit window event, returns true if egui consumed the event
    pub fn handle_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.state.on_window_event(window, event);
        // Consumed pointer/key events may not enter chart-navigation branches.
        // Honour egui's repaint signal so queued button releases are processed
        // even while the event-driven chart is otherwise idle.
        // The redraw event already satisfies the request; scheduling another
        // here would create an idle rendering loop.
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        response.consumed
    }

    /// Begin egui frame
    pub fn begin_frame(&mut self, window: &Window) {
        let raw_input = self.state.take_egui_input(window);
        self.ctx.begin_pass(raw_input);
        self.font_metrics_ready = true;
    }

    /// End egui frame and get render output
    pub fn end_frame(&mut self, window: &Window) -> egui::FullOutput {
        let mut output = self.ctx.end_pass();
        let mut textures = std::mem::take(&mut self.pending_font_textures);
        textures.append(output.textures_delta);
        output.textures_delta = textures;
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
        // UI zoom can differ from the native OS scale. Match tessellation.
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            pixels_per_point: full_output.pixels_per_point,
            ..screen_descriptor
        };
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

    /// Upload chart/UI atlas deltas once, before ordered chart glyph draws.
    pub fn prepare_chart_atlas(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output: &mut egui::FullOutput,
    ) {
        for (id, delta) in output.textures_delta.set.drain(..) {
            self.renderer.update_texture(device, queue, id, &delta);
        }
    }
    /// Flush font changes without an extra UI pass or consuming native input.
    pub(crate) fn flush_chart_fonts(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        for (id, delta) in std::mem::take(&mut self.pending_font_textures).set {
            self.renderer.update_texture(device, queue, id, &delta);
        }
        if let Some(delta) = self.ctx.fonts(|fonts| fonts.font_image_delta()) {
            self.ctx
                .tex_manager()
                .write()
                .set(egui::TextureId::default(), delta);
        }
        let delta = self.ctx.tex_manager().write().take_delta();
        for (id, image) in delta.set {
            self.renderer.update_texture(device, queue, id, &image);
        }
        for id in delta.free {
            self.renderer.free_texture(&id);
        }
    }
    pub fn chart_atlas_bind_group(&self, id: egui::TextureId) -> Option<wgpu::BindGroup> {
        self.renderer
            .texture(&id)
            .map(|texture| texture.bind_group.clone())
    }

    /// Draw the UI and return the app state changes
    fn draw_temporal_dialog(ctx: &egui::Context, state: &mut AppUiState) {
        let theme = Theme::current(ctx);
        if state.pending_temporal_view.is_none() {
            state.pending_temporal_view = Some(state.temporal_view.clone());
        }
        let mut open = state.show_temporal;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Viewing date and time").open(&mut open).resizable(false).default_width(480.0).show(ctx, |ui| {
            let draft = state.pending_temporal_view.as_mut().unwrap();
            egui::ComboBox::from_id_salt("temporal_mode").selected_text(draft.mode.label()).show_ui(ui, |ui| {
                for mode in [TemporalViewMode::Live,TemporalViewMode::Date,TemporalViewMode::Instant,TemporalViewMode::All] {
                    ui.selectable_value(&mut draft.mode,mode,mode.label());
                }
            });
            ui.add_space(8.0);
            match draft.mode {
                TemporalViewMode::Live => {ui.label("Uses the current clock. Objects refresh when their time conditions change.");}
                TemporalViewMode::Date => {
                    ui.label("Date (YYYY-MM-DD)");
                    ui.text_edit_singleline(&mut draft.date);
                    ui.label("Evaluated at midnight in the source-local offset below.");
                }
                TemporalViewMode::Instant => {
                    ui.label("Date and time, including Z or a UTC offset");
                    ui.text_edit_singleline(&mut draft.instant);
                    ui.weak("Example: 2026-10-04T09:30:00+09:00");
                }
                TemporalViewMode::All => {ui.label("Shows objects regardless of their date and time conditions.");}
            }
            ui.separator();
            ui.label("Source-local UTC offset");
            ui.text_edit_singleline(&mut draft.source_offset);
            ui.weak("Z, +09:00 or -03:30. Applies to source dates and times without a zone; it does not change the viewing instant's offset.");
            let error = draft.validation_error();
            if let Some(error) = &error {ui.colored_label(theme.error,error);}
            ui.separator();
            ui.horizontal(|ui| {
                apply = ui.add_enabled(error.is_none(),egui::Button::new("Apply")).clicked();
                cancel = ui.button("Cancel").clicked();
                if ui.button("Use live clock").clicked() {draft.mode = TemporalViewMode::Live;}
            });
        });
        if apply {
            let draft = state.pending_temporal_view.take().unwrap();
            if draft.validation_error().is_none() {
                state.temporal_changed = draft != state.temporal_view;
                state.temporal_view = draft;
                open = false;
            }
        }
        if cancel {
            open = false;
        }
        if !open {
            state.pending_temporal_view = None;
        }
        state.show_temporal = open;
    }

    pub fn draw_ui(&self, ui_state: &mut AppUiState) {
        let theme = Theme::for_profile(&ui_state.color_profile);
        theme.apply(&self.ctx, ui_state.reduced_motion);
        let now = self.ctx.input(|i| i.time);
        let identity = ui_state
            .selected_feature
            .as_ref()
            .map(|f| (f.cell_index, f.feature_id));
        if identity != ui_state.selection_ui_identity {
            ui_state.selection_ui_identity = identity;
            ui_state.object_attribute_query.clear();
            ui_state.selection_ui_started = now;
        }
        let emphasis =
            selection_emphasis(now - ui_state.selection_ui_started, ui_state.reduced_motion);
        if emphasis < 1. && identity.is_some() {
            self.ctx
                .request_repaint_after(std::time::Duration::from_millis(16));
        }

        // Combined toolbar with menu and buttons
        egui::TopBottomPanel::top("toolbar")
            .min_height(40.0)
            .show(&self.ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    // File menu
                    ui.menu_button("File", |ui| {
                        if ui.button("Open datasets…").clicked() {
                            ui_state.open_file_requested = true;
                            ui.close_menu();
                        }
                        if ui.button("Open exchange set folder…").clicked() {
                            ui_state.open_exchange_requested = true;
                            ui.close_menu();
                        }
                        if ui_state.chart_count > 0 && ui.button("Clear All Charts").clicked() {
                            ui_state.clear_charts_requested = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Open catalogue set (FC + PC)…").clicked() {
                            ui_state.open_catalogue_set_requested = true;
                            ui.close_menu();
                        }
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
                            ui_state.close_requested = true;
                            ui.close_menu();
                        }
                    });

                    if icon_button(ui, Icon::Open, "Open S-101 / S-102 datasets").clicked() {
                        ui_state.open_file_requested = true;
                    }

                    if ui
                        .selectable_label(ui_state.debug_mode, "DEBUG MODE")
                        .on_hover_text("Toggle performance statistics and profiling (F12)")
                        .clicked()
                    {
                        ui_state.debug_mode = !ui_state.debug_mode;
                    }

                    // View menu
                    ui.menu_button("View", |ui| {
                        for (globe, label) in [(false, "2D Chart"), (true, "3D Globe")] {
                            if ui
                                .selectable_label(ui_state.globe_preview == globe, label)
                                .clicked()
                            {
                                if ui_state.globe_preview != globe {
                                    ui_state.globe_preview = globe;
                                    ui_state.view_mode_changed = true;
                                }
                                ui.close_menu();
                            }
                        }
                        ui.separator();
                        if ui_state.globe_preview {
                            if ui.button("Whole Earth").clicked() {
                                ui_state.fit_globe_requested = true;
                                ui.close_menu();
                            }
                            for (tilt, label) in
                                [(0., "Look straight down"), (45., "Tilt toward horizon")]
                            {
                                if ui
                                    .selectable_label(ui_state.globe_tilt_deg == tilt, label)
                                    .clicked()
                                {
                                    ui_state.globe_tilt_deg = tilt;
                                    ui_state.view_mode_changed = true;
                                    ui.close_menu();
                                }
                            }
                        }
                        ui.menu_button("Color palette", |ui| {
                            for profile in ["Day", "Dusk", "Night"] {
                                if ui
                                    .selectable_label(ui_state.color_profile == profile, profile)
                                    .clicked()
                                {
                                    if ui_state.color_profile != profile {
                                        ui_state.color_profile = profile.into();
                                        ui_state.color_profile_changed = true;
                                    }
                                    ui.close_menu();
                                }
                            }
                        });
                        if ui.button("Viewing date and time…").clicked() {
                            ui_state.show_temporal = true;
                            ui.close_menu();
                        }
                        ui.separator();

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

                    if ui
                        .button(ui_state.temporal_view.summary())
                        .on_hover_text(format!(
                            "{}; source-local offset {}. Click to change viewing time.",
                            ui_state.temporal_view.mode.label(),
                            ui_state.temporal_view.source_offset
                        ))
                        .clicked()
                    {
                        ui_state.show_temporal = true;
                    }

                    // Settings menu (independent top-level menu)
                    ui.menu_button("Settings", |ui| {
                        if ui
                            .checkbox(&mut ui_state.reduced_motion, "Reduce UI motion")
                            .changed()
                        {
                            ui.ctx().request_repaint();
                        }
                        ui.separator();
                        signature_verification_toggle(ui, &mut ui_state.verify_dataset_signatures);
                        ui.separator();
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

                    // Keep the view choice visible without opening a menu.
                    ui.horizontal(|ui| {
                        for (globe, label, help) in [
                            (false, "2D Chart", "View the chart as a flat map"),
                            (true, "3D Globe", "View the chart on the WGS84 globe"),
                        ] {
                            if ui
                                .selectable_label(ui_state.globe_preview == globe, label)
                                .on_hover_text(help)
                                .clicked()
                                && ui_state.globe_preview != globe
                            {
                                ui_state.globe_preview = globe;
                                ui_state.view_mode_changed = true;
                            }
                        }
                    });
                    ui.separator();
                    // Zoom controls
                    if icon_button(ui, Icon::Plus, "Zoom in").clicked() {
                        ui_state.zoom_in_requested = true;
                    }
                    if icon_button(ui, Icon::Minus, "Zoom out").clicked() {
                        ui_state.zoom_out_requested = true;
                    }
                    if icon_button(ui, Icon::Fit, "Fit chart to view").clicked() {
                        ui_state.reset_view_requested = true;
                    }

                    ui.separator();

                    // Color profile selector (Day/Dusk/Night)
                    ui.allocate_ui_with_layout(
                        egui::vec2(168.0, 30.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.label("Palette:");
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
                                        if ui
                                            .selectable_label(current == profile, profile)
                                            .clicked()
                                            && current != profile
                                        {
                                            ui_state.color_profile = profile.to_string();
                                            ui_state.color_profile_changed = true;
                                        }
                                    }
                                });
                        },
                    );

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

        // Status bar
        egui::TopBottomPanel::bottom("status_bar")
            .min_height(28.0)
            .show(&self.ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    // Coordinate display (only show valid coords when chart is loaded)
                    if ui_state.chart_count > 0
                        && ui_state.cursor_world.0.is_finite()
                        && ui_state.cursor_world.1.is_finite()
                        && ui_state.cursor_world.1.abs() <= 90.0
                    {
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
                                .color(theme.accent),
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
            .min_width(260.0)
            .resizable(true)
            .show(&self.ctx, |ui| {
                // Panel title with larger font
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("Object details").size(18.0).strong());
                });
                ui.horizontal(|ui| {
                    if ui.small_button("Expand all").clicked() { ui_state.object_detail_sections.set_all(true); }
                    if ui.small_button("Collapse all").clicked() { ui_state.object_detail_sections.set_all(false); }
                });
                ui.separator();

                egui::ScrollArea::vertical().id_salt("object_details_scroll").auto_shrink([false,false]).show(ui,|ui| {
                if ui_state.globe_preview {
                    ui.colored_label(theme.error, "Globe preview: patterns, bathymetry, line text placement remain unavailable.");
                    ui.label(&ui_state.globe_summary);
                    ui.separator();
                }
                if let Some(notice)=ui_state.notice.clone() {
                    ui.colored_label(theme.error,&notice);
                    if ui.small_button("Dismiss message").clicked(){ui_state.notice=None;}
                    ui.separator();
                }
                if !ui_state.interoperability_status.is_empty() {
                    ui.label(&ui_state.interoperability_status);
                }
                if !ui_state.security_status.is_empty() {
                    ui.label(&ui_state.security_status);
                    if !ui_state.security_details.is_empty() {
                        detail_section(ui,"Verification details","security",&mut ui_state.object_detail_sections.security,|ui| {
                            ui.add(egui::Label::new(&ui_state.security_details).wrap().selectable(true));
                        });
                    }
                    ui.separator();
                }
                if ui_state.bathymetry_count > 0 {
                    ui.label(format!("S-102 layers: {}", ui_state.bathymetry_count));
                    if ui_state.interoperability_active {
                        ui.label("Display: authenticated interoperability catalogue");
                    } else {
                        ui.label("Display: ordinary overlays")
                            .on_hover_text("S-102 follows its portrayal catalogue. Opaque depth colours can cover ENC symbols. No active IC interleaving is applied.");
                    }
                    ui.label("Click the map for depth, uncertainty and survey quality.");
                }
                if let Some(info) = &ui_state.coverage_info {
                    if info.lines().any(|line|line.starts_with("Source encoding warning:")) {
                        ui.colored_label(theme.warning,"Source text encoding issue; original bytes retained. See details.");
                    }
                    detail_section(ui,"Depth and survey quality","coverage",&mut ui_state.object_detail_sections.coverage,|ui| {
                        ui.add(egui::Label::new(info).wrap().selectable(true));
                    });
                    ui.separator();
                }
                if ui_state.selection_candidates.len() > 1 {
                    detail_section(ui,format!("Objects at this position ({})",ui_state.selection_candidates.len()),"nearby",&mut ui_state.object_detail_sections.nearby,|ui| {
                    egui::ScrollArea::vertical().id_salt("overlapping_features").max_height(140.0).show(ui,|ui| {
                        for (index, candidate) in ui_state.selection_candidates.iter().enumerate() {
                            let active = ui_state.selected_feature.as_ref().is_some_and(|f|f.feature_id == candidate.feature_id && f.cell_index == candidate.cell_index);
                            let chart = candidate.source.as_deref().and_then(|s|std::path::Path::new(s).file_name()).map(|s|s.to_string_lossy()).unwrap_or_default();
                            let label = format!("{} · {} · {}",candidate.feature_type,candidate.primitive_type,chart);
                            if ui.selectable_label(active,label).on_hover_text(format!("Object ID {}",candidate.feature_id)).clicked() {
                                ui_state.selection_requested=Some(index); ui.ctx().request_repaint();
                            }
                        }
                    });
                    });
                    ui.separator();
                }
                if let Some(ref feature) = ui_state.selected_feature {
                    let (accent_rect,_) = ui.allocate_exact_size(egui::vec2(ui.available_width(),3.),egui::Sense::hover());
                    ui.painter().rect_filled(accent_rect,1.5,theme.accent.gamma_multiply(0.35+0.65*emphasis));
                    ui.label(egui::RichText::new("SELECTED OBJECT").small().color(theme.muted));

                    draw_selected_object_details(ui, feature, &mut ui_state.object_detail_sections,
                        &mut ui_state.object_attribute_query, ui_state.settings.safety_contour);
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

                            ui.label(egui::RichText::new("Tab / Shift+Tab").size(12.0));
                            ui.label(egui::RichText::new("Move keyboard focus").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("F12").size(12.0));
                            ui.label(egui::RichText::new("Performance details").size(12.0));
                            ui.end_row();

                            ui.label(egui::RichText::new("Right click").size(12.0));
                            ui.label(egui::RichText::new("Reset view").size(12.0));
                            ui.end_row();
                        });
                }
                });
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

        if ui_state.show_temporal {
            Self::draw_temporal_dialog(&self.ctx, ui_state);
        } else {
            ui_state.pending_temporal_view = None;
        }

        // Use the space actually left by panels, including user resizing,
        // borders, wrapped toolbar rows and the current UI scale.
        // Reading available_rect does not install a mouse-consuming CentralPanel.
        let available = self.ctx.available_rect();
        let chart_x = available.min.x;
        let chart_y = available.min.y;
        let chart_width = available.width().max(0.);
        let chart_height = available.height().max(0.);

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
        let theme = Theme::current(ctx);
        egui::Area::new(egui::Id::new("debug_overlay"))
            .fixed_pos(egui::pos2(chart_x + 10.0, chart_y + 10.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.style_mut().visuals.override_text_color = Some(theme.success);

                let frame_response = egui::Frame::new()
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::same(8))
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        ui.set_min_width(200.0);
                        ui.label(egui::RichText::new("DEBUG MODE").size(14.0).strong());

                        ui.separator();

                        // Performance stats
                        ui.label(format!("FPS: {:.1}", ui_state.debug_fps));
                        ui.label(ui_state.debug_cpu_usage.map_or_else(|| "CPU: —".into(), |v| format!("CPU: {v:.1}%")))
                            .on_hover_text("Process CPU share of available logical CPU capacity (100% uses all cores)");
                        ui.label(ui_state.debug_memory_mb.map_or_else(|| "RAM: —".into(), |v| format!("RAM: {v:.1} MiB")))
                            .on_hover_text("Current resident memory used by this process");

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
        let theme = Theme::current(ctx);
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
                                    .color(theme.success));
                            }
                        });
                        ui.separator();

                        // Rendering toggle at top of panel
                        let rendering_enabled = route_ui.get("rendering_enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                        ui.horizontal(|ui| {
                            ui.label("Route Display:");
                            let btn_text = if rendering_enabled { "ON" } else { "OFF" };
                            let btn_color = if rendering_enabled {
                                theme.success
                            } else {
                                theme.error
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
                                                theme.selection
                                            } else {
                                                theme.raised
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
                                                                .color(theme.muted));
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
                                            .fill(theme.raised)
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
                                                        ui.label(egui::RichText::new(pos).size(11.0).color(theme.muted));
                                                        if let Some(d) = leg {
                                                            ui.label(egui::RichText::new(format!("Leg: {}", d)).size(11.0).color(theme.accent));
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
                            ui.label(egui::RichText::new("No routes. Click 'New' to create one.").size(12.0).color(theme.muted));
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
                                            theme.success
                                        } else {
                                            theme.error
                                        };
                                        ui.label(egui::RichText::new(message).color(color).size(11.0));
                                    } else {
                                        ui.label(egui::RichText::new("Not loaded").color(theme.muted).size(11.0));
                                    }
                                });

                                // PC Status
                                ui.horizontal(|ui| {
                                    ui.label("PC:");
                                    if let Some(pc) = route_ui.get("pc_status") {
                                        let loaded = pc.get("loaded").and_then(|v| v.as_bool()).unwrap_or(false);
                                        let message = pc.get("message").and_then(|v| v.as_str()).unwrap_or("Unknown");
                                        let color = if loaded {
                                            theme.success
                                        } else {
                                            theme.error
                                        };
                                        ui.label(egui::RichText::new(message).color(color).size(11.0));
                                    } else {
                                        ui.label(egui::RichText::new("Not loaded").color(theme.muted).size(11.0));
                                    }
                                });

                                ui.add_space(4.0);
                                ui.label(egui::RichText::new("Catalogue path: ./Catalogues/*/S-421/").size(10.0).color(theme.muted));
                            });

                        ui.add_space(4.0);

                        // Instructions based on mode
                        if editing {
                            ui.label(egui::RichText::new("Editing Mode:").size(12.0).strong());
                            ui.label(egui::RichText::new("• Left click: Add waypoint").size(11.0));
                            ui.label(egui::RichText::new("• Right click: Remove last").size(11.0));
                            ui.label(egui::RichText::new("• Click 'Finish' when done").size(11.0));
                        } else if route_ui.get("active_route_index").is_some() {
                            ui.label(egui::RichText::new("Click 'New' to add another route").size(12.0).color(theme.muted));
                        } else {
                            ui.label(egui::RichText::new("Click 'New' to start a route").size(12.0).color(theme.muted));
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

    /// Draw the Settings dialog
    fn draw_settings_dialog(ctx: &egui::Context, ui_state: &mut AppUiState) {
        let theme = Theme::current(ctx);
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

                ui.heading("Digital signatures");
                signature_verification_toggle(ui, &mut ui_state.verify_dataset_signatures);
                ui.label(if ui_state.verify_dataset_signatures { "ON" } else { "OFF" });
                ui.label("Applies immediately to newly opened S-100 datasets.");
                ui.label("Reload existing charts to verify their signatures.");
                ui.separator();

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
                    .color(theme.muted),
                );

                if !ui_state.optional_viewing_layers.is_empty() {
                    ui.collapsing("Chart text and additional layers", |ui| {
                        for (id,name) in &ui_state.optional_viewing_layers {
                            let mut selected=pending.viewing_layers.contains(id);
                            if ui.checkbox(&mut selected,name).changed() {
                                if selected {pending.viewing_layers.insert(id.clone());}
                                else {pending.viewing_layers.remove(id);}
                            }
                        }
                    });
                }
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                if ui_state.interoperability_available {
                    ui.checkbox(&mut pending.interoperability_enabled, "Combine products using interoperability rules");
                    ui.label(egui::RichText::new("Turn off to use each product's original portrayal.").size(11.0).color(theme.muted));
                    ui.separator();
                }

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
                        ui.label("Safety Contour:").on_hover_text("Depth boundary used to classify underwater dangers. The magenta X marks an isolated danger at or shallower than this setting.");
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
                        ui.checkbox(&mut pending.isolated_dangers, "Shallow Water Dangers")
                            .on_hover_text("Show the magenta isolated-danger symbol in shallow water too. Dangers surrounded by water at or deeper than the safety contour remain displayed.");
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
                        ui_state.verify_dataset_signatures = false;
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
        let theme = Theme::current(ui.ctx());
        egui::Grid::new(format!("catalogue_grid_{}", item_label))
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                // Status indicator
                ui.label(egui::RichText::new("Status:").strong());
                if status.loaded {
                    ui.label(egui::RichText::new("Loaded").color(theme.success));
                } else {
                    ui.label(egui::RichText::new("Not Loaded").color(theme.error));
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
                            .color(theme.muted),
                    );
                    ui.end_row();

                    // Validation status
                    if let Some(ref msg) = status.validation_message {
                        ui.label(egui::RichText::new("Validation:").strong());
                        let color = if msg.starts_with("Valid") {
                            theme.success
                        } else {
                            theme.warning
                        };
                        ui.label(egui::RichText::new(msg).color(color));
                        ui.end_row();
                    }
                }
            });
    }
}

#[cfg(test)]
mod temporal_dialog_tests {
    use super::*;
    fn texts(shape: &egui::Shape, output: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => output.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    texts(shape, output);
                }
            }
            _ => {}
        }
    }
    #[test]
    fn dialogs_show_mode_help_and_keep_unapplied_edits_separate() {
        for (mode, help) in [
            (TemporalViewMode::Live, "Uses the current clock"),
            (TemporalViewMode::Date, "Date (YYYY-MM-DD)"),
            (TemporalViewMode::Instant, "Date and time, including Z"),
            (TemporalViewMode::All, "Shows objects regardless"),
        ] {
            let ctx = egui::Context::default();
            let mut state = AppUiState::default();
            state.show_temporal = true;
            let active = state.temporal_view.clone();
            state.pending_temporal_view = Some(TemporalView {
                mode,
                ..active.clone()
            });
            let mut output = Vec::new();
            // First egui pass sizes the window; second paints it at its measured size.
            for _ in 0..2 {
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 600.),
                    )),
                    ..Default::default()
                };
                let frame = ctx.run(raw, |ctx| {
                    EguiIntegration::draw_temporal_dialog(ctx, &mut state)
                });
                for clipped in &frame.shapes {
                    texts(&clipped.shape, &mut output);
                }
            }
            assert!(
                output.iter().any(|text| text.contains(help)),
                "mode {mode:?}: {output:?}"
            );
            assert!(output
                .iter()
                .any(|text| text.contains("Source-local UTC offset")));
            assert_eq!(state.temporal_view, active);
            assert!(!state.temporal_changed);
            assert_eq!(state.pending_temporal_view.as_ref().unwrap().mode, mode);
        }
    }
    #[test]
    fn invalid_instant_is_visible_in_dialog_without_changing_active_view() {
        let ctx = egui::Context::default();
        let mut state = AppUiState::default();
        state.show_temporal = true;
        let active = state.temporal_view.clone();
        state.pending_temporal_view = Some(TemporalView {
            mode: TemporalViewMode::Instant,
            instant: "2026-10-04T09:30:00".into(),
            ..active.clone()
        });
        let mut output = Vec::new();
        for _ in 0..2 {
            let frame = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800., 600.),
                    )),
                    ..Default::default()
                },
                |ctx| EguiIntegration::draw_temporal_dialog(ctx, &mut state),
            );
            for clipped in &frame.shapes {
                texts(&clipped.shape, &mut output);
            }
        }
        assert!(
            output.iter().any(|text| text.starts_with("Date and time:")),
            "{output:?}"
        );
        assert_eq!(state.temporal_view, active);
        assert!(!state.temporal_changed);
    }
}

fn signature_verification_toggle(ui: &mut egui::Ui, enabled: &mut bool) -> egui::Response {
    ui.checkbox(enabled, "Verify digital signatures")
        .on_hover_text("Applies to newly opened S-100 datasets. Reload existing charts to verify them.")
}

#[cfg(test)]
mod signature_setting_tests {
    use super::*;
    #[test]
    fn digital_signature_verification_defaults_off() {
        assert!(!AppUiState::default().verify_dataset_signatures);
    }
    #[test]
    fn settings_signature_widget_clicks_toggle_both_ways_without_portrayal_rebuild() {
        let context = egui::Context::default();
        let mut state = AppUiState::default();
        let portrayal = state.settings.clone();
        let frame = |events, state: &mut AppUiState| {
            let mut rect = egui::Rect::NOTHING;
            let _ = context.run(egui::RawInput { events, ..Default::default() }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    rect = signature_verification_toggle(ui, &mut state.verify_dataset_signatures).rect;
                });
            });
            rect
        };
        let center = frame(Vec::new(), &mut state).center();
        for expected in [true, false] {
            frame(vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton { pos: center, button: egui::PointerButton::Primary,
                    pressed: true, modifiers: egui::Modifiers::default() },
            ], &mut state);
            frame(vec![
                egui::Event::PointerButton { pos: center, button: egui::PointerButton::Primary,
                    pressed: false, modifiers: egui::Modifiers::default() },
            ], &mut state);
            assert_eq!(state.verify_dataset_signatures, expected);
            assert_eq!(state.settings, portrayal);
            assert!(!state.settings_changed);
            assert!(state.pending_settings.is_none());
        }
    }
}
