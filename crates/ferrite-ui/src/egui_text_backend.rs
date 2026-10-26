//! egui implementation of the renderer's chart text backend.
use ferrite_gpu::chart_text_backend::{
    epaint, ChartFontContext, ChartTextAtlas, FontPassInput, FontPassOutput, PrivateFontSpec,
};
use std::sync::Arc;

/// An egui `Context` as a chart text layout context.
pub struct EguiFonts(pub egui::Context);

impl ChartFontContext for EguiFonts {
    fn pixels_per_point(&self) -> f32 {
        self.0.pixels_per_point()
    }
    fn zoom_factor(&self) -> f32 {
        self.0.zoom_factor()
    }
    fn layout_job(&self, job: epaint::text::LayoutJob) -> Arc<epaint::Galley> {
        self.0
            .layer_painter(egui::LayerId::background())
            .layout_job(job)
    }
    fn tessellate(
        &self,
        shapes: Vec<epaint::ClippedShape>,
        pixels_per_point: f32,
    ) -> Vec<epaint::ClippedPrimitive> {
        self.0.tessellate(shapes, pixels_per_point)
    }
    fn tessellation_options(&self) -> epaint::TessellationOptions {
        self.0.tessellation_options(|o| *o)
    }
    fn font_image_size(&self) -> [usize; 2] {
        self.0.fonts(|f| f.font_image_size())
    }
    fn font_atlas_fill_ratio(&self) -> f32 {
        self.0.fonts(|fonts| fonts.font_atlas_fill_ratio())
    }
    fn begin_pass(&self, input: &FontPassInput) {
        let mut raw = egui::RawInput {
            screen_rect: input
                .screen_size_points
                .map(|[w, h]| egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))),
            max_texture_side: input.max_texture_side,
            ..Default::default()
        };
        if let Some(density) = input.native_pixels_per_point {
            raw.viewports
                .entry(self.0.viewport_id())
                .or_default()
                .native_pixels_per_point = Some(density);
        }
        self.0.begin_pass(raw);
    }
    fn end_pass(&self) -> FontPassOutput {
        let output = self.0.end_pass();
        FontPassOutput {
            pixels_per_point: output.pixels_per_point,
            textures_delta: output.textures_delta,
        }
    }
    fn new_private(&self, spec: PrivateFontSpec) -> Box<dyn ChartFontContext> {
        // Read options only. No Context clone, live begin_pass, live font definitions or atlas writes.
        let context = egui::Context::default();
        context.options_mut(|o| *o = self.0.options(Clone::clone));
        context.set_zoom_factor(spec.zoom_factor);
        context.set_fonts(spec.fonts);
        Box::new(Self(context))
    }
    fn new_atlas(
        &self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Box<dyn ChartTextAtlas> {
        Box::new(EguiAtlas(egui_wgpu::Renderer::new(
            device, format, None, 1, false,
        )))
    }
}

/// An `egui_wgpu` renderer used only as a glyph atlas.
pub struct EguiAtlas(pub egui_wgpu::Renderer);

impl ChartTextAtlas for EguiAtlas {
    fn update_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: epaint::TextureId,
        delta: &epaint::ImageDelta,
    ) {
        self.0.update_texture(device, queue, id, delta);
    }
    fn bind_group(&self, id: epaint::TextureId) -> Option<wgpu::BindGroup> {
        self.0
            .texture(&id)
            .map(|texture| texture.bind_group.clone())
    }
}
