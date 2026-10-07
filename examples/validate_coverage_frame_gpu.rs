use anyhow::{ensure, Context, Result};
use ferrite_kernel::coverage_frame::CoverageFrame;
use ferrite_kernel::coverage_selection::{CoverageFootprint, Region, SelectedCoverage, Selection};
use ferrite_kernel::scale_policy::CoverageScaleRange;
use ferrite_render::{
    DrawingInstruction, InstructionCoverageClass, PointInstruction, PortrayalOrigin,
    PreparedCoverage, PreparedCoveragePass, WorldPoint,
};
use ferrite_wgpu::coverage_clip::{clip_wgsl, create_clip_layout, ClipTransform};
use ferrite_wgpu::coverage_gpu_frame::{CoverageGpuBinding, CoverageGpuFrame, CoverageGpuPlan};
use std::sync::Arc;
fn trace(_: &str) {}
fn read_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    asset: &wgpu::BindGroup,
    empty: &wgpu::BindGroup,
    clip: Option<&wgpu::BindGroup>,
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
        if let Some(clip) = clip {
            pass.set_bind_group(2, clip, &[]);
        }
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

fn build_prepared(extent: [u32; 2], revision: u64, view_revision: u64) -> Result<PreparedCoverage> {
    let rect = |x: f64, width: f64| {
        Region::from_rings(
            &[
                [x, 2.],
                [x + width, 2.],
                [x + width, 22.],
                [x, 22.],
                [x, 2.],
            ],
            &[],
        )
    };
    let mut instructions = (0..5)
        .map(|_| {
            DrawingInstruction::Point(PointInstruction::new(
                "symbol".into(),
                WorldPoint::new(1., 1.),
            ))
        })
        .collect::<Vec<_>>();
    for (i, instruction) in instructions.iter_mut().enumerate() {
        instruction.set_portrayal_origin(if i == 2 || i == 3 {
            PortrayalOrigin::feature_point(WorldPoint::new(if i == 2 { 31. } else { 8. }, 8.))?
        } else {
            PortrayalOrigin::NonPoint
        });
    }
    let mut passes = Vec::new();
    for pass in 0..2 {
        let x = 4. + 20. * pass as f64;
        let coarse = Region::from_rings(
            &[[x, 2.], [x + 26., 2.], [x + 26., 22.], [x, 22.], [x, 2.]],
            &[vec![
                [x + 17., 8.],
                [x + 21., 8.],
                [x + 21., 12.],
                [x + 17., 12.],
                [x + 17., 8.],
            ]],
        )?;
        let inventory = vec![(180000, coarse), (45000, rect(x, 12.)?)]
            .into_iter()
            .enumerate()
            .map(|(id, (min, region))| CoverageFootprint {
                dataset_id: id,
                coverage_id: id as i64,
                region,
                scales: CoverageScaleRange {
                    minimum_denominator: Some(min),
                    optimum_denominator: min / 2,
                    maximum_denominator: min / 4,
                },
            })
            .collect::<Vec<_>>();
        let selection = Selection {
            display_band: 10,
            coverages: (0..2)
                .map(|i| SelectedCoverage {
                    inventory_index: i,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                })
                .collect(),
            uncovered: Region::from_polygons(vec![])?,
        };
        let viewport =
            Region::from_rings(&[[0., 0.], [64., 0.], [64., 64.], [0., 64.], [0., 0.]], &[])?;
        let frame = Arc::new(CoverageFrame::new(
            &inventory, &selection, &viewport, extent, 20000,
        )?);
        passes.push(PreparedCoveragePass::prepare(
            &instructions,
            frame,
            |instruction| {
                let i = instructions
                    .iter()
                    .position(|item| std::ptr::eq(item, instruction))
                    .unwrap();
                Ok(if i == 4 {
                    InstructionCoverageClass::Exempt
                } else {
                    InstructionCoverageClass::Dataset(if i == 1 { 1 } else { 0 })
                })
            },
            |_, p| {
                Ok(Some(match p {
                    ferrite_render::PointOriginGeometry::FeaturePoint(p) => {
                        [p.x + 20. * pass as f64, p.y]
                    }
                    _ => unreachable!(),
                }))
            },
        )?);
    }
    Ok(PreparedCoverage::new(
        revision,
        view_revision,
        instructions.len(),
        passes,
    )?)
}
async fn run() -> Result<()> {
    let instance = wgpu::Instance::new(&Default::default());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })
        .await
        .context("No GPU")?;
    let info = adapter.get_info();
    ensure!(
        matches!(
            info.device_type,
            wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::DiscreteGpu
        ),
        "Hardware GPU required"
    );
    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                label: Some("coverage-frame-check"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: Default::default(),
            },
            None,
        )
        .await?;
    let clip_layout = create_clip_layout(&device);
    let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[],
    });
    let empty = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &empty_layout,
        entries: &[],
    });
    let mut checks = Vec::new();
    for samples in [1, 4] {
        let pipeline = |masked: bool| {
            let code = format!(
                r#"
struct Out {{@builtin(position) position:vec4<f32>}}
@vertex fn vs_main(@builtin(vertex_index) index:u32)->Out {{
 let uv=vec2<f32>(f32((index<<1u)&2u),f32(index&2u));
 var o:Out;o.position=vec4<f32>(uv*2.-vec2<f32>(1.),0.,1.);return o;
}}
@fragment fn fs_main(v:Out)->@location(0) vec4<f32> {{{} return vec4<f32>(1.,0.,0.,1.);}}
{}"#,
                if masked {
                    "if !s100_clip_visible(v.position.xy) {discard;}"
                } else {
                    ""
                },
                if masked { clip_wgsl(2) } else { String::new() }
            );
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: None,
                source: wgpu::ShaderSource::Wgsl(code.into()),
            });
            let clipped_layouts = [&empty_layout, &empty_layout, &clip_layout];
            let plain_layouts = [&empty_layout, &empty_layout];
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: if masked {
                    &clipped_layouts
                } else {
                    &plain_layouts
                },
                push_constant_ranges: &[],
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
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
            })
        };
        let plain = pipeline(false);
        let clipped = pipeline(true);
        for extent in [[64, 64], [0, 0]] {
            let prepared = build_prepared(extent, 1, 2)?;
            let plan = CoverageGpuPlan::new(
                &prepared,
                1,
                2,
                5,
                2,
                device.limits().max_texture_dimension_2d,
                20000,
            )?;
            let bytes = plan.pixel_bytes();
            let masks = plan.mask_count();
            let mut frame = CoverageGpuFrame::upload(&device, &queue, &clip_layout, plan)?;
            let stale_view = build_prepared(extent, 1, 3)?;
            let stale_geometry = build_prepared(extent, 2, 2)?;
            ensure!(
                frame.resolve_instruction(&stale_view, 0, 0).is_err(),
                "Stale view accepted"
            );
            ensure!(
                frame.resolve_instruction(&stale_geometry, 0, 0).is_err(),
                "Stale geometry accepted"
            );
            ensure!(
                frame.resolve_instruction(&prepared, 0, 5).is_err(),
                "Out of bounds instruction accepted"
            );
            ensure!(
                frame.pixel_bytes() == bytes,
                "Uploaded byte accounting mismatch"
            );
            ensure!(
                frame
                    .resolve(
                        2,
                        ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped
                    )
                    .is_err(),
                "Out of bounds pass accepted"
            );
            ensure!(
                frame
                    .resolve(
                        0,
                        ferrite_kernel::coverage_frame::FrameCoverageDecision::ClipDataset(99)
                    )
                    .is_err(),
                "Missing dataset accepted"
            );
            for transform in [
                ClipTransform::IDENTITY,
                ClipTransform::new([0.5, 1.], [-2., 0.75])?,
            ] {
                frame.set_transform(&queue, transform);
                for pass in 0..2 {
                    for i in 0..5 {
                        let decision = prepared.pass(pass)?.decision(i)?;
                        let actual = match frame.resolve_instruction(&prepared, pass, i)? {
                            CoverageGpuBinding::Masked(binding) => read_frame(
                                &device,
                                &queue,
                                &clipped,
                                &empty,
                                &empty,
                                Some(binding),
                                samples,
                            )?,
                            CoverageGpuBinding::Unclipped => {
                                read_frame(&device, &queue, &plain, &empty, &empty, None, samples)?
                            }
                            CoverageGpuBinding::Hidden => vec![0; 64 * 256],
                        };
                        for y in 0..64 {
                            for x in 0..64 {
                                let p = transform.prepared_pixel([x as f32 + 0.5, y as f32 + 0.5]);
                                let visible = prepared
                                    .pass(pass)?
                                    .frame()
                                    .accepts_fragment(decision, [p[0] as f64, p[1] as f64])?;
                                let offset = (y * 64 + x) * 4;
                                ensure!(&actual[offset..offset+4]==if visible {&[255,0,0,255]} else {&[0,0,0,0]},"Mismatch {samples} samples, extent {extent:?}, pass {pass}, instruction {i}, pixel {x},{y}");
                            }
                        }
                    }
                }
            }
            checks.push(serde_json::json!({"samples":samples,"extent":extent,"masks":masks,"logical_r8_bytes":bytes,"checked_decisions":20,"different_pixels":0}));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"gpu":info.name,"backend":format!("{:?}",info.backend),"checks":checks,"checked_decisions":80,"application_rendering_verified":false})
        )?
    );
    Ok(())
}
fn main() -> Result<()> {
    pollster::block_on(run())
}
