//! Test-only chart text backend over `epaint::Fonts`, the font engine the
//! application's egui contexts use. Lets renderer unit tests run without a
//! GUI toolkit; `ferrite-ui` tests cover the egui implementation itself.
use crate::chart_text_backend::{
    ChartFontContext, ChartTextAtlas, FontPassInput, FontPassOutput, PrivateFontSpec,
};
use epaint::{
    text::{FontDefinitions, LayoutJob},
    textures::{TextureOptions, TexturesDelta},
    ClippedPrimitive, ClippedShape, Color32, FontId, Fonts, Galley, ImageDelta,
    TessellationOptions, TextureId,
};
use std::sync::{Arc, Mutex};

/// Unwrapped single-section layout, as egui's `Painter::layout_no_wrap`.
pub(crate) fn layout_no_wrap(
    context: &dyn ChartFontContext,
    text: String,
    font: FontId,
    color: Color32,
) -> Arc<Galley> {
    context.layout_job(LayoutJob::simple(text, font, color, f32::INFINITY))
}

const DEFAULT_MAX_TEXTURE_SIDE: usize = 2048;

struct State {
    definitions: FontDefinitions,
    zoom: f32,
    ppp: f32,
    fonts: Option<Fonts>,
}

/// A text context with egui's default options (one `Fonts` per density).
pub(crate) struct TestFonts(Mutex<State>);

impl Default for TestFonts {
    fn default() -> Self {
        Self::with_fonts(FontDefinitions::default())
    }
}

impl TestFonts {
    pub(crate) fn with_fonts(definitions: FontDefinitions) -> Self {
        Self(Mutex::new(State {
            definitions,
            zoom: 1.,
            ppp: 1.,
            fonts: None,
        }))
    }
    pub(crate) fn set_fonts(&self, definitions: FontDefinitions) {
        let mut state = self.0.lock().unwrap();
        state.definitions = definitions;
        state.fonts = None;
    }
    pub(crate) fn set_zoom_factor(&self, zoom: f32) {
        self.0.lock().unwrap().zoom = zoom;
    }
    fn fonts(&self) -> Fonts {
        self.0
            .lock()
            .unwrap()
            .fonts
            .clone()
            .expect("No fonts available until the first pass")
    }
    pub(crate) fn layout_no_wrap(&self, text: String, font: FontId, color: Color32) -> Arc<Galley> {
        layout_no_wrap(self, text, font, color)
    }
}

impl ChartFontContext for TestFonts {
    fn pixels_per_point(&self) -> f32 {
        self.0.lock().unwrap().ppp
    }
    fn zoom_factor(&self) -> f32 {
        self.0.lock().unwrap().zoom
    }
    fn layout_job(&self, job: LayoutJob) -> Arc<Galley> {
        self.fonts().layout_job(job)
    }
    fn tessellate(
        &self,
        shapes: Vec<ClippedShape>,
        pixels_per_point: f32,
    ) -> Vec<ClippedPrimitive> {
        let atlas = self.fonts().texture_atlas();
        let (font_tex_size, prepared_discs) = {
            let atlas = atlas.lock();
            (atlas.size(), atlas.prepared_discs())
        };
        epaint::Tessellator::new(
            pixels_per_point,
            TessellationOptions::default(),
            font_tex_size,
            prepared_discs,
        )
        .tessellate_shapes(shapes)
    }
    fn tessellation_options(&self) -> TessellationOptions {
        TessellationOptions::default()
    }
    fn font_image_size(&self) -> [usize; 2] {
        self.fonts().font_image_size()
    }
    fn font_atlas_fill_ratio(&self) -> f32 {
        self.fonts().font_atlas_fill_ratio()
    }
    fn begin_pass(&self, input: &FontPassInput) {
        let mut state = self.0.lock().unwrap();
        let ppp = input.native_pixels_per_point.unwrap_or(1.) * state.zoom;
        let side = input.max_texture_side.unwrap_or(DEFAULT_MAX_TEXTURE_SIDE);
        state.ppp = ppp;
        match &state.fonts {
            Some(fonts) => fonts.begin_pass(ppp, side),
            None => {
                let fonts = Fonts::new(ppp, side, state.definitions.clone());
                fonts.begin_pass(ppp, side);
                state.fonts = Some(fonts);
            }
        }
    }
    fn end_pass(&self) -> FontPassOutput {
        let state = self.0.lock().unwrap();
        let mut textures_delta = TexturesDelta::default();
        if let Some(delta) = state.fonts.as_ref().and_then(Fonts::font_image_delta) {
            textures_delta.set.push((TextureId::Managed(0), delta));
        }
        FontPassOutput {
            pixels_per_point: state.ppp,
            textures_delta,
        }
    }
    fn new_private(&self, spec: PrivateFontSpec) -> Box<dyn ChartFontContext> {
        let fonts = Self::with_fonts(spec.fonts);
        fonts.set_zoom_factor(spec.zoom_factor);
        Box::new(fonts)
    }
    fn new_atlas(
        &self,
        device: &wgpu::Device,
        _format: wgpu::TextureFormat,
    ) -> Box<dyn ChartTextAtlas> {
        Box::new(TestAtlas::new(device))
    }
}

/// RGBA8 sRGB atlas textures with egui-wgpu's bind group layout.
pub(crate) struct TestAtlas {
    layout: wgpu::BindGroupLayout,
    textures: std::collections::HashMap<TextureId, (wgpu::Texture, wgpu::BindGroup)>,
}

impl TestAtlas {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("test_text_atlas_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        Self {
            layout,
            textures: Default::default(),
        }
    }
}

impl ChartTextAtlas for TestAtlas {
    fn update_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: TextureId,
        delta: &ImageDelta,
    ) {
        let [width, height] = delta.image.size().map(|n| n as u32);
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let pixels: Vec<Color32> = match &delta.image {
            epaint::ImageData::Color(image) => image.pixels.clone(),
            epaint::ImageData::Font(image) => image.srgba_pixels(None).collect(),
        };
        let origin = match delta.pos {
            Some([x, y]) => wgpu::Origin3d {
                x: x as u32,
                y: y as u32,
                z: 0,
            },
            None => {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("test_text_atlas"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
                });
                let filter = |f| match f {
                    epaint::textures::TextureFilter::Nearest => wgpu::FilterMode::Nearest,
                    epaint::textures::TextureFilter::Linear => wgpu::FilterMode::Linear,
                };
                let TextureOptions {
                    magnification,
                    minification,
                    ..
                } = delta.options;
                let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                    mag_filter: filter(magnification),
                    min_filter: filter(minification),
                    ..Default::default()
                });
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("test_text_atlas"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(
                                &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                });
                self.textures.insert(id, (texture, bind_group));
                wgpu::Origin3d::ZERO
            }
        };
        let (texture, _) = &self.textures[&id];
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels
                .iter()
                .flat_map(|c| c.to_array())
                .collect::<Vec<u8>>(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            size,
        );
    }
    fn bind_group(&self, id: TextureId) -> Option<wgpu::BindGroup> {
        self.textures
            .get(&id)
            .map(|(_, bind_group)| bind_group.clone())
    }
}
