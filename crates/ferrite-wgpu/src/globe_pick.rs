//! On-demand ID rasterization from the exact prepared GPU buffers.
//! Only the query rectangle is allocated; no per-frame CPU mesh copy.
use super::*;

#[derive(Debug, Clone, Copy)]
pub struct GlobeDrawHit {
    pub draw_index: usize,
    pub distance_px: f64,
    pub pixel: [f64; 2],
}

pub(super) struct PickPipelines {
    base: wgpu::RenderPipeline,
    overlay: wgpu::RenderPipeline,
    screen: wgpu::RenderPipeline,
    texture: Option<wgpu::RenderPipeline>,
    surface_texture: Option<wgpu::RenderPipeline>,
    source_grid_texture: Option<wgpu::RenderPipeline>,
    pattern: Option<wgpu::RenderPipeline>,
    crop: wgpu::Buffer,
    crop_binding: wgpu::BindGroup,
}

/// Shared generated ID shader retains original billboard semantics and adds a
/// physical-coordinate pattern route with crop origin in masked and unmasked modes.
fn pick_shader_source(textured: bool, patterned: bool, masked: bool, grid: bool) -> String {
    let group = if textured || patterned { 1 } else { 0 };
    let declarations = if patterned {
        crate::globe_pattern::PATTERN_SAMPLE_WGSL
    } else if textured {
        "@group(0) @binding(0) var image:texture_2d<f32>; @group(0) @binding(1) var image_sampler:sampler;"
    } else {
        ""
    };
    let alpha = if patterned {
        "if s100_pattern_sample(v.position.xy+crop.query_origin).a<=0. {discard;}"
    } else if grid {
        "if grid_sample(vec4<f32>(v.color.xy,v.origin)).a<=0. {discard;}"
    } else if textured {
        "let sampled=textureSample(image,image_sampler,v.color.xy); let tint=select(1.,v.tint.a,v.color.z>=0.5); if sampled.a*tint<=0. {discard;}"
    } else {
        "if v.color.a<=0. {discard;}"
    };
    let uniform = if masked || patterned {
        format!("struct PickCrop {{ transform:vec4<f32>, query_origin:vec2<f32>, padding:vec2<f32> }}; @group({group}) @binding(0) var<uniform> crop:PickCrop;")
    } else {
        format!("@group({group}) @binding(0) var<uniform> crop:vec4<f32>;")
    };
    let crop_value = if masked || patterned {
        "crop.transform"
    } else {
        "crop"
    };
    let mask = if masked {
        "if !s100_clip_visible(v.position.xy+crop.query_origin) { discard; }"
    } else {
        ""
    };
    let mut code = format!(
        r#"
struct In {{ @location(0) clip:vec4<f32>, @location(1) color:vec4<f32>, @location(2) tint:vec4<f32>, @builtin(instance_index) id:u32 }};
struct Out {{ @builtin(position) position:vec4<f32>, @location(0) color:vec4<f32>, @location(1) tint:vec4<f32>, @location(2) @interpolate(flat) id:u32 }};
{uniform}
{declarations}
@vertex fn vs_main(v:In)->Out {{var o:Out; o.position=vec4<f32>(v.clip.xy*{crop_value}.xy+{crop_value}.zw*v.clip.w,v.clip.zw); o.color=v.color; o.tint=v.tint; o.id=v.id; return o;}}
@fragment fn fs_main(v:Out)->@location(0) u32 {{{alpha} {mask} return v.id;}}
"#
    );
    if grid {
        code=code.replace("@location(2) @interpolate(flat) id:u32", "@location(2) @interpolate(flat) id:u32, @location(3) @interpolate(flat) origin:vec2<f32>")
            .replace("o.id=v.id;", "o.id=v.id;o.origin=v.color.zw;");
        code.push_str(crate::continuous_raster_selector::SELECTOR_WGSL);
        code.push_str(GLOBE_GRID_SAMPLE);
    }
    if masked {
        code.push_str(&crate::coverage_clip::clip_wgsl(group + 1));
    }
    code
}

impl PickPipelines {
    fn new(
        device: &wgpu::Device,
        textures: Option<&wgpu::BindGroupLayout>,
        patterns: Option<&wgpu::BindGroupLayout>,
        coverage_layout: Option<&wgpu::BindGroupLayout>,
    ) -> Self {
        let crop_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Globe pick crop"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let crop = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Globe pick crop"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let crop_binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Globe pick crop"),
            layout: &crop_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: crop.as_entire_binding(),
            }],
        });
        let make = |textured: bool, patterned: bool, mode: GlobeDepthMode| {
            let code = pick_shader_source(textured, patterned, coverage_layout.is_some(), mode==GlobeDepthMode::SourceGridTexture);
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Globe pick ID"),
                source: wgpu::ShaderSource::Wgsl(code.into()),
            });
            let mut layouts = if patterned {
                vec![patterns.unwrap(), &crop_layout]
            } else if textured {
                vec![textures.unwrap(), &crop_layout]
            } else {
                vec![&crop_layout]
            };
            if let Some(coverage_layout) = coverage_layout {
                layouts.push(coverage_layout);
            }
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Globe pick ID"),
                bind_group_layouts: &layouts,
                push_constant_ranges: &[],
            });
            let attributes = wgpu::vertex_attr_array![0=>Float32x4,1=>Float32x4,2=>Unorm8x4];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Globe pick ID"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 36,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attributes,
                    }],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R32Uint,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: mode == GlobeDepthMode::Occluder,
                    depth_compare: depth_compare(mode,textured),
                    stencil: Default::default(),
                    bias: depth_bias(mode,textured),
                }),
                multisample: Default::default(),
                multiview: None,
                cache: None,
            })
        };
        Self {
            base: make(false, false, GlobeDepthMode::Occluder),
            overlay: make(false, false, GlobeDepthMode::SurfaceOverlay),
            screen: make(false, false, GlobeDepthMode::ScreenOverlay),
            texture: textures.map(|_| make(true, false, GlobeDepthMode::SurfaceOverlay)),
            source_grid_texture: textures.map(|_| make(true,false,GlobeDepthMode::SourceGridTexture)),
            surface_texture: textures.map(|_| make(true,false,GlobeDepthMode::SurfaceTexture)),
            pattern: patterns.map(|_| make(false, true, GlobeDepthMode::SurfaceOverlay)),
            crop,
            crop_binding,
        }
    }
}

#[derive(Debug)]
struct Query {
    origin: [u32; 2],
    size: [u32; 2],
    crop: [f32; 4],
}
fn query(viewport: [f64; 2], point: [f64; 2], radius: f64) -> Result<Query, String> {
    if !viewport
        .iter()
        .all(|v| v.is_finite() && *v >= 1. && *v <= 16384.)
        || !point.iter().all(|v| v.is_finite())
        || !radius.is_finite()
        || !(0. ..=64.).contains(&radius)
        || point.iter().zip(viewport).any(|(p, v)| *p < 0. || *p >= v)
    {
        return Err(
            "Invalid globe pick viewport, pixel or radius (maximum 64 physical pixels)".into(),
        );
    }
    let origin = [
        (point[0] - radius).floor().max(0.) as u32,
        (point[1] - radius).floor().max(0.) as u32,
    ];
    let end = [
        (point[0] + radius).floor().min(viewport[0] - 1.) as u32 + 1,
        (point[1] + radius).floor().min(viewport[1] - 1.) as u32 + 1,
    ];
    let size = [end[0] - origin[0], end[1] - origin[1]];
    let crop = [
        viewport[0] / size[0] as f64,
        viewport[1] / size[1] as f64,
        (viewport[0] - 2. * origin[0] as f64 - size[0] as f64) / size[0] as f64,
        (size[1] as f64 - viewport[1] + 2. * origin[1] as f64) / size[1] as f64,
    ]
    .map(|x| x as f32);
    Ok(Query { origin, size, crop })
}

impl GlobeSceneRenderer {
    /// Pick topmost nonzero-alpha draws at physical pixel centers. The radius
    /// collects visible draws around the pointer; fully covered draws are absent.
    /// This single-sample predicate is independent of display MSAA edge coverage.
    pub fn pick_draws(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        point: [f64; 2],
        radius: f64,
    ) -> Result<Vec<GlobeDrawHit>, String> {
        self.validate_coverage()?;
        let q = query(
            self.prepared_viewport
                .ok_or("No prepared globe for picking")?,
            point,
            radius,
        )?;
        if self.pick_pipelines.is_none()
            || (self.pattern_pipeline.is_some()
                && self
                    .pick_pipelines
                    .as_ref()
                    .is_some_and(|p| p.pattern.is_none()))
        {
            self.pick_pipelines = Some(PickPipelines::new(
                device,
                self.pick_texture_layout.as_ref(),
                self.pattern_pipeline.as_ref().map(|_| &self.pattern_layout),
                None,
            ));
        }
        let masked = self.coverage_binding.as_ref().is_some_and(|c| {
            c.actions
                .iter()
                .any(|a| matches!(a, coverage::CoverageAction::Clip(_)))
        });
        if masked
            && (self.pick_coverage_pipelines.is_none()
                || (self.pattern_pipeline.is_some()
                    && self
                        .pick_coverage_pipelines
                        .as_ref()
                        .is_some_and(|p| p.pattern.is_none())))
        {
            self.pick_coverage_pipelines = Some(PickPipelines::new(
                device,
                self.pick_texture_layout.as_ref(),
                self.pattern_pipeline.as_ref().map(|_| &self.pattern_layout),
                Some(&self.coverage_pipelines.as_ref().unwrap().layout),
            ));
        }
        let pipelines = self.pick_pipelines.as_ref().unwrap();
        let crop = [
            q.crop[0],
            q.crop[1],
            q.crop[2],
            q.crop[3],
            q.origin[0] as f32,
            q.origin[1] as f32,
            0.,
            0.,
        ];
        queue.write_buffer(&pipelines.crop, 0, bytemuck::cast_slice(&crop));
        if let Some(p) = &self.pick_coverage_pipelines {
            queue.write_buffer(&p.crop, 0, bytemuck::cast_slice(&crop));
        }
        let make_target = |format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Globe pick query rectangle"),
                size: wgpu::Extent3d {
                    width: q.size[0],
                    height: q.size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let ids = make_target(
            wgpu::TextureFormat::R32Uint,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let depth = make_target(
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let id_view = ids.create_view(&Default::default());
        let depth_view = depth.create_view(&Default::default());
        let stride = (q.size[0] * 4 + 255) & !255;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Globe pick readback"),
            size: (stride * q.size[1]) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Globe pick query"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Globe pick query"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &id_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if self.index_count > 0 {
                pass.set_vertex_buffer(0, self.vertices.as_ref().unwrap().slice(..));
                pass.set_index_buffer(
                    self.indices.as_ref().unwrap().slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                for (draw, range, mode, texture, pattern) in &self.pick_ranges {
                    let action = self
                        .coverage_binding
                        .as_ref()
                        .map(|c| &c.actions[*draw as usize]);
                    if matches!(action, Some(coverage::CoverageAction::Hidden)) {
                        continue;
                    }
                    let clip = if let Some(coverage::CoverageAction::Clip(clip)) = action {
                        Some(clip)
                    } else {
                        None
                    };
                    let pipelines = if clip.is_some() {
                        self.pick_coverage_pipelines.as_ref().unwrap()
                    } else {
                        pipelines
                    };
                    if let Some(pattern) = pattern {
                        pass.set_pipeline(
                            pipelines
                                .pattern
                                .as_ref()
                                .ok_or("Globe pick pattern unavailable")?,
                        );
                        pass.set_bind_group(0, pattern, &[]);
                        pass.set_bind_group(1, &pipelines.crop_binding, &[]);
                    } else if let Some(texture) = texture {
                        pass.set_pipeline(
                            (match mode {GlobeDepthMode::SourceGridTexture=>pipelines.source_grid_texture.as_ref(),GlobeDepthMode::SurfaceTexture=>pipelines.surface_texture.as_ref(),_=>pipelines.texture.as_ref()})
                                .ok_or("Globe pick texture unavailable")?,
                        );
                        pass.set_bind_group(0, texture, &[]);
                        pass.set_bind_group(1, &pipelines.crop_binding, &[]);
                    } else {
                        pass.set_pipeline(match mode {
                            GlobeDepthMode::Occluder => &pipelines.base,
                            GlobeDepthMode::SurfaceOverlay | GlobeDepthMode::SurfaceTexture | GlobeDepthMode::SourceGridTexture => &pipelines.overlay,
                            GlobeDepthMode::ScreenOverlay => &pipelines.screen,
                        });
                        pass.set_bind_group(0, &pipelines.crop_binding, &[]);
                    }
                    if let Some(clip) = clip {
                        pass.set_bind_group(
                            if texture.is_some() || pattern.is_some() {
                                2
                            } else {
                                1
                            },
                            &clip.bind_group,
                            &[],
                        );
                    }
                    pass.draw_indexed(range.clone(), 0, (*draw + 1)..(*draw + 2));
                }
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &ids,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(q.size[1]),
                },
            },
            wgpu::Extent3d {
                width: q.size[0],
                height: q.size[1],
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let bytes = staging.slice(..).get_mapped_range();
        let mut hits: rustc_hash::FxHashMap<usize, GlobeDrawHit> = Default::default();
        for y in 0..q.size[1] {
            for x in 0..q.size[0] {
                let offset = (y * stride + x * 4) as usize;
                let id = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                if id == 0 {
                    continue;
                }
                let pixel = [
                    (q.origin[0] + x) as f64 + 0.5,
                    (q.origin[1] + y) as f64 + 0.5,
                ];
                let distance = (pixel[0] - point[0]).hypot(pixel[1] - point[1]);
                if radius > 0. && distance > radius {
                    continue;
                }
                let hit = GlobeDrawHit {
                    draw_index: (id - 1) as usize,
                    distance_px: distance,
                    pixel,
                };
                hits.entry(hit.draw_index)
                    .and_modify(|old| {
                        if hit.distance_px < old.distance_px {
                            *old = hit;
                        }
                    })
                    .or_insert(hit);
            }
        }
        drop(bytes);
        staging.unmap();
        let mut result: Vec<_> = hits.into_values().collect();
        result.sort_by(|a, b| {
            a.distance_px
                .total_cmp(&b.distance_px)
                .then_with(|| b.draw_index.cmp(&a.draw_index))
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_preserves_physical_pixels_and_bounds_allocations() {
        for viewport in [[640., 400.], [2780., 1934.], [16384., 16384.]] {
            for point in [
                [0.1, 0.1],
                [100.25, 200.75],
                [viewport[0] - 0.1, viewport[1] - 0.1],
            ] {
                for radius in [0., 1., 40., 64.] {
                    let q = query(viewport, point, radius).unwrap();
                    assert!(q.size.iter().all(|x| *x <= 130));
                    let ndc = [
                        2. * point[0] / viewport[0] - 1.,
                        1. - 2. * point[1] / viewport[1],
                    ];
                    let mapped = [
                        (ndc[0] * q.crop[0] as f64 + q.crop[2] as f64 + 1.) * q.size[0] as f64 / 2.,
                        (1. - ndc[1] * q.crop[1] as f64 - q.crop[3] as f64) * q.size[1] as f64 / 2.,
                    ];
                    assert!((mapped[0] - (point[0] - q.origin[0] as f64)).abs() < 0.002);
                    assert!((mapped[1] - (point[1] - q.origin[1] as f64)).abs() < 0.002);
                }
            }
        }
    }
    #[test]
    fn invalid_query_is_rejected_before_gpu_allocation() {
        for point in [[f64::NAN, 0.], [-1., 0.], [640., 0.]] {
            assert!(query([640., 400.], point, 20.).is_err());
        }
        for radius in [-1., 65., f64::INFINITY] {
            assert!(query([640., 400.], [100., 100.], radius).is_err());
        }
    }
}

#[cfg(test)]
mod pattern_shader_tests {
    use super::*;
    #[test]
    fn pattern_id_shaders_validate_and_keep_physical_crop_origin() {
        for masked in [false, true] {
            let source = pick_shader_source(false, true, masked, false);
            assert!(source.contains("s100_pattern_sample(v.position.xy+crop.query_origin)"));
            let module = wgpu::naga::front::wgsl::parse_str(&source).unwrap();
            wgpu::naga::valid::Validator::new(
                wgpu::naga::valid::ValidationFlags::all(),
                wgpu::naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap();
        }
    }
}

#[cfg(test)] mod grid_pick_shader_tests {
    use super::*;
    #[test] fn original_cell_ids_validate_with_crop_and_coverage() {
        for masked in [false,true] {
            let source=pick_shader_source(true,false,masked,true);
            let module=wgpu::naga::front::wgsl::parse_str(&source).unwrap();
            wgpu::naga::valid::Validator::new(wgpu::naga::valid::ValidationFlags::all(),wgpu::naga::valid::Capabilities::all()).validate(&module).unwrap();
        }
    }
}
