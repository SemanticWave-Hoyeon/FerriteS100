//! Native fill oracle for a concavity, an interior hole and rejected geometry.
use ferrite_render::{
    AreaFillType, AreaInstruction, Color, DrawingInstruction, FlatProjection, GeoBounds,
    RenderContext, ScreenPoint, Viewport, WorldPoint,
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
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Polygon hole coverage oracle")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        renderer.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.out).unwrap();
        let mut rows = Vec::new();
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for reverse in [false, true] {
                let mut ctx =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                ctx.scaler.set_projection(projection);
                ctx.set_bounds(GeoBounds::new(-1., 59., 11., 81.));
                let ring = |pairs: &[[f64; 2]]| {
                    pairs
                        .iter()
                        .map(|p| WorldPoint::new(p[0], p[1]))
                        .collect::<Vec<_>>()
                };
                let mut area = AreaInstruction::new(ring(&[
                    [0., 60.],
                    [10., 60.],
                    [10., 70.],
                    [4., 70.],
                    [4., 80.],
                    [0., 80.],
                    [0., 60.],
                ]));
                area.interiors.push(ring(&[
                    [1., 61.],
                    [2., 61.],
                    [2., 62.],
                    [1., 62.],
                    [1., 61.],
                ]));
                if reverse {
                    area.exterior.reverse();
                    area.interiors[0].reverse();
                }
                area.fill = AreaFillType::Solid(Color::rgb(0., 1., 0.));
                ctx.add_instruction(DrawingInstruction::Area(area));
                renderer.begin_frame();
                renderer.add_instructions(&mut ctx);
                assert_eq!(renderer.rejected_area_fill_count(), 0);
                let prefix = format!("{projection:?}-{reverse}");
                let path = self.out.join(format!("{prefix}.png"));
                renderer.save_screenshot(&path).unwrap();
                let img = image::open(path).unwrap().to_rgb8();
                let filled = |p: WorldPoint| {
                    p.x > 0.
                        && p.x < 10.
                        && p.y > 60.
                        && p.y < 80.
                        && !(p.x > 4. && p.y > 70.)
                        && !(p.x > 1. && p.x < 2. && p.y > 61. && p.y < 62.)
                };
                let (mut probes, mut inside, mut hole_probes) = (0, 0, 0);
                for y in (3..size.height - 3).step_by(3) {
                    for x in (3..size.width - 3).step_by(3) {
                        let at = |dx, dy| {
                            ctx.scaler.screen_to_world(ScreenPoint::new(
                                x as f32 + 0.5 + dx,
                                y as f32 + 0.5 + dy,
                            ))
                        };
                        let p = at(0., 0.);
                        let expected = filled(p);
                        if ![at(-2., -2.), at(-2., 2.), at(2., -2.), at(2., 2.)]
                            .into_iter()
                            .all(|q| filled(q) == expected)
                        {
                            continue;
                        }
                        probes += 1;
                        if expected {
                            inside += 1;
                        }
                        if p.x > 1. && p.x < 2. && p.y > 61. && p.y < 62. {
                            hole_probes += 1;
                        }
                        let color = img.get_pixel(x, y);
                        assert_eq!(
                            color[0] < 50 && color[1] > 200 && color[2] < 50,
                            expected,
                            "Coverage mismatch at {x},{y} ({},{})",
                            p.x,
                            p.y
                        );
                    }
                }
                assert!(inside > 100 && hole_probes > 0);
                ctx.clear_instructions();
                let mut invalid =
                    AreaInstruction::new(ring(&[[0., 60.], [10., 60.], [10., 80.], [0., 80.]]));
                invalid.fill = AreaFillType::Solid(Color::rgb(0., 1., 0.));
                invalid
                    .interiors
                    .push(ring(&[[1., 61.], [1., 61.], [1., 61.]]));
                ctx.add_instruction(DrawingInstruction::Area(invalid));
                for _ in 0..2 {
                    renderer.begin_frame();
                    renderer.add_instructions(&mut ctx);
                    assert_eq!(renderer.rejected_area_fill_count(), 1);
                }
                let path = self.out.join(format!("{prefix}-rejected.png"));
                renderer.save_screenshot(&path).unwrap();
                let bad = image::open(path).unwrap().to_rgb8();
                assert!(bad.pixels().all(|p| p.0 == [255, 255, 255]));
                rows.push(serde_json::json!({"projection":format!("{projection:?}"),"reverse":reverse,"probes":probes,
      "filled_probes":inside,"hole_probes":hole_probes,"rejected_area_count":renderer.rejected_area_fill_count()}));
            }
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("Output directory"));
    EventLoop::new().unwrap().run_app(&mut App { out }).unwrap();
}
