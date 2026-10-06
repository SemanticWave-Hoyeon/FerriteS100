//! Pixel oracles for the real chart-pane compositor at a low tilted camera.
use ferrite_kernel::{
    geodesy::{inverse, GeographicPosition},
    globe_camera::GlobeCamera,
    rhumb::RhumbSegment,
};
use ferrite_render::{
    AreaInstruction, Color, DrawingInstruction, FlatProjection, GeoBounds, LineInstruction,
    LineStyle, RenderContext, Viewport, WorldPoint,
};
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
fn forward_root_y(c: &GlobeCamera) -> f64 {
    let route = RhumbSegment::new(
        GeographicPosition::new(-0.02, 0.).unwrap(),
        GeographicPosition::new(0.02, 0.).unwrap(),
    )
    .unwrap();
    let mut behind = 0.;
    let mut forward = 1.;
    assert!(
        c.clip_ecef(route.point(0.).unwrap().to_ecef(0.).unwrap())
            .unwrap()[3]
            < 1.
    );
    for _ in 0..100 {
        let t = (behind + forward) / 2.;
        if t == behind || t == forward {
            break;
        }
        let e = route.point(t).unwrap().to_ecef(0.).unwrap();
        if c.clip_ecef(e).unwrap()[3] >= 1. {
            forward = t;
        } else {
            behind = t;
        }
    }
    let clip = c
        .clip_ecef(route.point(forward).unwrap().to_ecef(0.).unwrap())
        .unwrap();
    (1. - clip[1] / clip[3]) * c.viewport()[1] / 2.
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Low tilted globe clipping pixel oracles")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let v = Viewport::with_origin(50., 60., 900., 600.);
        let mut ctx = RenderContext::new(v);
        ctx.scaler
            .set_projection(FlatProjection::EllipsoidalMercator);
        ctx.set_bounds(GeoBounds::new(-0.1, -0.1, 0.1, 0.1));
        let span = inverse(
            GeographicPosition::new(-0.1, 0.).unwrap(),
            GeographicPosition::new(0.1, 0.).unwrap(),
        )
        .unwrap()
        .distance_m;
        r.ui_state.globe_preview = true;
        r.ui_state.globe_tilt_deg = 45.;
        r.ui_state.globe_range_factor = 500. * (2. * 22.5_f64.to_radians().tan()) / span;
        let camera = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            500.,
            0.,
            45.,
            [900., 600.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let mut area = AreaInstruction::new(vec![
            WorldPoint::new(-0.1, -0.1),
            WorldPoint::new(0.1, -0.1),
            WorldPoint::new(0.1, 0.1),
            WorldPoint::new(-0.1, 0.1),
        ])
        .with_solid_fill(Color::rgb(0., 0.7, 0.2));
        area.interiors.push(vec![
            WorldPoint::new(-0.001, -0.001),
            WorldPoint::new(-0.001, 0.001),
            WorldPoint::new(0.001, 0.001),
            WorldPoint::new(0.001, -0.001),
        ]);
        ctx.add_instruction(DrawingInstruction::Area(area));
        r.begin_frame();
        r.prepare_globe_preview(&mut ctx, None).unwrap();
        assert_eq!(
            r.globe_preview_diagnostics().unwrap().rejected_geometries,
            0
        );
        r.save_screenshot(self.out.join("area-eye-crossing-hole.png"))
            .unwrap();
        let img = image::open(self.out.join("area-eye-crossing-hole.png"))
            .unwrap()
            .to_rgb8();
        let (mut filled, mut hole) = (0, 0);
        for y in (2..598).step_by(3) {
            for x in (2..898).step_by(3) {
                let p = camera
                    .pick([x as f64 + 0.5, y as f64 + 0.5])
                    .unwrap()
                    .unwrap()
                    .geodetic
                    .surface;
                let lon = p.longitude();
                let lat = p.latitude();
                // Exclude a 5 m guard at the known authored hole boundary; this oracle
                // classifies geographic interior independently of tessellation/triangle IDs.
                if ((lon.abs() - 0.001).abs() < 0.00005 && lat.abs() < 0.00105)
                    || ((lat.abs() - 0.001).abs() < 0.00005 && lon.abs() < 0.00105)
                {
                    continue;
                }
                let expected = !(lon.abs() < 0.001 && lat.abs() < 0.001);
                let q = img.get_pixel(x + v.x as u32, y + v.y as u32).0;
                let green = q[1] as u16 > q[0] as u16 + 20 && q[1] as u16 > q[2] as u16 + 20;
                assert_eq!(
                    green, expected,
                    "area coverage mismatch at {x},{y}: {lon},{lat} {q:?}"
                );
                if expected {
                    filled += 1;
                } else {
                    hole += 1;
                }
            }
        }
        assert!(filled > 10000 && hole > 100);
        let root_y = forward_root_y(&camera);
        let mut dash_rows = Vec::new();
        let mut previous = None;
        for suppress in [false, true] {
            ctx.clear_instructions();
            let mut low =
                LineInstruction::new(vec![WorldPoint::new(0., -0.02), WorldPoint::new(0., 0.02)])
                    .with_priority(1);
            low.style = LineStyle::dashed(Color::BLACK, 6., vec![12., 8.]);
            ctx.add_instruction(DrawingInstruction::Line(low));
            if suppress {
                let mut high =
                    LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(0., 0.02)])
                        .with_priority(2);
                high.style = LineStyle::solid(Color::rgb(1., 0., 0.), 6.);
                ctx.add_instruction(DrawingInstruction::Line(high));
            }
            r.begin_frame();
            r.prepare_globe_preview(&mut ctx, None).unwrap();
            let diag = r.globe_preview_diagnostics().unwrap().as_json();
            assert_eq!(diag["rejected_geometries"], 0);
            let name = if suppress {
                "dash-eye-crossing-suppressed.png"
            } else {
                "dash-eye-crossing.png"
            };
            r.save_screenshot(self.out.join(name)).unwrap();
            let img = image::open(self.out.join(name)).unwrap().to_rgb8();
            let (mut black, mut gap, mut red) = (0, 0, 0);
            for y in 2..598 {
                let q = img.get_pixel(v.x as u32 + 450, v.y as u32 + y).0;
                if suppress && y < 298 {
                    assert!(
                        q[0] > 200 && q[1] < 50 && q[2] < 50,
                        "higher-priority curve absent at {y}: {q:?}"
                    );
                    red += 1;
                    continue;
                }
                if suppress && y <= 302 {
                    continue;
                }
                let phase = (root_y - (y as f64 + 0.5)).abs().rem_euclid(20.);
                if phase < 0.75 || (phase - 12.).abs() < 0.75 || phase > 19.25 {
                    continue;
                }
                let actual = q.iter().all(|c| *c < 20);
                let expected = phase < 12.;
                assert_eq!(
                    actual, expected,
                    "dash phase reset/mismatch at {y}: phase {phase} {q:?}"
                );
                if expected {
                    black += 1;
                } else {
                    gap += 1;
                }
                if let Some(before) = &previous {
                    let before: &image::RgbImage = before;
                    assert_eq!(
                        before.get_pixel(v.x as u32 + 450, v.y as u32 + y),
                        img.get_pixel(v.x as u32 + 450, v.y as u32 + y),
                        "suppression reset phase in remaining interval"
                    );
                }
            }
            assert!(black > 80 && gap > 50);
            dash_rows.push(serde_json::json!({"suppressed":suppress,"black_probes":black,"gap_probes":gap,"red_probes":red,"diagnostics":diag,"independent_near_root_y":root_y,"dash_origin_restarted_at_viewport":false}));
            previous = Some(img);
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"actual_chart_pane_renderer":true,"camera_range_m":500.,"camera_tilt_deg":45.,"area_filled_probes":filled,"area_hole_probes":hole,"dash_rows":dash_rows,"physical_input_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
}
fn main() {
    let mut app = App {
        out: std::env::args().nth(1).unwrap().into(),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
