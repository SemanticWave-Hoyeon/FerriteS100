//! Exercise product-neutral coverage quads on the native GPU at three zoom levels.
use ferrite_render::{Color, GeoBounds, RasterLayer, RenderContext, Viewport, WorldPoint};
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
                    .with_title("Coverage zoom regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 1., 1.));
        let bounds = GeoBounds::new(0.25, 0.25, 0.75, 0.75);
        let a = ctx
            .scaler
            .world_to_screen(WorldPoint::new(bounds.min_x, bounds.max_y));
        let b = ctx
            .scaler
            .world_to_screen(WorldPoint::new(bounds.max_x, bounds.min_y));
        renderer
            .add_raster_layer(
                RasterLayer {
                    viewing_groups: Vec::new(),
                    draw_order: Default::default(),
                    grid: None,
                    id: "four-cell-test".into(),
                    bounds,
                    width: 2,
                    height: 2,
                    rgba: vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 0],
                },
                &ctx.scaler,
            )
            .unwrap();
        std::fs::create_dir_all(&self.output).unwrap();
        let pivot = (size.width as f32 / 2., size.height as f32 / 2.);
        for (name, zoom) in [
            ("raster-0.5.png", 0.5),
            ("raster-1.png", 1.),
            ("raster-2.png", 2.),
        ] {
            renderer.set_gpu_zoom(zoom, pivot.0, pivot.1);
            renderer.save_screenshot(self.output.join(name)).unwrap();
        }
        std::fs::write(self.output.join("metadata.json"),serde_json::to_string_pretty(&serde_json::json!({"size":[size.width,size.height],"corner_a":[a.x,a.y],"corner_b":[b.x,b.y],"pivot":[pivot.0,pivot.1]})).unwrap()).unwrap();
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
