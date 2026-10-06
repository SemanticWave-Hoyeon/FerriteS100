//! Native GPU check for changing off-centre scroll pivots, fast view -> rebuild.
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    anchored_zoom_bounds_projected, Color, DrawingInstruction, GeoBounds, PointInstruction,
    RenderContext, ScreenPoint, Viewport,
};
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
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
    pc: PathBuf,
    mercator: bool,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(1000, 800))
                    .with_title("Zoom anchor end continuity"),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let pc = PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let viewport = Viewport::new(size.width as f32, size.height as f32);
        let base = GeoBounds::new(-3., 47., -1., 51.);
        let mut c = RenderContext::new(viewport);
        if self.mercator {
            c.scaler
                .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
        }
        c.set_bounds(base);
        // A pre-existing panned view, then multiple scroll pivots without rebuild.
        c.set_bounds(GeoBounds::new(-2.8, 47.2, -0.8, 51.2));
        let mut rows = Vec::new();
        std::fs::create_dir_all(&self.out).unwrap();
        for (case, zoom, fx, fy) in [
            (0, 1.7, 0.15, 0.75),
            (1, 0.8, 0.8, 0.2),
            (2, 25., 0.2, 0.75),
            (3, 200., 0.8, 0.25),
        ] {
            // Official symbol positioned at the invariant cursor anchor for this case.
            let pivot = ScreenPoint::new(viewport.width * fx, viewport.height * fy);
            let anchor = c.scaler.screen_to_world(pivot);
            c.clear_instructions();
            c.add_instruction(DrawingInstruction::Point(
                PointInstruction::new("ACHBRT07".into(), anchor)
                    .with_offset(5., 3.)
                    .with_feature_id(7),
            ));
            r.reset_pan_offset();
            r.begin_frame();
            r.add_instructions_with_symbols(&mut c, Some(&mut cache), Some(profile), None);
            let before = c.scaler.clone();
            let mut camera = before.clone();
            // Retarget around a changed cursor while geometry stays at the original view.
            for (step, z) in [(0, (zoom + 1.) * 0.5), (1, zoom)] {
                let actual_pivot = if step == 0 {
                    pivot
                } else {
                    ScreenPoint::new(pivot.x + 7., pivot.y - 9.)
                };
                let actual_anchor = camera.screen_to_world(actual_pivot);
                let bounds = anchored_zoom_bounds_projected(
                    camera.projection(),
                    base,
                    viewport,
                    z,
                    actual_anchor,
                    actual_pivot,
                    camera.geo_bounds.center().y,
                )
                .unwrap();
                camera.set_bounds(bounds);
                assert!(r.set_gpu_view_scaler(&camera));
            }
            let path = self.out.join(format!("case-{case}-fast.png"));
            r.save_screenshot(&path).unwrap();
            let fast = image::open(&path).unwrap().to_rgb8();
            c.set_bounds(camera.geo_bounds);
            r.reset_pan_offset();
            r.begin_frame();
            r.add_instructions_with_symbols(&mut c, Some(&mut cache), Some(profile), None);
            let path = self.out.join(format!("case-{case}-rebuilt.png"));
            r.save_screenshot(&path).unwrap();
            let rebuilt = image::open(path).unwrap().to_rgb8();
            let bbox = |im: &image::RgbImage| {
                let pts: Vec<_> = im
                    .enumerate_pixels()
                    .filter(|(_, _, p)| p.0 != [255, 255, 255])
                    .map(|(x, y, _)| (x, y))
                    .collect();
                assert!(!pts.is_empty());
                [
                    pts.iter().map(|p| p.0).min().unwrap(),
                    pts.iter().map(|p| p.1).min().unwrap(),
                    pts.iter().map(|p| p.0).max().unwrap(),
                    pts.iter().map(|p| p.1).max().unwrap(),
                ]
            };
            let a = bbox(&fast);
            let b = bbox(&rebuilt);
            let drift = a.iter().zip(b).map(|(a, b)| a.abs_diff(b)).max().unwrap();
            assert!(drift <= 1, "end jump: {a:?} vs {b:?}");
            rows.push(serde_json::json!({"projection":format!("{:?}",camera.projection()),"case":case,"zoom":zoom,"fast_bbox":a,"rebuilt_bbox":b,"max_pixel_bbox_drift":drift,"native_density":density,"changing_pivot_without_rebuild":true,"physical_os_input_verified":false}));
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
}
fn main() {
    let mut a = std::env::args().skip(1);
    let mut app = App {
        out: a.next().unwrap().into(),
        pc: a.next().unwrap().into(),
        mercator: a.next().as_deref() == Some("--mercator"),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
