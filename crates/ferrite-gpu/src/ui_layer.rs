//! What the renderer needs from its host: a presentation surface and the
//! overlay (UI panels, selection marker and the live chart text context).
//!
//! The application implements these with winit and egui in `ferrite-ui`.
use crate::chart_text_backend::{epaint, ChartFontContext};
use ferrite_render::host_settings::SettingsState;

/// Physical size in pixels.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct PhysicalSize<P> {
    pub width: P,
    pub height: P,
}
impl<P> PhysicalSize<P> {
    pub const fn new(width: P, height: P) -> Self {
        Self { width, height }
    }
}

/// The window or offscreen target the surface presents to.
///
/// Calls happen on the thread that owns the renderer; a worker never calls in.
pub trait SurfaceHost: Send + Sync {
    fn inner_size(&self) -> PhysicalSize<u32>;
    fn scale_factor(&self) -> f64;
    /// `None` when the platform cannot tell.
    fn is_visible(&self) -> Option<bool>;
    fn has_focus(&self) -> bool;
    fn request_redraw(&self);
}

/// Physical-pixel selection marker drawn above the chart.
pub struct SelectionOverlay {
    pub anchor: [f32; 2],
    pub paths: Vec<Vec<[f32; 2]>>,
}

/// Overlay compositor and the UI state the renderer reads or publishes.
///
/// One frame is `begin_frame`, optional `draw_panels`/`draw_selection`, chart
/// text layout through `fonts`, `end_frame`, `upload_chart_atlas`, and `render`.
pub trait UiLayer {
    /// Live chart text context (shared with the UI text system).
    fn fonts(&self) -> &dyn ChartFontContext;
    /// Initialize font metrics before the first frame without consuming input.
    fn ensure_font_metrics(&mut self);

    fn settings(&self) -> &SettingsState;
    fn settings_mut(&mut self) -> &mut SettingsState;
    fn color_profile(&self) -> &str;
    fn color_profile_mut(&mut self) -> &mut String;
    /// Chart rectangle after UI panels, in points.
    fn chart_area(&self) -> (f32, f32, f32, f32);
    fn selected_feature(&self) -> Option<&SelectedFeature>;
    fn has_selection_candidates(&self) -> bool;
    fn coverage_scale_indication(&self) -> Option<CoverageScaleIndication>;
    fn set_coverage_scale_indication(&mut self, indication: Option<CoverageScaleIndication>);
    fn set_zoom_level(&mut self, zoom: f64);
    fn coordinate_ruler_scaler(&self) -> Option<&ferrite_render::Scaler>;
    fn set_coordinate_ruler_scaler(&mut self, scaler: Option<ferrite_render::Scaler>);
    fn debug_mode(&self) -> bool;
    fn debug_gpu_mut(&mut self) -> &mut DebugGpuStats;
    fn debug_cpu_frames_mut(&mut self) -> &mut crate::debug_metrics::CpuDebugMetrics;

    fn begin_frame(&mut self);
    fn draw_panels(&mut self);
    fn draw_selection(&mut self, selection: SelectionOverlay);
    /// Ends the frame; returns its pixels per point.
    fn end_frame(&mut self) -> f32;
    /// Upload the live atlas deltas of the ended frame before chart glyph draws.
    fn upload_chart_atlas(&mut self, device: &wgpu::Device, queue: &wgpu::Queue);
    fn chart_atlas_bind_group(&self, id: epaint::TextureId) -> Option<wgpu::BindGroup>;
    /// Composite the ended frame onto `view`.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size_in_pixels: [u32; 2],
        pixels_per_point: f32,
    );
}

/// Coverage-owner numerical scale and actual active-PC SCLBR colour.
#[derive(Debug, Clone, Copy)]
pub struct CoverageScaleIndication {
    pub viewing_denominator: f64,
    pub overscale_factor: f64,
    pub physical_viewport: ferrite_render::Viewport,
    /// None is a missing portrayal resource, never a theme-colour substitution.
    pub sclbr: Option<epaint::Color32>,
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
    pub fn timing_label(&self) -> String {
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
