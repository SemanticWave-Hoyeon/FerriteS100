//! Actual GPU oracle: large pattern footprints against bounded coverage at 200x.
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, DrawingInstruction, FlatProjection, GeoBounds, RenderContext,
    ScreenPoint, Viewport, WorldPoint,
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
}
fn ring(c: &RenderContext, rect: [f32; 4]) -> Vec<WorldPoint> {
    [
        [rect[0], rect[1]],
        [rect[2], rect[1]],
        [rect[2], rect[3]],
        [rect[0], rect[3]],
    ]
    .map(|p| c.scaler.screen_to_world(ScreenPoint::new(p[0], p[1])))
    .to_vec()
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(1000, 800))
                    .with_title("Large pattern coverage oracle"),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let density = w.scale_factor();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let pc = PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let viewport = Viewport::new(size.width as f32, size.height as f32);
        let base = GeoBounds::new(-1., 47., 1., 51.);
        let mut rows = Vec::new();
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for zoom in [1., 25., 200.] {
                for shift in [0., 360.] {
                    let name = format!("{:?}-{zoom}-{shift}", projection);
                    let mut c = RenderContext::new(viewport);
                    c.scaler.set_projection(projection);
                    c.set_bounds(projection.view_bounds(base, zoom, [0., 0.]).unwrap());
                    r.set_lon_wrap_pixels((360. * c.scaler.scale_x()) as f32);
                    let hole = ring(&c, [420., 320., 580., 480.]);
                    let mut large = AreaInstruction::new(vec![
                        WorldPoint::new(-170. + shift, 0.),
                        WorldPoint::new(170. + shift, 0.),
                        WorldPoint::new(170. + shift, 85.),
                        WorldPoint::new(-170. + shift, 85.),
                    ])
                    .with_pattern_fill("FOULAR01P".into(), (5., 0.), (2., 6.));
                    large.interiors.push(
                        hole.iter()
                            .map(|p| WorldPoint::new(p.x + shift, p.y))
                            .collect(),
                    );
                    let mut reference =
                        AreaInstruction::new(ring(&c, [-1000., -1000., 2000., 1800.]))
                            .with_pattern_fill("FOULAR01P".into(), (5., 0.), (2., 6.));
                    reference.interiors.push(hole);
                    let mut images = Vec::new();
                    for (label, area) in [("large", large), ("reference", reference)] {
                        c.clear_instructions();
                        let offset = if label == "large" { shift } else { 0. };
                        let underlay = AreaInstruction::new(
                            ring(&c, [64., 96., 160., 192.])
                                .into_iter()
                                .map(|p| WorldPoint::new(p.x + offset, p.y))
                                .collect(),
                        )
                        .with_solid_fill(Color::rgb(0., 1., 0.));
                        c.add_instruction(DrawingInstruction::Area(underlay));
                        c.add_instruction(DrawingInstruction::Area(area));
                        r.reset_pan_offset();
                        r.begin_frame();
                        r.add_instructions_with_symbols(
                            &mut c,
                            Some(&mut cache),
                            Some(profile),
                            None,
                        );
                        if label == "large" {
                            assert!(r.requires_visibility_rebuild_for_navigation());
                            assert!(!r.set_gpu_view_scaler(&c.scaler));
                        }
                        let path = self.out.join(format!("{name}-{label}.png"));
                        r.save_screenshot(&path).unwrap();
                        images.push(image::open(path).unwrap().to_rgb8());
                    }
                    let a = &images[0];
                    let b = &images[1];
                    let mut changed = 0;
                    let mut max_delta = 0;
                    for (a, b) in a.pixels().zip(b.pixels()) {
                        if a != b {
                            changed += 1;
                        }
                        for i in 0..3 {
                            max_delta = max_delta.max(a[i].abs_diff(b[i]));
                        }
                    }
                    assert!(
                        max_delta <= 2,
                        "pattern mismatch {name}: changed{changed},max{max_delta}"
                    );
                    let colored = a.pixels().filter(|p| p.0 != [255, 255, 255]).count();
                    assert!(colored > 1000);
                    for x in 480..520 {
                        for y in 380..420 {
                            assert_eq!(a.get_pixel(x, y).0, [255, 255, 255], "hole filled");
                        }
                    }
                    rows.push(serde_json::json!({"projection":format!("{:?}",projection),"zoom":zoom,"source_longitude_shift":shift,"changed_pixels":changed,"max_channel_difference":max_delta,"pattern_pixels":colored,"hole_probes":1600,"clipped_navigation_rebuilt":true,"native_density":density}));
                }
            }
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"rows":rows,"physical_input_verified":false,"bounded_reference_actual_gpu":true})).unwrap()).unwrap();
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
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: a.next().unwrap().into(),
            pc: a.next().unwrap().into(),
        })
        .unwrap();
}
