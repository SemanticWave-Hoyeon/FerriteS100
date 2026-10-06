//! Real scale-filter and camera/calibration cache-key pixel invariants.
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
}
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(1100, 800)),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        let v = Viewport::with_origin(40., 50., 900., 600.);
        let mut ctx = RenderContext::new(v);
        ctx.set_bounds(GeoBounds::new(-0.1, -0.1, 0.1, 0.1));
        ctx.scaler.set_pixel_ratio(density);
        r.ui_state.globe_preview = true;
        let range =
            16000. * 600. / (2. * 22.5_f64.to_radians().tan() * ctx.scaler.pixels_per_mm() * 1000.);
        for (id, color, scale) in [
            (
                "small",
                Color::rgb(0., 1., 0.),
                ScaleRange {
                    scale_minimum: Some(20000),
                    scale_maximum: None,
                },
            ),
            (
                "large",
                Color::rgb(1., 0., 0.),
                ScaleRange {
                    scale_minimum: None,
                    scale_maximum: Some(20000),
                },
            ),
        ] {
            let mut l =
                LineInstruction::new(vec![WorldPoint::new(0., -0.02), WorldPoint::new(0., 0.02)]);
            l.style = LineStyle::solid(color, 12.);
            l.scale_range = scale;
            let mut x = DrawingInstruction::Line(l);
            x.set_dependency(DrawingDependency::new(1, Some(id), None, false));
            ctx.add_instruction(x);
        }
        let mut child = DrawingInstruction::Area(
            AreaInstruction::new(vec![
                WorldPoint::new(-0.02, -0.02),
                WorldPoint::new(0.02, -0.02),
                WorldPoint::new(0.02, 0.02),
                WorldPoint::new(-0.02, 0.02),
            ])
            .with_solid_fill(Color::rgb(0., 0., 1.)),
        );
        child.set_dependency(DrawingDependency::new(
            1,
            Some("child"),
            Some("small"),
            false,
        ));
        ctx.add_instruction(child);
        let mut rows = Vec::new();
        for (tilt, factor, green) in [(0., 1., true), (45., 1., false), (45., 0.5, true)] {
            let p = GlobePose {
                focus: GeographicPosition::new(0., 0.).unwrap(),
                range_m: range * factor,
                heading_deg: 0.,
                tilt_deg: tilt,
            };
            r.ui_state.globe_pose = Some(p);
            r.ui_state.globe_tilt_deg = tilt;
            assert!(!r.geometry_matches_view(&ctx.scaler));
            r.begin_frame();
            r.prepare_globe_preview(&mut ctx, None).unwrap();
            assert!(r.geometry_matches_view(&ctx.scaler));
            let expected = 16000. * factor / tilt.to_radians().cos();
            let d = r.globe_preview_diagnostics().unwrap();
            assert!((d.scale_denominator / expected - 1.).abs() < 1e-9);
            assert_eq!(d.lines, 1);
            assert_eq!(d.areas, usize::from(green));
            assert_eq!(d.rejected_geometries, 0);
            assert_eq!(r.viewing_scale(), expected.round() as u32);
            let file = self.out.join(format!("scale-{tilt}-{factor}.png"));
            r.save_screenshot(&file).unwrap();
            let img = image::open(file).unwrap().to_rgb8();
            let q = img.get_pixel(490, 350).0;
            assert!(
                if green {
                    q[1] > 200 && q[0] < 20 && q[2] < 20
                } else {
                    q[0] > 200 && q[1] < 20 && q[2] < 20
                },
                "wrong scale-filtered line {q:?}"
            );
            let old = r.ui_state.globe_pose.unwrap();
            r.ui_state.globe_pose = Some(GlobePose {
                heading_deg: 90.,
                ..old
            });
            assert!(!r.geometry_matches_view(&ctx.scaler));
            r.ui_state.globe_pose = Some(old);
            assert!(r.geometry_matches_view(&ctx.scaler));
            r.ui_state.globe_range_factor = 2.;
            assert!(!r.geometry_matches_view(&ctx.scaler));
            r.ui_state.globe_range_factor = 1.;
            ctx.scaler.set_pixel_ratio(density * 2.);
            assert!(!r.geometry_matches_view(&ctx.scaler));
            ctx.scaler.set_pixel_ratio(density);
            assert!(r.geometry_matches_view(&ctx.scaler));
            rows.push(serde_json::json!({"tilt":tilt,"range_factor":factor,"expected_denominator":expected,"actual_denominator":r.viewing_scale(),"areas":usize::from(green),"parent_scale_visibility_verified":true,"line_pixel":q,"pose_heading_calibration_range_keys_verified":true}));
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"native_pixel_ratio":density,"rows":rows,"policy_is_application_defined":true,"physical_hardware_verified":false})).unwrap()).unwrap();
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
