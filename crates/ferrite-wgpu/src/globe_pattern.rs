//! Periodic area material. Sampling uses local physical output coordinates in
//! color, coverage and ID passes; it must never reuse point-billboard UV tags.
use ferrite_render::PatternLattice;
use wgpu::util::DeviceExt;

/// Shared fragment implementation. The ID crop pass must add its query origin
/// before calling this function; its local crop position is not the chart origin.
/// Explicit level zero gives identical sampling after coverage/alpha discards.
pub const PATTERN_SAMPLE_WGSL: &str = r#"
struct PatternParams { row0:vec4<f32>, row1:vec4<f32> };
@group(0) @binding(0) var pattern_image:texture_2d<f32>;
@group(0) @binding(1) var pattern_sampler:sampler;
@group(0) @binding(2) var<uniform> pattern:PatternParams;
fn s100_pattern_sample(physical:vec2<f32>)->vec4<f32> {
    let uv=vec2<f32>(dot(pattern.row0.xy,physical)+pattern.row0.z,
                    dot(pattern.row1.xy,physical)+pattern.row1.z);
    return textureSampleLevel(pattern_image,pattern_sampler,uv,0.);
}
"#;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PatternParams {
    pub row0: [f32; 4],
    pub row1: [f32; 4],
}
impl PatternParams {
    /// Bound uniform quantization and fragment dot arithmetic over the complete
    /// physical pane. Finite inverse coefficients alone do not establish usable
    /// subpixel precision for nearly singular lattices.
    pub fn for_view(
        lattice: PatternLattice,
        origin: [f64; 2],
        extent: [f64; 2],
    ) -> Result<Self, String> {
        if extent.iter().any(|v| !v.is_finite() || *v <= 0.) {
            return Err("Invalid pattern physical viewport".into());
        }
        let params = Self::new(lattice, origin)?;
        let inverse = lattice.inverse();
        let phase = lattice.phase(origin)?;
        let mut errors = [0.; 2];
        for (i, row) in [params.row0, params.row1].iter().enumerate() {
            let magnitude = f64::from(row[0]).abs() * extent[0]
                + f64::from(row[1]).abs() * extent[1]
                + f64::from(row[2]).abs();
            errors[i] = (f64::from(row[0]) - inverse[i][0]).abs() * extent[0]
                + (f64::from(row[1]) - inverse[i][1]).abs() * extent[1]
                + (f64::from(row[2]) - phase[i]).abs()
                + 4. * f64::from(f32::EPSILON) * magnitude
                + 4. * f64::EPSILON
                    * (inverse[i][0].abs() * origin[0].abs()
                        + inverse[i][1].abs() * origin[1].abs());
        }
        let columns = lattice.columns();
        let error_x = columns[0][0].abs() * errors[0] + columns[1][0].abs() * errors[1];
        let error_y = columns[0][1].abs() * errors[0] + columns[1][1].abs() * errors[1];
        if !error_x.is_finite() || !error_y.is_finite() || error_x.max(error_y) > 0.125 {
            return Err("Pattern GPU precision budget exceeded".into());
        }
        Ok(params)
    }
    pub fn new(lattice: PatternLattice, authored_origin_px: [f64; 2]) -> Result<Self, String> {
        let phase = lattice.phase(authored_origin_px)?;
        let inverse = lattice.inverse();
        let rows = [
            [
                inverse[0][0] as f32,
                inverse[0][1] as f32,
                phase[0] as f32,
                0.,
            ],
            [
                inverse[1][0] as f32,
                inverse[1][1] as f32,
                phase[1] as f32,
                0.,
            ],
        ];
        if !rows.iter().flatten().all(|v| v.is_finite()) {
            return Err("Pattern GPU uniform overflow".into());
        }
        // Rounding a reduced phase to exactly one is equivalent to zero for a
        // repeating sampler. Never round or clamp the authored lattice itself.
        Ok(Self {
            row0: rows[0],
            row1: rows[1],
        })
    }
}

pub fn create_pattern_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("S100 output-lattice pattern material"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: std::num::NonZeroU64::new(32),
                },
                count: None,
            },
        ],
    })
}

/// A texture and its authored lattice are resolved as one resource. The scene
/// derives phase parameters from this same lattice, never a caller's substitute.
pub(crate) struct PreparedPatternResource {
    pub texture: Option<std::sync::Arc<PatternTexture>>,
    pub whole: Option<PreparedWholeResource>,
    pub lattice: PatternLattice,
}
pub(crate) struct PreparedWholeResource {
    pub texture: std::sync::Arc<crate::whole_motif_gpu::NaturalMotifTexture>,
    pub resource: std::sync::Arc<crate::whole_motif::NaturalMotifResource>,
}
pub(crate) type PatternResources =
    std::collections::HashMap<usize, Result<PreparedPatternResource, String>>;

/// Uploaded once per immutable PC/color/lattice/device resource key. A distinct
/// per-frame binding can reuse it for any authored AreaCRS origin.
pub struct PatternTexture {
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    pub key: String,
    pub rgba_bytes: usize,
    pub has_coverage: bool,
}
impl PatternTexture {
    pub fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        cell: &crate::PeriodicPatternGeometry,
    ) -> Result<Self, String> {
        let dimension = device.limits().max_texture_dimension_2d;
        if cell.width == 0 || cell.height == 0 || cell.width > dimension || cell.height > dimension
        {
            return Err("Pattern texture exceeds device dimension budget".into());
        }
        let bytes = (cell.width as usize)
            .checked_mul(cell.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("Pattern texture byte overflow")?;
        if bytes > 64 * 1024 * 1024 || cell.pixels.len() != bytes {
            return Err("Invalid pattern texture byte size".into());
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Periodic S100 SVG cell"),
            size: wgpu::Extent3d {
                width: cell.width,
                height: cell.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &cell.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(cell.width * 4),
                rows_per_image: Some(cell.height),
            },
            texture.size(),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Periodic SVG sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            view: texture.create_view(&Default::default()),
            sampler,
            key: cell.name.clone(),
            rgba_bytes: bytes,
            has_coverage: cell.pixels.iter().skip(3).step_by(4).any(|a| *a != 0),
        })
    }
    pub fn material(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        params: PatternParams,
    ) -> PatternMaterial {
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("S100 authored pattern phase"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("S100 area pattern"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        PatternMaterial { bind_group, params, kind: AreaMaterialKind::Periodic }
    }
}
/// Different phases/lattices are different materials even when textures match.
/// GPU ranges may merge only when the complete binding identity also matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaMaterialKind { Periodic, WholeMotif }

pub struct PatternMaterial {
    pub bind_group: wgpu::BindGroup,
    pub params: PatternParams,
    pub(crate) kind: AreaMaterialKind,
}
impl PatternMaterial {
    pub fn kind(&self) -> AreaMaterialKind { self.kind }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewport_precision_gate_accepts_calibrated_lattice_and_rejects_cancellation() {
        let normal = PatternLattice::from_mm((4., 0.), (2., 4.), 4.).unwrap();
        assert!(PatternParams::for_view(normal, [-123.4, 678.9], [1600., 1018.]).is_ok());
        let ill = PatternLattice::from_mm((4., 4.), (4., 4.00001), 4.).unwrap();
        assert!(PatternParams::for_view(ill, [0., 0.], [1600., 1018.])
            .unwrap_err()
            .contains("precision"));
        assert!(PatternParams::for_view(normal, [0., 0.], [f64::NAN, 1018.]).is_err());
    }
    #[test]
    fn uniform_has_stable_layout_and_authored_phase() {
        assert_eq!(std::mem::size_of::<PatternParams>(), 32);
        let lattice = PatternLattice::from_mm((2., 0.), (1., 3.), 4.).unwrap();
        let a = PatternParams::new(lattice, [-11.25, 19.5]).unwrap();
        let shift = lattice.site([128., -96.]);
        let b = PatternParams::new(lattice, [-11.25 + shift[0], 19.5 + shift[1]]).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, PatternParams::new(lattice, [0., 0.]).unwrap());
        assert!(PatternParams::new(lattice, [f64::INFINITY, 0.]).is_err());
    }
    #[test]
    #[ignore = "requires an actual GPU; explicit offscreen validation only"]
    fn pattern_shader_native_color_crop_and_alpha_id_agree() {
        assert_eq!(std::env::var("FERRITE_BACKGROUND_TEST").as_deref(), Ok("1"));
        pollster::block_on(async {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .expect("actual GPU adapter required");
            println!("OFFSCREEN_PATTERN_ADAPTER {:?}", adapter.get_info());
            let (device, queue) = adapter
                .request_device(
                    &wgpu::DeviceDescriptor {
                        label: Some("Offscreen S100 pattern contract"),
                        required_features: wgpu::Features::empty(),
                        required_limits: wgpu::Limits::default(),
                        memory_hints: Default::default(),
                    },
                    None,
                )
                .await
                .unwrap();
            let root = std::env::temp_dir().join(format!(
                "ferrite-pattern-gpu-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("ASYM.svg"),r#"<svg xmlns="http://www.w3.org/2000/svg" width="4mm" height="4mm" viewBox="-2 -2 4 4"><rect x="0.25" y="-1" width="1.25" height="1" fill="red"/></svg>"#).unwrap();
            let mut cache = crate::SymbolCache::new(&root);
            let layout = create_pattern_layout(&device);
            device.push_error_scope(wgpu::ErrorFilter::Validation);
            let source = format!(
                "{}\n{}",
                PATTERN_SAMPLE_WGSL,
                r#"
@vertex fn vs_main(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32>{let p=array<vec2<f32>,3>(vec2<f32>(-1.,-1.),vec2<f32>(3.,-1.),vec2<f32>(-1.,3.));return vec4<f32>(p[i],0.,1.);}
@fragment fn fs_main(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32>{return s100_pattern_sample(p.xy);}
@fragment fn fs_crop(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32>{return s100_pattern_sample(p.xy+vec2<f32>(7.,9.));}
@fragment fn fs_pick(@builtin(position) p:vec4<f32>)->@location(0) u32{if s100_pattern_sample(p.xy+vec2<f32>(7.,9.)).a<=0.{discard;}return 17u;}
"#
            );
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Shared pattern color/crop/ID"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });
            let make = |entry: &str, format| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(entry),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    multiview: None,
                    cache: None,
                })
            };
            let color = make("fs_main", wgpu::TextureFormat::Rgba8Unorm);
            let crop = make("fs_crop", wgpu::TextureFormat::Rgba8Unorm);
            let pick = make("fs_pick", wgpu::TextureFormat::R32Uint);
            assert!(
                device.pop_error_scope().await.is_none(),
                "Pattern pipeline validation failed"
            );
            let render =
                |size: u32, format, pipeline: &wgpu::RenderPipeline, material: &PatternMaterial| {
                    let texture = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("Pattern contract target"),
                        size: wgpu::Extent3d {
                            width: size,
                            height: size,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    });
                    let view = texture.create_view(&Default::default());
                    let readback = device.create_buffer(&wgpu::BufferDescriptor {
                        label: None,
                        size: u64::from(size) * 256,
                        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    });
                    let mut encoder = device.create_command_encoder(&Default::default());
                    {
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("Offscreen pattern contract"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, &material.bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                    encoder.copy_texture_to_buffer(
                        wgpu::TexelCopyTextureInfo {
                            texture: &texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::TexelCopyBufferInfo {
                            buffer: &readback,
                            layout: wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(256),
                                rows_per_image: Some(size),
                            },
                        },
                        texture.size(),
                    );
                    queue.submit([encoder.finish()]);
                    let (sender, receiver) = std::sync::mpsc::channel();
                    readback
                        .slice(..)
                        .map_async(wgpu::MapMode::Read, move |r| sender.send(r).unwrap());
                    let _ = device.poll(wgpu::Maintain::Wait);
                    receiver
                        .recv_timeout(std::time::Duration::from_secs(30))
                        .unwrap()
                        .unwrap();
                    let data = readback.slice(..).get_mapped_range();
                    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
                    for row in data.chunks(256) {
                        rgba.extend_from_slice(&row[..(size * 4) as usize]);
                    }
                    drop(data);
                    readback.unmap();
                    rgba
                };
            let origin = [11.25, -5.5];
            for (v1, v2) in [
                ((4., 0.), (0., 4.)),
                ((4., 0.), (2., 4.)),
                ((-4., 0.), (2., -4.)),
                ((0., 4.), (4., 0.)),
            ] {
                let lattice = PatternLattice::from_mm(v1, v2, 4.).unwrap();
                let cell = cache
                    .get_symbol_for_lattice("ASYM", &Default::default(), lattice, 4.)
                    .unwrap();
                let texture = PatternTexture::upload(&device, &queue, cell).unwrap();
                assert!(texture.has_coverage);
                let material = texture.material(
                    &device,
                    &layout,
                    PatternParams::new(lattice, origin).unwrap(),
                );
                let full = render(64, wgpu::TextureFormat::Rgba8Unorm, &color, &material);
                let cropped = render(16, wgpu::TextureFormat::Rgba8Unorm, &crop, &material);
                let ids = render(16, wgpu::TextureFormat::R32Uint, &pick, &material);
                for y in 0..16 {
                    for x in 0..16 {
                        let a = ((y + 9) * 64 + x + 7) * 4;
                        let b = (y * 16 + x) * 4;
                        assert_eq!(
                            &full[a..a + 4],
                            &cropped[b..b + 4],
                            "crop phase changed at {x}/{y}, vectors {v1:?}/{v2:?}"
                        );
                        let id = u32::from_le_bytes(ids[b..b + 4].try_into().unwrap());
                        assert_eq!(
                            id,
                            if cropped[b + 3] > 0 { 17 } else { 0 },
                            "ID alpha differs from color"
                        );
                    }
                }
                let mut checked = 0;
                let mut positive_probes = 0;
                for y in 0..64 {
                    for x in 0..64 {
                        let physical = [x as f64 + 0.5 - origin[0], y as f64 + 0.5 - origin[1]];
                        let mut expected = false;
                        let mut edge = false;
                        for m in -8..9 {
                            for n in -8..9 {
                                let site = [
                                    f64::from(v1.0) * 4. * f64::from(n)
                                        + f64::from(v2.0) * 4. * f64::from(m),
                                    -f64::from(v1.1) * 4. * f64::from(n)
                                        - f64::from(v2.1) * 4. * f64::from(m),
                                ];
                                let q = [physical[0] - site[0], physical[1] - site[1]];
                                expected |= q[0] > 1. && q[0] < 6. && q[1] > -4. && q[1] < 0.;
                                if q[0] > 0. && q[0] < 7. && q[1] > -5. && q[1] < 1. {
                                    edge |= (q[0] - 1.).abs() < 1.
                                        || (q[0] - 6.).abs() < 1.
                                        || (q[1] + 4.).abs() < 1.
                                        || q[1].abs() < 1.;
                                }
                            }
                        }
                        if !edge {
                            assert_eq!(
                                full[(y * 64 + x) * 4 + 3] > 127,
                                expected,
                                "GPU warped motif shape {x}/{y}, {v1:?}/{v2:?}"
                            );
                            checked += 1;
                            positive_probes += usize::from(expected);
                        }
                    }
                }
                assert!(checked > 1000);
                assert!(
                    positive_probes > 30,
                    "GPU solid motif interiors were not probed"
                );
                println!("PATTERN_GPU_CONTRACT {v1:?}/{v2:?}: {checked} independent interior probes; 256 crop+ID comparisons exact");
            }
            std::fs::remove_dir_all(root).unwrap();
        });
    }
}
