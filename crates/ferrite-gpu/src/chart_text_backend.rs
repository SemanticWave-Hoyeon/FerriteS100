//! Chart text system supplied by the host.
//!
//! Chart labels are laid out, tessellated and rasterized by a text context the
//! host owns (in the application, egui's `Context` and `egui_wgpu` atlas,
//! implemented in `ferrite-ui`). The renderer only sees `epaint` shapes,
//! galleys and texture deltas, so it does not depend on a GUI toolkit.
use std::sync::Arc;

pub use epaint;
use epaint::{
    text::{FontDefinitions, LayoutJob},
    textures::TexturesDelta,
    ClippedPrimitive, ClippedShape, Galley, ImageDelta, TessellationOptions, TextureId,
};

/// Input of one layout pass on a text context.
///
/// Every `None` keeps the context's default for that input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontPassInput {
    /// Screen size in points, anchored at the origin.
    pub screen_size_points: Option<[f32; 2]>,
    pub max_texture_side: Option<usize>,
    /// Native display density of the root viewport.
    pub native_pixels_per_point: Option<f32>,
}

/// Result of one layout pass on a text context.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontPassOutput {
    pub pixels_per_point: f32,
    pub textures_delta: TexturesDelta,
}

/// Fonts and zoom for a context that never shares the live UI atlas.
pub struct PrivateFontSpec {
    pub zoom_factor: f32,
    pub fonts: FontDefinitions,
}

/// A text layout context: fonts, glyph atlas and tessellation.
pub trait ChartFontContext: Send + Sync {
    fn pixels_per_point(&self) -> f32;
    fn zoom_factor(&self) -> f32;
    /// Lay out one job with the fonts of the current density.
    fn layout_job(&self, job: LayoutJob) -> Arc<Galley>;
    fn tessellate(&self, shapes: Vec<ClippedShape>, pixels_per_point: f32)
        -> Vec<ClippedPrimitive>;
    fn tessellation_options(&self) -> TessellationOptions;
    fn font_image_size(&self) -> [usize; 2];
    fn font_atlas_fill_ratio(&self) -> f32;
    fn begin_pass(&self, input: &FontPassInput);
    fn end_pass(&self) -> FontPassOutput;
    /// A new context with this context's options and the given zoom and fonts.
    /// It shares no state, fonts or atlas with `self`.
    fn new_private(&self, spec: PrivateFontSpec) -> Box<dyn ChartFontContext>;
    /// A new GPU glyph atlas for a private context.
    fn new_atlas(
        &self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Box<dyn ChartTextAtlas>;
}

/// GPU textures for a text context's atlas deltas.
pub trait ChartTextAtlas: Send + Sync {
    fn update_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: TextureId,
        delta: &ImageDelta,
    );
    /// Bind group compatible with the chart text pipeline's texture layout.
    fn bind_group(&self, id: TextureId) -> Option<wgpu::BindGroup>;
}
