//! Native screen-stroke visibility; independent equatorial tangent oracle.
use ferrite_kernel::{
    geodesy::{GeographicPosition, WGS84_A},
    globe_navigation::GlobePose,
};
use ferrite_render::*;
use ferrite_wgpu::WgpuRenderer;
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
impl ApplicationHandler for App {
    fn resumed(&mut self, event: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let window = Arc::new(
            event
                .create_window(
                    Window::default_attributes()
                        .with_title("Screen strokes: exact source visibility")
                        .with_inner_size(PhysicalSize::new(900, 700)),
                )
                .unwrap(),
        );
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.ui_state.globe_preview = true;
        let mut context = RenderContext::new(Viewport::with_origin(60., 50., 640., 480.));
        context.set_bounds(GeoBounds::new(-0.2, -0.2, 0.2, 0.2));
        let mut rows = Vec::new();
        for samples in [1, 4] {
            renderer.set_globe_sample_count(samples).unwrap();
            for gpu in [false, true] {
                renderer.set_globe_gpu_projection_enabled(gpu);
                for range in [150., 20_000_000.] {
                    let pose = GlobePose {
                        focus: GeographicPosition::new(0., 0.).unwrap(),
                        range_m: range,
                        heading_deg: 0.,
                        tilt_deg: 0.,
                    };
                    let camera = pose.camera([640., 480.]).unwrap();
                    renderer.ui_state.globe_pose = Some(pose);
                    renderer.ui_state.globe_tilt_deg = 0.;
                    context.clear_instructions();
                    renderer.begin_frame();
                    renderer.prepare_globe_preview(&mut context, None).unwrap();
                    let basepath = self.out.join(format!("base-{samples}-{gpu}-{range}.png"));
                    renderer.save_screenshot(&basepath).unwrap();
                    let baseline = image::open(basepath).unwrap().to_rgb8();
                    // Wide-view ends lie behind Earth; the visible middle must
                    // survive. Near-view ends are independently projected.
                    let source = if range > 1_000_000. {
                        vec![
                            WorldPoint::new(-100., 0.),
                            WorldPoint::new(0., 0.),
                            WorldPoint::new(100., 0.),
                        ]
                    } else {
                        vec![WorldPoint::new(-0.0002, 0.), WorldPoint::new(0.0002, 0.)]
                    };
                    let half_length = if range > 1_000_000. {
                        let eye_radius = WGS84_A + range;
                        320. * WGS84_A
                            / (eye_radius * eye_radius - WGS84_A * WGS84_A).sqrt()
                            / camera.projection_frame().divisors[0]
                    } else {
                        let clip = camera
                            .clip_ecef(
                                GeographicPosition::new(0., 0.0002)
                                    .unwrap()
                                    .to_ecef(0.)
                                    .unwrap(),
                            )
                            .unwrap();
                        320. * (clip[0] / clip[3]).abs()
                    };
                    for hidden in [false, true] {
                        for dashed in [false, true] {
                            let points = if hidden {
                                vec![WorldPoint::new(160., 0.), WorldPoint::new(170., 0.)]
                            } else {
                                source.clone()
                            };
                            assert!(
                                points.iter().all(|p| camera
                                    .project_visible(
                                        GeographicPosition::new(p.y, p.x)
                                            .unwrap()
                                            .to_ecef(0.)
                                            .unwrap()
                                    )
                                    .unwrap()
                                    .is_none())
                                    == (hidden || range > 1_000_000. && points.len() == 2)
                            );
                            // In the wide visible case only the two end anchors are
                            // hidden. Check them without classifying the middle.
                            if !hidden && range > 1_000_000. {
                                for p in [points.first().unwrap(), points.last().unwrap()] {
                                    assert!(camera
                                        .project_visible(
                                            GeographicPosition::new(p.y, p.x)
                                                .unwrap()
                                                .to_ecef(0.)
                                                .unwrap()
                                        )
                                        .unwrap()
                                        .is_none());
                                }
                            }
                            let phase_start = if range > 1_000_000. {
                                let clip = camera
                                    .clip_ecef(
                                        GeographicPosition::new(0., -100.)
                                            .unwrap()
                                            .to_ecef(0.)
                                            .unwrap(),
                                    )
                                    .unwrap();
                                let authored_x = 320. * (clip[0] / clip[3] + 1.);
                                (authored_x - (320. - half_length)).abs()
                            } else {
                                0.
                            };
                            let mut line = LineInstruction::new(points)
                                .with_feature_id(19)
                                .with_cell_index(0)
                                .with_style(LineStyle::solid_mm(Color::RED, 1.6));
                            if dashed {
                                line.style.dash_cycle =
                                    Some(ferrite_kernel::DashCycle::new(8., [(5., -4.)]).unwrap());
                            }
                            context.clear_instructions();
                            context.add_instruction(DrawingInstruction::Line(line));
                            renderer.begin_frame();
                            renderer.prepare_globe_preview(&mut context, None).unwrap();
                            assert_eq!(
                                renderer
                                    .globe_preview_diagnostics()
                                    .unwrap()
                                    .rejected_geometries,
                                0
                            );
                            let name = format!("{samples}-{gpu}-{range}-{hidden}-{dashed}");
                            let path = self.out.join(format!("{name}.png"));
                            renderer.save_screenshot(&path).unwrap();
                            let pixels = image::open(path).unwrap().to_rgb8();
                            let half_width = 0.8 * context.scaler.pixels_per_mm();
                            let mut checked = 0;
                            let mut coloured = 0;
                            let mut on = None;
                            for y in 180..300 {
                                for x in 40..600 {
                                    let across = (y as f64 + 0.5 - 240.).abs();
                                    let along = (x as f64 + 0.5 - 320.).abs();
                                    if (across - half_width).abs() < 1.1
                                        || (along - half_length).abs() < 1.1
                                    {
                                        continue;
                                    }
                                    let phase = ((phase_start + x as f64 + 0.5
                                        - (320. - half_length))
                                        / context.scaler.pixels_per_mm())
                                    .rem_euclid(8.);
                                    if dashed
                                        && ((phase - 1.).abs() * context.scaler.pixels_per_mm()
                                            < 1.1
                                            || (phase - 5.).abs() * context.scaler.pixels_per_mm()
                                                < 1.1)
                                    {
                                        continue;
                                    }
                                    let expected = !hidden
                                        && across < half_width
                                        && along < half_length
                                        && (!dashed || phase > 1. && phase < 5.);
                                    let actual = pixels.get_pixel(x + 60, y + 50).0;
                                    if expected {
                                        assert!(
                                            actual[0] >= 252 && actual[1] <= 2 && actual[2] <= 2,
                                            "{name}: missing {x},{y} {actual:?}"
                                        );
                                        coloured += 1;
                                        on.get_or_insert([x as f32 + 60.5, y as f32 + 50.5]);
                                    } else {
                                        let base = baseline.get_pixel(x + 60, y + 50).0;
                                        assert!(
                                            (0..3)
                                                .all(|i| (actual[i] as i16 - base[i] as i16).abs()
                                                    <= 2),
                                            "{name}: excess {x},{y} {actual:?}/{base:?}"
                                        );
                                    }
                                    checked += 1;
                                }
                            }
                            assert!(
                                checked > 10000
                                    && (hidden && coloured == 0 || !hidden && coloured > 20)
                            );
                            let hits = renderer
                                .globe_feature_candidates(
                                    &context,
                                    {
                                        let p = on.unwrap_or([380.5, 289.5]);
                                        ScreenPoint::new(p[0], p[1])
                                    },
                                    0.,
                                )
                                .unwrap();
                            assert_eq!(hits.iter().any(|(i, _)| *i == 0), !hidden);
                            rows.push(serde_json::json!({"samples":samples,"gpu_projection":gpu,"range_m":range,"hidden":hidden,"dashed":dashed,"checked_pixels":checked,"coloured_pixels":coloured,"pick_matches":true,"analytic_half_length_px":half_length}));
                        }
                    }
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":rows,"equatorial_tangent_oracle":true,"native_physical_gestures_verified":false})).unwrap()).unwrap();
        event.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: std::env::args().nth(1).expect("output").into(),
        })
        .unwrap();
}
