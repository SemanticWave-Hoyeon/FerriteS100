//! Geographic text anchored on the visible WGS84 surface, with physical glyph size.
use crate::globe_scene::{GlobeMesh, GlobeVertex};
use ferrite_kernel::globe_camera::GlobeCamera;
use ferrite_render::{HAlign, TextFootprint, TextInstruction, VAlign};
pub(crate) struct GlobeText {
    pub shape: egui::epaint::ClippedShape,
    pub footprint: TextFootprint,
    pub anchor: [f64; 3],
    pub projected: [f64; 2],
    pub foreground: [u8; 4],
    pub background: Option<GlobeMesh>,
    pub background_color: Option<[u8; 4]>,
    pub pixels_per_point: f32,
}
impl GlobeText {
    pub fn layout(
        t: &TextInstruction,
        c: &GlobeCamera,
        ctx: &egui::Context,
        pixels_per_mm: f64,
    ) -> Result<Option<Self>, String> {
        if !t.has_visible_content() {
            return Ok(None);
        }
        if t.text.len() > 16384
            || !t.font_size.is_finite()
            || t.font_size <= 0.
            || t.font_size > 4096.
            || !pixels_per_mm.is_finite()
            || pixels_per_mm <= 0.
            || ![t.offset.x, t.offset.y].iter().all(|x| x.is_finite())
        {
            return Err("Invalid globe text dimensions/budget".into());
        }
        if t.color
            .to_array()
            .iter()
            .any(|v| !v.is_finite() || !(0. ..=1.).contains(v))
            || t.background.is_some_and(|c| {
                c.to_array()
                    .iter()
                    .any(|v| !v.is_finite() || !(0. ..=1.).contains(v))
            })
        {
            return Err("Invalid globe text color".into());
        }
        let Some(source) = crate::globe_device_point::GlobePointAnchor::resolve(&t.portrayal_origin,t.position,c,pixels_per_mm)? else { return Ok(None); };
        let anchor = source.ecef_m;
        let projected = source.screen_px;
        let rotation = ferrite_render::screen_text_rotation(t, |bearing| source.project_bearing(c,bearing))?;
        let ppp = ctx.pixels_per_point();
        // Same native display calibration as map labels: 1pt=1/72inch.
        let physical_font = t.font_size as f64 * pixels_per_mm * 25.4 / 72.;
        if !physical_font.is_finite() || physical_font > 1024. {
            return Err("Globe font raster size exceeds backend budget".into());
        }
        let mut job = egui::text::LayoutJob::single_section(
            t.text.clone(),
            egui::TextFormat {
                font_id: egui::FontId {
                    size: physical_font as f32 / ppp,
                    family: if t.bold {
                        egui::FontFamily::Name("ChartBold".into())
                    } else {
                        egui::FontFamily::Proportional
                    },
                },
                color: egui::Color32::WHITE,
                italics: t.italic,
                ..Default::default()
            },
        );
        job.wrap = egui::text::TextWrapping {
            max_rows: 1,
            break_anywhere: false,
            ..Default::default()
        };
        let galley = ctx
            .layer_painter(egui::LayerId::background())
            .layout_job(job);
        if galley.is_empty() {
            return Ok(None);
        }
        let sx = (projected[0] + t.offset.x as f64 * pixels_per_mm) as f32 / ppp;
        let sy = (projected[1] - t.offset.y as f64 * pixels_per_mm) as f32 / ppp;
        let x = match t.h_align {
            HAlign::Left => 0.,
            HAlign::Center => -galley.rect.width() / 2.,
            HAlign::Right => -galley.rect.width(),
        };
        let y = match t.v_align {
            VAlign::Top => 0.,
            VAlign::Middle => -galley.rect.height() / 2.,
            VAlign::Bottom => -galley.rect.height(),
        };
        let angle = rotation.to_radians();
        let (sin, cos) = angle.sin_cos();
        let origin = egui::pos2(sx + x * cos - y * sin, sy + x * sin + y * cos);
        let bounds = if t.background.is_some() {
            galley.rect.union(galley.mesh_bounds)
        } else {
            galley.mesh_bounds
        };
        if !bounds.is_finite() || !bounds.is_positive() {
            return Ok(None);
        }
        let footprint = TextFootprint::rotated(
            [
                origin.x + bounds.min.x * cos - bounds.min.y * sin,
                origin.y + bounds.min.x * sin + bounds.min.y * cos,
            ],
            [bounds.width(), bounds.height()],
            angle,
        );
        let [min, max] = footprint.bounds();
        let viewport = c.viewport();
        if max[0] <= 0.
            || max[1] <= 0.
            || min[0] * ppp >= viewport[0] as f32
            || min[1] * ppp >= viewport[1] as f32
        {
            return Ok(None);
        }
        let mut background_color = None;
        let background = if let Some(color) = t
            .background
            .filter(|color| color.a.is_finite() && color.a > 0.)
        {
            background_color = Some(
                egui::Color32::from_rgba_unmultiplied(
                    (color.r * 255.) as u8,
                    (color.g * 255.) as u8,
                    (color.b * 255.) as u8,
                    (color.a * 255.) as u8,
                )
                .to_array(),
            );
            let mut vertices = Vec::new();
            for p in footprint.corners {
                vertices.push(GlobeVertex {
                    ecef_m: c
                        .offset_pixels(
                            anchor,
                            [
                                p[0] as f64 * ppp as f64 - projected[0],
                                p[1] as f64 * ppp as f64 - projected[1],
                            ],
                        )
                        .map_err(|e| e.to_string())?,
                    color: [egui::epaint::WHITE_UV.x, egui::epaint::WHITE_UV.y, 0., 1.],
                });
            }
            Some(GlobeMesh {
                vertices,
                indices: vec![0, 1, 2, 0, 2, 3],
            })
        } else {
            None
        };
        let background = if background_color.is_some_and(|c| c[3] == 0) {
            background_color = None;
            None
        } else {
            background
        };
        let foreground = egui::Color32::from_rgba_unmultiplied(
            (t.color.r * 255.) as u8,
            (t.color.g * 255.) as u8,
            (t.color.b * 255.) as u8,
            (t.color.a * 255.) as u8,
        )
        .to_array();
        Ok(Some(Self {
            shape: egui::epaint::ClippedShape {
                clip_rect: egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(viewport[0] as f32 / ppp, viewport[1] as f32 / ppp),
                ),
                shape: egui::epaint::TextShape::new(origin, galley, egui::Color32::WHITE)
                    .with_angle(angle)
                    .into(),
            },
            footprint,
            anchor,
            projected: projected,
            foreground,
            background,
            background_color,
            pixels_per_point: ppp,
        }))
    }
    pub fn mesh(&self, c: &GlobeCamera, ctx: &egui::Context) -> Result<GlobeMesh, String> {
        let mut result = GlobeMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        };
        if self.foreground[3] == 0 {
            return Ok(result);
        }
        for job in ctx.tessellate(vec![self.shape.clone()], self.pixels_per_point) {
            if let egui::epaint::Primitive::Mesh(mesh) = job.primitive {
                if mesh.texture_id != egui::TextureId::default() {
                    return Err("Globe text requires shared font atlas".into());
                }
                let base = result.vertices.len() as u32;
                if result.vertices.len() + mesh.vertices.len() > 262144
                    || result.indices.len() + mesh.indices.len() > 1572864
                {
                    return Err("Globe glyph mesh budget exceeded".into());
                }
                for v in mesh.vertices {
                    let expected = [
                        v.pos.x as f64 * self.pixels_per_point as f64,
                        v.pos.y as f64 * self.pixels_per_point as f64,
                    ];
                    let ecef_m = c
                        .offset_pixels(
                            self.anchor,
                            [
                                expected[0] - self.projected[0],
                                expected[1] - self.projected[1],
                            ],
                        )
                        .map_err(|e| e.to_string())?;
                    let clip = c.clip_ecef(ecef_m).map_err(|e| e.to_string())?;
                    let viewport = c.viewport();
                    let actual = [
                        (clip[0] / clip[3] + 1.) * viewport[0] / 2.,
                        (1. - clip[1] / clip[3]) * viewport[1] / 2.,
                    ];
                    if (actual[0] - expected[0]).hypot(actual[1] - expected[1]) > 1e-5 {
                        return Err("Globe glyph physical projection drift".into());
                    }
                    result.vertices.push(GlobeVertex {
                        ecef_m,
                        color: [v.uv.x, v.uv.y, 0., 1.],
                    });
                }
                result
                    .indices
                    .extend(mesh.indices.into_iter().map(|i| i + base));
            }
        }
        result.validate()?;
        Ok(result)
    }
}
