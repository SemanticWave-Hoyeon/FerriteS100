//! The egui overlay layer: panels, selection marker, chart text context and
//! compositing for `ferrite_gpu::WgpuRenderer`.
use crate::egui_integration::{self, AppUiState, EguiIntegration};
use crate::egui_text_backend::EguiFonts;
use ferrite_gpu::chart_text_backend::{epaint, ChartFontContext};
use ferrite_gpu::{
    CoverageScaleIndication, DebugGpuStats, PhysicalSize, SelectedFeature, SelectionOverlay,
    SettingsState, SurfaceHost, UiLayer,
};
use std::sync::Arc;
use winit::event::WindowEvent;
use winit::window::Window;

/// The renderer with the application's egui overlay.
pub type WgpuRenderer = ferrite_gpu::WgpuRenderer<EguiLayer>;

/// Create a renderer presenting to `window` with the egui overlay.
pub async fn create_renderer(window: Arc<Window>) -> ferrite_gpu::Result<WgpuRenderer> {
    WgpuRenderer::new(
        window.clone().into(),
        Arc::new(WinitSurface(window.clone())),
        |device, format| EguiLayer::new(device, format, window),
    )
    .await
}

/// A winit window as the renderer's presentation host.
pub struct WinitSurface(pub Arc<Window>);
impl SurfaceHost for WinitSurface {
    fn inner_size(&self) -> PhysicalSize<u32> {
        let size = self.0.inner_size();
        PhysicalSize::new(size.width, size.height)
    }
    fn scale_factor(&self) -> f64 {
        self.0.scale_factor()
    }
    fn is_visible(&self) -> Option<bool> {
        self.0.is_visible()
    }
    fn has_focus(&self) -> bool {
        self.0.has_focus()
    }
    fn request_redraw(&self) {
        self.0.request_redraw()
    }
}

/// egui integration, application UI state and the pending overlay frame.
pub struct EguiLayer {
    /// egui integration for UI overlay
    egui: EguiIntegration,
    fonts: EguiFonts,
    window: Arc<Window>,
    /// UI state shared with main app
    pub ui_state: AppUiState,
    output: Option<egui::FullOutput>,
}

impl EguiLayer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, window: Arc<Window>) -> Self {
        let egui = EguiIntegration::new(
            device,
            format,
            1, // No MSAA for egui
            window.clone(),
        );
        Self {
            fonts: EguiFonts(egui.ctx.clone()),
            egui,
            window,
            ui_state: AppUiState::default(),
            output: None,
        }
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    /// Handle winit window event for egui, returns true if egui consumed the event
    pub fn handle_egui_event(&mut self, event: &WindowEvent) -> bool {
        self.egui.handle_event(&self.window, event)
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
    pub fn take_chart_image_request(&mut self) -> bool {
        std::mem::take(&mut self.ui_state.chart_image_requested)
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
        self.ui_state
            .pan_adjust_pixels
            .take()
            .map(|adjust| adjust * self.egui.ctx.pixels_per_point())
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
        self.ui_state.publish_plugin_ui_data(data);
    }

    /// Take pending plugin UI events
    #[inline]
    pub fn take_plugin_ui_events(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.ui_state.plugin_ui_events)
    }
}

impl UiLayer for EguiLayer {
    fn fonts(&self) -> &dyn ChartFontContext {
        &self.fonts
    }
    fn ensure_font_metrics(&mut self) {
        self.egui.ensure_font_metrics(&self.window);
    }
    fn settings(&self) -> &SettingsState {
        &self.ui_state.settings
    }
    fn settings_mut(&mut self) -> &mut SettingsState {
        &mut self.ui_state.settings
    }
    fn color_profile(&self) -> &str {
        &self.ui_state.color_profile
    }
    fn color_profile_mut(&mut self) -> &mut String {
        &mut self.ui_state.color_profile
    }
    fn chart_area(&self) -> (f32, f32, f32, f32) {
        self.ui_state.chart_area
    }
    fn selected_feature(&self) -> Option<&SelectedFeature> {
        self.ui_state.selected_feature.as_ref()
    }
    fn has_selection_candidates(&self) -> bool {
        !self.ui_state.selection_candidates.is_empty()
    }
    fn coverage_scale_indication(&self) -> Option<CoverageScaleIndication> {
        self.ui_state.coverage_scale_indication
    }
    fn set_coverage_scale_indication(&mut self, indication: Option<CoverageScaleIndication>) {
        self.ui_state.coverage_scale_indication = indication;
    }
    fn set_zoom_level(&mut self, zoom: f64) {
        self.ui_state.zoom_level = zoom;
    }
    fn coordinate_ruler_scaler(&self) -> Option<&ferrite_render::Scaler> {
        self.ui_state.coordinate_ruler_scaler.as_ref()
    }
    fn set_coordinate_ruler_scaler(&mut self, scaler: Option<ferrite_render::Scaler>) {
        self.ui_state.coordinate_ruler_scaler = scaler;
    }
    fn debug_mode(&self) -> bool {
        self.ui_state.debug_mode
    }
    fn debug_gpu_mut(&mut self) -> &mut DebugGpuStats {
        &mut self.ui_state.debug_gpu
    }
    fn debug_cpu_frames_mut(&mut self) -> &mut ferrite_gpu::debug_metrics::CpuDebugMetrics {
        &mut self.ui_state.debug_cpu_frames
    }

    fn begin_frame(&mut self) {
        self.egui.begin_frame(&self.window);
    }
    fn draw_panels(&mut self) {
        self.egui.draw_ui(&mut self.ui_state);
    }
    fn draw_selection(&mut self, selection: SelectionOverlay) {
        let ppp = self.egui.ctx.pixels_per_point();
        let [x, y] = selection.anchor;
        let pos = egui::pos2(x / ppp, y / ppp);
        let (x, y, w, h) = self.ui_state.chart_area;
        let clip = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h));
        let painter = self
            .egui
            .ctx
            .layer_painter(egui::LayerId::background())
            .with_clip_rect(clip);
        for path in &selection.paths {
            let points: Vec<_> = path
                .iter()
                .map(|p| egui::pos2(p[0] / ppp, p[1] / ppp))
                .collect();
            if points.len() >= 2 {
                painter.add(egui::Shape::line(
                    points.clone(),
                    egui::Stroke::new(5.0_f32, egui::Color32::BLACK),
                ));
                painter.add(egui::Shape::line(
                    points,
                    egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 230, 255)),
                ));
            }
        }
        painter.circle_stroke(pos, 18.0, egui::Stroke::new(5.0_f32, egui::Color32::BLACK));
        painter.circle_stroke(
            pos,
            18.0,
            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 230, 255)),
        );
        if let Some(f) = &self.ui_state.selected_feature {
            painter.text(
                pos + egui::vec2(23.0, -22.0),
                egui::Align2::LEFT_BOTTOM,
                &f.feature_type,
                egui::FontId::proportional(13.0),
                egui::Color32::BLACK,
            );
        }
    }
    fn end_frame(&mut self) -> f32 {
        let output = self.egui.end_frame(&self.window);
        let pixels_per_point = output.pixels_per_point;
        self.output = Some(output);
        pixels_per_point
    }
    fn upload_chart_atlas(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if let Some(output) = self.output.as_mut() {
            self.egui.prepare_chart_atlas(device, queue, output);
        }
    }
    fn chart_atlas_bind_group(&self, id: epaint::TextureId) -> Option<wgpu::BindGroup> {
        self.egui.chart_atlas_bind_group(id)
    }
    fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size_in_pixels: [u32; 2],
        pixels_per_point: f32,
    ) {
        if let Some(output) = self.output.take() {
            self.egui.render(
                device,
                queue,
                encoder,
                view,
                egui_wgpu::ScreenDescriptor {
                    size_in_pixels,
                    pixels_per_point,
                },
                output,
            );
        }
    }
}
