//! egui Integration for wgpu Renderer
//!
//! Provides egui GUI overlay for the chart viewer.

use crate::diagnostics::{DiagnosticLog, DiagnosticView};
use crate::object_details::{
    detail_section, draw_report_fields, draw_selected_object_details, ObjectDetailSections,
};
use crate::ui_chrome::{
    icon_button, icon_toggle, icon_toggle_with_badge, selection_emphasis, Icon, Theme,
};
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
    /// Host-captured primary S-101 PC digest for these lexical layer IDs.
    /// UI intent identity only; not dataset authentication or portrayal authority.
    pub viewing_layer_owner: Option<[u8; 32]>,
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
            viewing_layer_owner: None,
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

/// Stable dataset identity: UI requests never rely on compacting cell indices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DatasetLayerId {
    S101 {
        product: String,
        name: String,
    },
    S102(std::path::PathBuf),
    /// Native route lifetime; not an ENC cell index or a raster path.
    S421 {
        route_id: u32,
    },
}
#[derive(Debug, Clone)]
pub struct DatasetLayerEntry {
    pub id: DatasetLayerId,
    pub name: String,
    pub source: String,
    pub detail: String,
}
#[derive(Debug, Clone)]
pub struct DatasetProductLayer {
    pub product: String,
    pub fc: String,
    pub pc: String,
    pub files: Vec<DatasetLayerEntry>,
}

/// Actual catalogue metadata supplied by the application, separate from opened files.
#[derive(Debug, Clone, Default)]
pub struct DatasetCatalogueLayer {
    pub product: String,
    pub fc: CatalogueStatus,
    pub pc: CatalogueStatus,
}

/// Coverage-owner numerical scale and actual active-PC SCLBR colour.
#[derive(Debug, Clone, Copy)]
pub struct CoverageScaleIndication {
    pub viewing_denominator: f64,
    pub overscale_factor: f64,
    pub physical_viewport: ferrite_render::Viewport,
    /// None is a missing portrayal resource, never a theme-colour substitution.
    pub sclbr: Option<egui::Color32>,
}

/// Application state shared between egui UI and main app
#[derive(Debug, Clone, Default)]
pub struct AppUiState {
    pub s100_mcp: crate::mcp_ui::State,
    pub reduced_motion: bool,
    /// Optional edge annotations; never mutate chart geometry or portrayal settings.
    pub coordinate_rulers: bool,
    pub coordinate_ruler_scaler: Option<ferrite_render::Scaler>,
    pub dataset_tree_hidden: bool,
    pub object_details_hidden: bool,
    pub dataset_layers: Vec<DatasetProductLayer>,
    /// Exact per-source metadata, replaced atomically with the published cells.
    pub dataset_catalogue_bindings: Vec<crate::DatasetCatalogueBinding>,
    pub catalogue_layers: Vec<DatasetCatalogueLayer>,
    pub diagnostics: Arc<std::sync::Mutex<DiagnosticLog>>,
    pub diagnostic_sink_dropped: Arc<std::sync::atomic::AtomicU64>,
    pub show_logs: bool,
    pub show_load_status: bool,
    /// Click-to-inspect is explicitly enabled by the user; default OFF.
    pub object_selection_mode: bool,
    /// Clear retained highlight geometry after a UI mode transition.
    pub clear_selection_requested: bool,
    /// Session verification counters; None means the host has not supplied them.
    pub signature_counts: Option<(usize, usize)>,
    pub diagnostic_view: DiagnosticView,
    diagnostic_notice_logged: Option<String>,
    pub selected_dataset: Option<DatasetLayerId>,
    pub dataset_selection_requested: Option<DatasetLayerId>,
    pub dataset_unload_requested: Option<DatasetLayerId>,
    pub dataset_loading: bool,

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
    pub selected_symbol_preview: Option<crate::SelectedSymbolPreview>,
    pub selection_candidates: Vec<SelectedFeature>,
    pub selection_requested: Option<usize>,
    pub bathymetry_count: usize,
    pub coverage_info: Option<String>,
    pub coverage_scale_indication: Option<CoverageScaleIndication>,
    pub security_status: String,
    /// Application load policy, separate from portrayal settings. Default OFF.
    pub verify_dataset_signatures: bool,
    /// Operational profile forbids downgrading dataset authentication.
    pub signature_verification_locked: bool,
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
    /// Parsed immutable native DTO; updated only with provider data publication.
    native_route_ui: Option<std::sync::Arc<serde_json::Value>>,
    route_panel_provider: Option<&'static str>,
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
    pub debug_main_thread_cpu: Option<f32>,
    pub debug_cpu_frames: crate::debug_metrics::CpuDebugMetrics,
    /// Debug stats: Memory usage in MB
    pub debug_memory_mb: Option<f32>,
    pub debug_gpu: DebugGpuStats,
    pub debug_history: DebugHistory,
    /// Debug stats: Render instruction count
    pub debug_instruction_count: usize,
    /// Debug stats: Symbol count
    pub debug_symbol_count: usize,
}

/// Actual renderer adapter and completed timestamp sample. Utilization is a
/// distinct device-wide OS metric and is never synthesized from pass timing.
#[derive(Debug, Clone, Default)]
pub struct DebugGpuStats {
    pub adapter: String,
    pub backend: String,
    pub timestamp_supported: bool,
    pub timing_enabled: bool,
    /// Mean GPU chart execution time from samples completed in the last window.
    pub chart_ms: Option<f64>,
    pub chart_max_ms: Option<f64>,
    pub chart_window_samples: u64,
    pub chart_window_seconds: f64,
    pub sample_age_seconds: Option<f64>,
    pub completed_samples: u64,
    pub utilization_percent: Option<f32>,
}
impl DebugGpuStats {
    fn timing_label(&self) -> String {
        if !self.timestamp_supported {
            return "GPU chart: unsupported".into();
        }
        if !self.timing_enabled {
            return "GPU chart: profiling off".into();
        }
        match (self.chart_ms, self.chart_max_ms, self.sample_age_seconds) {
            (Some(mean), Some(max), Some(age)) if age <= 2.0 => format!(
                "GPU chart mean/max: {mean:.2}/{max:.2} ms · n={}",
                self.chart_window_samples
            ),
            (Some(mean), Some(max), Some(age)) => {
                format!("GPU chart mean/max: {mean:.2}/{max:.2} ms (last {age:.0}s ago)",)
            }
            _ => "GPU chart: waiting for completed window".into(),
        }
    }
}

/// Fixed-size sampling history with allocation-free recording.
#[derive(Debug, Clone)]
pub struct DebugHistory {
    rows: [[Option<f32>; 3]; 120],
    used: usize,
    next: usize,
}
impl Default for DebugHistory {
    fn default() -> Self {
        Self {
            rows: [[None; 3]; 120],
            used: 0,
            next: 0,
        }
    }
}
impl DebugHistory {
    pub fn record(&mut self, fps: f32, cpu: Option<f32>, ram: Option<f32>) {
        let valid = |v: Option<f32>| v.filter(|v| v.is_finite() && *v >= 0.0);
        self.rows[self.next] = [valid(Some(fps)), valid(cpu), valid(ram)];
        self.next = (self.next + 1) % self.rows.len();
        self.used = (self.used + 1).min(self.rows.len());
    }
    fn sample(&self, ordinal: usize, metric: usize) -> Option<f32> {
        let first = if self.used == self.rows.len() {
            self.next
        } else {
            0
        };
        self.rows[(first + ordinal) % self.rows.len()][metric]
    }
    fn draw(&self, ui: &mut egui::Ui) {
        for (metric, name, floor) in [
            (0, "redraw/s", 144.0f32),
            (1, "CPU", 100.0),
            (2, "RAM", 1.0),
        ] {
            ui.horizontal(|ui| {
                ui.small(name);
                let (rect, response) = ui.allocate_exact_size(egui::vec2(142.0, 22.0), egui::Sense::hover());
                response.on_hover_text("Recent 120 statistics updates. Missing measurements create gaps. Each graph has an independent vertical scale.");
                let p = ui.painter_at(rect);
                let ceiling = (0..self.used).filter_map(|i| self.sample(i, metric)).fold(floor, f32::max);
                let color = ui.visuals().text_color();
                p.line_segment([rect.left_bottom(), rect.right_bottom()], egui::Stroke::new(0.5_f32, color.gamma_multiply(0.3)));
                let point = |i: usize, v: f32| egui::pos2(rect.left() + rect.width() * i as f32 / 119.0,
                    rect.bottom() - rect.height() * (v / ceiling).clamp(0.0, 1.0));
                for i in 1..self.used {
                    if let (Some(a), Some(b)) = (self.sample(i-1, metric), self.sample(i, metric)) {
                        p.line_segment([point(i-1, a), point(i, b)], egui::Stroke::new(1.0_f32, color));
                    }
                }
            });
        }
    }
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

impl AppUiState {
    pub(crate) fn publish_plugin_ui_data(&mut self, data: Vec<(String, String)>) {
        self.native_route_ui = data
            .iter()
            .find(|(id, _)| id == "native:s421-route")
            .and_then(|(_, json)| serde_json::from_str(json).ok())
            .map(std::sync::Arc::new);
        self.plugin_ui_data = data;
    }

    pub fn clear_selection(&mut self) {
        self.selected_feature = None;
        self.selected_symbol_preview = None;
        self.selection_candidates.clear();
        self.selection_requested = None;
        self.coverage_info = None;
        self.selected_dataset = None;
        self.dataset_selection_requested = None;
        self.object_details_hidden = true;
        self.clear_selection_requested = true;
    }
    pub fn set_object_selection_mode(&mut self, enabled: bool) {
        let was_enabled = self.object_selection_mode;
        self.object_selection_mode = enabled;
        if was_enabled && !enabled {
            self.clear_selection();
        }
    }
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
        ctx.set_fonts(crate::chart_fonts::chart_font_definitions());

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
        egui::Window::new("Viewing Date and Time").open(&mut open).resizable(false).default_width(480.0).show(ctx, |ui| {
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

    fn draw_catalogue_tree(ui: &mut egui::Ui, state: &AppUiState) {
        egui::CollapsingHeader::new("Catalogue")
            .id_salt("catalogue-tree")
            .default_open(true)
            .show(ui, |ui| {
                let mut products: Vec<&str> = state
                    .catalogue_layers
                    .iter()
                    .map(|p| p.product.as_str())
                    .collect();
                products.extend(state.dataset_layers.iter().map(|p| p.product.as_str()));
                if products.is_empty() {
                    products.push("S-101");
                }
                products.sort_unstable();
                products.dedup();
                for product in products {
                    egui::CollapsingHeader::new(product)
                        .id_salt((product, "catalogue-product"))
                        .default_open(true)
                        .show(ui, |ui| {
                            let files = state
                                .dataset_layers
                                .iter()
                                .filter(|p| p.product == product)
                                .flat_map(|p| p.files.iter())
                                .collect::<Vec<_>>();
                            for kind in ["FC", "PC"] {
                                let mut statuses: Vec<&CatalogueStatus> = Vec::new();
                                // Loaded S-101 uses actual per-source owners; a missing owner
                                // cannot be replaced with the default installed catalogue.
                                if product == "S-101" && !files.is_empty() {
                                    for file in &files {
                                        if let Some(owner) =
                                            crate::dataset_catalogue_ui::for_dataset(
                                                &state.dataset_catalogue_bindings,
                                                &file.id,
                                            )
                                        {
                                            statuses.push(if kind == "FC" {
                                                &owner.fc
                                            } else {
                                                &owner.pc
                                            });
                                        }
                                    }
                                } else if let Some(layer) =
                                    state.catalogue_layers.iter().find(|p| p.product == product)
                                {
                                    statuses.push(if kind == "FC" { &layer.fc } else { &layer.pc });
                                } else if product == "S-101" {
                                    statuses.push(if kind == "FC" {
                                        &state.fc_status
                                    } else {
                                        &state.pc_status
                                    });
                                }
                                let mut seen = std::collections::HashSet::new();
                                statuses.retain(|v| {
                                    seen.insert((v.path.as_str(), v.version.as_str(), v.loaded))
                                });
                                if statuses.is_empty() {
                                    ui.weak(format!("{kind} · Owner unavailable"));
                                }
                                for status in statuses {
                                    let label = if status.loaded {
                                        format!("{kind} {} · Loaded", status.version)
                                    } else {
                                        format!("{kind} · Not loaded")
                                    };
                                    ui.add(egui::Label::new(label).truncate()).on_hover_text(
                                        format!(
                                            "{}\n{}",
                                            status.path,
                                            status.validation_message.as_deref().unwrap_or("")
                                        ),
                                    );
                                }
                                if product == "S-101"
                                    && !files.is_empty()
                                    && files.iter().any(|file| {
                                        crate::dataset_catalogue_ui::for_dataset(
                                            &state.dataset_catalogue_bindings,
                                            &file.id,
                                        )
                                        .is_none()
                                    })
                                {
                                    ui.weak(format!("{kind} · Some owners unavailable"));
                                }
                            }
                        });
                }
            });
    }
    fn draw_dataset_tree(ctx: &egui::Context, state: &mut AppUiState) {
        if state.dataset_tree_hidden {
            return;
        }
        egui::SidePanel::left("dataset_tree_panel")
            .resizable(true)
            .default_width(250.)
            .min_width(160.)
            .max_width(640.)
            .show(ctx, |ui| {
                // Preserve the SidePanel width chosen by the user even with short content.
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.heading("Data Layers");
                    if ui.small_button("Hide").clicked() {
                        state.dataset_tree_hidden = true;
                    }
                });
                if state.dataset_loading {
                    ui.label("Loading datasets…");
                }
                egui::ScrollArea::vertical()
                    .id_salt("dataset_tree_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        Self::draw_catalogue_tree(ui, state);
                        egui::CollapsingHeader::new("Chart Data")
                            .id_salt("chart-data-tree")
                            .default_open(true)
                            .show(ui, |ui| {
                                if state.dataset_layers.iter().all(|l| l.files.is_empty()) {
                                    ui.weak("No chart data loaded");
                                }
                                for layer in &state.dataset_layers {
                                    egui::CollapsingHeader::new(format!(
                                        "{} ({})",
                                        layer.product,
                                        layer.files.len()
                                    ))
                                    .id_salt((&layer.product, "product-layer"))
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        if layer.files.is_empty() {
                                            ui.weak("No files open");
                                        }
                                        // Stable row budget: preceding rows must not widen the next row.
                                        let row_width =
                                            (ui.max_rect().right() - ui.cursor().min.x).max(90.);
                                        for file in &layer.files {
                                            ui.push_id(&file.id, |ui| {
                                                ui.horizontal(|ui| {
                                                    let selected = state.selected_dataset.as_ref()
                                                        == Some(&file.id);
                                                    let response = ui
                                                        .add_sized(
                                                            [(row_width - 70.).max(45.), 20.],
                                                            egui::Button::new(&file.name)
                                                                .selected(selected)
                                                                .truncate(),
                                                        )
                                                        .on_hover_text(format!(
                                                            "{}\n{}{}",
                                                            file.detail, file.source,
                                                            crate::dataset_catalogue_ui::for_dataset(&state.dataset_catalogue_bindings,&file.id)
                                                                .map(|owner|format!("\nFC {} · {}\nPC {} · {}",owner.fc.version,owner.fc.path,owner.pc.version,owner.pc.path))
                                                                .unwrap_or_default()
                                                        ));
                                                    if response.clicked() {
                                                        state.selected_dataset =
                                                            Some(file.id.clone());
                                                        state.dataset_selection_requested =
                                                            Some(file.id.clone());
                                                    }
                                                    response.context_menu(|ui| {
                                                        ui.add(
                                                            egui::Label::new(&file.source)
                                                                .truncate(),
                                                        )
                                                        .on_hover_text(&file.source);
                                                        if ui
                                                            .add_enabled(
                                                                !state.dataset_loading,
                                                                egui::Button::new("Unload"),
                                                            )
                                                            .clicked()
                                                        {
                                                            state.dataset_unload_requested =
                                                                Some(file.id.clone());
                                                            ui.close_menu();
                                                        }
                                                    });
                                                    if ui
                                                        .add_enabled(
                                                            !state.dataset_loading,
                                                            egui::Button::new("Unload").small(),
                                                        )
                                                        .clicked()
                                                    {
                                                        state.dataset_unload_requested =
                                                            Some(file.id.clone());
                                                    }
                                                });
                                            });
                                        }
                                    });
                                }
                            });
                    });
            });
    }
    fn draw_object_panel(ctx: &egui::Context, ui_state: &mut AppUiState) {
        if ui_state.object_details_hidden
            || (ui_state.selected_feature.is_none() && ui_state.coverage_info.is_none())
        {
            return;
        }
        let theme = Theme::current(ctx);
        // Feature info panel (right side)
        egui::SidePanel::right("feature_panel")
            .default_width(320.0)
            .min_width(220.0)
            .max_width(520.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Panel title with larger font
                ui.horizontal_wrapped(|ui| {
                    ui.heading(egui::RichText::new("Object Details").size(18.0).strong());
                    if ui.small_button("Hide").clicked(){ui_state.object_details_hidden=true;}
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("Expand All").clicked() { ui_state.object_detail_sections.set_all(true); }
                    if ui.small_button("Collapse All").clicked() { ui_state.object_detail_sections.set_all(false); }
                });
                ui.separator();

                egui::ScrollArea::vertical().id_salt("object_details_scroll").auto_shrink([false,false]).show(ui,|ui| {

                if let Some(ref feature) = ui_state.selected_feature {
                    if let Some(preview) = &mut ui_state.selected_symbol_preview {
                        preview.draw(ui, feature, &ui_state.color_profile);
                    }
                    draw_selected_object_details(ui, feature, &mut ui_state.object_detail_sections,
                        &mut ui_state.object_attribute_query, ui_state.settings.safety_contour);
                    // Keep exact bound owner information available without displacing
                    // the object's name, position and attributes with long paths.
                    detail_section(ui, "Catalogues · FC / PC", "catalogues",
                        &mut ui_state.object_detail_sections.catalogues, |ui| {
                            crate::dataset_catalogue_ui::draw_inspector(ui,
                                crate::dataset_catalogue_ui::for_feature(&ui_state.dataset_catalogue_bindings, feature));
                        });

                }
                if !ui_state.interoperability_status.is_empty() || !ui_state.security_status.is_empty() || !ui_state.security_details.is_empty() {
                    detail_section(ui,"Security and interoperability","security",&mut ui_state.object_detail_sections.security,|ui|{
                        for text in [&ui_state.security_status,&ui_state.interoperability_status,&ui_state.security_details] {
                            if !text.is_empty(){draw_report_fields(ui,text);}
                        }
                    });
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
                        draw_report_fields(ui,info);
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
                if ui_state.selected_feature.is_none() {
                    ui.weak("Select a chart object to inspect it.");
                }
                });
            });
    }
    fn draw_logs(ctx: &egui::Context, state: &mut AppUiState) {
        if !state.show_logs {
            return;
        }
        // No egui/widget/tracing call while the log mutex is held. Snapshot strings
        // are immutable Arc<str>; revision changes clone bounded row handles only.
        if let Ok(log) = state.diagnostics.try_lock() {
            state.diagnostic_view.refresh(&log);
        }
        let sink_dropped = state
            .diagnostic_sink_dropped
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut clear = false;
        let mut open = state.show_logs;
        egui::Window::new("Logs")
            .open(&mut open)
            .default_size([700., 440.])
            .max_width(800.)
            .resizable(true)
            .show(ctx, |ui| {
                let view = &mut state.diagnostic_view;
                ui.horizontal(|ui| {
                    for (i, name) in ["Error", "Warning", "Info"].iter().enumerate() {
                        ui.checkbox(&mut view.levels[i], format!("{name} ({})", view.counts[i]));
                    }
                    if ui.button("Clear").clicked() {
                        clear = true;
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut view.query)
                        .hint_text("Search message or path")
                        .desired_width(f32::INFINITY),
                );
                ui.small(format!(
                    "{} rows · {} bytes · {} evicted/rejected · {} pending sink drops",
                    view.snapshot.len(),
                    view.retained_bytes,
                    view.dropped.saturating_add(sink_dropped),
                    sink_dropped
                ));
                egui::ScrollArea::vertical()
                    .id_salt("diagnostic_log_scroll")
                    .show(ui, |ui| {
                        for row in &view.snapshot {
                            if !view.includes(row.id) {
                                continue;
                            }
                            ui.push_id(row.id, |ui| {
                                ui.horizontal(|ui| {
                                    let theme = Theme::current(ui.ctx());
                                    let color = match row.level {
                                        crate::DiagnosticLevel::Error => theme.error,
                                        crate::DiagnosticLevel::Warning => theme.warning,
                                        crate::DiagnosticLevel::Info => theme.muted,
                                    };
                                    ui.colored_label(color, row.level.label());
                                    if row.repeats > 1 {
                                        ui.weak(format!("×{}", row.repeats));
                                    }
                                    if ui.small_button("Copy").clicked() {
                                        ui.ctx().copy_text(format!(
                                            "{} · {}\n{}",
                                            row.level.label(),
                                            row.source,
                                            row.message
                                        ));
                                    }
                                });
                                ui.add(egui::Label::new(row.source.as_ref()).truncate())
                                    .on_hover_text(row.source.as_ref());
                                let first = row.message.lines().next().unwrap_or("");
                                let end = first
                                    .char_indices()
                                    .nth(240)
                                    .map_or(first.len(), |(i, _)| i);
                                egui::collapsing_header::CollapsingState::load_with_default_open(
                                    ui.ctx(),
                                    ui.make_persistent_id("diagnostic-entry"),
                                    false,
                                )
                                .show_header(ui, |ui| {
                                    ui.add(egui::Label::new(&first[..end]).truncate())
                                        .on_hover_text(first);
                                })
                                .body(|ui| {
                                    ui.add(
                                        egui::Label::new(row.message.as_ref())
                                            .wrap()
                                            .selectable(true),
                                    );
                                });
                                ui.separator();
                            });
                        }
                    });
            });
        state.show_logs = open;
        if clear {
            if let Ok(mut log) = state.diagnostics.try_lock() {
                log.clear();
                state
                    .diagnostic_sink_dropped
                    .store(0, std::sync::atomic::Ordering::Relaxed);
            }
            state.diagnostic_notice_logged = None;
        }
    }

    /// Structured status only. Decoder errors and source paths remain in Logs.
    fn draw_load_status(ctx: &egui::Context, state: &mut AppUiState) {
        if !state.show_load_status {
            return;
        }
        let mut open = state.show_load_status;
        egui::Window::new("Load Status")
            .id(egui::Id::new("dataset-load-status-window"))
            .open(&mut open)
            .default_size([340., 240.])
            .show(ctx, |ui| {
                ui.heading(
                    if state.dataset_loading || state.loading_progress.is_some() {
                        "Loading datasets"
                    } else if state.chart_count > 0 || state.bathymetry_count > 0 {
                        "Datasets ready"
                    } else {
                        "No datasets loaded"
                    },
                );
                if let Some((total, loaded)) = state.loading_progress {
                    let fraction = if total == 0 {
                        0.
                    } else {
                        (loaded as f32 / total as f32).clamp(0., 1.)
                    };
                    ui.add(
                        egui::ProgressBar::new(fraction).text(format!("{loaded} / {total} files")),
                    );
                } else if state.dataset_loading {
                    ui.spinner();
                }
                ui.separator();
                let vector_count = state
                    .dataset_layers
                    .iter()
                    .find(|layer| layer.product == "S-101")
                    .map(|layer| layer.files.len())
                    .unwrap_or_else(|| state.chart_count.saturating_sub(state.bathymetry_count));
                ui.label(format!("S-101 chart cells: {vector_count}"));
                ui.label(format!("Chart features: {}", state.feature_count));
                ui.label(format!(
                    "S-102 coverage instances: {}",
                    state.bathymetry_count
                ));
                ui.separator();
                ui.label(if state.verify_dataset_signatures {
                    "Signature checking: ON"
                } else {
                    "Signature checking: OFF"
                });
                if let Some((verified, unsigned)) = state.signature_counts {
                    ui.label(format!(
                        "Session results: {verified} verified / {unsigned} unsigned evaluation"
                    ));
                } else {
                    ui.weak("Signature counters unavailable");
                }
                if ui.button("Open Logs").clicked() {
                    state.show_logs = true;
                }
            });
        state.show_load_status = open;
    }

    pub fn draw_ui(&self, ui_state: &mut AppUiState) {
        if let Some(notice) = ui_state.notice.as_ref() {
            if notice.len() <= DiagnosticLog::MAX_BYTES
                && ui_state.diagnostic_notice_logged.as_deref() != Some(notice.as_str())
            {
                if let Ok(mut log) = ui_state.diagnostics.try_lock() {
                    log.push_notice(notice);
                    if notice.len() <= DiagnosticLog::MAX_BYTES {
                        ui_state.diagnostic_notice_logged = Some(notice.clone());
                    }
                }
            }
        }
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
            ui_state.object_detail_sections = ObjectDetailSections::default();
        }
        let emphasis =
            selection_emphasis(now - ui_state.selection_ui_started, ui_state.reduced_motion);
        if emphasis < 1. && identity.is_some() {
            self.ctx
                .request_repaint_after(std::time::Duration::from_millis(16));
        }

        // One fixed-height row: narrow windows scroll instead of changing chart extent.
        // Left: text menus holding every command. Right: icon shortcuts, each the
        // same command as a menu item of the same name (also its tooltip).
        egui::TopBottomPanel::top("toolbar")
            .exact_height(44.)
            .show(&self.ctx, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("toolbar-scroll")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            Self::draw_menus(ui, ui_state);
                            ui.separator();
                            Self::draw_toolbar_icons(ui, ui_state, &theme);
                        });
                    });
            });

        // Route panel (left side) - shown when route plugin is active
        Self::draw_route_panel(&self.ctx, ui_state);

        crate::mcp_ui::draw(&self.ctx, &mut ui_state.s100_mcp);

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
                    ).on_hover_text("Magnification relative to the fitted dataset view (up to 500×). Nautical scale is shown separately on the chart.");

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

        Self::draw_dataset_tree(&self.ctx, ui_state);
        Self::draw_logs(&self.ctx, ui_state);
        Self::draw_load_status(&self.ctx, ui_state);

        Self::draw_object_panel(&self.ctx, ui_state);

        // About dialog
        if ui_state.show_about {
            crate::ui_overlay_layout::centered_window("About", &self.ctx)
                .collapsible(false)
                .resizable(false)
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
            let selected_owner = if let Some(feature) = ui_state.selected_feature.as_ref() {
                crate::dataset_catalogue_ui::for_feature(
                    &ui_state.dataset_catalogue_bindings,
                    feature,
                )
            } else {
                ui_state.selected_dataset.as_ref().and_then(|id| {
                    crate::dataset_catalogue_ui::for_dataset(
                        &ui_state.dataset_catalogue_bindings,
                        id,
                    )
                })
            };
            let inventory_only = ui_state
                .dataset_layers
                .iter()
                .all(|p| p.product != "S-101" || p.files.is_empty());
            crate::ui_overlay_layout::centered_window("S-101 Catalogues", &self.ctx)
                .collapsible(false)
                .resizable(true)
                .min_width(450.0)
                .show(&self.ctx, |ui| {
                    // Feature Catalogue section
                    ui.heading("Feature Catalogue (FC)");
                    ui.add_space(4.0);

                    if let Some(owner) = selected_owner {
                        Self::draw_catalogue_status(ui, &owner.fc, "Feature Types");
                    } else if inventory_only {
                        ui.weak("Default catalogue inventory");
                        Self::draw_catalogue_status(ui, &ui_state.fc_status, "Feature Types");
                    } else {
                        ui.weak("Select a dataset with an available catalogue owner");
                    }

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // Portrayal Catalogue section
                    ui.heading("Portrayal Catalogue (PC)");
                    ui.add_space(4.0);

                    if let Some(owner) = selected_owner {
                        Self::draw_catalogue_status(ui, &owner.pc, "Symbols/Styles");
                    } else if inventory_only {
                        Self::draw_catalogue_status(ui, &ui_state.pc_status, "Symbols/Styles");
                    }

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

        crate::coordinate_rulers::draw(&self.ctx, ui_state, available);

        // Display-base indication is independent of Debug/panel toggles. Area
        // owns no layout space, so changing scale text cannot resize the chart.
        let pixels_per_point = self.ctx.pixels_per_point();
        if let Some(indication) = ui_state.coverage_scale_indication.filter(|indication| {
            indication.physical_viewport.matches_physical_rect((
                chart_x * pixels_per_point,
                chart_y * pixels_per_point,
                chart_width * pixels_per_point,
                chart_height * pixels_per_point,
            ))
        }) {
            let denominator = indication.viewing_denominator;
            let factor = indication.overscale_factor;
            let text = if factor > 1. {
                format!("Overscale X{factor:.2} · Scale 1:{denominator:.0}")
            } else {
                format!("Scale 1:{denominator:.0}")
            };
            let (text, colour) = match indication.sclbr {
                Some(sclbr) => (text, sclbr),
                None => (format!("{text} · SCLBR unavailable"), theme.error),
            };
            // A scale indication has no wrapping/layout width. Clip the single
            // line to the chart rather than inheriting an Area's shrinking width.
            let clip = egui::Rect::from_min_size(
                egui::pos2(chart_x, chart_y),
                egui::vec2(chart_width, chart_height),
            );
            self.ctx
                .layer_painter(egui::LayerId::new(
                    egui::Order::Background,
                    egui::Id::new("coverage_scale_indication"),
                ))
                .with_clip_rect(clip)
                .text(
                    egui::pos2(chart_x + 12., chart_y + chart_height - 12.),
                    egui::Align2::LEFT_BOTTOM,
                    text,
                    egui::FontId::proportional(14.),
                    colour,
                );
        }

        // Debug overlay (only in debug mode)
        if ui_state.debug_mode {
            Self::draw_debug_overlay(&self.ctx, ui_state, chart_x, chart_y);
        }
    }

    /// Text menus. Labels use Title Case; "…" marks items that open a window
    /// or dialog. Toolbar icons repeat a subset of these under the same names.
    fn draw_menus(ui: &mut egui::Ui, ui_state: &mut AppUiState) {
        ui.menu_button("File", |ui| {
            if ui.button("Open Dataset…").clicked() {
                ui_state.open_file_requested = true;
                ui.close_menu();
            }
            if ui.button("Open Dataset Folder…").clicked() {
                ui_state.open_exchange_requested = true;
                ui.close_menu();
            }
            if ui
                .add_enabled(
                    ui_state.chart_count > 0,
                    egui::Button::new("Close All Datasets"),
                )
                .clicked()
            {
                ui_state.clear_charts_requested = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Open Catalogue Set (FC + PC)…").clicked() {
                ui_state.open_catalogue_set_requested = true;
                ui.close_menu();
            }
            if ui.button("Open Feature Catalogue…").clicked() {
                ui_state.open_fc_requested = true;
                ui.close_menu();
            }
            if ui.button("Open Portrayal Catalogue…").clicked() {
                ui_state.open_pc_requested = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Save Screenshot…").clicked() {
                ui_state.screenshot_requested = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Exit").clicked() {
                ui_state.close_requested = true;
                ui.close_menu();
            }
        });
        ui.menu_button("View", |ui| {
            let mut layers = !ui_state.dataset_tree_hidden;
            if ui.checkbox(&mut layers, "Data Layers").changed() {
                ui_state.dataset_tree_hidden = !layers;
            }
            let mut details = !ui_state.object_details_hidden;
            if ui.checkbox(&mut details, "Object Details").changed() {
                ui_state.object_details_hidden = !details;
            }
            ui.checkbox(&mut ui_state.coordinate_rulers, "Coordinate Rulers");
            ui.separator();
            ui.menu_button("Color Palette", |ui| {
                for profile in ["Day", "Dusk", "Night"] {
                    if ui
                        .radio(ui_state.color_profile == profile, profile)
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
            if ui.button("Viewing Date and Time…").clicked() {
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
            if ui.button("Fit to Chart").clicked() {
                ui_state.reset_view_requested = true;
                ui.close_menu();
            }
        });
        ui.menu_button("Tools", |ui| {
            let mut selecting = ui_state.object_selection_mode;
            if ui.checkbox(&mut selecting, "Select Objects").changed() {
                ui_state.set_object_selection_mode(selecting);
            }
            ui.separator();
            let chart_loaded = ui_state.chart_count > 0;
            let mut toggled = None;
            for btn in &ui_state.plugin_buttons {
                let allowed = Self::plugin_button_allowed(btn, chart_loaded);
                let response = ui
                    .add_enabled(allowed, egui::Button::new(&btn.label).selected(btn.active))
                    .on_disabled_hover_text("Load a chart first");
                if response.clicked() {
                    toggled = Some(btn.plugin_id.clone());
                    ui.close_menu();
                }
            }
            if toggled.is_some() {
                ui_state.plugin_toggle_requested = toggled;
            }
            if ui.button("S-100 MCP Service…").clicked() {
                ui_state.s100_mcp.open = true;
                ui.close_menu();
            }
            ui.separator();
            ui.checkbox(&mut ui_state.show_load_status, "Load Status");
            ui.checkbox(&mut ui_state.show_logs, "Logs");
            if ui.button("Catalogues…").clicked() {
                ui_state.show_catalogues = true;
                ui.close_menu();
            }
            ui.separator();
            let mut debug = ui_state.debug_mode;
            if ui
                .add(egui::Checkbox::new(&mut debug, "Debug Mode"))
                .on_hover_text("Performance statistics and profiling (F12)")
                .changed()
            {
                ui_state.debug_mode = debug;
            }
        });
        ui.menu_button("Settings", |ui| {
            if ui.button("Display Settings…").clicked() {
                ui_state.show_settings = true;
                ui.close_menu();
            }
            ui.separator();
            if ui
                .checkbox(&mut ui_state.reduced_motion, "Reduce UI Motion")
                .changed()
            {
                ui.ctx().request_repaint();
            }
            signature_verification_toggle(
                ui,
                &mut ui_state.verify_dataset_signatures,
                ui_state.signature_verification_locked,
            );
        });
        ui.menu_button("Help", |ui| {
            if ui.button("About FerriteS100…").clicked() {
                ui_state.show_about = true;
                ui.close_menu();
            }
        });
    }

    /// Icon shortcuts grouped as Data | Panels | Navigation | Tools | Status |
    /// Settings. Tooltips are the menu item names.
    fn draw_toolbar_icons(ui: &mut egui::Ui, ui_state: &mut AppUiState, theme: &Theme) {
        if icon_button(ui, Icon::File, "Open Dataset…").clicked() {
            ui_state.open_file_requested = true;
        }
        if icon_button(ui, Icon::Folder, "Open Dataset Folder…").clicked() {
            ui_state.open_exchange_requested = true;
        }
        ui.separator();
        if icon_toggle(
            ui,
            Icon::Layers,
            "Data Layers",
            !ui_state.dataset_tree_hidden,
        )
        .clicked()
        {
            ui_state.dataset_tree_hidden = !ui_state.dataset_tree_hidden;
        }
        if icon_toggle(
            ui,
            Icon::Inspect,
            "Object Details",
            !ui_state.object_details_hidden,
        )
        .clicked()
        {
            ui_state.object_details_hidden = !ui_state.object_details_hidden;
        }
        Self::draw_coordinate_ruler_buttons(ui, ui_state);
        ui.separator();
        if icon_button(ui, Icon::Plus, "Zoom In").clicked() {
            ui_state.zoom_in_requested = true;
        }
        if icon_button(ui, Icon::Minus, "Zoom Out").clicked() {
            ui_state.zoom_out_requested = true;
        }
        if icon_button(ui, Icon::Fit, "Fit to Chart").clicked() {
            ui_state.reset_view_requested = true;
        }
        if icon_button(
            ui,
            Icon::Clock,
            &format!(
                "Viewing Date and Time… ({})",
                ui_state.temporal_view.summary()
            ),
        )
        .clicked()
        {
            ui_state.show_temporal = true;
        }
        ui.separator();
        if icon_toggle(
            ui,
            Icon::Select,
            "Select Objects",
            ui_state.object_selection_mode,
        )
        .clicked()
        {
            ui_state.set_object_selection_mode(!ui_state.object_selection_mode);
        }
        let chart_loaded = ui_state.chart_count > 0;
        let mut toggled = None;
        for btn in &ui_state.plugin_buttons {
            let allowed = Self::plugin_button_allowed(btn, chart_loaded);
            let icon = if Self::is_route_plugin(&btn.plugin_id) {
                Icon::Route
            } else {
                Icon::Plugin
            };
            let tooltip = match (&btn.tooltip, allowed) {
                (_, false) => format!("{} (load a chart first)", btn.label),
                (Some(t), true) if t != &btn.label => format!("{}: {t}", btn.label),
                _ => btn.label.clone(),
            };
            let response = ui
                .add_enabled_ui(allowed, |ui| icon_toggle(ui, icon, &tooltip, btn.active))
                .inner;
            if response.clicked() && allowed {
                toggled = Some(btn.plugin_id.clone());
            }
        }
        if toggled.is_some() {
            ui_state.plugin_toggle_requested = toggled;
        }
        let mcp = ui_state.s100_mcp.health();
        if icon_toggle_with_badge(
            ui,
            Icon::Mcp,
            &format!("S-100 MCP Service… ({})", mcp.label()),
            ui_state.s100_mcp.open,
            mcp.badge(theme),
        )
        .clicked()
        {
            ui_state.s100_mcp.open = !ui_state.s100_mcp.open;
        }
        ui.separator();
        if icon_toggle(ui, Icon::Status, "Load Status", ui_state.show_load_status).clicked() {
            ui_state.show_load_status = !ui_state.show_load_status;
        }
        if icon_toggle(ui, Icon::Logs, "Logs", ui_state.show_logs).clicked() {
            ui_state.show_logs = !ui_state.show_logs;
        }
        if icon_toggle(ui, Icon::Debug, "Debug Mode (F12)", ui_state.debug_mode).clicked() {
            ui_state.debug_mode = !ui_state.debug_mode;
        }
        ui.separator();
        if icon_button(ui, Icon::Settings, "Display Settings…").clicked() {
            ui_state.show_settings = true;
        }
    }

    /// Plugins need a chart, except deactivation and the native route editor.
    fn plugin_button_allowed(btn: &PluginButton, chart_loaded: bool) -> bool {
        chart_loaded || btn.active || btn.plugin_id == "native:s421-route"
    }
    fn is_route_plugin(id: &str) -> bool {
        matches!(id, "native:s421-route" | "com.ferrite.route-planner")
    }

    fn draw_coordinate_ruler_buttons(ui: &mut egui::Ui, state: &mut AppUiState) -> egui::Response {
        let response = icon_toggle(
            ui,
            Icon::Ruler,
            "Coordinate Rulers",
            state.coordinate_rulers,
        );
        if response.clicked() {
            state.coordinate_rulers = !state.coordinate_rulers;
        }
        response
    }

    /// Draw debug overlay on chart (top-left corner, green text)
    fn draw_debug_overlay(ctx: &egui::Context, ui_state: &AppUiState, chart_x: f32, chart_y: f32) {
        let theme = Theme::current(ctx);
        egui::Area::new(egui::Id::new("debug_overlay"))
            .fixed_pos(egui::pos2(chart_x + 10.0, chart_y + 10.0))
            // Chart HUD cannot cover floating windows or menu popups.
            .order(egui::Order::Background)
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
                        ui.label(format!("Redraw/s: {:.1}", ui_state.debug_fps))
                            .on_hover_text("Submitted redraw callbacks per second; on-demand idle redraws are included. This is not physical refresh FPS.");
                        ui.label(ui_state.debug_main_thread_cpu.map_or_else(|| "Main thread CPU: —".into(), |v| format!("Main thread CPU: {v:.1}% (1 core)")))
                            .on_hover_text("Native cumulative current-thread user+kernel CPU time difference over wall time, sampled on the winit main thread every 0.5 seconds. Includes input callbacks; excludes sleep. Not normalized by logical core count.");
                        ui.collapsing("Frame timing and reuse", |ui| {
                        if let Some(s) = &ui_state.debug_cpu_frames.navigation {
                            ui.small("Navigation redraw wall (ms)");
                            ui.label(format!("mean {:.2} · p95 {:.2}", s.mean_ms, s.p95_ms))
                                .on_hover_text("Mean / p95 / p99 / max of navigation-active RedrawRequested host scope. Includes surface waits; excludes independent input callbacks. Not CPU execution time.");
                            ui.small(format!("p99 {:.2} · max {:.2}", s.p99_ms, s.max_ms));
                            ui.label(format!("Over 60 / 144 Hz budget: {:.1}% / {:.1}%", s.over_60_percent, s.over_144_percent));
                            ui.small(format!("n={} · p95 n={} · {:.2}s window{}", s.samples, s.quantile_samples, ui_state.debug_cpu_frames.window_seconds, if ui_state.debug_cpu_frames.navigation_active { " · active" } else { " · settled" }));
                            if let Some(ratio) = s.redraw_wall_interval_percent {
                                ui.small(format!("Redraw wall / entry interval: {ratio:.1}%"))
                                    .on_hover_text("Previous active redraw host wall divided by its following active entry interval. Includes blocking waits. This is NOT main-thread CPU utilization.");
                            }
                        } else { ui.small("Navigation redraw wall: no active samples"); }
                        if ui_state.debug_cpu_frames.fastpath_attempts > 0 {
                            ui.small(format!("Affine reuse: {} / {} accepted", ui_state.debug_cpu_frames.fastpath_accepted, ui_state.debug_cpu_frames.fastpath_attempts));
                            for (name, count) in crate::debug_metrics::REASONS.iter().zip(ui_state.debug_cpu_frames.fastpath_rejections) {
                                if count > 0 { ui.small(format!("{name}: {count}")); }
                            }
                        } else { ui.small("Affine reuse: no attempts in window"); }
                        });
                        ui.label(ui_state.debug_cpu_usage.map_or_else(|| "Process CPU: —".into(), |v| format!("Process CPU: {v:.1}%")))
                            .on_hover_text("Process CPU share of available logical CPU capacity (100% uses all cores)");
                        ui.label(ui_state.debug_memory_mb.map_or_else(|| "RAM: —".into(), |v| format!("RAM: {v:.1} MiB")))
                            .on_hover_text("Current resident memory used by this process");

                        ui.label(ui_state.debug_gpu.timing_label())
                            .on_hover_text("Mean/max of valid chart samples completed within each approximately 0.5-second window; not their GPU execution start times. Excludes UI, surface wait and presentation. This is not GPU utilization.");
                        ui.collapsing("Hardware details and history", |ui| {
                        ui_state.debug_history.draw(ui);
                        ui.label(format!("GPU: {}", ui_state.debug_gpu.adapter))
                            .on_hover_text(format!("Active renderer backend: {}", ui_state.debug_gpu.backend));

                        ui.label(ui_state.debug_gpu.utilization_percent.map_or_else(
                            || "GPU usage: unavailable".into(),
                            |percent| format!("GPU usage: {percent:.1}% (device)"),
                        )).on_hover_text("Device-wide GPU utilization reported by the macOS driver, including other applications. Sampled every 0.5 seconds; not this process alone or chart-pass time.");

                        ui.separator();

                        });

                        // Render stats
                        ui.collapsing("Scene statistics", |ui| {
                        ui.label(format!(
                            "Instructions: {}",
                            ui_state.debug_instruction_count
                        ));
                        ui.label(format!("Symbols: {}", ui_state.debug_symbol_count));
                        ui.label(format!("Charts: {}", ui_state.chart_count));
                        ui.label(format!("Features: {}", ui_state.feature_count));

                        ui.separator();

                        });

                        // View stats
                        ui.label(format!("Zoom: {:.2}x", ui_state.zoom_level)).on_hover_text("Magnification relative to the fitted dataset view; maximum 500×.");
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
        // Exact provider identity only: a plugin whose ID merely contains
        // "route" cannot claim the native route panel or receive its commands.
        let provider = ["native:s421-route", "com.ferrite.route-planner"]
            .into_iter()
            .find(|id| {
                ui_state
                    .plugin_buttons
                    .iter()
                    .any(|b| b.plugin_id == *id && b.active)
            });
        let Some(provider) = provider else {
            return;
        };
        if ui_state.route_panel_provider != Some(provider) {
            ui_state.route_editing = None;
            ui_state.waypoint_editing = None;
            ui_state.route_panel_provider = Some(provider);
        }
        let route_data = if provider == "native:s421-route" {
            ui_state.native_route_ui.clone()
        } else {
            ui_state
                .plugin_ui_data
                .iter()
                .find(|(id, _)| id == provider)
                .and_then(|(_, data)| serde_json::from_str(data).ok())
                .map(std::sync::Arc::new)
        };

        let mut body = |ui: &mut egui::Ui| {
            if let Some(route_ui) = &route_data {
                {
                    let editing = route_ui
                        .get("editing")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let title = route_ui
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Route Plan");

                    if provider != "native:s421-route" {
                        // Header with editing indicator
                        ui.vertical_centered(|ui| {
                            ui.heading(egui::RichText::new(title).size(18.0).strong());
                            if editing {
                                ui.label(
                                    egui::RichText::new("Click on chart to add waypoints")
                                        .size(12.0)
                                        .color(theme.success),
                                );
                            }
                        });
                        ui.separator();
                    }
                    if let Some(notice) = route_ui.get("notice").and_then(|v| v.as_str()) {
                        ui.label(notice);
                    }
                    let display_available = route_ui
                        .get("display_available")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    if display_available {
                        // Rendering toggle at top of panel
                        let rendering_enabled = route_ui
                            .get("rendering_enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);
                        ui.horizontal(|ui| {
                            ui.label("Route Display:");
                            let btn_text = if rendering_enabled { "ON" } else { "OFF" };
                            let btn_color = if rendering_enabled {
                                theme.success
                            } else {
                                theme.error
                            };
                            if ui
                                .add_enabled(
                                    display_available,
                                    egui::Button::new(
                                        egui::RichText::new(btn_text).color(btn_color),
                                    ),
                                )
                                .clicked()
                            {
                                ui_state.plugin_ui_events.push((
                                    provider.to_string(),
                                    r#"{"type":"ToggleRendering"}"#.to_string(),
                                ));
                            }
                        });
                        ui.separator();
                    }
                    // Action buttons
                    if let Some(actions) = route_ui.get("actions").and_then(|v| v.as_array()) {
                        ui.horizontal_wrapped(|ui| {
                            for action in actions {
                                let id = action.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let label =
                                    action.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                                let enabled = action
                                    .get("enabled")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);

                                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                                    let event = match id {
                                        "new" => r#"{"type":"New"}"#,
                                        "finish" => r#"{"type":"Finish"}"#,
                                        "clear" => r#"{"type":"Clear"}"#,
                                        "export" => r#"{"type":"Export"}"#,
                                        "import" => r#"{"type":"Import"}"#,
                                        _ => continue,
                                    };
                                    ui_state
                                        .plugin_ui_events
                                        .push((provider.to_string(), event.to_string()));
                                }
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

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
                                            let route_editable = route.get("editable").and_then(|v| v.as_bool()).unwrap_or(provider != "native:s421-route");
                                            let is_editing = route_editable && ui_state.route_editing.as_ref().is_some_and(|(id, _)| *id == route_id);

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
                                                                        let event = serde_json::json!({"type":"RenameRoute", "id":route_id, "name":new_name}).to_string();
                                                                        ui_state.plugin_ui_events.push((
                                                                            provider.to_string(),
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
                                                                    provider.to_string(),
                                                                    event
                                                                ));
                                                            }
                                                            // Edit route name button
                                                            if !is_editing {
                                                                let edit_btn = ui.add_enabled(route_editable, egui::Button::new("E").small().sense(egui::Sense::click()));
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
                                                    provider.to_string(),
                                                    event
                                                ));
                                            }
                                            ui.add_space(2.0);
                                        }
                                    });
                            ui.add_space(4.0);
                        }
                    }

                    let content_editable = route_ui
                        .get("content_editable")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(provider != "native:s421-route");
                    let turn_radius_editable = route_ui
                        .get("turn_radius_editable")
                        .or_else(|| route_ui.get("content_editable"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let active_route_id = route_ui
                        .get("routes")
                        .and_then(|v| v.as_array())
                        .and_then(|routes| {
                            routes
                                .iter()
                                .find(|r| r.get("active").and_then(|v| v.as_bool()) == Some(true))
                        })
                        .and_then(|r| r.get("id"))
                        .and_then(|v| v.as_u64())
                        .and_then(|id| u32::try_from(id).ok());

                    // Active route waypoints
                    if let Some(_active_idx) =
                        route_ui.get("active_route_index").and_then(|v| v.as_u64())
                    {
                        ui.separator();

                        // Waypoint count and total distance for active route
                        if let Some(count) = route_ui.get("waypoint_count").and_then(|v| v.as_u64())
                        {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Waypoints:").strong());
                                ui.label(format!("{}", count));
                            });
                        }
                        if let Some(dist) = route_ui.get("total_distance").and_then(|v| v.as_str())
                        {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Total:").strong());
                                ui.label(dist);
                            });
                        }
                        ui.add_space(8.0);

                        // Waypoint list with delete and edit buttons
                        if let Some(waypoints) =
                            route_ui.get("waypoints").and_then(|v| v.as_array())
                        {
                            // Text, combo/editor controls, frame margins and spacing.
                            // Only visible waypoint rows allocate/layout widgets.
                            let row_height = (ui.text_style_height(&egui::TextStyle::Body) * 4.0
                                + 52.0)
                                .max(96.0);
                            egui::ScrollArea::vertical().max_height(250.0).show_rows(ui, row_height, waypoints.len(), |ui, range| {
                                    let available_width = ui.available_width();

                                    for wp in &waypoints[range] {
                                        let wp_id = wp.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                        let name = wp.get("name").and_then(|v| v.as_str()).unwrap_or("WP");
                                        let pos = wp.get("position").and_then(|v| v.as_str()).unwrap_or("");
                                        let leg = wp.get("leg_distance").and_then(|v| v.as_str());

                                        // Check if this waypoint is being edited
                                        let is_wp_editing = content_editable && ui_state.waypoint_editing.as_ref().is_some_and(|(id, _)| *id == wp_id);

                                        egui::Frame::new()
                                            .fill(theme.raised)
                                            .corner_radius(4.0)
                                            .inner_margin(egui::Margin::symmetric(8, 4))
                                            .show(ui, |ui| {
                                                ui.set_min_width((available_width - 16.0).max(0.0));
                                                ui.set_min_height(row_height - 10.0);
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
                                                                    let event = serde_json::json!({"type":"RenameWaypoint", "id":wp_id, "name":edit_text}).to_string();
                                                                    ui_state.plugin_ui_events.push((
                                                                        provider.to_string(),
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
                                                            ui.add(egui::Label::new(egui::RichText::new(name).strong()).truncate());
                                                        }
                                                        ui.add(egui::Label::new(egui::RichText::new(pos).size(11.0).color(theme.muted)).truncate());
                                                        if let Some(event) = crate::route_turn_radius_ui::show(ui, provider, active_route_id.unwrap_or(0), wp_id, wp.get("turn_radius_nm").and_then(|v| v.as_f64()), turn_radius_editable && active_route_id.is_some()) {
                                                            ui_state.plugin_ui_events.push((provider.to_string(), event));
                                                        }
                                                        if let Some(d) = leg {
                                                            ui.add(egui::Label::new(egui::RichText::new(format!("Leg: {}", d)).size(11.0).color(theme.accent)).truncate())
                                                                .on_hover_text(format!("WGS84 · initial bearing: {} · final bearing: {}", wp.get("initial_bearing_deg").and_then(|v|v.as_f64()).map(|v|format!("{v:.1}°")).unwrap_or_else(||"Unavailable".into()), wp.get("final_bearing_deg").and_then(|v|v.as_f64()).map(|v|format!("{v:.1}°")).unwrap_or_else(||"Unavailable".into())));
                                                            let geometry = wp.get("leg_geometry").and_then(|v|v.as_str());
                                                            let editable = route_ui.get("leg_geometry_editable").and_then(|v|v.as_bool()).unwrap_or(false);
                                                            let mut chosen = geometry.unwrap_or("").to_owned();
                                                            ui.add_enabled_ui(editable, |ui| {
                                                                egui::ComboBox::from_id_salt(("s421_leg_geometry", wp_id))
                                                                    .selected_text(match chosen.as_str() { "Loxodrome" => "Rhumb line", "Orthodrome" => "Geodesic", _ => "Choose leg geometry" })
                                                                    .show_ui(ui, |ui| {
                                                                        ui.selectable_value(&mut chosen, "Loxodrome".into(), "Rhumb line");
                                                                        ui.selectable_value(&mut chosen, "Orthodrome".into(), "Geodesic");
                                                                    });
                                                            });
                                                            if Some(chosen.as_str()) != geometry && !chosen.is_empty() {
                                                                let event = serde_json::json!({"type":"SetLegGeometry", "id":wp_id, "geometry":chosen}).to_string();
                                                                ui_state.plugin_ui_events.push((provider.to_string(),event));
                                                            }
                                                        }
                                                    });

                                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                        // Delete button - use sense to capture click properly
                                                        let delete_btn = ui.add_enabled(content_editable, egui::Button::new("X").small().sense(egui::Sense::click()));
                                                        if delete_btn.on_hover_text("Delete waypoint").clicked() {
                                                            let event = format!(r#"{{"type":"DeleteWaypoint","id":{}}}"#, wp_id);
                                                            ui_state.plugin_ui_events.push((
                                                                provider.to_string(),
                                                                event
                                                            ));
                                                        }
                                                        // Edit button
                                                        if !is_wp_editing {
                                                            let edit_btn = ui.add_enabled(content_editable, egui::Button::new("E").small().sense(egui::Sense::click()));
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
                    } else if route_ui
                        .get("routes")
                        .and_then(|v| v.as_array())
                        .is_some_and(|r| r.is_empty())
                    {
                        let message = if provider == "native:s421-route" {
                            "No routes. Import an S-421 file."
                        } else {
                            "No routes. Click 'New' to create one."
                        };
                        ui.label(egui::RichText::new(message).size(12.0).color(theme.muted));
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
                                    let loaded =
                                        fc.get("loaded").and_then(|v| v.as_bool()).unwrap_or(false);
                                    let message = fc
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("Unknown");
                                    let color = if loaded { theme.success } else { theme.error };
                                    ui.label(egui::RichText::new(message).color(color).size(11.0));
                                } else {
                                    ui.label(
                                        egui::RichText::new("Not loaded")
                                            .color(theme.muted)
                                            .size(11.0),
                                    );
                                }
                            });

                            // PC Status
                            ui.horizontal(|ui| {
                                ui.label("PC:");
                                if let Some(pc) = route_ui.get("pc_status") {
                                    let loaded =
                                        pc.get("loaded").and_then(|v| v.as_bool()).unwrap_or(false);
                                    let message = pc
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("Unknown");
                                    let color = if loaded { theme.success } else { theme.error };
                                    ui.label(egui::RichText::new(message).color(color).size(11.0));
                                } else {
                                    ui.label(
                                        egui::RichText::new("Not loaded")
                                            .color(theme.muted)
                                            .size(11.0),
                                    );
                                }
                            });

                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("Catalogue path: ./Catalogues/*/S-421/")
                                    .size(10.0)
                                    .color(theme.muted),
                            );
                        });

                    ui.add_space(4.0);

                    // Instructions based on mode
                    if editing {
                        ui.label(egui::RichText::new("Editing Mode:").size(12.0).strong());
                        ui.label(egui::RichText::new("• Left click: Add waypoint").size(11.0));
                        ui.label(egui::RichText::new("• Right click: Remove last").size(11.0));
                        ui.label(egui::RichText::new("• Click 'Finish' when done").size(11.0));
                    } else if provider == "native:s421-route" {
                        // The notice above describes this host's actual capabilities.
                    } else if route_ui.get("active_route_index").is_some() {
                        ui.label(
                            egui::RichText::new("Click 'New' to add another route")
                                .size(12.0)
                                .color(theme.muted),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("Click 'New' to start a route")
                                .size(12.0)
                                .color(theme.muted),
                        );
                    }
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("Route Plan").size(18.0).strong());
                });
                ui.separator();
                ui.label("Loading plugin...");
            }
        };
        if provider == "native:s421-route" {
            let mut open = true;
            egui::Window::new("S-421 Routes")
                .id(egui::Id::new("native_s421_routes"))
                .default_pos([280.0, 70.0])
                .default_width(360.0)
                .default_height(420.0)
                .max_height((ctx.screen_rect().height() - 80.0).max(160.0))
                .open(&mut open)
                .vscroll(true)
                .show(ctx, &mut body);
            if !open {
                ui_state.plugin_toggle_requested = Some(provider.into());
            }
        } else {
            egui::SidePanel::left("route_panel")
                .default_width(280.0)
                .resizable(true)
                .show(ctx, body);
        }
    }

    /// Draw the Settings dialog
    fn draw_settings_dialog(ctx: &egui::Context, ui_state: &mut AppUiState) {
        let theme = Theme::current(ctx);
        // Initialize pending settings when dialog opens
        if ui_state.pending_settings.is_none() {
            ui_state.pending_settings = Some(ui_state.settings.clone());
        }

        let mut open = ui_state.show_settings;
        crate::ui_overlay_layout::display_settings_window(ctx)
            .open(&mut open)
            .show(ctx, |ui| {
                // Get mutable reference to pending settings
                let pending = ui_state.pending_settings.as_mut().unwrap();

                ui.add_space(4.0);

                ui.heading("Digital signatures");
                signature_verification_toggle(ui, &mut ui_state.verify_dataset_signatures, ui_state.signature_verification_locked);
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
                    ui.collapsing("Primary S-101 PC: text and additional layers", |ui| {
                        ui.label("These choices apply only to the primary catalogue. Other catalogue owners retain their own display mode and foundation groups.");
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

                // All controls remain candidates until App validates and commits
                // the complete vector/raster scene. Pattern visibility is included.

                // Check if pending settings differ from applied (for Apply button highlight)
                let has_pending_changes = ui_state
                    .pending_settings
                    .as_ref()
                    .is_some_and(|p| *p != ui_state.settings);

                // Buttons
                ui.horizontal(|ui| {
                    if ui.button("Reset to Defaults").clicked() {
                        ui_state.pending_settings = Some(SettingsState::default());
                        ui_state.verify_dataset_signatures = ui_state.signature_verification_locked;
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

fn signature_verification_toggle(
    ui: &mut egui::Ui,
    enabled: &mut bool,
    locked: bool,
) -> egui::Response {
    if locked {
        *enabled = true;
    }
    ui.add_enabled(
        !locked,
        egui::Checkbox::new(enabled, "Verify digital signatures"),
    )
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
    fn operational_widget_is_disabled_and_restores_required_verification() {
        let context = egui::Context::default();
        let mut enabled = false;
        let _ = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = signature_verification_toggle(ui, &mut enabled, true);
                assert!(!response.enabled());
            });
        });
        assert!(enabled);
    }
    #[test]
    fn settings_signature_widget_clicks_toggle_both_ways_without_portrayal_rebuild() {
        let context = egui::Context::default();
        let mut state = AppUiState::default();
        let portrayal = state.settings.clone();
        let frame = |events, state: &mut AppUiState| {
            let mut rect = egui::Rect::NOTHING;
            let _ = context.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        rect = signature_verification_toggle(
                            ui,
                            &mut state.verify_dataset_signatures,
                            state.signature_verification_locked,
                        )
                        .rect;
                    });
                },
            );
            rect
        };
        let center = frame(Vec::new(), &mut state).center();
        for expected in [true, false] {
            frame(
                vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::PointerButton {
                        pos: center,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    },
                ],
                &mut state,
            );
            frame(
                vec![egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                }],
                &mut state,
            );
            assert_eq!(state.verify_dataset_signatures, expected);
            assert_eq!(state.settings, portrayal);
            assert!(!state.settings_changed);
            assert!(state.pending_settings.is_none());
        }
    }
}

#[cfg(test)]
mod dataset_tree_ui_tests {
    use super::*;
    fn texts(shape: &egui::epaint::Shape, output: &mut Vec<(String, egui::Pos2)>) {
        match shape {
            egui::epaint::Shape::Text(text) => {
                output.push((text.galley.text().to_owned(), text.pos))
            }
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    texts(shape, output);
                }
            }
            _ => {}
        }
    }
    fn draw(
        ctx: &egui::Context,
        state: &mut AppUiState,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Pos2)> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000., 700.),
            )),
            events,
            ..Default::default()
        };
        let frame = ctx.run(raw, |ctx| EguiIntegration::draw_dataset_tree(ctx, state));
        let mut output = Vec::new();
        for clipped in frame.shapes {
            texts(&clipped.shape, &mut output);
        }
        output
    }
    fn state() -> AppUiState {
        AppUiState {
            dataset_layers: vec![DatasetProductLayer {
                product: "S-101".into(),
                fc: "FC 2.0.0".into(),
                pc: "PC 2.0.0".into(),
                files: vec![DatasetLayerEntry {
                    id: DatasetLayerId::S101 {
                        product: "S-101".into(),
                        name: "cell".into(),
                    },
                    name: "CELL.000".into(),
                    source: "/data/CELL.000".into(),
                    detail: "Edition 2".into(),
                }],
            }],
            ..Default::default()
        }
    }
    #[test]
    fn mixed_file_tree_shows_source_catalogues_and_suppresses_global_pair() {
        let ctx = egui::Context::default();
        let mut state = state();
        let first = state.dataset_layers[0].files[0].clone();
        let mut second = first.clone();
        second.id = DatasetLayerId::S101 {
            product: "S-101".into(),
            name: "shom".into(),
        };
        second.name = "SHOM.000".into();
        second.source = "/data/SHOM.000".into();
        state.dataset_layers[0].files.push(second.clone());
        state.catalogue_layers.push(DatasetCatalogueLayer {
            product: "S-101".into(),
            fc: CatalogueStatus {
                loaded: true,
                version: "WRONG-GLOBAL".into(),
                ..Default::default()
            },
            pc: CatalogueStatus::default(),
        });
        for (i, file, version) in [(0, first, "1.0.2"), (1, second, "2.0.0")] {
            state
                .dataset_catalogue_bindings
                .push(crate::DatasetCatalogueBinding {
                    cell_index: Some(i),
                    id: file.id,
                    source: file.source,
                    fc: CatalogueStatus {
                        loaded: true,
                        version: version.into(),
                        ..Default::default()
                    },
                    pc: CatalogueStatus {
                        loaded: true,
                        version: version.into(),
                        ..Default::default()
                    },
                });
        }
        draw(&ctx, &mut state, Vec::new());
        let text = draw(&ctx, &mut state, Vec::new());
        assert!(text.iter().any(|(s, _)| s == "FC 1.0.2 · Loaded"));
        assert!(text.iter().any(|(s, _)| s == "FC 2.0.0 · Loaded"));
        assert!(!text.iter().any(|(s, _)| s.contains("WRONG-GLOBAL")));
    }
    fn click(ctx: &egui::Context, state: &mut AppUiState, position: egui::Pos2) {
        draw(
            ctx,
            state,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        draw(
            ctx,
            state,
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
    }
    #[test]
    fn actual_tree_draws_product_catalogues_and_selection_uses_stable_identity() {
        let ctx = egui::Context::default();
        let mut state = state();
        // Published charts now carry explicit catalogue ownership; product-wide
        // labels cannot stand in for a missing cell owner in this fixture.
        state
            .dataset_catalogue_bindings
            .push(crate::DatasetCatalogueBinding {
                cell_index: Some(0),
                id: state.dataset_layers[0].files[0].id.clone(),
                source: state.dataset_layers[0].files[0].source.clone(),
                fc: CatalogueStatus {
                    loaded: true,
                    version: "2.0.0".into(),
                    ..Default::default()
                },
                pc: CatalogueStatus {
                    loaded: true,
                    version: "2.0.0".into(),
                    ..Default::default()
                },
            });
        draw(&ctx, &mut state, vec![]);
        let output = draw(&ctx, &mut state, vec![]);
        for label in ["FC 2.0.0 · Loaded", "PC 2.0.0 · Loaded", "CELL.000"] {
            assert!(
                output.iter().any(|(text, _)| text == label),
                "missing {label}: {output:?}"
            );
        }
        // Chart Data alone names the file; select its stable row identity.
        let pos = output
            .iter()
            .rev()
            .find(|(text, _)| text == "CELL.000")
            .unwrap()
            .1
            + egui::vec2(3., 5.);
        click(&ctx, &mut state, pos);
        assert_eq!(
            state.dataset_selection_requested,
            Some(state.dataset_layers[0].files[0].id.clone())
        );
        assert_eq!(state.selected_dataset, state.dataset_selection_requested);
    }
    #[test]
    fn catalogue_metadata_is_visible_without_chart_files() {
        let ctx = egui::Context::default();
        let mut state = AppUiState::default();
        let status = CatalogueStatus {
            loaded: true,
            product_id: "S-102".into(),
            version: "3.0.0".into(),
            path: "/catalogues/actual.xml".into(),
            ..Default::default()
        };
        state.catalogue_layers.push(DatasetCatalogueLayer {
            product: "S-102".into(),
            fc: status.clone(),
            pc: status,
        });
        draw(&ctx, &mut state, vec![]);
        let output = draw(&ctx, &mut state, vec![]);
        for label in [
            "Catalogue",
            "Chart Data",
            "FC 3.0.0 · Loaded",
            "PC 3.0.0 · Loaded",
            "No chart data loaded",
        ] {
            assert!(
                output.iter().any(|(text, _)| text == label),
                "missing {label}"
            );
        }
        assert!(state.dataset_selection_requested.is_none());
        assert!(state.dataset_unload_requested.is_none());
    }
    #[test]
    fn busy_tree_disables_unload_then_emits_only_stable_file_request() {
        let ctx = egui::Context::default();
        let mut state = state();
        state.dataset_loading = true;
        draw(&ctx, &mut state, vec![]);
        let output = draw(&ctx, &mut state, vec![]);
        let pos = output.iter().find(|(text, _)| text == "Unload").unwrap().1 + egui::vec2(3., 5.);
        click(&ctx, &mut state, pos);
        assert!(state.dataset_unload_requested.is_none());
        state.dataset_loading = false;
        draw(&ctx, &mut state, vec![]);
        let output = draw(&ctx, &mut state, vec![]);
        // The loading label disappears, so use the button's current position.
        let pos = output.iter().find(|(text, _)| text == "Unload").unwrap().1 + egui::vec2(3., 5.);
        click(&ctx, &mut state, pos);
        assert_eq!(
            state.dataset_unload_requested,
            Some(state.dataset_layers[0].files[0].id.clone())
        );
        state.dataset_tree_hidden = true;
        assert!(draw(&ctx, &mut state, vec![]).is_empty());
    }
}

#[cfg(test)]
mod diagnostics_window_ui_tests {
    use super::*;
    fn texts(s: &egui::epaint::Shape, out: &mut Vec<(String, egui::Pos2)>) {
        match s {
            egui::epaint::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.pos)),
            egui::epaint::Shape::Vec(v) => {
                for s in v {
                    texts(s, out);
                }
            }
            _ => {}
        }
    }
    fn draw(
        ctx: &egui::Context,
        state: &mut AppUiState,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Pos2)> {
        let f = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000., 800.),
                )),
                events,
                ..Default::default()
            },
            |ctx| EguiIntegration::draw_logs(ctx, state),
        );
        let mut out = Vec::new();
        for s in f.shapes {
            texts(&s.shape, &mut out);
        }
        out
    }
    #[test]
    fn actual_logs_window_clear_and_close_preserve_shared_sink() {
        let ctx = egui::Context::default();
        let mut state = AppUiState {
            show_logs: true,
            ..Default::default()
        };
        let sink = state.diagnostics.clone();
        sink.lock().unwrap().push(
            crate::DiagnosticLevel::Warning,
            "/charts/FILE.h5",
            "Metadata sentinel",
        );
        draw(&ctx, &mut state, vec![]);
        let output = draw(&ctx, &mut state, vec![]);
        assert!(output.iter().any(|(t, _)| t == "Metadata sentinel"));
        let pos = output.iter().find(|(t, _)| t == "Clear").unwrap().1 + egui::vec2(3., 5.);
        draw(
            &ctx,
            &mut state,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
            ],
        );
        draw(
            &ctx,
            &mut state,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        );
        assert_eq!(sink.lock().unwrap().entries().count(), 0);
        state.show_logs = false;
        assert!(draw(&ctx, &mut state, vec![]).is_empty());
        sink.lock()
            .unwrap()
            .push(crate::DiagnosticLevel::Error, "decoder", "After clear");
        assert_eq!(state.diagnostics.lock().unwrap().counts(), [1, 0, 0]);
    }
}

#[cfg(test)]
mod panel_layout_regression_tests {
    use super::*;
    #[test]
    fn long_dataset_summary_cannot_expand_object_panel_or_take_chart_space() {
        let ctx = egui::Context::default();
        let mut state = AppUiState {
            notice: Some("Dataset loading complete. ".repeat(1000)),
            ..Default::default()
        };
        for _ in 0..4 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 800.),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    EguiIntegration::draw_object_panel(ctx, &mut state);
                    assert!(
                        ctx.available_rect().width() >= 839.,
                        "Object panel consumed chart width: {:?}",
                        ctx.available_rect()
                    );
                },
            );
        }
        state.object_details_hidden = true;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 800.),
                )),
                ..Default::default()
            },
            |ctx| {
                EguiIntegration::draw_object_panel(ctx, &mut state);
                assert!(ctx.available_rect().width() >= 1199.);
            },
        );
    }
}

#[cfg(test)]
mod load_status_separation_tests {
    use super::*;
    fn texts(shape: &egui::epaint::Shape, output: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(text) => output.push(text.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    texts(shape, output);
                }
            }
            _ => {}
        }
    }
    #[test]
    fn load_window_shows_counts_without_raw_error_or_opening_logs() {
        let ctx = egui::Context::default();
        let mut state = AppUiState {
            show_load_status: true,
            chart_count: 42,
            bathymetry_count: 25,
            feature_count: 200,
            signature_counts: Some((4, 38)),
            notice: Some("RAW_PRIVATE_DECODER_ERROR_SENTINEL".into()),
            security_details: "RAW_SIGNATURE_DETAIL_SENTINEL".into(),
            ..Default::default()
        };
        let mut output = Vec::new();
        for _ in 0..3 {
            let frame = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000., 800.),
                    )),
                    ..Default::default()
                },
                |ctx| EguiIntegration::draw_load_status(ctx, &mut state),
            );
            for shape in frame.shapes {
                texts(&shape.shape, &mut output);
            }
        }
        assert!(output.iter().any(|t| t.contains("S-101 chart cells: 17")));
        assert!(output
            .iter()
            .any(|t| t.contains("S-102 coverage instances: 25")));
        assert!(output
            .iter()
            .any(|t| t.contains("4 verified / 38 unsigned")));
        assert!(!output.iter().any(|t| t.contains("SENTINEL")));
        assert!(!state.show_logs);
        assert!(state.show_load_status);
    }
    #[test]
    fn default_selection_is_off_and_empty_details_consume_no_chart_width() {
        let ctx = egui::Context::default();
        let mut state = AppUiState::default();
        assert!(!state.object_selection_mode);
        assert!(!state.show_logs && !state.show_load_status);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 800.),
                )),
                ..Default::default()
            },
            |ctx| {
                EguiIntegration::draw_object_panel(ctx, &mut state);
                assert!(ctx.available_rect().width() >= 1199.);
            },
        );
    }
}

#[cfg(test)]
mod dataset_panel_width_tests {
    use super::*;
    #[test]
    fn restored_user_width_survives_short_contents_and_data_changes() {
        let ctx = egui::Context::default();
        let mut state = AppUiState::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200., 800.),
            )),
            ..Default::default()
        };
        // Initialize the same persisted panel ID at a width chosen by a user.
        let _ = ctx.run(input(), |ctx| {
            egui::SidePanel::left("dataset_tree_panel")
                .resizable(true)
                .default_width(470.)
                .show(ctx, |ui| {
                    ui.set_min_width(ui.available_width());
                });
        });
        for _ in 0..4 {
            let _ = ctx.run(input(), |ctx| {
                EguiIntegration::draw_dataset_tree(ctx, &mut state);
                assert!(
                    ctx.available_rect().min.x >= 450.,
                    "Panel shrank: {:?}",
                    ctx.available_rect()
                );
            });
        }
        state.catalogue_layers.push(DatasetCatalogueLayer {
            product: "S-102".into(),
            ..Default::default()
        });
        let _ = ctx.run(input(), |ctx| {
            EguiIntegration::draw_dataset_tree(ctx, &mut state);
            assert!(ctx.available_rect().min.x >= 450.);
        });
    }
}

#[cfg(test)]
mod native_route_ui_cache_tests {
    use super::*;
    #[test]
    fn native_dto_is_immutable_between_frames_and_replaced_on_publication() {
        let mut state = AppUiState::default();
        state.publish_plugin_ui_data(vec![(
            "native:s421-route".into(),
            r#"{"title":"First"}"#.into(),
        )]);
        let first = state.native_route_ui.clone().unwrap();
        let frame = state.native_route_ui.clone().unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &frame));
        state.publish_plugin_ui_data(vec![(
            "native:s421-route".into(),
            r#"{"title":"Second"}"#.into(),
        )]);
        assert!(!std::sync::Arc::ptr_eq(
            &first,
            state.native_route_ui.as_ref().unwrap()
        ));
        assert_eq!(first["title"], "First");
        state.publish_plugin_ui_data(vec![(
            "com.attacker.route".into(),
            r#"{"title":"Other"}"#.into(),
        )]);
        assert!(state.native_route_ui.is_none());
    }
}

#[cfg(test)]
mod coordinate_ruler_button_tests {
    use super::*;
    fn frame(ctx: &egui::Context, state: &mut AppUiState, events: Vec<egui::Event>) -> egui::Rect {
        let mut rect = egui::Rect::NOTHING;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(500., 200.),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        rect = EguiIntegration::draw_coordinate_ruler_buttons(ui, state).rect;
                    });
                });
            },
        );
        rect
    }
    #[test]
    fn actual_button_toggles_both_rulers_without_portrayal_requests() {
        let ctx = egui::Context::default();
        let mut state = AppUiState::default();
        assert!(!state.coordinate_rulers);
        for _ in 0..4 {
            let position = frame(&ctx, &mut state, vec![]).center();
            let before = state.coordinate_rulers;
            frame(
                &ctx,
                &mut state,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                &ctx,
                &mut state,
                vec![egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(state.coordinate_rulers, !before);
            assert!(state.pending_settings.is_none());
            assert!(
                !state.color_profile_changed
                    && !state.zoom_in_requested
                    && !state.zoom_out_requested
            );
        }
    }
}

#[cfg(test)]
mod debug_gpu_stats_tests {
    use super::DebugGpuStats;
    #[test]
    fn gpu_status_never_converts_missing_or_stale_data_into_zero_utilization() {
        let mut g = DebugGpuStats::default();
        assert_eq!(g.timing_label(), "GPU chart: unsupported");
        g.timestamp_supported = true;
        assert_eq!(g.timing_label(), "GPU chart: profiling off");
        g.timing_enabled = true;
        assert_eq!(g.timing_label(), "GPU chart: waiting for completed window");
        g.chart_ms = Some(0.68);
        g.chart_max_ms = Some(2.3);
        g.chart_window_samples = 20;
        g.sample_age_seconds = Some(0.1);
        assert_eq!(g.timing_label(), "GPU chart mean/max: 0.68/2.30 ms · n=20");
        g.sample_age_seconds = Some(3.0);
        assert_eq!(
            g.timing_label(),
            "GPU chart mean/max: 0.68/2.30 ms (last 3s ago)"
        );
    }
}

#[cfg(test)]
mod debug_history_tests {
    use super::DebugHistory;
    #[test]
    fn bounded_history_preserves_order_and_missing_measurements() {
        let mut h = DebugHistory::default();
        for n in 0..125 {
            h.record(n as f32, None, Some(f32::NAN));
        }
        assert_eq!(h.used, 120);
        assert_eq!(h.sample(0, 0), Some(5.0));
        assert_eq!(h.sample(119, 0), Some(124.0));
        assert_eq!(h.sample(119, 1), None);
        assert_eq!(h.sample(119, 2), None);
    }
}
