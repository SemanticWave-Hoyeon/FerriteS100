//! Native GPU oracle for longitude-wrapped text and source-point culling.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, HAlign, RenderContext, TextInstruction, VAlign, Viewport,
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
fn red_count(image: &image::RgbImage, x: f32, y: f32) -> usize {
    let mut count = 0;
    for yy in (y as i32 - 70).max(0)..(y as i32 + 70).min(image.height() as i32) {
        for xx in (x as i32 - 85).max(0)..(x as i32 + 85).min(image.width() as i32) {
            let c = image.get_pixel(xx as u32, yy as u32).0;
            count += usize::from(c[0] > 180 && c[1] < 100 && c[2] < 100);
        }
    }
    count
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Longitude text regression")
                    .with_inner_size(PhysicalSize::new(1920, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        r.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.output).unwrap();
        let mut checks = Vec::new();
        for (name, bounds, source, copies) in [
            (
                "wide",
                GeoBounds::new(0., 0., 1080., 10.),
                510.,
                vec![150., 510., 870.],
            ),
            (
                "dateline",
                GeoBounds::new(350., 0., 370., 10.),
                0.,
                vec![360.],
            ),
            (
                "left-copy",
                GeoBounds::new(-370., 0., -350., 10.),
                0.,
                vec![-360.],
            ),
        ] {
            let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            ctx.set_bounds(bounds);
            ctx.add_instruction(DrawingInstruction::Text(
                TextInstruction::new("ABCD".into(), WorldPoint::new(source, 5.))
                    .with_font_size(24.)
                    .with_color(Color::rgb(1., 0., 0.))
                    .with_alignment(HAlign::Center, VAlign::Middle)
                    .with_rotation(15.)
                    .with_offset(1.2, 0.8),
            ));
            r.set_lon_wrap_pixels(360. * ctx.scaler.scale_x() as f32);
            for zoom in [0.5f32, 1., 2.] {
                r.reset_pan_offset();
                r.set_gpu_zoom(1., 0., 0.);
                r.begin_frame();
                r.add_instructions(&mut ctx);
                r.set_pan_offset(24., -12.);
                r.set_gpu_zoom(zoom, size.width as f32 / 2., size.height as f32 / 2.);
                let path = self.output.join(format!("{name}-{zoom}.png"));
                r.save_screenshot(&path).unwrap();
                let image = image::open(&path).unwrap().to_rgb8();
                for x in &copies {
                    let p = ctx.scaler.world_to_screen(WorldPoint::new(*x, 5.));
                    let x = (p.x + 24. - size.width as f32 / 2.) * zoom + size.width as f32 / 2.;
                    let y = (p.y - 12. - size.height as f32 / 2.) * zoom + size.height as f32 / 2.;
                    if x < 85. || x > size.width as f32 - 85. {
                        continue;
                    }
                    let count = red_count(&image, x, y);
                    assert!(
                        count > 25,
                        "{name} zoom {zoom} copy {x}: missing wrapped glyphs ({count})"
                    );
                    checks.push(serde_json::json!({"case":name,"zoom":zoom,"copy_screen":[x,y],"red_glyph_pixels":count}));
                }
                r.render().unwrap();
                let after = self.output.join(format!("{name}-{zoom}-after-surface.png"));
                r.save_screenshot(&after).unwrap();
                assert_eq!(image, image::open(after).unwrap().to_rgb8());
            }
        }
        std::fs::write(self.output.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"glyph_probes":checks,"surface_export_pairs":9,"native_mouse_and_menu_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).unwrap());
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
