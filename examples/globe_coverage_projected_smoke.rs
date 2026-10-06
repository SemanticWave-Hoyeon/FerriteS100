//! Synthetic native ID picking: actual prepared CPU/GPU clips, alpha and depth.
use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera};
use ferrite_wgpu::{
    globe_scene::{
        GlobeDepthMode, GlobeDraw, GlobeLayer, GlobeMesh, GlobeSceneRenderer, GlobeVertex,
    },
    GpuState,
};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
fn quad(c: &GlobeCamera, rect: [f64; 4], texture: bool, color: [f32; 4]) -> GlobeMesh {
    let [x, y, w, h] = rect;
    let positions = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
    let uv = [
        [0., 0., 0., 1.],
        [1., 0., 0., 1.],
        [1., 1., 0., 1.],
        [0., 1., 0., 1.],
    ];
    GlobeMesh {
        vertices: positions
            .into_iter()
            .enumerate()
            .map(|(i, p)| GlobeVertex {
                ecef_m: c.device_plane_point(p).unwrap(),
                color: if texture { uv[i] } else { color },
            })
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

fn read_color(gpu: &GpuState, scene: &mut GlobeSceneRenderer, size: [u32; 2]) -> Vec<u8> {
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("coverage color oracle"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let stride = (size[0] * 4 + 255) / 256 * 256;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (stride * size[1]) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    scene
        .render(
            &gpu.device,
            &mut encoder,
            &target.create_view(&Default::default()),
            size,
            wgpu::Color::TRANSPARENT,
        )
        .unwrap();
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
                bytes_per_row: Some(stride),
                rows_per_image: Some(size[1]),
            },
        },
        wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |v| {
        tx.send(v).unwrap();
    });
    gpu.device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    let data = buffer.slice(..).get_mapped_range();
    let mut result = Vec::new();
    for row in data.chunks_exact(stride as usize) {
        result.extend_from_slice(&row[..size[0] as usize * 4]);
    }
    drop(data);
    buffer.unmap();
    result
}
fn coverage_tests(
    gpu: &GpuState,
    layout: &wgpu::BindGroupLayout,
    binding: &wgpu::BindGroup,
    size: [u32; 2],
    out: &std::path::Path,
) -> Vec<serde_json::Value> {
    use ferrite_kernel::{
        coverage_frame::{CoverageFrame, FrameCoverageDecision as D},
        coverage_selection::{CoverageFootprint, Region, SelectedCoverage, Selection},
        scale_policy::CoverageScaleRange,
    };
    let ring = |r: [f64; 4]| {
        vec![
            [r[0], r[1]],
            [r[2], r[1]],
            [r[2], r[3]],
            [r[0], r[3]],
            [r[0], r[1]],
        ]
    };
    let viewport =
        Region::from_rings(&ring([0., 0., size[0] as f64, size[1] as f64]), &[]).unwrap();
    let geographic_ring = |radius: f64| {
        vec![
            ferrite_render::WorldPoint::new(179.99 - radius, 48. - radius),
            ferrite_render::WorldPoint::new(179.99 + radius, 48. - radius),
            ferrite_render::WorldPoint::new(179.99 + radius, 48. + radius),
            ferrite_render::WorldPoint::new(179.99 - radius, 48. + radius),
            ferrite_render::WorldPoint::new(179.99 - radius, 48. - radius),
        ]
    };
    let mut geographic_area = ferrite_render::AreaInstruction::new(geographic_ring(0.05));
    geographic_area.interiors.push(geographic_ring(0.01));
    let decisions = [
        D::ClipDataset(7),
        D::ClipDataset(7),
        D::Hidden,
        D::ClipDataset(7),
        D::Unclipped,
        D::Hidden,
    ];
    let mut checks = Vec::new();
    for samples in [1, 4] {
        for projection in [false, true] {
            for (heading, tilt) in [(0., 0.), (90., 70.)] {
                let camera = GlobeCamera::orbit(
                    GeographicPosition::new(48., 179.99).unwrap(),
                    30000.,
                    heading,
                    tilt,
                    size.map(|v| v as f64),
                    45.,
                    3.,
                    1e9,
                )
                .unwrap();
                let (region,projected_stats)=ferrite_wgpu::globe_coverage_projection::project_coverage_area(&geographic_area,&camera,ferrite_wgpu::globe_portrayal::DrapingLimits::default(),ferrite_kernel::globe_coverage_projection::CoverageProjectionLimits::default()).unwrap();
                assert!(region.polygons().iter().any(|p| !p.interiors().is_empty()));
                let inventory = [CoverageFootprint {
                    dataset_id: 7,
                    coverage_id: 31,
                    region,
                    scales: CoverageScaleRange {
                        minimum_denominator: Some(100000),
                        optimum_denominator: 50000,
                        maximum_denominator: 25000,
                    },
                }];
                let selection = Selection {
                    display_band: 10,
                    coverages: vec![SelectedCoverage {
                        inventory_index: 0,
                        selection_band: 10,
                        selected_to_fill_gap: false,
                    }],
                    uncovered: Region::from_polygons(vec![]).unwrap(),
                };
                let frame =
                    CoverageFrame::new(&inventory, &selection, &viewport, size, 2000000).unwrap();
                let mask = frame.mask(7).unwrap();
                let expected_bytes = mask.pixels().len();
                let mut scene = GlobeSceneRenderer::new_with_texture_samples(
                    &gpu.device,
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(layout),
                    samples,
                );
                scene.set_gpu_projection_enabled(projection);
                let meshes = [
                    quad(&camera, [40., 40., 560., 400.], false, [0., 1., 0., 1.]),
                    quad(&camera, [100., 100., 440., 280.], false, [1., 0., 0., 1.]),
                    quad(&camera, [100., 100., 440., 280.], false, [1., 0., 0., 0.]),
                    quad(&camera, [100., 100., 440., 280.], true, [1.; 4]),
                    quad(&camera, [600., 70., 20., 20.], false, [0., 0., 1., 1.]),
                    quad(&camera, [600., 100., 20., 20.], false, [1., 1., 0., 1.]),
                ];
                let draws: Vec<_> = meshes
                    .iter()
                    .enumerate()
                    .map(|(i, m)| GlobeDraw {
                        layer: GlobeLayer {
                            mesh: m,
                            depth_mode: if i == 0 {
                                GlobeDepthMode::Occluder
                            } else {
                                GlobeDepthMode::SurfaceOverlay
                            },
                        },
                        texture: if i == 3 { Some(binding) } else { None },
                        pattern: None,
                        font_color: None,
                    })
                    .collect();
                scene
                    .prepare_draws(&gpu.device, &gpu.queue, &camera, &draws[..5])
                    .unwrap();
                let reference = read_color(gpu, &mut scene, size);
                scene
                    .prepare_draws(&gpu.device, &gpu.queue, &camera, &draws)
                    .unwrap();
                let epoch = scene.coverage_epoch().unwrap();
                let bytes = scene
                    .bind_prepared_coverage(
                        &gpu.device,
                        &gpu.queue,
                        epoch,
                        &frame,
                        &decisions,
                        expected_bytes,
                    )
                    .unwrap();
                assert_eq!(bytes, expected_bytes);
                assert_eq!(scene.coverage_pixel_bytes(), expected_bytes);
                let actual = read_color(gpu, &mut scene, size);
                let mut mismatches = 0;
                for y in 0..size[1] {
                    for x in 0..size[0] {
                        let k = ((y * size[0] + x) * 4) as usize;
                        let p = &reference[k..k + 4];
                        let expected = if mask.contains_pixel(x, y) || p[2] > 0 {
                            p
                        } else {
                            &[0, 0, 0, 0]
                        };
                        if &actual[k..k + 4] != expected {
                            mismatches += 1;
                        }
                    }
                }
                assert_eq!(
                    mismatches, 0,
                    "color samples={samples} gpu={projection} heading={heading}"
                );
                let mut probes = Vec::new();
                for y in (8..size[1]).step_by(23) {
                    for x in (8..size[0]).step_by(23) {
                        let point = [x as f64 + 0.5, y as f64 + 0.5];
                        let expected = if (600..620).contains(&x) && (70..90).contains(&y) {
                            Some(4)
                        } else if !mask.contains_pixel(x, y) {
                            None
                        } else if (100..540).contains(&x) && (100..380).contains(&y) {
                            Some(if x < 320 && y < 240 { 1 } else { 3 })
                        } else if (40..600).contains(&x) && (40..440).contains(&y) {
                            Some(0)
                        } else {
                            None
                        };
                        probes.push((point, expected));
                    }
                }
                probes.extend([
                    ([320.5, 240.5], None),
                    ([610.5, 80.5], Some(4)),
                    ([610.5, 110.5], None),
                ]);
                let probe_count = probes.len();
                for (point, expected) in probes {
                    let hits = scene
                        .pick_draws(&gpu.device, &gpu.queue, point, 0.)
                        .unwrap();
                    assert_eq!(
                        hits.first().map(|h| h.draw_index),
                        expected,
                        "pick {point:?} samples={samples} gpu={projection}"
                    );
                    if let Some(h) = hits.first() {
                        assert_eq!(h.pixel, point);
                    }
                }
                let hits = scene
                    .pick_draws(&gpu.device, &gpu.queue, [320., 240.], 64.)
                    .unwrap();
                assert!(hits.iter().any(|h| h.draw_index == 1));
                assert!(hits.iter().any(|h| h.draw_index == 3));
                for h in &hits {
                    assert!(
                        mask.contains_pixel(h.pixel[0].floor() as u32, h.pixel[1].floor() as u32)
                    );
                }
                // Every rejected rebinding clears the old binding; picking must fail closed.
                assert!(scene
                    .bind_prepared_coverage(
                        &gpu.device,
                        &gpu.queue,
                        epoch,
                        &frame,
                        &decisions,
                        expected_bytes - 1
                    )
                    .is_err());
                assert!(scene
                    .pick_draws(&gpu.device, &gpu.queue, [120.5, 120.5], 0.)
                    .is_err());
                let mut unknown = decisions;
                unknown[0] = D::ClipDataset(99);
                assert!(scene
                    .bind_prepared_coverage(
                        &gpu.device,
                        &gpu.queue,
                        epoch,
                        &frame,
                        &unknown,
                        usize::MAX
                    )
                    .is_err());
                assert!(scene
                    .pick_draws(&gpu.device, &gpu.queue, [120.5, 120.5], 0.)
                    .is_err());
                assert!(scene
                    .bind_prepared_coverage(
                        &gpu.device,
                        &gpu.queue,
                        epoch,
                        &frame,
                        &decisions[..5],
                        usize::MAX
                    )
                    .is_err());
                scene
                    .bind_prepared_coverage(
                        &gpu.device,
                        &gpu.queue,
                        epoch,
                        &frame,
                        &decisions,
                        expected_bytes,
                    )
                    .unwrap();
                scene
                    .prepare_draws(&gpu.device, &gpu.queue, &camera, &draws)
                    .unwrap();
                assert_ne!(scene.coverage_epoch(), Some(epoch));
                assert!(scene
                    .bind_prepared_coverage(
                        &gpu.device,
                        &gpu.queue,
                        epoch,
                        &frame,
                        &decisions,
                        expected_bytes
                    )
                    .is_err());
                assert!(scene
                    .pick_draws(&gpu.device, &gpu.queue, [120.5, 120.5], 0.)
                    .is_err());
                let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d {
                        width: size[0],
                        height: size[1],
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                });
                let mut encoder = gpu.device.create_command_encoder(&Default::default());
                assert!(scene
                    .render(
                        &gpu.device,
                        &mut encoder,
                        &target.create_view(&Default::default()),
                        size,
                        wgpu::Color::TRANSPARENT
                    )
                    .is_err());
                // One identical scene, six original IDs and three exact color
                // groups. Different per-vertex font tints must also survive.
                let batch_draws: Vec<_> = [0usize,0,1,1,3,3].iter().enumerate().map(|(j, &i)| GlobeDraw {
                    layer: GlobeLayer { mesh: &meshes[i], depth_mode: if i == 0 { GlobeDepthMode::Occluder } else { GlobeDepthMode::SurfaceOverlay } },
                    texture: if i == 3 { Some(binding) } else { None },
                    pattern: None,
                    font_color: if j == 5 { Some([30,90,180,128]) } else { None },
                }).collect();
                let batch_decisions = [D::ClipDataset(7);6];
                scene.set_coverage_batching_enabled(false);
                scene.prepare_draws(&gpu.device,&gpu.queue,&camera,&batch_draws).unwrap();
                let batch_epoch = scene.coverage_epoch().unwrap();
                scene.bind_prepared_coverage(&gpu.device,&gpu.queue,batch_epoch,&frame,&batch_decisions,expected_bytes).unwrap();
                let batch_off = read_color(gpu,&mut scene,size);
                let off_calls = scene.resource_usage()["draw_calls"].as_u64().unwrap();
                assert_eq!(scene.resource_usage()["coverage_batch_capacity_bytes"],0);
                let points = [[160.,120.],[320.,240.],[420.,300.],[608.,80.]];
                let mut off_hits = Vec::new();
                for point in points {
                    let hits = scene.pick_draws(&gpu.device,&gpu.queue,point,3.).unwrap();
                    off_hits.push(hits.iter().map(|h|(h.draw_index,h.distance_px.to_bits(),h.pixel[0].to_bits(),h.pixel[1].to_bits())).collect::<Vec<_>>());
                }
                scene.set_coverage_batching_enabled(true);
                // Toggling may never leave the previous coverage binding usable.
                assert!(scene.render(&gpu.device,&mut gpu.device.create_command_encoder(&Default::default()),
                    &target.create_view(&Default::default()),size,wgpu::Color::TRANSPARENT).is_err());
                scene.bind_prepared_coverage(&gpu.device,&gpu.queue,batch_epoch,&frame,&batch_decisions,expected_bytes).unwrap();
                let batch_on = read_color(gpu,&mut scene,size);
                assert_eq!(batch_off,batch_on,"Batching changed a color pixel");
                let on_calls = scene.resource_usage()["draw_calls"].as_u64().unwrap();
                assert_eq!((off_calls,on_calls),(6,3));
                for (index,point) in points.into_iter().enumerate() {
                    let hits = scene.pick_draws(&gpu.device,&gpu.queue,point,3.).unwrap();
                    assert_eq!(off_hits[index],hits.iter().map(|h|(h.draw_index,h.distance_px.to_bits(),h.pixel[0].to_bits(),h.pixel[1].to_bits())).collect::<Vec<_>>());
                }
                let stats = scene.resource_usage();
                assert_eq!(stats["gpu_projection_used"], projection);
                checks.push(serde_json::json!({"samples":samples,"gpu_projection":projection,"heading":heading,"tilt":tilt,"color_pixel_mismatches":mismatches,"color_pixels_compared":size[0]*size[1],"picking_probes":probe_count,"radius_mask_origin_verified":true,"stale_epoch_fail_closed":true,"budget_fail_closed":true,"unknown_dataset_rejected":true,"draw_count_rejected":true,"geographic_coverage_projected":true,"geographic_hole_preserved":true,"projected_triangles":projected_stats.final_triangles,"unique_mask_bytes":bytes,"batch_off_draw_calls":off_calls,"batch_on_draw_calls":on_calls,"batch_color_pixels_exact":true,"batch_picking_queries_exact":4,"resource_usage":stats}));
                if samples == 1 && !projection && heading == 0. {
                    image::save_buffer(
                        out.join("masked.png"),
                        &actual,
                        size[0],
                        size[1],
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                    image::save_buffer(
                        out.join("reference.png"),
                        &reference,
                        size[0],
                        size[1],
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
            }
        }
    }
    checks
}

struct App {
    out: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes()
                    .with_title("Globe ID picking")
                    .with_inner_size(PhysicalSize::new(640, 480)),
            )
            .unwrap(),
        );
        let gpu = pollster::block_on(GpuState::new(w)).unwrap();
        let size = [gpu.config.width, gpu.config.height];
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
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
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 0, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255],
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
        let view = texture.create_view(&Default::default());
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let binding = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let mut checks = Vec::new();
        for gpu_projection in [false, true] {
            for (heading, tilt) in [(0., 0.), (90., 70.)] {
                let camera = GlobeCamera::orbit(
                    GeographicPosition::new(48., 179.9).unwrap(),
                    30000.,
                    heading,
                    tilt,
                    size.map(|x| x as f64),
                    45.,
                    3.,
                    1e9,
                )
                .unwrap();
                let mut scene = GlobeSceneRenderer::new_with_textures(
                    &gpu.device,
                    gpu.config.format,
                    Some(&layout),
                );
                scene.set_gpu_projection_enabled(gpu_projection);
                let base = quad(&camera, [80., 80., 240., 240.], false, [0., 1., 0., 1.]);
                let opaque = quad(&camera, [100., 100., 160., 160.], false, [1., 0., 0., 1.]);
                let transparent = quad(&camera, [100., 100., 160., 160.], false, [1., 0., 0., 0.]);
                let glyph = quad(&camera, [100., 100., 160., 160.], true, [1.; 4]);
                let draws = [
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &base,
                            depth_mode: GlobeDepthMode::Occluder,
                        },
                        texture: None,
                        pattern: None,
                        font_color: None,
                    },
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &opaque,
                            depth_mode: GlobeDepthMode::SurfaceOverlay,
                        },
                        texture: None,
                        pattern: None,
                        font_color: None,
                    },
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &transparent,
                            depth_mode: GlobeDepthMode::SurfaceOverlay,
                        },
                        texture: None,
                        pattern: None,
                        font_color: None,
                    },
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &glyph,
                            depth_mode: GlobeDepthMode::SurfaceOverlay,
                        },
                        texture: Some(&binding),
                        pattern: None,
                        font_color: None,
                    },
                ];
                scene
                    .prepare_draws(&gpu.device, &gpu.queue, &camera, &draws)
                    .unwrap();
                for (point, expected) in
                    [([120.5, 120.5], 1), ([230.5, 230.5], 3), ([90.5, 90.5], 0)]
                {
                    let hits = scene
                        .pick_draws(&gpu.device, &gpu.queue, point, 0.)
                        .unwrap();
                    assert_eq!(hits.len(), 1);
                    assert_eq!(hits[0].draw_index, expected);
                    assert_eq!(hits[0].pixel, point);
                    checks.push(serde_json::json!({"gpu_projection_requested":gpu_projection,"heading":heading,"tilt":tilt,"pixel":point,"draw_index":expected,"alpha_and_draw_order":true}));
                }
                assert!(scene
                    .pick_draws(&gpu.device, &gpu.queue, [20.5, 20.5], 0.)
                    .unwrap()
                    .is_empty());
                let hits = scene
                    .pick_draws(&gpu.device, &gpu.queue, [180., 180.], 40.)
                    .unwrap();
                assert!(hits.iter().any(|h| h.draw_index == 1));
                assert!(hits.iter().any(|h| h.draw_index == 3));
                assert!(!hits.iter().any(|h| h.draw_index == 2));
                // A mesh missing its central triangles is not selectable through its hole.
                let mut ring = quad(&camera, [100., 100., 160., 40.], false, [1.; 4]);
                for rect in [
                    [100., 220., 160., 40.],
                    [100., 140., 40., 80.],
                    [220., 140., 40., 80.],
                ] {
                    ring.append(&quad(&camera, rect, false, [1.; 4])).unwrap();
                }
                scene
                    .prepare_draws(
                        &gpu.device,
                        &gpu.queue,
                        &camera,
                        &[GlobeDraw {
                            layer: GlobeLayer {
                                mesh: &ring,
                                depth_mode: GlobeDepthMode::SurfaceOverlay,
                            },
                            texture: None,
                            pattern: None,
                            font_color: None,
                        }],
                    )
                    .unwrap();
                assert!(scene
                    .pick_draws(&gpu.device, &gpu.queue, [180.5, 180.5], 0.)
                    .unwrap()
                    .is_empty());
                assert_eq!(
                    scene
                        .pick_draws(&gpu.device, &gpu.queue, [120.5, 180.5], 0.)
                        .unwrap()[0]
                        .draw_index,
                    0
                );
                // An occluder at the camera-facing plane hides real surface geometry.
                let center = GeographicPosition::new(48., 179.9).unwrap();
                let far = GlobeMesh {
                    vertices: [
                        center,
                        GeographicPosition::new(48.001, 179.9).unwrap(),
                        GeographicPosition::new(48., 179.901).unwrap(),
                    ]
                    .into_iter()
                    .map(|p| GlobeVertex {
                        ecef_m: p.to_ecef(0.).unwrap(),
                        color: [1.; 4],
                    })
                    .collect(),
                    indices: vec![0, 1, 2],
                };
                let full = quad(
                    &camera,
                    [0., 0., size[0] as f64, size[1] as f64],
                    false,
                    [1.; 4],
                );
                scene
                    .prepare_draws(
                        &gpu.device,
                        &gpu.queue,
                        &camera,
                        &[
                            GlobeDraw {
                                layer: GlobeLayer {
                                    mesh: &full,
                                    depth_mode: GlobeDepthMode::Occluder,
                                },
                                texture: None,
                                pattern: None,
                                font_color: None,
                            },
                            GlobeDraw {
                                layer: GlobeLayer {
                                    mesh: &far,
                                    depth_mode: GlobeDepthMode::SurfaceOverlay,
                                },
                                texture: None,
                                pattern: None,
                                font_color: None,
                            },
                        ],
                    )
                    .unwrap();
                let hit = scene
                    .pick_draws(
                        &gpu.device,
                        &gpu.queue,
                        [size[0] as f64 / 2. + 0.5, size[1] as f64 / 2. + 0.5],
                        0.,
                    )
                    .unwrap();
                assert_eq!(hit[0].draw_index, 0);
            }
        }
        let coverage_checks = coverage_tests(&gpu, &layout, &binding, size, &self.out);
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"coverage_checks":coverage_checks,"checks":checks,"hole_cases":4,"occlusion_cases":4,"empty_cases":4,"radius_cases":4,"single_sample_pixel_centers":true,"product_adapter_verified":false,"ui_connected":false})).unwrap()).unwrap();
        e.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut app = App {
        out: std::env::args().nth(1).map(PathBuf::from).expect("output"),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
