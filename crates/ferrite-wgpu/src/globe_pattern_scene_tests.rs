//! Explicit ignored hardware contract for actual scene/color/coverage/cropped IDs.
//! No window or surface. This does not exercise GlobePane/AreaCRS or foreground FPS.
use super::*;
use crate::globe_pattern::{PatternParams, PatternTexture};
use ferrite_kernel::{
    coverage_frame::{CoverageFrame, FrameCoverageDecision as D},
    coverage_selection::{CoverageFootprint, Region, SelectedCoverage, Selection},
    geodesy::GeographicPosition,
    scale_policy::CoverageScaleRange,
};

pub(super) fn quad(camera: &GlobeCamera, color: [f32; 4]) -> GlobeMesh {
    GlobeMesh {
        vertices: [[0., 0.], [64., 0.], [64., 64.], [0., 64.]]
            .into_iter()
            .map(|p| GlobeVertex {
                ecef_m: camera.device_plane_point(p).unwrap(),
                color,
            })
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}
pub(super) fn read_scene(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    scene: &mut GlobeSceneRenderer,
) -> Vec<u8> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Actual pattern scene contract"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 64 * 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    scene
        .render(
            device,
            &mut encoder,
            &target.create_view(&Default::default()),
            [64, 64],
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
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(64),
            },
        },
        target.size(),
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    read.slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
    // Synchronous readback is confined to this explicit ignored correctness test.
    let _ = device.poll(wgpu::Maintain::Wait);
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .unwrap()
        .unwrap();
    let result = read.slice(..).get_mapped_range().to_vec();
    read.unmap();
    result
}
pub(super) fn coverage() -> CoverageFrame {
    let ring = |r: [f64; 4]| {
        vec![
            [r[0], r[1]],
            [r[2], r[1]],
            [r[2], r[3]],
            [r[0], r[3]],
            [r[0], r[1]],
        ]
    };
    let viewport = Region::from_rings(&ring([0., 0., 64., 64.]), &[]).unwrap();
    let region =
        Region::from_rings(&ring([0., 0., 40., 64.]), &[ring([14., 18., 25., 31.])]).unwrap();
    let inventory = [CoverageFootprint {
        dataset_id: 7,
        coverage_id: 1,
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
    CoverageFrame::new(&inventory, &selection, &viewport, [64, 64], 64 * 64 * 8).unwrap()
}
/// Independent placement oracle: explicit authored sites, no inverse lattice,
/// cell sampler, PatternCellPlan or material shader reused.
fn motif_at(p: [f64; 2], origin: [f64; 2], v1: (f32, f32), v2: (f32, f32)) -> (bool, bool) {
    let mut inside = false;
    let mut edge = false;
    for m in -8..9 {
        for n in -8..9 {
            let x = p[0]
                - origin[0]
                - 4. * (f64::from(v1.0) * f64::from(n) + f64::from(v2.0) * f64::from(m));
            let y = p[1] - origin[1]
                + 4. * (f64::from(v1.1) * f64::from(n) + f64::from(v2.1) * f64::from(m));
            inside |= x > 1. && x < 6. && y > 1. && y < 5.;
            if x > 0. && x < 7. && y > 0. && y < 6. {
                edge |= (x - 1.).abs() < 1.
                    || (x - 6.).abs() < 1.
                    || (y - 1.).abs() < 1.
                    || (y - 5.).abs() < 1.;
            }
        }
    }
    (inside, edge)
}
#[test]
#[ignore = "actual offscreen GPU; only execute in allocated hardware correctness slot"]
fn actual_pattern_scene_color_coverage_phase_and_cropped_ids() {
    assert_eq!(std::env::var("FERRITE_BACKGROUND_TEST").as_deref(), Ok("1"));
    pollster::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("GPU required");
        println!("ACTUAL_PATTERN_SCENE_ADAPTER {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Actual pattern scene contract"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "ferrite-actual-pattern-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("ASYM.svg"),r#"<svg xmlns="http://www.w3.org/2000/svg" width="4mm" height="4mm" viewBox="-1 -1 4 4"><rect x="0.25" y="0.25" width="1.25" height="1" fill="red"/></svg>"#).unwrap();
        let mut cache = crate::SymbolCache::new(&root);
        let frame = coverage();
        let origins = [[3.25, 6.5], [5.75, 7.5]];
        for samples in [1, 4] {
            for (v1, v2) in [
                ((4., 0.), (0., 4.)),
                ((4., 0.), (2., 4.)),
                ((-4., 0.), (2., -4.)),
                ((0., 4.), (4., 0.)),
            ] {
                let camera = GlobeCamera::orbit(
                    GeographicPosition::new(48., 0.).unwrap(),
                    30000.,
                    0.,
                    0.,
                    [64., 64.],
                    45.,
                    3.,
                    1e9,
                )
                .unwrap();
                let ground = quad(&camera, [0., 1., 0., 1.]);
                let surface = quad(&camera, [1.; 4]);
                let mut scene = GlobeSceneRenderer::new_with_texture_samples(
                    &device,
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    samples,
                );
                let ground_draw = GlobeDraw {
                    layer: GlobeLayer {
                        mesh: &ground,
                        depth_mode: GlobeDepthMode::Occluder,
                    },
                    texture: None,
                    pattern: None,
                    font_color: None,
                };
                scene
                    .prepare_draws(&device, &queue, &camera, &[ground_draw])
                    .unwrap();
                let original_ground = read_scene(&device, &queue, &mut scene);
                let lattice = ferrite_render::PatternLattice::from_mm(v1, v2, 4.).unwrap();
                let cell = cache
                    .get_symbol_for_lattice("ASYM", &Default::default(), lattice, 4.)
                    .unwrap();
                let texture = PatternTexture::upload(&device, &queue, cell).unwrap();
                let materials = origins.map(|origin| {
                    texture.material(
                        &device,
                        scene.pattern_layout(),
                        PatternParams::new(lattice, origin).unwrap(),
                    )
                });
                let draws = [
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &ground,
                            depth_mode: GlobeDepthMode::Occluder,
                        },
                        texture: None,
                        pattern: None,
                        font_color: None,
                    },
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &surface,
                            depth_mode: GlobeDepthMode::SurfaceOverlay,
                        },
                        texture: None,
                        pattern: Some(&materials[0]),
                        font_color: None,
                    },
                    GlobeDraw {
                        layer: GlobeLayer {
                            mesh: &surface,
                            depth_mode: GlobeDepthMode::SurfaceOverlay,
                        },
                        texture: None,
                        pattern: Some(&materials[1]),
                        font_color: None,
                    },
                ];
                scene.set_gpu_projection_enabled(true);
                for policy in [D::Unclipped, D::ClipDataset(7), D::Hidden] {
                    let mut reference = None;
                    for batching in [false, true] {
                        scene.set_coverage_batching_enabled(batching);
                        scene
                            .prepare_draws(&device, &queue, &camera, &draws)
                            .unwrap();
                        assert_eq!(scene.resource_usage()["gpu_projection_used"], false);
                        assert_eq!(scene.ranges.len(), 3, "different phase bindings merged");
                        let epoch = scene.coverage_epoch().unwrap();
                        scene
                            .bind_prepared_coverage(
                                &device,
                                &queue,
                                epoch,
                                &frame,
                                &[D::Unclipped, D::Unclipped, policy],
                                64 * 64 * 2,
                            )
                            .unwrap();
                        let rgba = read_scene(&device, &queue, &mut scene);
                        if let Some(ref pixels) = reference {
                            assert_eq!(pixels, &rgba, "coverage batch changed pattern pixels");
                        } else {
                            reference = Some(rgba.clone());
                        }
                        let mut checked = 0;
                        let mut probes = [None, None, None];
                        for y in 2..62 {
                            for x in 2..62 {
                                let p = [x as f64 + 0.5, y as f64 + 0.5];
                                let (a, ea) = motif_at(p, origins[0], v1, v2);
                                let (b, eb) = motif_at(p, origins[1], v1, v2);
                                if ea || eb {
                                    continue;
                                }
                                let mask = p[0] < 40.
                                    && !(p[0] > 14. && p[0] < 25. && p[1] > 18. && p[1] < 31.);
                                let b = b
                                    && match policy {
                                        D::Unclipped => true,
                                        D::ClipDataset(7) => mask,
                                        D::Hidden => false,
                                        _ => unreachable!(),
                                    };
                                let expected = if a || b {
                                    [255, 0, 0, 255]
                                } else {
                                    [0, 255, 0, 255]
                                };
                                let i = (y * 64 + x) * 4;
                                assert_eq!(&rgba[i..i+4],&expected,"independent scene shape oracle at {p:?}, {v1:?}/{v2:?}, {policy:?}, samples{samples}");
                                let id = if b {
                                    2
                                } else if a {
                                    1
                                } else {
                                    0
                                };
                                probes[id].get_or_insert(p);
                                checked += 1;
                            }
                        }
                        assert!(checked > 1000);
                        assert!(probes[0].is_some(), "transparent gap not probed");
                        assert!(probes[1].is_some(), "first material not probed");
                        if policy != D::Hidden {
                            assert!(probes[2].is_some(), "phase2 material not probed");
                        }
                        for (id, point) in probes.into_iter().enumerate() {
                            if let Some(point) = point {
                                let hits = scene.pick_draws(&device, &queue, point, 0.).unwrap();
                                assert_eq!(
                                    hits.len(),
                                    1,
                                    "actual cropped ID did not select topmost pixel"
                                );
                                assert_eq!(
                                    hits[0].draw_index, id,
                                    "material phase/transparent/coverage cropped ID mismatch"
                                );
                            }
                        }
                        println!("ACTUAL_PATTERN_SCENE {samples} {v1:?}/{v2:?} {policy:?} batching{batching}: {checked} independent pixels + actual cropped ID probes");
                    }
                }
                scene.require_coverage(false);
                scene.set_gpu_projection_enabled(false);
                scene
                    .prepare_draws(
                        &device,
                        &queue,
                        &camera,
                        &[GlobeDraw {
                            layer: GlobeLayer {
                                mesh: &ground,
                                depth_mode: GlobeDepthMode::Occluder,
                            },
                            texture: None,
                            pattern: None,
                            font_color: None,
                        }],
                    )
                    .unwrap();
                assert_eq!(
                    original_ground,
                    read_scene(&device, &queue, &mut scene),
                    "nonpattern route changed after pattern lifecycle"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    });
}
