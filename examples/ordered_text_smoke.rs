//! Native GPU oracle for chart glyph plane/priority ordering and deconfliction.
use ferrite_render::{
    AreaInstruction, Color, DisplayPlane, DrawingInstruction, GeoBounds, HAlign, RenderContext,
    TextInstruction, VAlign, Viewport, WorldPoint,
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
    out: PathBuf,
}
fn area(x: f64, priority: i32, plane: DisplayPlane) -> DrawingInstruction {
    DrawingInstruction::Area(
        AreaInstruction::new(vec![
            WorldPoint::new(x - 2., 4.),
            WorldPoint::new(x + 2., 4.),
            WorldPoint::new(x + 2., 6.),
            WorldPoint::new(x - 2., 6.),
        ])
        .with_solid_fill(Color::rgb(0., 0., 1.))
        .with_priority(priority)
        .with_display_plane(plane),
    )
}
fn text(x: f64, color: Color, priority: i32, plane: DisplayPlane) -> DrawingInstruction {
    DrawingInstruction::Text(
        TextInstruction::new("HH".into(), WorldPoint::new(x, 5.))
            .with_font_size(24.)
            .with_color(color)
            .with_alignment(HAlign::Center, VAlign::Middle)
            .with_priority(priority)
            .with_display_plane(plane),
    )
}
fn colored(image: &image::RgbImage, point: [f32; 2], channel: usize) -> usize {
    let mut count = 0;
    for y in (point[1] as i32 - 40).max(0)..(point[1] as i32 + 40).min(image.height() as i32) {
        for x in (point[0] as i32 - 120).max(0)..(point[0] as i32 + 120).min(image.width() as i32) {
            let c = image.get_pixel(x as u32, y as u32).0;
            if c[channel] > 180 && c[(channel + 1) % 3] < 100 && c[(channel + 2) % 3] < 100 {
                count += 1;
            }
        }
    }
    count
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Ordered chart glyph oracle")
                    .with_inner_size(PhysicalSize::new(1600, 1000)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        r.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.out).unwrap();
        let mut checks = Vec::new();
        for zoom in [0.5f32, 1., 2.] {
            let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            ctx.set_bounds(GeoBounds::new(0., 0., 20., 10.));
            // Lower-priority text is covered by the later blue area.
            ctx.add_instruction(text(
                5.,
                Color::rgb(1., 0., 0.),
                3,
                DisplayPlane::UnderRadar,
            ));
            ctx.add_instruction(area(5., 4, DisplayPlane::UnderRadar));
            // Equal-priority text is painted after the area.
            ctx.add_instruction(text(
                10.,
                Color::rgb(1., 0., 0.),
                4,
                DisplayPlane::UnderRadar,
            ));
            ctx.add_instruction(area(10., 4, DisplayPlane::UnderRadar));
            // Over-radar plane follows every under-radar priority.
            ctx.add_instruction(text(
                15.,
                Color::rgb(1., 0., 0.),
                1,
                DisplayPlane::OverRadar,
            ));
            ctx.add_instruction(area(15., 99, DisplayPlane::UnderRadar));
            // Text collision winner is the higher priority even if inserted later.
            ctx.add_instruction(text(
                10.,
                Color::rgb(0., 1., 0.),
                2,
                DisplayPlane::UnderRadar,
            ));
            r.reset_pan_offset();
            r.set_gpu_zoom(1., 0., 0.);
            r.begin_frame();
            r.add_instructions(&mut ctx);
            r.set_pan_offset(11.25, -8.5);
            r.set_gpu_zoom(zoom, size.width as f32 / 2., size.height as f32 / 2.);
            let path = self.out.join(format!("ordering-{zoom}.png"));
            r.save_screenshot(&path).unwrap();
            let image = image::open(path).unwrap().to_rgb8();
            for (x, visible) in [(5., false), (10., true), (15., true)] {
                let point = ctx.scaler.world_to_screen(WorldPoint::new(x, 5.));
                let pos = [
                    (point.x + 11.25 - size.width as f32 / 2.) * zoom + size.width as f32 / 2.,
                    (point.y - 8.5 - size.height as f32 / 2.) * zoom + size.height as f32 / 2.,
                ];
                if pos[0] < 120. || pos[0] > size.width as f32 - 120. {
                    continue;
                }
                let red = colored(&image, pos, 0);
                let green = colored(&image, pos, 1);
                assert_eq!(
                    red > 40,
                    visible,
                    "Priority/plane failure x={x}, zoom={zoom}, red={red}"
                );
                assert_eq!(green, 0, "Lower priority collision text survived");
                checks.push(serde_json::json!({"zoom":zoom,"source_x":x,"expected_text_visible":visible,"red_pixels":red,"green_pixels":green}));
            }
            r.render().unwrap();
            let after = self.out.join(format!("ordering-{zoom}-after-surface.png"));
            r.save_screenshot(&after).unwrap();
            assert!(image == image::open(after).unwrap().to_rgb8());
        }
        // A near-coincident longitude wrap must not blend the same label three times.
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.add_instruction(text(
            5.,
            Color::rgb(1., 0., 0.),
            5,
            DisplayPlane::UnderRadar,
        ));
        r.reset_pan_offset();
        r.set_gpu_zoom(1., 0., 0.);
        r.set_lon_wrap_pixels(0.);
        r.begin_frame();
        r.add_instructions(&mut ctx);
        let base = self.out.join("self-wrap-baseline.png");
        r.save_screenshot(&base).unwrap();
        r.set_lon_wrap_pixels(0.1);
        let wrapped = self.out.join("self-wrap-collision.png");
        r.save_screenshot(&wrapped).unwrap();
        assert!(
            image::open(base).unwrap().to_rgb8() == image::open(wrapped).unwrap().to_rgb8(),
            "Near-coincident copies overpainted the original glyphs"
        );
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"probes":checks,"surface_export_pairs":3,"near_coincident_copy_collision":true,"native_os_input_verified":false})).unwrap()).unwrap();
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
