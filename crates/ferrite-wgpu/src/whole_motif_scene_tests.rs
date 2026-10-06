//! Actual Scene gate: independently accepted/removed translucent motifs, shared
//! colour/coverage/cropped IDs. Exact planar fixture, no Window/Surface; not Pane
//! AreaCRS/geodesic domain or curve/AA edge certification.
use super::pattern_scene_hardware_tests::{coverage, quad, read_scene};
use super::*;
use crate::{globe_pattern::AreaMaterialKind, whole_motif_gpu::NaturalMotifTexture};
use ferrite_kernel::{
    coverage_frame::FrameCoverageDecision as D,
    geodesy::GeographicPosition,
    whole_symbol::{
        select_whole_symbols, ShapeLimits, SymbolSite, WholeSymbolArea, WholeSymbolLimits,
    },
};

fn inside_rect(p: [f64; 2], r: [f64; 4]) -> bool {
    p[0] > r[0] && p[0] < r[2] && p[1] > r[1] && p[1] < r[3]
}
fn edge_rect(p: [f64; 2], r: [f64; 4]) -> bool {
    p[0] > r[0] - 1.5
        && p[0] < r[2] + 1.5
        && p[1] > r[1] - 1.5
        && p[1] < r[3] + 1.5
        && [
            (p[0] - r[0]).abs(),
            (p[0] - r[2]).abs(),
            (p[1] - r[1]).abs(),
            (p[1] - r[3]).abs(),
        ]
        .into_iter()
        .any(|d| d < 1.5)
}
/// Independent authored rectangle/hole oracle, no support triangles or shader UV.
fn paint(p: [f64; 2], site: [f64; 2], colour: [f64; 3]) -> (Option<([f64; 3], f64)>, bool) {
    let q = [p[0] - site[0], p[1] - site[1]];
    let a = [0., 0., 12., 12.];
    let h = [4., 4., 8., 8.];
    let b = [16., 0., 20., 4.];
    let pixel = if inside_rect(q, a) && !inside_rect(q, h) {
        Some((colour, 0.5))
    } else if inside_rect(q, b) {
        Some(([1., 1., 0.], 0.75))
    } else {
        None
    };
    (pixel, edge_rect(q, a) || edge_rect(q, h) || edge_rect(q, b))
}
#[test]
#[ignore = "actual offscreen GPU; explicit exclusive hardware slot only"]
fn actual_whole_motif_scene_removal_alpha_coverage_and_cropped_ids() {
    assert_eq!(std::env::var("FERRITE_BACKGROUND_TEST").as_deref(), Ok("1"));
    pollster::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("actual GPU required");
        println!("ACTUAL_WHOLE_MOTIF_ADAPTER {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Actual whole motif scene contract"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let root = std::env::temp_dir().join(format!(
            "ferrite-whole-scene-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("M.svg"),"<svg xmlns='http://www.w3.org/2000/svg' width='6.5mm' height='4mm' viewBox='-.5 -.5 6.5 4'><path class='fCHBLK' fill-rule='evenodd' fill-opacity='.5' d='M0 0H3V3H0Z M1 1H2V2H1Z'/><rect x='4' y='0' width='1' height='1' fill='yellow' fill-opacity='.75'/></svg>").unwrap();
        let mut cache = crate::SymbolCache::new(&root);
        let profile = |rgb: [u8; 3]| {
            let mut p = ferrite_portrayal_catalog::ColorProfile::default();
            p.colors.insert(
                "CHBLK".into(),
                ferrite_portrayal_catalog::ColorDefinition {
                    token: "CHBLK".into(),
                    srgb: Some(ferrite_portrayal_catalog::SrgbColor::new(
                        rgb[0], rgb[1], rgb[2],
                    )),
                    cie: None,
                },
            );
            p
        };
        let resources = [[255, 0, 0], [0, 0, 255]].map(|rgb| {
            cache
                .get_whole_motif(
                    "M",
                    &profile(rgb),
                    4.,
                    Default::default(),
                    Default::default(),
                )
                .unwrap()
                .unwrap()
        });
        let textures = resources
            .each_ref()
            .map(|r| NaturalMotifTexture::upload(&device, &queue, r).unwrap());
        assert_ne!(textures[0].resource_key, textures[1].resource_key);
        assert!(textures.iter().all(|t| t.has_coverage));
        let ring = |r: [f64; 4]| {
            vec![
                [r[0], r[1]],
                [r[2], r[1]],
                [r[2], r[3]],
                [r[0], r[3]],
                [r[0], r[1]],
            ]
        };
        let area = WholeSymbolArea::from_rings(
            &ring([0., 0., 60., 60.]),
            &[ring([26., 24., 29., 27.])],
            ShapeLimits {
                max_coordinates: 32,
                max_components: 1,
                max_rings: 2,
            },
        )
        .unwrap();
        let frame = coverage();
        let mut cases = 0;
        let mut checked_total = 0;
        for samples in [1, 4] {
            for v2 in [(-4., -8.), (8., 4.), (0., -8.), (4., -8.)] {
                let lattice = ferrite_render::PatternLattice::from_mm((4., -4.), v2, 1.).unwrap();
                let sites = [[-1, 1], [0, 0], [7, 0], [2, 0], [10, 7]].map(|index| {
                    let offset = lattice.site(index.map(|v| v as f64));
                    SymbolSite {
                        source_ordinal: 37,
                        lattice_index: index,
                        origin: [10. + offset[0], 10. + offset[1]],
                    }
                });
                let decisions = select_whole_symbols(
                    &area,
                    &resources[0].support.support,
                    &sites,
                    WholeSymbolLimits {
                        max_sites: 5,
                        max_cross_coordinate_pairs: 1_000_000,
                        max_support_coordinate_pairs: 1_000_000,
                        max_decision_bytes: 4096,
                        max_translation_error: 1e-10,
                    },
                )
                .unwrap();
                assert_eq!(
                    decisions
                        .iter()
                        .map(|d| d.completely_contained)
                        .collect::<Vec<_>>(),
                    [true, true, true, false, false]
                );
                assert_eq!(decisions.iter().map(|d| d.site).collect::<Vec<_>>(), sites);
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
                let materials = [0, 1, 2].map(|i| {
                    textures[[0, 1, 0][i]]
                        .material(&device, scene.pattern_layout(), sites[i].origin, [64., 64.])
                        .unwrap()
                });
                assert!(materials
                    .iter()
                    .all(|m| m.scene_material().kind() == AreaMaterialKind::WholeMotif));
                for policy in [D::Unclipped, D::ClipDataset(7), D::Hidden] {
                    for order in [[0, 1, 2], [2, 1, 0]] {
                        let mut reference = None;
                        for batching in [false, true] {
                            // Only accepted independent sites enter the actual Scene;
                            // rejected entries are retained in decisions but never drawn.
                            let mut draws = vec![GlobeDraw {
                                layer: GlobeLayer {
                                    mesh: &ground,
                                    depth_mode: GlobeDepthMode::Occluder,
                                },
                                texture: None,
                                pattern: None,
                                font_color: None,
                            }];
                            for &i in &order {
                                draws.push(GlobeDraw {
                                    layer: GlobeLayer {
                                        mesh: &surface,
                                        depth_mode: GlobeDepthMode::SurfaceOverlay,
                                    },
                                    texture: None,
                                    pattern: Some(materials[i].scene_material()),
                                    font_color: None,
                                });
                            }
                            scene.set_gpu_projection_enabled(true);
                            scene.set_coverage_batching_enabled(batching);
                            scene
                                .prepare_draws(&device, &queue, &camera, &draws)
                                .unwrap();
                            assert_eq!(scene.resource_usage()["gpu_projection_used"], false);
                            let epoch = scene.coverage_epoch().unwrap();
                            scene
                                .bind_prepared_coverage(
                                    &device,
                                    &queue,
                                    epoch,
                                    &frame,
                                    &[D::Unclipped, policy, policy, policy],
                                    64 * 64 * 2,
                                )
                                .unwrap();
                            let rgba = read_scene(&device, &queue, &mut scene);
                            if let Some(ref pixels) = reference {
                                assert_eq!(
                                    pixels, &rgba,
                                    "batching changed natural motif composition"
                                );
                            } else {
                                reference = Some(rgba.clone());
                            }
                            let mut probes = [None, None, None, None];
                            let mut checked = 0;
                            for y in 2..62 {
                                for x in 2..62 {
                                    let p = [x as f64 + 0.5, y as f64 + 0.5];
                                    let paint0 = paint(p, sites[0].origin, [1., 0., 0.]);
                                    let paint1 = paint(p, sites[1].origin, [0., 0., 1.]);
                                    let paint2 = paint(p, sites[2].origin, [1., 0., 0.]);
                                    if paint0.1 || paint1.1 || paint2.1 {
                                        continue;
                                    }
                                    let mask = p[0] < 40. && !inside_rect(p, [14., 18., 25., 31.]);
                                    let allowed = match policy {
                                        D::Unclipped => true,
                                        D::ClipDataset(7) => mask,
                                        D::Hidden => false,
                                        _ => unreachable!(),
                                    };
                                    let mut colour = [0., 1., 0.];
                                    let mut top = 0;
                                    if allowed {
                                        for (draw, &i) in order.iter().enumerate() {
                                            if let Some((rgb, a)) =
                                                [paint0.0, paint1.0, paint2.0][i]
                                            {
                                                colour = std::array::from_fn(|c| {
                                                    rgb[c] * a + colour[c] * (1. - a)
                                                });
                                                top = draw + 1;
                                            }
                                        }
                                    }
                                    let at = (y * 64 + x) * 4;
                                    for c in 0..3 {
                                        assert!((f64::from(rgba[at+c])-255.*colour[c]).abs()<=2.,
                                    "independent alpha oracle {p:?} component{c} order{order:?} policy{policy:?} samples{samples}");
                                    }
                                    assert_eq!(rgba[at + 3], 255);
                                    probes[top].get_or_insert(p);
                                    checked += 1;
                                }
                            }
                            assert!(checked > 2000);
                            assert!(probes[0].is_some());
                            if policy != D::Hidden {
                                assert!(
                                    probes[1].is_some()
                                        && probes[2].is_some()
                                        && probes[3].is_some()
                                );
                            }
                            for (draw, p) in probes.into_iter().enumerate() {
                                if let Some(p) = p {
                                    let hits = scene.pick_draws(&device, &queue, p, 0.).unwrap();
                                    assert_eq!(hits.len(), 1);
                                    assert_eq!(hits[0].draw_index, draw);
                                    if draw > 0 {
                                        let site = decisions[order[draw - 1]].site;
                                        assert_eq!(site.source_ordinal, 37);
                                        assert!(decisions[order[draw - 1]].completely_contained);
                                        assert_eq!(site, sites[order[draw - 1]]);
                                    }
                                }
                            }
                            for (label, p) in [
                                (
                                    "donut-hole",
                                    [sites[0].origin[0] + 6.5, sites[0].origin[1] + 6.5],
                                ),
                                (
                                    "disconnected-component",
                                    [sites[0].origin[0] + 18.5, sites[0].origin[1] + 2.5],
                                ),
                                (
                                    "component-gap",
                                    [sites[0].origin[0] + 14.5, sites[0].origin[1] + 2.5],
                                ),
                                ("rejected-site-paint", [28.5, 28.5]),
                                ("coverage-hole", [16.5, 20.5]),
                                ("coverage-exterior", [48.5, 48.5]),
                            ] {
                                let mask = p[0] < 40. && !inside_rect(p, [14., 18., 25., 31.]);
                                let allowed = match policy {
                                    D::Unclipped => true,
                                    D::ClipDataset(7) => mask,
                                    D::Hidden => false,
                                    _ => unreachable!(),
                                };
                                let mut top = 0;
                                if allowed {
                                    for (draw, &i) in order.iter().enumerate() {
                                        if paint(
                                            p,
                                            sites[i].origin,
                                            [[1., 0., 0.], [0., 0., 1.], [1., 0., 0.]][i],
                                        )
                                        .0
                                        .is_some()
                                        {
                                            top = draw + 1;
                                        }
                                    }
                                }
                                let hits = scene.pick_draws(&device, &queue, p, 0.).unwrap();
                                assert_eq!(hits.len(), 1, "classified {label} cropped query");
                                assert_eq!(
                                    hits[0].draw_index, top,
                                    "classified {label} ID, {v2:?}, {policy:?}, order{order:?}"
                                );
                                if label == "rejected-site-paint" {
                                    assert_eq!(top, 0);
                                }
                            }
                            println!("ACTUAL_WHOLE_MOTIF {samples} {v2:?} {policy:?} order{order:?} batch{batching}: {checked} interior pixels, removal, actual cropped-ID probes");
                            checked_total += checked;
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 96);
        assert!(checked_total > 192000);
        assert!(device.pop_error_scope().await.is_none());
        std::fs::remove_dir_all(root).unwrap();
        println!("ACTUAL_WHOLE_MOTIF_TOTAL {cases} cases / {checked_total} independent interior pixel comparisons");
    });
}
