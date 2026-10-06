use anyhow::{ensure, Context, Result};
use ferrite_kernel::coverage_raster::{rasterize, PixelMask};
use ferrite_kernel::coverage_selection::Region;
use ferrite_wgpu::coverage_clip::{clip_wgsl, create_clip_layout, ClipTransform, CoverageClip};

fn trace(message: &str) {
    if std::env::var_os("FERRITE_CLIP_TRACE").is_some() {
        use std::io::Write;
        eprintln!("coverage-clip: {message}");
        let _ = std::io::stderr().flush();
    }
}

fn read_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    asset: &wgpu::BindGroup,
    empty: &wgpu::BindGroup,
    clip: &CoverageClip,
    samples: u32,
) -> Result<Vec<u8>> {
    let desc = |sample_count, usage| wgpu::TextureDescriptor {
        label: Some("clip-test-frame"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage,
        view_formats: &[],
    };
    trace("read-frame target begin");
    let target = device.create_texture(&desc(
        1,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    ));
    let view = target.create_view(&Default::default());
    let msaa = (samples > 1)
        .then(|| device.create_texture(&desc(samples, wgpu::TextureUsages::RENDER_ATTACHMENT)));
    let msaa_view = msaa.as_ref().map(|t| t.create_view(&Default::default()));
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("clip-test-readback"),
        size: 64 * 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    trace("read-frame textures/buffer ready; encoder begin");
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clip-test"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: msaa_view.as_ref().unwrap_or(&view),
                resolve_target: msaa_view.as_ref().map(|_| &view),
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
        pass.set_bind_group(0, asset, &[]);
        pass.set_bind_group(1, empty, &[]);
        pass.set_bind_group(2, &clip.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(64),
            },
        },
        wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
    );
    trace("read-frame commands ready; submit begin");
    queue.submit([encoder.finish()]);
    trace("read-frame submitted; map begin");
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
    trace("read-frame map scheduled; poll begin");
    device.poll(wgpu::Maintain::Wait);
    trace("read-frame poll complete");
    rx.recv()??;
    let result = buffer.slice(..).get_mapped_range().to_vec();
    buffer.unmap();
    trace("read-frame mapped/unmapped complete");
    Ok(result)
}

fn compare(
    reference: &[u8],
    actual: &[u8],
    mask: Option<&PixelMask>,
    transform: ClipTransform,
) -> Result<()> {
    for y in 0..64 {
        for x in 0..64 {
            let p = transform.prepared_pixel([x as f32 + 0.5, y as f32 + 0.5]);
            let inside = mask.is_none_or(|m| {
                p[0] >= 0.
                    && p[1] >= 0.
                    && m.contains_pixel(p[0].floor() as u32, p[1].floor() as u32)
            });
            let start = (y * 64 + x) as usize * 4;
            let expected = if inside {
                &reference[start..start + 4]
            } else {
                &[0; 4]
            };
            ensure!(
                &actual[start..start + 4] == expected,
                "GPU clip mismatch pixel {x},{y}"
            );
        }
    }
    Ok(())
}

async fn run() -> Result<()> {
    trace("instance begin");
    let backends = match std::env::var("FERRITE_CLIP_BACKEND").as_deref() {
        Ok("vulkan") => wgpu::Backends::VULKAN,
        Ok("dx12") => wgpu::Backends::DX12,
        Ok("metal") => wgpu::Backends::METAL,
        Ok("gl") => wgpu::Backends::GL,
        Ok(other) => anyhow::bail!("Unsupported diagnostic GPU backend: {other}"),
        Err(_) => wgpu::Backends::all(),
    };
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends,
        ..Default::default()
    });
    trace("instance ready");
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .context("No hardware GPU adapter")?;
    let info = adapter.get_info();
    trace(&format!(
        "adapter {} {:?} {:?}",
        info.name, info.backend, info.device_type
    ));
    ensure!(
        matches!(
            info.device_type,
            wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::DiscreteGpu
        ),
        "Hardware GPU required: {:?}",
        info.device_type
    );
    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                label: Some("coverage-mask-check"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: Default::default(),
            },
            None,
        )
        .await?;
    trace("device ready");
    let clip_layout = create_clip_layout(&device);
    let asset_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
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
        ],
    });
    let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[],
    });
    let empty = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &empty_layout,
        entries: &[],
    });
    trace("asset texture begin");
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: 2,
            height: 2,
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
        &[255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 0, 0, 0, 0],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(2),
        },
        wgpu::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
    );
    let tv = texture.create_view(&Default::default());
    let sampler = device.create_sampler(&Default::default());
    let asset = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &asset_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&tv),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    trace("asset ready, mask raster begin");
    let region = Region::from_rings(
        &[[4., 5.], [60., 10.], [56., 60.], [8., 56.], [4., 5.]],
        &[vec![
            [20., 20.],
            [40., 20.],
            [40., 40.],
            [20., 40.],
            [20., 20.],
        ]],
    )?;
    let mask = rasterize(&region, [64, 64], 4096)?;
    let empty_mask = rasterize(&region.difference(&region), [64, 64], 0)?;
    trace("allow mask upload begin");
    let allow = CoverageClip::new(
        &device,
        &queue,
        &clip_layout,
        None,
        ClipTransform::IDENTITY,
        1,
    )?;
    trace("denied mask upload begin");
    let denied = CoverageClip::new(
        &device,
        &queue,
        &clip_layout,
        Some(&empty_mask),
        ClipTransform::IDENTITY,
        1,
    )?;
    ensure!(
        CoverageClip::new(
            &device,
            &queue,
            &clip_layout,
            Some(&mask),
            ClipTransform::IDENTITY,
            mask.pixels().len() - 1
        )
        .is_err(),
        "Budget check missing"
    );
    trace("mut masked mask upload begin");
    let mut masked = CoverageClip::new(
        &device,
        &queue,
        &clip_layout,
        Some(&mask),
        ClipTransform::IDENTITY,
        4096,
    )?;
    let transformed = ClipTransform::new([0.5, 1.], [-2., 0.75])?;
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[&asset_layout, &empty_layout, &clip_layout],
        push_constant_ranges: &[],
    });
    let mut checks = Vec::new();
    for samples in [1, 4] {
        for (kind, label) in ["solid", "line", "symbol-texture", "text-atlas"]
            .iter()
            .enumerate()
        {
            let source = format!(
                r#"
@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var image_sampler:sampler;
struct Out {{@builtin(position) position:vec4<f32>,@location(0) uv:vec2<f32>}}
@vertex fn vs_main(@builtin(vertex_index) index:u32)->Out {{
 // Identical fullscreen triangle without a dynamically indexed local array.
 let uv=vec2<f32>(f32((index<<1u)&2u),f32(index&2u));
 var o:Out;o.position=vec4<f32>(uv*2.-1.,0.,1.);o.uv=uv;return o;
}}
@fragment fn fs_main(v:Out)->@location(0) vec4<f32> {{
 let sampled=textureSample(image,image_sampler,v.uv);
 var color=vec4<f32>(0.2,0.4,0.8,1.);
 if {kind}u==1u {{color=select(vec4<f32>(0.),color,v.position.y>=26. && v.position.y<30.);}}
 if {kind}u==2u {{color=sampled;}}
 if {kind}u==3u {{color=vec4<f32>(sampled.a);}}
 if !s100_clip_visible(v.position.xy) {{discard;}}
 return color;
}}
{}"#,
                clip_wgsl(2)
            );
            trace(&format!("shader {label} samples {samples} begin"));
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            trace("shader ready, pipeline begin");
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                multiview: None,
                cache: None,
            });
            trace("pipeline ready, reference draw begin");
            let reference =
                read_frame(&device, &queue, &pipeline, &asset, &empty, &allow, samples)?;
            trace("reference readback complete; identity mask draw begin");
            masked.set_transform(&queue, ClipTransform::IDENTITY);
            compare(
                &reference,
                &read_frame(&device, &queue, &pipeline, &asset, &empty, &masked, samples)?,
                Some(&mask),
                ClipTransform::IDENTITY,
            )?;
            compare(
                &reference,
                &read_frame(&device, &queue, &pipeline, &asset, &empty, &denied, samples)?,
                Some(&empty_mask),
                ClipTransform::IDENTITY,
            )?;
            trace("identity/empty comparisons complete; transformed draw begin");
            masked.set_transform(&queue, transformed);
            compare(
                &reference,
                &read_frame(&device, &queue, &pipeline, &asset, &empty, &masked, samples)?,
                Some(&mask),
                transformed,
            )?;
            trace("frame comparisons complete");
            checks.push(
                serde_json::json!({"kind":label,"samples":samples,"cases":3,"different_pixels":0}),
            );
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"gpu":info.name,"backend":format!("{:?}",info.backend),
        "checks":checks,"checked_frames":24,"mask_r8_texel_bytes":masked.pixel_bytes(),"application_rendering_verified":false})
        )?
    );
    Ok(())
}
fn main() -> Result<()> {
    pollster::block_on(run())
}
