//! Real WgpuRenderer globe line path, independent straight-line pixel oracle.
//! Synthetic fixture; no official IHO certification or OS gesture claim.
use ferrite_kernel::{geodesy::GeographicPosition, globe_navigation::GlobePose};
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
    zero_only: bool,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let window = Arc::new(
            event
                .create_window(
                    Window::default_attributes()
                        .with_title("Physical line offset pixel oracle")
                        .with_inner_size(PhysicalSize::new(900, 700)),
                )
                .unwrap(),
        );
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.ui_state.globe_preview = true;
        let mut context = RenderContext::new(Viewport::with_origin(60., 50., 640., 480.));
        context.set_bounds(GeoBounds::new(-0.2, -0.2, 0.2, 0.2));
        let source = vec![WorldPoint::new(-0.025, 0.), WorldPoint::new(0.025, 0.)];
        let mut rows = Vec::new();
        for samples in [1, 4] {
            renderer.set_globe_sample_count(samples).unwrap();
            for gpu in [false, true] {
                renderer.set_globe_gpu_projection_enabled(gpu);
                for range in [30000., 60000.] {
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
                    let project = |p: WorldPoint| {
                        let ecef = GeographicPosition::new(p.y, p.x)
                            .unwrap()
                            .to_ecef(0.)
                            .unwrap();
                        let c = camera.clip_ecef(ecef).unwrap();
                        [(c[0] / c[3] + 1.) * 320., (1. - c[1] / c[3]) * 240.]
                    };
                    let a = project(source[0]);
                    let b = project(source[1]);
                    assert!((a[1] - b[1]).abs() < 1e-8 && a[0] < b[0]);
                    for dashed in [false, true] {
                        for offset in [-2.43, -1., 0., 1.74, 2.43] {
                            if self.zero_only && offset != 0. {
                                continue;
                            }
                            let mut line = LineInstruction::new(source.clone())
                                .with_feature_id(17)
                                .with_cell_index(0)
                                .with_style(LineStyle::solid_mm(Color::RED, 1.6));
                            line.style.offset_mm = offset;
                            if dashed {
                                line.style.dash_cycle =
                                    Some(ferrite_kernel::DashCycle::new(8., [(5., -4.)]).unwrap());
                            }
                            context.clear_instructions();
                            context.add_instruction(DrawingInstruction::Line(line));
                            renderer.begin_frame();
                            renderer.prepare_globe_preview(&mut context, None).unwrap();
                            let diag = renderer.globe_preview_diagnostics().unwrap();
                            assert_eq!(diag.lines, 1, "{:?}", diag.reasons);
                            assert_eq!(diag.rejected_geometries, 0, "{:?}", diag.reasons);
                            let name = format!("{samples}-{gpu}-{range}-{dashed}-{offset}");
                            let path = self.out.join(format!("{name}.png"));
                            renderer.save_screenshot(&path).unwrap();
                            let pixels = image::open(path).unwrap().to_rgb8();
                            let ppm = context.scaler.pixels_per_mm();
                            let half = 0.8 * ppm;
                            let yline = a[1] - offset * ppm;
                            let mut checked = 0;
                            let mut coloured = 0;
                            let mut on = None;
                            for y in 180..300 {
                                for x in 40..600 {
                                    let p = [x as f64 + 0.5, y as f64 + 0.5];
                                    let across = (p[1] - yline).abs();
                                    if (across - half).abs() < 1.1
                                        || (p[0] - a[0]).abs() < 1.1
                                        || (p[0] - b[0]).abs() < 1.1
                                    {
                                        continue;
                                    }
                                    let phase = ((p[0] - a[0]) / ppm).rem_euclid(8.);
                                    if dashed
                                        && ((phase - 1.).abs() * ppm < 1.1
                                            || (phase - 5.).abs() * ppm < 1.1)
                                    {
                                        continue;
                                    }
                                    let expected = p[0] > a[0]
                                        && p[0] < b[0]
                                        && across < half
                                        && (!dashed || (phase > 1. && phase < 5.));
                                    let actual = pixels.get_pixel(x + 60, y + 50).0;
                                    if expected {
                                        assert!(
                                            actual[0] >= 252 && actual[1] <= 2 && actual[2] <= 2,
                                            "{name}: missing {p:?} {actual:?}"
                                        );
                                        coloured += 1;
                                        if on.is_none() {
                                            on = Some(p);
                                        }
                                    } else {
                                        let base = baseline.get_pixel(x + 60, y + 50).0;
                                        assert!(
                                            (0..3)
                                                .all(|i| (actual[i] as i16 - base[i] as i16).abs()
                                                    <= 2),
                                            "{name}: excess {p:?} {actual:?}/{base:?}"
                                        );
                                    }
                                    checked += 1;
                                }
                            }
                            assert!(checked > 10000 && coloured > 20, "{name}: checked={checked}, coloured={coloured}, ppm={ppm}");
                            let on = on.unwrap();
                            assert!(renderer
                                .globe_feature_candidates(
                                    &context,
                                    ScreenPoint::new(on[0] as f32 + 60., on[1] as f32 + 50.),
                                    0.
                                )
                                .unwrap()
                                .iter()
                                .any(|(i, _)| *i == 0));
                            if offset != 0. {
                                assert!(
                                    renderer
                                        .globe_feature_candidates(
                                            &context,
                                            ScreenPoint::new(on[0] as f32 + 60., a[1] as f32 + 50.),
                                            0.
                                        )
                                        .unwrap()
                                        .is_empty(),
                                    "{name}: unshifted source selectable"
                                );
                            }
                            rows.push(serde_json::json!({"name":name,"samples":samples,"gpu_projection":gpu,"range_m":range,"dashed":dashed,"offset_mm":offset,"pixels_per_mm":ppm,"checked_pixels":checked,"coloured_pixels":coloured,"pick_on":true,"source_position_miss":offset!=0.}));
                        }
                    }
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":rows,"native_app_render_path":true,"official_iho_fixture":false,"zero_only":self.zero_only})).unwrap()).unwrap();
        event.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: args[1].clone().into(),
            zero_only: args.get(2).is_some_and(|s| s == "--zero-only"),
        })
        .unwrap();
}
