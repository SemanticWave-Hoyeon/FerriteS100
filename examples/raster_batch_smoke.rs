use ferrite_render::{Color, GeoBounds, RasterGrid, RasterLayer, RenderContext, Viewport};
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
    out: PathBuf,
}
fn layer(color: [u8; 4]) -> RasterLayer {
    RasterLayer {
        viewing_groups: Vec::new(),
        draw_order: Default::default(),
        grid: None,
        id: "transaction-fixture".into(),
        width: 2,
        height: 2,
        rgba: color.repeat(4),
        bounds: GeoBounds::new(2., 2., 8., 8.),
    }
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(
            el.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        r.begin_frame();
        r.add_instructions(&mut c);
        r.add_raster_layer(layer([255, 0, 0, 255]), &c.scaler)
            .unwrap();
        std::fs::create_dir_all(&self.out).unwrap();
        let base = self.out.join("before.png");
        r.save_screenshot(&base).unwrap();
        let original = image::open(base).unwrap().to_rgb8();
        for replace in [false, true] {
            for invalid_grid in [false, true] {
                let result: Result<(), anyhow::Error> =
                    r.raster_batch(&c.scaler, replace, |upload| {
                        upload(layer([0, 0, 255, 255]))?;
                        let mut bad = layer([0, 255, 0, 255]);
                        if invalid_grid {
                            bad.grid = Some(RasterGrid {
                                bounds: bad.bounds,
                                width: 2,
                                height: 2,
                                column: 1,
                                row: 0,
                            });
                        } else {
                            bad.width = 0;
                        }
                        upload(bad)?;
                        Ok(())
                    });
                assert!(result.is_err());
                let f = self
                    .out
                    .join(format!("failed-{replace}-{invalid_grid}.png"));
                r.save_screenshot(&f).unwrap();
                assert!(
                    original == image::open(f).unwrap().to_rgb8(),
                    "Failed batch replaced visible data"
                );
            }
        }
        r.raster_batch(&c.scaler, true, |upload| -> Result<(), anyhow::Error> {
            upload(layer([0, 0, 255, 255]))?;
            Ok(())
        })
        .unwrap();
        let success = self.out.join("success.png");
        r.save_screenshot(&success).unwrap();
        let image = image::open(success).unwrap().to_rgb8();
        let p = c
            .scaler
            .world_to_screen(ferrite_render::WorldPoint::new(5., 5.));
        assert_eq!(image.get_pixel(p.x as u32, p.y as u32).0, [0, 0, 255]);
        r.render().unwrap();
        let f = self.out.join("after-surface.png");
        r.save_screenshot(&f).unwrap();
        assert!(image == image::open(f).unwrap().to_rgb8());
        std::fs::write(
            self.out.join("result.json"),
            "{\"rollback_cases\":4,\"successful_replace\":true,\"surface_export_equal\":true}",
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke {
            out: std::env::args().nth(1).unwrap().into(),
        })
        .unwrap();
}
