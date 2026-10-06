//! Native warm/cold globe GPU resource reuse, resize, invalidation and pixel equivalence.
use ferrite_render::{
    AreaInstruction, Color, DrawingDependency, DrawingInstruction, FlatProjection, GeoBounds,
    LineInstruction, PointInstruction, RenderContext, ScreenPoint, Viewport, WorldPoint,
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
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Actual globe chart-pane regression")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let mut c = RenderContext::new(Viewport::with_origin(93., 91., 800., 600.));
        c.scaler.set_projection(FlatProjection::EllipsoidalMercator);
        c.set_bounds(GeoBounds::new(-20., -15., 20., 15.));
        let rect = |n: f64, color| {
            DrawingInstruction::Area(
                AreaInstruction::new(vec![
                    WorldPoint::new(-n, -n),
                    WorldPoint::new(n, -n),
                    WorldPoint::new(n, n),
                    WorldPoint::new(-n, n),
                ])
                .with_solid_fill(color),
            )
        };
        c.add_instruction(rect(5., Color::rgba(0., 0.7, 0.2, 1.)));
        let mut parent = DrawingInstruction::Point(PointInstruction::new(
            "UNSUPPORTED_PREVIEW_POINT".into(),
            WorldPoint::new(0., 0.),
        ));
        parent.set_dependency(DrawingDependency::new(
            9,
            Some("missing-resource"),
            None,
            false,
        ));
        c.add_instruction(parent);
        let mut child = rect(2., Color::rgba(1., 0., 0., 1.));
        child.set_dependency(DrawingDependency::new(
            9,
            Some("red-child"),
            Some("missing-resource"),
            false,
        ));
        c.add_instruction(child);
        let mut low = DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(-4., 0.), WorldPoint::new(4., 0.)])
                .with_priority(9),
        );
        low.set_dependency(DrawingDependency::new(
            10,
            Some("suppressed-parent"),
            None,
            false,
        ));
        c.add_instruction(low);
        c.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(-4., 0.), WorldPoint::new(4., 0.)])
                .with_priority(10),
        ));
        let mut suppressed_child = rect(2., Color::rgba(1., 0., 0., 1.));
        suppressed_child.set_dependency(DrawingDependency::new(
            10,
            Some("red-suppressed-child"),
            Some("suppressed-parent"),
            false,
        ));
        c.add_instruction(suppressed_child);
        r.begin_frame();
        r.add_instructions_with_symbols(&mut c, None, None, None);
        r.save_screenshot(self.out.join("flat-before.png")).unwrap();
        r.ui_state.globe_preview = true;
        r.ui_state.globe_range_factor = 5.;
        let mut rows = Vec::new();
        for tilt in [0., 45.] {
            r.ui_state.globe_tilt_deg = tilt;
            r.begin_frame();
            r.prepare_globe_preview(&mut c, None).unwrap();
            assert!(
                !r.set_gpu_view_scaler(&c.scaler),
                "perspective cannot use flat ScreenAffine"
            );
            assert!(r.geometry_matches_view(&c.scaler));
            let v = c.scaler.viewport;
            let centre = r.globe_world_at(v.center()).unwrap();
            assert!(
                centre.x.abs() < 1e-7 && centre.y.abs() < 1e-7,
                "chart-pane centre must hit geographic focus: {centre:?}"
            );
            assert!(
                r.globe_world_at(ScreenPoint::new(v.x - 1., v.y + v.height / 2.))
                    .is_none(),
                "side panel must not pick Earth"
            );
            assert!(
                r.globe_world_at(ScreenPoint::new(v.x + 1., v.y + 1.))
                    .is_none(),
                "space must not return flat geographic coordinates"
            );
            let d = r.globe_preview_diagnostics().unwrap();
            assert_eq!(d.areas, 1);
            assert_eq!(d.unsupported_commands, 0);
            assert_eq!(d.missing_symbol_resources, 1);
            assert_eq!(d.symbols, 0);
            let row = d.as_json();
            let path = self.out.join(format!("globe-tilt{tilt}.png"));
            r.save_screenshot(&path).unwrap();
            let image = image::open(&path).unwrap().to_rgb8();
            assert_eq!(
                image.get_pixel(0, 0).0,
                [255, 255, 255],
                "globe pass escaped chart scissor"
            );
            let green = image
                .pixels()
                .filter(|p| p[1] as u16 > p[0] as u16 + 20 && p[1] as u16 > p[2] as u16 + 20)
                .count();
            assert!(green > 20, "supported area absent from actual composite");
            let red = image
                .pixels()
                .filter(|p| p[0] > 200 && p[1] < 50 && p[2] < 50)
                .count();
            assert_eq!(red, 0, "unsupported parent activated child area");
            rows.push(serde_json::json!({"tilt":tilt,"diagnostics":row,"green_pixels":green,"red_pixels":red,"centre_ray_lon_lat":[centre.x,centre.y],"space_and_side_panel_miss":true,"perspective_affine_rejected":true}));
        }
        let mut resource_rows = Vec::new();
        let cases = [
            (0., 93., 91., 800., 600., 5.),
            (45., 93., 91., 800., 600., 5.),
            (70., 93., 91., 800., 600., 4.),
            (45., 121., 105., 780., 580., 5.),
            (0., 141., 115., 780., 580., 5.),
            (0., 93., 91., 800., 600., 5.),
        ];
        for (case, (tilt, x, y, w, h, range)) in cases.iter().copied().enumerate() {
            c.set_viewport_rect(x, y, w, h);
            r.ui_state.globe_pose = None;
            r.ui_state.globe_tilt_deg = tilt;
            r.ui_state.globe_range_factor = range;
            r.prepare_globe_preview(&mut c, None).unwrap();
            let first = r.globe_preview_diagnostics().unwrap().resources.clone();
            let mut timings = Vec::new();
            for _ in 0..8 {
                let start = std::time::Instant::now();
                r.prepare_globe_preview(&mut c, None).unwrap();
                timings.push(start.elapsed().as_secs_f64() * 1000.);
                let usage = &r.globe_preview_diagnostics().unwrap().resources;
                for key in [
                    "pipeline_creations",
                    "buffer_allocations",
                    "depth_allocations",
                    "msaa_allocations",
                    "color_allocations",
                    "composite_buffer_allocations",
                    "quad_writes",
                    "earth_mesh_builds",
                    "cpu_clip_capacity_bytes",
                    "cpu_index_capacity_bytes",
                ] {
                    assert_eq!(first[key], usage[key], "steady view allocated {key}");
                }
            }
            r.save_screenshot(self.out.join(format!("warm-{case}.png")))
                .unwrap();
            resource_rows.push(serde_json::json!({"case":case,"resources":r.globe_preview_diagnostics().unwrap().resources,"cpu_prepare_and_enqueue_ms":timings,"steady_frame_new_tracked_gpu_allocations":0}));
        }
        // A failed preparation must not keep the previous target/picking live.
        let before_failure = r.globe_preview_diagnostics().unwrap().resources.clone();
        c.set_viewport_rect(93., 91., 0., 600.);
        assert!(r.prepare_globe_preview(&mut c, None).is_err());
        assert!(r.globe_preview_diagnostics().is_none());
        assert!(r.globe_world_at(ScreenPoint::new(493., 391.)).is_none());
        c.set_viewport_rect(93., 91., 800., 600.);
        r.prepare_globe_preview(&mut c, None).unwrap();
        for key in [
            "buffer_allocations",
            "color_allocations",
            "depth_allocations",
            "msaa_allocations",
            "pipeline_creations",
        ] {
            assert_eq!(
                before_failure[key],
                r.globe_preview_diagnostics().unwrap().resources[key]
            );
        }
        for (case, (tilt, x, y, w, h, range)) in cases.iter().copied().enumerate() {
            r.clear_globe_preview();
            c.set_viewport_rect(x, y, w, h);
            r.ui_state.globe_tilt_deg = tilt;
            r.ui_state.globe_range_factor = range;
            let start = std::time::Instant::now();
            r.prepare_globe_preview(&mut c, None).unwrap();
            let cold_ms = start.elapsed().as_secs_f64() * 1000.;
            let usage = &r.globe_preview_diagnostics().unwrap().resources;
            assert_eq!(usage["frames"], 1);
            assert_eq!(usage["color_allocations"], 1);
            assert_eq!(usage["depth_allocations"], 1);
            assert_eq!(usage["msaa_allocations"], 1);
            assert_eq!(usage["sample_count"], 4);
            assert_eq!(usage["buffer_allocations"], 2);
            let path = self.out.join(format!("cold-{case}.png"));
            r.save_screenshot(&path).unwrap();
            let warm = image::open(self.out.join(format!("warm-{case}.png")))
                .unwrap()
                .to_rgba8();
            let cold = image::open(path).unwrap().to_rgba8();
            assert_eq!(warm, cold, "warm/cold changed case {case} pixels");
            resource_rows[case]["warm_cold_changed_pixels"] = 0.into();
            resource_rows[case]["cold_cpu_prepare_and_enqueue_ms"] = cold_ms.into();
        }
        c.set_viewport_rect(93., 91., 800., 600.);
        r.ui_state.globe_preview = false;
        r.clear_globe_preview();
        r.begin_frame();
        r.add_instructions_with_symbols(&mut c, None, None, None);
        r.save_screenshot(self.out.join("flat-restored.png"))
            .unwrap();
        let before = image::open(self.out.join("flat-before.png"))
            .unwrap()
            .to_rgba8();
        let after = image::open(self.out.join("flat-restored.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(before, after, "Flat -> Globe -> Flat changed chart pixels");
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rows":rows,"resource_rows":resource_rows,"failure_invalidated_picking":true,"resource_release_on_flat":true,"flat_restoration_pixel_difference":0,"native_renderer_integration":true,"physical_input_verified":false})).unwrap()).unwrap();
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
