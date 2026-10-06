//! Real renderer camera-anchor and terminal-rebuild pixel invariants.
use ferrite_kernel::{
    geodesy::{direct, inverse, GeographicPosition},
    globe_navigation::GlobePose,
};
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, RenderContext, ScreenPoint,
    Viewport, WorldPoint,
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
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(1100, 900)),
            )
            .unwrap(),
        );
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        let v = Viewport::with_origin(60., 70., 900., 700.);
        let mut ctx = RenderContext::new(v);
        ctx.set_bounds(GeoBounds::new(-0.5, -0.5, 0.5, 0.5));
        r.ui_state.globe_preview = true;
        let mut rows = Vec::new();
        for lat in [0., 70., 89.] {
            for tilt in [0., 45., 70.] {
                let p = GlobePose {
                    focus: GeographicPosition::new(lat, 179.99).unwrap(),
                    range_m: 30000.,
                    heading_deg: 0.,
                    tilt_deg: tilt,
                };
                let target = [620., 450.];
                let local = [target[0] - 60., target[1] - 70.];
                let anchor = p
                    .camera([900., 700.])
                    .unwrap()
                    .pick(local)
                    .unwrap()
                    .unwrap()
                    .geodetic
                    .surface;
                ctx.clear_instructions();
                let a = direct(anchor, 0., 200.).unwrap();
                let b = direct(anchor, 180., 200.).unwrap();
                let mut l = LineInstruction::new(vec![
                    WorldPoint::new(a.longitude(), a.latitude()),
                    WorldPoint::new(b.longitude(), b.latitude()),
                ]);
                l.style = LineStyle::solid(Color::BLACK, 12.);
                ctx.add_instruction(DrawingInstruction::Line(l));
                r.ui_state.globe_pose = Some(p);
                r.ui_state.globe_tilt_deg = tilt;
                r.begin_frame();
                r.prepare_globe_preview(&mut ctx, None).unwrap();
                let mut scale = 1.;
                let mut max_drift: f64 = 0.;
                for next in [1.15, 2., 10., 200., 20., 1.] {
                    assert!(r.move_globe_anchor(
                        WorldPoint::new(anchor.longitude(), anchor.latitude()),
                        ScreenPoint::new(target[0] as f32, target[1] as f32),
                        next / scale
                    ));
                    scale = next;
                    r.begin_frame();
                    r.prepare_globe_preview(&mut ctx, None).unwrap();
                    let q = r
                        .globe_pose()
                        .unwrap()
                        .camera([900., 700.])
                        .unwrap()
                        .project_visible(anchor.to_ecef(0.).unwrap())
                        .unwrap()
                        .unwrap();
                    max_drift =
                        max_drift.max((q.screen_px[0] - local[0]).hypot(q.screen_px[1] - local[1]));
                    assert!(max_drift < 1e-5);
                }
                let to = ScreenPoint::new(650., 480.);
                assert!(r.move_globe_anchor(
                    WorldPoint::new(anchor.longitude(), anchor.latitude()),
                    to,
                    1.
                ));
                r.begin_frame();
                r.prepare_globe_preview(&mut ctx, None).unwrap();
                let hit = r.globe_world_at(to).unwrap();
                let ground = inverse(anchor, GeographicPosition::new(hit.y, hit.x).unwrap())
                    .unwrap()
                    .distance_m;
                assert!(ground < 0.001);
                let before = self.out.join("before.png");
                r.save_screenshot(&before).unwrap();
                // A real end-of-animation/declutter rebuild must retain the explicit pose.
                r.begin_frame();
                r.prepare_globe_preview(&mut ctx, None).unwrap();
                let after = self.out.join("after.png");
                r.save_screenshot(&after).unwrap();
                let aa = image::open(&before).unwrap().to_rgba8();
                let bb = image::open(&after).unwrap().to_rgba8();
                assert_eq!(aa, bb);
                let black = bb.get_pixel(650, 480);
                assert!(
                    black[0] < 20 && black[1] < 20 && black[2] < 20,
                    "anchored line absent {lat}/{tilt}: {black:?}"
                );
                assert_eq!(
                    r.globe_preview_diagnostics().unwrap().rejected_geometries,
                    0
                );
                rows.push(serde_json::json!({"latitude":lat,"tilt":tilt,"max_anchor_drift_px":max_drift,"pan_ground_error_m":ground,"final_rebuild_changed_pixels":0,"anchor_pixel_black":true,"scales":[1.15,2.,10.,200.,20.,1.]}));
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"actual_renderer_navigation":true,"physical_hardware_input_verified":false,"rows":rows})).unwrap()).unwrap();
        e.exit();
    }
    fn window_event(&mut self, e: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            e.exit();
        }
    }
}
fn main() {
    let mut a = App {
        out: std::env::args().nth(1).unwrap().into(),
    };
    EventLoop::new().unwrap().run_app(&mut a).unwrap();
}
