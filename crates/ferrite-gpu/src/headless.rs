//! Offscreen rendering without a window or GUI toolkit: a fixed-size
//! [`SurfaceHost`], an overlay layer that only carries chart text
//! ([`HeadlessUi`], fonts from [`crate::headless_text`]) and
//! [`WgpuRenderer::new_headless`]. Frames are drawn into offscreen textures
//! and read back with [`WgpuRenderer::render_offscreen`].
use crate::chart_text_backend::{epaint, ChartFontContext, ChartTextAtlas, FontPassInput};
use crate::headless_text::{HeadlessAtlas, HeadlessFonts};
use crate::ui_layer::{
    CoverageScaleIndication, DebugGpuStats, PhysicalSize, SelectedFeature, SelectionOverlay,
    SurfaceHost, UiLayer,
};
use crate::{Result, WgpuRenderer};
use ferrite_render::host_settings::SettingsState;
use std::sync::{Arc, Mutex};

/// A fixed-size offscreen target: never visible, never focused.
#[derive(Debug)]
pub struct HeadlessSurface {
    size: Mutex<PhysicalSize<u32>>,
    scale_factor: f64,
}

impl HeadlessSurface {
    pub fn new(size: PhysicalSize<u32>, scale_factor: f64) -> Self {
        Self {
            size: Mutex::new(size),
            scale_factor,
        }
    }
    /// Change the reported size (follow with [`WgpuRenderer::resize`]).
    pub fn set_size(&self, size: PhysicalSize<u32>) {
        if let Ok(mut current) = self.size.lock() {
            *current = size;
        }
    }
}

impl SurfaceHost for HeadlessSurface {
    fn inner_size(&self) -> PhysicalSize<u32> {
        self.size.lock().map(|s| *s).unwrap_or_default()
    }
    fn scale_factor(&self) -> f64 {
        self.scale_factor
    }
    fn is_visible(&self) -> Option<bool> {
        Some(false)
    }
    fn has_focus(&self) -> bool {
        false
    }
    fn request_redraw(&self) {}
}

/// Overlay layer of an offscreen renderer: chart text only (no panels and
/// no selection marker), with the application's chart fonts.
pub struct HeadlessUi {
    surface: Arc<HeadlessSurface>,
    fonts: HeadlessFonts,
    atlas: HeadlessAtlas,
    settings: SettingsState,
    color_profile: String,
    coverage_scale_indication: Option<CoverageScaleIndication>,
    coordinate_ruler_scaler: Option<ferrite_render::Scaler>,
    debug_gpu: DebugGpuStats,
    debug_cpu_frames: crate::debug_metrics::CpuDebugMetrics,
    textures: epaint::textures::TexturesDelta,
}

impl HeadlessUi {
    pub fn new(device: &wgpu::Device, surface: Arc<HeadlessSurface>) -> Self {
        let mut ui = Self {
            surface,
            fonts: HeadlessFonts::with_fonts(crate::chart_fonts::chart_font_definitions()),
            atlas: HeadlessAtlas::new(device),
            settings: SettingsState::default(),
            color_profile: "Day".into(),
            coverage_scale_indication: None,
            coordinate_ruler_scaler: None,
            debug_gpu: DebugGpuStats::default(),
            debug_cpu_frames: Default::default(),
            textures: Default::default(),
        };
        // Fonts exist from the first pass on; scene emission lays out text
        // before the first frame.
        ui.begin_frame();
        ui.end_frame();
        ui
    }
    fn pass_input(&self) -> FontPassInput {
        let size = self.surface.inner_size();
        let density = self.surface.scale_factor() as f32;
        FontPassInput {
            screen_size_points: Some([size.width as f32 / density, size.height as f32 / density]),
            max_texture_side: None,
            native_pixels_per_point: Some(density),
        }
    }
}

impl UiLayer for HeadlessUi {
    fn fonts(&self) -> &dyn ChartFontContext {
        &self.fonts
    }
    fn ensure_font_metrics(&mut self) {}
    fn settings(&self) -> &SettingsState {
        &self.settings
    }
    fn settings_mut(&mut self) -> &mut SettingsState {
        &mut self.settings
    }
    fn color_profile(&self) -> &str {
        &self.color_profile
    }
    fn color_profile_mut(&mut self) -> &mut String {
        &mut self.color_profile
    }
    fn chart_area(&self) -> (f32, f32, f32, f32) {
        let size = self.surface.inner_size();
        let density = self.fonts.pixels_per_point();
        (
            0.,
            0.,
            size.width as f32 / density,
            size.height as f32 / density,
        )
    }
    fn selected_feature(&self) -> Option<&SelectedFeature> {
        None
    }
    fn has_selection_candidates(&self) -> bool {
        false
    }
    fn coverage_scale_indication(&self) -> Option<CoverageScaleIndication> {
        self.coverage_scale_indication
    }
    fn set_coverage_scale_indication(&mut self, indication: Option<CoverageScaleIndication>) {
        self.coverage_scale_indication = indication;
    }
    fn set_zoom_level(&mut self, _zoom: f64) {}
    fn coordinate_ruler_scaler(&self) -> Option<&ferrite_render::Scaler> {
        self.coordinate_ruler_scaler.as_ref()
    }
    fn set_coordinate_ruler_scaler(&mut self, scaler: Option<ferrite_render::Scaler>) {
        self.coordinate_ruler_scaler = scaler;
    }
    fn debug_mode(&self) -> bool {
        false
    }
    fn debug_gpu_mut(&mut self) -> &mut DebugGpuStats {
        &mut self.debug_gpu
    }
    fn debug_cpu_frames_mut(&mut self) -> &mut crate::debug_metrics::CpuDebugMetrics {
        &mut self.debug_cpu_frames
    }
    fn begin_frame(&mut self) {
        self.fonts.begin_pass(&self.pass_input());
    }
    fn draw_panels(&mut self) {}
    fn draw_selection(&mut self, _selection: SelectionOverlay) {}
    fn end_frame(&mut self) -> f32 {
        let output = self.fonts.end_pass();
        self.textures.append(output.textures_delta);
        output.pixels_per_point
    }
    fn upload_chart_atlas(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        for (id, delta) in std::mem::take(&mut self.textures).set {
            self.atlas.update_texture(device, queue, id, &delta);
        }
    }
    fn chart_atlas_bind_group(&self, id: epaint::TextureId) -> Option<wgpu::BindGroup> {
        self.atlas.bind_group(id)
    }
    fn render(
        &mut self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        _view: &wgpu::TextureView,
        _size_in_pixels: [u32; 2],
        _pixels_per_point: f32,
    ) {
    }
}

impl WgpuRenderer<HeadlessUi> {
    /// An offscreen renderer of `size` physical pixels at `scale_factor`
    /// physical pixels per logical pixel, with no window or GUI toolkit.
    pub async fn new_headless_with(
        size: PhysicalSize<u32>,
        scale_factor: f64,
    ) -> Result<(Self, Arc<HeadlessSurface>)> {
        let surface = Arc::new(HeadlessSurface::new(size, scale_factor));
        let host = Arc::clone(&surface);
        let renderer =
            Self::new_headless(Arc::clone(&surface) as Arc<dyn SurfaceHost>, |device, _| {
                HeadlessUi::new(device, host)
            })
            .await?;
        Ok((renderer, surface))
    }
}
