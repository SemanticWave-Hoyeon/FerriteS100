//! Real WgpuRenderer hatch path, analytic physical pixel oracle and ROI picking.
//! Synthetic kernel fixture, not an official IHO certification test.
use ferrite_kernel::{geodesy::GeographicPosition, globe_navigation::GlobePose};
use ferrite_render::*;
use ferrite_wgpu::{globe_hatch::pattern_origin, WgpuRenderer};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct App {
    out: PathBuf,
}
fn inside(ring: &[[f64; 2]], p: [f64; 2]) -> bool {
    let mut hit = false;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            hit = !hit;
        }
    }
    hit
}
fn distance(ring: &[[f64; 2]], p: [f64; 2]) -> f64 {
    ring.iter()
        .enumerate()
        .map(|(i, &a)| {
            let b = ring[(i + 1) % ring.len()];
            let dx = b[0] - a[0];
            let dy = b[1] - a[1];
            let len = dx * dx + dy * dy;
            let t = if len > 0. {
                ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len
            } else {
                0.
            }
            .clamp(0., 1.);
            (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
        })
        .fold(f64::INFINITY, f64::min)
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            event
                .create_window(
                    Window::default_attributes()
                        .with_title("Globe hatch kernel oracle")
                        .with_inner_size(PhysicalSize::new(900, 700)),
                )
                .unwrap(),
        );
        let mut renderer = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        renderer.ui_state.globe_preview = true;
        let mut ctx = RenderContext::new(Viewport::with_origin(60., 50., 640., 480.));
        ctx.set_bounds(GeoBounds::new(-0.2, -0.2, 0.2, 0.2));
        let ring = |r: f64| {
            vec![
                WorldPoint::new(-r, -r),
                WorldPoint::new(r, -r),
                WorldPoint::new(r, r),
                WorldPoint::new(-r, r),
            ]
        };
        let mut rows = Vec::new();
        for samples in [1, 4] {
            renderer.set_globe_sample_count(samples).unwrap();
            for gpu in [false, true] {
                renderer.set_globe_gpu_projection_enabled(gpu);
                for (heading, tilt) in [(0., 0.), (90., 60.)] {
                    let pose = GlobePose {
                        focus: GeographicPosition::new(0., 0.).unwrap(),
                        range_m: 30000.,
                        heading_deg: heading,
                        tilt_deg: tilt,
                    };
                    let camera = pose.camera([640., 480.]).unwrap();
                    renderer.ui_state.globe_pose = Some(pose);
                    renderer.ui_state.globe_tilt_deg = tilt;
                    ctx.clear_instructions();
                    renderer.begin_frame();
                    renderer.prepare_globe_preview(&mut ctx, None).unwrap();
                    let baseline_path = self.out.join(format!("base-{samples}-{gpu}-{tilt}.png"));
                    renderer.save_screenshot(&baseline_path).unwrap();
                    let baseline = image::open(baseline_path).unwrap().to_rgb8();
                    for crs in [
                        PatternCrs::Global,
                        PatternCrs::LocalGeometry,
                        PatternCrs::GlobalGeometry,
                    ] {
                        for (mode, offset_mm) in [
                            ("solid", 0.),
                            ("butt", 0.),
                            ("square", 0.),
                            ("round", 0.),
                            ("round_overlap", 0.),
                            ("round_alpha_overlap", 0.),
                            ("solid", 0.7),
                            ("butt", 0.7),
                            ("square", 0.7),
                            ("round", 0.7),
                        ] {
                            let dashed = mode != "solid";
                            let square = mode == "square";
                            let round = mode.starts_with("round");
                            let angle = 35.;
                            let width = 1.6;
                            let spacing = if mode.contains("overlap") { 1. } else { 5. };
                            let alpha = if mode.contains("alpha") { 0.5 } else { 1. };
                            let mut style =
                                LineStyle::solid_mm(Color::RED.with_alpha(alpha), width);
                            style.offset_mm = offset_mm;
                            if square {
                                style.cap = CapStyle::Square;
                            }
                            if round {
                                style.cap = CapStyle::Round;
                            }
                            if dashed {
                                style.dash_cycle =
                                    Some(ferrite_kernel::DashCycle::new(8., [(5., -4.)]).unwrap());
                            }
                            let area = AreaInstruction::new(ring(0.09))
                                .with_interiors(vec![ring(0.015)])
                                .with_hatch_fill(Color::RED, width, spacing, angle)
                                .with_pattern_crs(crs)
                                .with_hatch_strokes(vec![HatchStroke {
                                    style,
                                    interval_length_mm: 8.,
                                    symbols: Box::default(),
                                }])
                                .with_feature_id(42);
                            ctx.clear_instructions();
                            ctx.add_instruction(DrawingInstruction::Area(area.clone()));
                            renderer.begin_frame();
                            renderer.prepare_globe_preview(&mut ctx, None).unwrap();
                            let diag = renderer.globe_preview_diagnostics().unwrap();
                            assert_eq!(diag.areas, 1);
                            assert_eq!(diag.rejected_geometries, 0, "{:?}", diag.reasons);
                            let name = format!("{samples}-{gpu}-{tilt}-{crs:?}-{mode}-{offset_mm}");
                            let path = self.out.join(format!("{name}.png"));
                            renderer.save_screenshot(&path).unwrap();
                            let im = image::open(path).unwrap().to_rgb8();
                            let source = ferrite_wgpu::globe_portrayal::drape_area_geometry(
                                &area.exterior,
                                &area.interiors,
                                &camera,
                                Default::default(),
                            )
                            .unwrap()
                            .0;
                            let region=ferrite_kernel::globe_coverage_projection::project_coverage_triangles(&camera,source.indices.chunks_exact(3).map(|t|std::array::from_fn(|i|source.vertices[t[i]as usize].ecef_m)),Default::default()).unwrap();
                            let polygons: Vec<_> = region
                                .polygons()
                                .iter()
                                .map(|p| {
                                    (
                                        p.exterior()
                                            .0
                                            .iter()
                                            .map(|c| [c.x, c.y])
                                            .collect::<Vec<_>>(),
                                        p.interiors()
                                            .iter()
                                            .map(|h| {
                                                h.0.iter().map(|c| [c.x, c.y]).collect::<Vec<_>>()
                                            })
                                            .collect::<Vec<_>>(),
                                    )
                                })
                                .collect();
                            let origin = pattern_origin(
                                crs,
                                &camera,
                                area.exterior[0],
                                WorldPoint::new(0., 0.),
                            )
                            .unwrap();
                            let ppm = ctx.scaler.pixels_per_mm();
                            let a = (angle as f64).to_radians();
                            let half = width as f64 * ppm * 0.5;
                            let gap = spacing as f64 * ppm;
                            let mut checked = 0;
                            let mut coloured = 0;
                            let mut pick_on = None;
                            let mut pick_off = None;
                            for y in (2..480).step_by(4) {
                                for x in (2..640).step_by(4) {
                                    let p = [x as f64 + 0.5, y as f64 + 0.5];
                                    if polygons.iter().any(|(e, h)| {
                                        distance(e, p) < 2. || h.iter().any(|h| distance(h, p) < 2.)
                                    }) {
                                        continue;
                                    }
                                    let polygon = polygons.iter().any(|(e, h)| {
                                        inside(e, p) && !h.iter().any(|h| inside(h, p))
                                    });
                                    let q = (a.sin() * (p[0] - origin[0])
                                        + a.cos() * (p[1] - origin[1])
                                        + offset_mm * ppm)
                                        .rem_euclid(gap);
                                    let q = q.min(gap - q);
                                    if (q - half).abs() < 1.1 {
                                        continue;
                                    }
                                    let along = ((a.cos() * (p[0] - origin[0])
                                        - a.sin() * (p[1] - origin[1]))
                                        / ppm)
                                        .rem_euclid(8.);
                                    let start = if square { 1. - width as f64 * 0.5 } else { 1. };
                                    let end = if square { 5. + width as f64 * 0.5 } else { 5. };
                                    if dashed
                                        && ((along - start).abs() * ppm < 1.1
                                            || (along - end).abs() * ppm < 1.1)
                                    {
                                        continue;
                                    }
                                    let dx = if along < 1. {
                                        (1. - along).min(along + 3.)
                                    } else if along > 5. {
                                        (along - 5.).min(9. - along)
                                    } else {
                                        0.
                                    };
                                    let radial = q.hypot(dx * ppm);
                                    if round && (radial - half).abs() < 1.1 {
                                        continue;
                                    }
                                    let expected = polygon
                                        && if round {
                                            radial < half
                                        } else {
                                            q < half && (!dashed || (along > start && along < end))
                                        };
                                    let pixel = im.get_pixel(x + 60, y + 50).0;
                                    if expected {
                                        let base = baseline.get_pixel(x + 60, y + 50).0;
                                        let expected_rgb = std::array::from_fn::<_, 3, _>(|i| {
                                            ((if i == 0 { 255. } else { 0. }) * alpha as f64
                                                + base[i] as f64 * (1. - alpha as f64))
                                                .round()
                                                as u8
                                        });
                                        assert!((0..3).all(|i|pixel[i].abs_diff(expected_rgb[i])<=2),
                                            "{name}: missing or duplicate-alpha hatch {p:?} {pixel:?} expected {expected_rgb:?}");
                                        coloured += 1;
                                        if pick_on.is_none() {
                                            pick_on = Some(p);
                                        }
                                    } else {
                                        let base = baseline.get_pixel(x + 60, y + 50).0;
                                        assert!((0..3).all(|i|(pixel[i]as i16-base[i]as i16).abs()<=2),"{name}: unwanted hatch {p:?} {pixel:?} baseline {base:?}");
                                        if !polygon
                                            && p[0] > 280.
                                            && p[0] < 360.
                                            && p[1] > 200.
                                            && p[1] < 280.
                                        {
                                            pick_off = Some(p);
                                        }
                                    }
                                    checked += 1;
                                }
                            }
                            assert!(coloured > 20 && checked > 1000);
                            let on = pick_on.unwrap();
                            let on_hits = renderer
                                .globe_feature_candidates(
                                    &ctx,
                                    ScreenPoint::new(on[0] as f32 + 60., on[1] as f32 + 50.),
                                    0.,
                                )
                                .unwrap();
                            assert!(
                                on_hits.iter().any(|(source, _)| *source == 0),
                                "hatch not pickable"
                            );
                            let off = pick_off.expect("No hole pixel was probed");
                            {
                                assert!(
                                    renderer
                                        .globe_feature_candidates(
                                            &ctx,
                                            ScreenPoint::new(
                                                off[0] as f32 + 60.,
                                                off[1] as f32 + 50.
                                            ),
                                            0.
                                        )
                                        .unwrap()
                                        .is_empty(),
                                    "hole hatch selectable"
                                );
                            }
                            rows.push(serde_json::json!({"name":name,"samples":samples,"gpu_projection":gpu,"tilt":tilt,"crs":format!("{crs:?}"),"dashed":dashed,"cap_mode":mode,"offset_mm":offset_mm,"checked_pixels":checked,"coloured_pixels":coloured,"pixels_per_mm":ppm,"pick_on":true,"pick_hole":true}));
                        }
                    }
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":rows,"native_app_render_path":true,"official_iho_fixture":false})).unwrap()).unwrap();
        event.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let out = std::env::args().nth(1).expect("output directory");
    EventLoop::new()
        .unwrap()
        .run_app(&mut App { out: out.into() })
        .unwrap();
}
