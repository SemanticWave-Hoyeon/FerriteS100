//! Actual GPU regression for partly coincident strokes and long-line clipping.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, RenderContext, Viewport,
    WorldPoint,
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
struct Smoke {
    output: PathBuf,
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Line visibility regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let pixels_per_mm = 96.0 / 25.4 * window.scale_factor() as f32;
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(1., 5.), WorldPoint::new(9., 5.)])
                .with_priority(2)
                .with_style(LineStyle::solid(Color::rgb(0., 0., 1.), 12.)),
        ));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![
                WorldPoint::new(3., 5.),
                WorldPoint::new(5., 5.),
                WorldPoint::new(7., 5.),
            ])
            .with_priority(8)
            .with_style(LineStyle::solid(Color::rgb(1., 0., 0.), 4.)),
        ));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(1., 7.), WorldPoint::new(9., 7.)])
                .with_priority(8)
                .with_style(LineStyle::solid_mm(Color::rgb(0., 1., 0.), 1.)),
        ));
        let mm_probe = ctx.scaler.world_to_screen(WorldPoint::new(5., 7.));
        renderer.begin_frame();
        renderer.add_instructions(&mut ctx);
        let source = ctx.scaler.world_to_screen(WorldPoint::new(2., 5.));
        let overlap = ctx.scaler.world_to_screen(WorldPoint::new(5., 5.));
        let pivot = (size.width as f32 / 2., size.height as f32 / 2.);
        std::fs::create_dir_all(&self.output).unwrap();
        let mut rows = Vec::new();
        for (name, zoom) in [
            ("line-0.5.png", 0.5),
            ("line-1.png", 1.),
            ("line-2.png", 2.),
        ] {
            renderer.set_gpu_zoom(zoom, pivot.0, pivot.1);
            renderer.save_screenshot(self.output.join(name)).unwrap();
            rows.push(serde_json::json!({"file":name,"zoom":zoom,"source":[source.x,source.y],"overlap":[overlap.x,overlap.y],"pivot":[pivot.0,pivot.1],"size":[size.width,size.height],"mm_probe":[mm_probe.x,mm_probe.y],"mm_width_pixels":pixels_per_mm,"line_visibility_counts":renderer.line_visibility_counts()}));
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
    let output = PathBuf::from(std::env::args().nth(1).expect("Output directory"));
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
