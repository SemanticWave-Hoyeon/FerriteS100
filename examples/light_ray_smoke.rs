//! GPU proof: fixed-size dashed light sector ray is independent of fast zoom.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, RenderContext, ScreenRay,
    Viewport, WorldPoint,
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
                    .with_inner_size(PhysicalSize::new(960, 640))
                    .with_title("Light sector ray units"),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(-5., -5., 5., 5.));
        let mut line = LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(0., 0.)]);
        line.screen_ray = Some(ScreenRay {
            direction: 90.,
            length_mm: 25.,
            geographic_direction: true,
        });
        line.style = LineStyle::solid_mm(Color::rgb(0., 0., 1.), 0.32);
        line.style.dash_pattern = vec![3.6, 1.8];
        c.add_instruction(DrawingInstruction::Line(line));
        r.begin_frame();
        r.add_instructions(&mut c);
        let origin = c.scaler.world_to_screen(WorldPoint::new(0., 0.));
        let mm = 96. / 25.4 * density;
        std::fs::create_dir_all(&self.out).unwrap();
        let mut baseline = None;
        let mut cases = Vec::new();
        for zoom in [0.5, 1., 2.] {
            r.set_gpu_zoom(zoom, origin.x, origin.y);
            let p = self.out.join(format!("ray-{zoom}.png"));
            r.save_screenshot(&p).unwrap();
            let im = image::open(&p).unwrap().to_rgb8();
            for (m, blue) in [
                (2., true),
                (4.2, false),
                (6., true),
                (9.6, false),
                (12., true),
                (26., false),
            ] {
                let pixel = im
                    .get_pixel(
                        (origin.x + (m * mm) as f32).round() as u32,
                        origin.y.round() as u32,
                    )
                    .0;
                assert_eq!(
                    pixel,
                    if blue { [0, 0, 255] } else { [255, 255, 255] },
                    "zoom{zoom}: {m}mm"
                );
            }
            let blue: Vec<_> = im
                .enumerate_pixels()
                .filter(|(_, _, p)| p.0[2] > 200 && p.0[0] < 100 && p.0[1] < 100)
                .map(|(x, y, _)| (x, y))
                .collect();
            let minx = blue.iter().map(|p| p.0).min().unwrap();
            let maxx = blue.iter().map(|p| p.0).max().unwrap();
            assert!(((maxx - minx + 1) as f64 - 25. * mm).abs() < 2.);
            if let Some(b) = &baseline {
                assert_eq!(&im, b, "Fixed ray must not stretch at fast zoom");
            } else {
                baseline = Some(im.clone());
            }
            r.render().unwrap();
            let after = self.out.join(format!("ray-{zoom}-after.png"));
            r.save_screenshot(&after).unwrap();
            assert_eq!(im, image::open(&after).unwrap().to_rgb8());
            cases.push(serde_json::json!({"zoom":zoom,"blue_width_px":maxx-minx+1,"physical_length_mm":25,"dash_probes":6,"surface_export_equal":true}));
        }
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"density":density,"cases":cases,"all_zoom_images_equal":true,"native_os_input_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: std::env::args().nth(1).unwrap().into(),
        })
        .unwrap();
}
