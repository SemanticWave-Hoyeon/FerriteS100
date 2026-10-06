//! Actual GPU regression for geometry allocation reuse across chart contexts.
use ferrite_render::{
    AreaInstruction, Color, DrawingInstruction, GeoBounds, RenderContext, Viewport, WorldPoint,
};
use ferrite_wgpu::WgpuRenderer;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct Smoke {
    output: PathBuf,
}
fn ring(x: f64, y: f64, w: f64, h: f64) -> Vec<WorldPoint> {
    vec![
        WorldPoint::new(x, y),
        WorldPoint::new(x + w, y),
        WorldPoint::new(x + w, y + h),
        WorldPoint::new(x, y + h),
    ]
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Geometry cache lifetime regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.output).unwrap();
        let mut previous = HashMap::new();
        let mut rows = Vec::new();
        for step in 0..32 {
            let shape = step % 4;
            let x = if shape % 2 == 0 { 1. } else { 6. };
            let hole_y = if shape < 2 { 2. } else { 6. };
            let area = AreaInstruction::new(ring(x, 1., 3., 8.))
                .with_interiors(vec![ring(x + 0.5, hole_y, 2., 2.)])
                .with_solid_fill_token(Color::rgb(0., 0., 1.), "TEST_FILL");
            let pointer = area.exterior.as_ptr() as usize;
            let reused_for_different_shape = previous
                .insert(pointer, shape)
                .is_some_and(|prior| prior != shape);
            let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
            ctx.add_instruction(DrawingInstruction::Area(area));
            renderer.precompute_triangulations(&ctx);
            let revision = ctx.geometry_revision();
            for warm in [false, true] {
                if warm {
                    ctx.remap_colors(&|_| Color::rgb(0., 1., 0.));
                }
                assert_eq!(revision, ctx.geometry_revision());
                renderer.begin_frame();
                renderer.add_instructions(&mut ctx);
                let file = format!("scene-{step:02}-{}.png", if warm { "warm" } else { "cold" });
                renderer.save_screenshot(self.output.join(&file)).unwrap();
                let image = image::open(self.output.join(&file)).unwrap().to_rgb8();
                for (label, world, expected) in [
                    (
                        "fill",
                        WorldPoint::new(x + 0.25, 5.),
                        if warm { [0, 255, 0] } else { [0, 0, 255] },
                    ),
                    (
                        "hole",
                        WorldPoint::new(x + 1.5, hole_y + 1.),
                        [255, 255, 255],
                    ),
                    (
                        "opposite",
                        WorldPoint::new(if x < 5. { 8. } else { 2. }, 5.),
                        [255, 255, 255],
                    ),
                    (
                        "other_hole_position",
                        WorldPoint::new(x + 1.5, if hole_y < 5. { 7. } else { 3. }),
                        if warm { [0, 255, 0] } else { [0, 0, 255] },
                    ),
                ] {
                    let screen = ctx.scaler.world_to_screen(world);
                    assert_eq!(
                        image
                            .get_pixel(screen.x.round() as u32, screen.y.round() as u32)
                            .0,
                        expected,
                        "step {step} warm {warm} {label}"
                    );
                }
                rows.push(serde_json::json!({"step":step,"shape":shape,"warm":warm,"file":file,"geometry_revision":revision,"reused_for_different_shape":reused_for_different_shape,"pixel_probes_passed":4}));
            }
        }
        std::fs::write(
            self.output.join("metadata.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
