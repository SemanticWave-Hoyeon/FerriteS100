//! GPU proof: fixed-size three-point arc is independent of fast zoom.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, PortrayalPath, RenderContext,
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
        line.portrayal_path = Some(PortrayalPath::Group(vec![
            PortrayalPath::Polyline(vec![(0., 0.), (4., 0.)]),
            PortrayalPath::Polyline(vec![(4., 0.), (10., 0.)]),
            PortrayalPath::Polyline(vec![(20., 0.), (24., 0.)]),
            PortrayalPath::Polyline(vec![(24., 0.), (30., 0.)]),
        ]));
        line.style = LineStyle::solid_mm(Color::rgb(0., 0., 1.), 0.32);
        line.style.dash_cycle =
            Some(ferrite_kernel::DashCycle::new(10., [(2., 2.), (6., 1.)]).unwrap());
        c.add_instruction(DrawingInstruction::Line(line));
        let mut ring = LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(0., 0.)]);
        ring.portrayal_path = Some(PortrayalPath::Group(
            [15., 10.]
                .into_iter()
                .map(|radius| PortrayalPath::Arc {
                    center: (0., 20.),
                    radius,
                    start: 0.,
                    sweep: 360.,
                    geographic_angle: false,
                })
                .collect(),
        ));
        ring.style = LineStyle::solid_mm(Color::rgb(0., 1., 0.), 0.32);
        c.add_instruction(DrawingInstruction::Line(ring));
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
            // A 0.32 mm line can cover only part of an MSAA pixel at density 1.
            // Detect its independently known blue/green tint, retaining gap,
            // physical extent and exact zoom/export equality checks.
            for (x, y, blue) in [
                (1., 0., false),
                (2.5, 0., true),
                (3.5, 0., true),
                (4.5, 0., false),
                (6.5, 0., true),
                (8., 0., false),
                (15., 0., false),
                (21., 0., false),
                (22.5, 0., true),
                (26.5, 0., true),
            ] {
                let px = (origin.x + (x * mm) as f32).round() as i32;
                let py = (origin.y - (y * mm) as f32).round() as i32;
                let found = (-1..=1).any(|dy| {
                    (-1..=1).any(|dx| {
                        let p = im.get_pixel((px + dx) as u32, (py + dy) as u32).0;
                        p[2] as i16 - p[0] as i16 > 16 && p[0] == p[1]
                    })
                });
                assert_eq!(found, blue, "zoom{zoom} at {x},{y}");
            }
            for (y, green) in [(35., true), (32.5, false), (30., true), (20., false)] {
                let px = origin.x.round() as i32;
                let py = (origin.y - (y * mm) as f32).round() as i32;
                let found = (-1..=1).any(|dy| {
                    (-1..=1).any(|dx| {
                        let p = im.get_pixel((px + dx) as u32, (py + dy) as u32).0;
                        p[1] as i16 - p[0] as i16 > 16 && p[0] == p[2]
                    })
                });
                assert_eq!(found, green, "ring zoom{zoom}: {y}mm");
            }
            let blue: Vec<_> = im
                .enumerate_pixels()
                .filter(|(_, _, p)| p.0[2] as i16 - p.0[0] as i16 > 16 && p.0[0] == p.0[1])
                .map(|(x, y, _)| (x, y))
                .collect();
            let minx = blue.iter().map(|p| p.0).min().unwrap();
            let maxx = blue.iter().map(|p| p.0).max().unwrap();
            assert!(((maxx - minx + 1) as f64 - 25. * mm).abs() < 2.);
            if let Some(b) = &baseline {
                assert_eq!(&im, b, "Fixed arc must not stretch at fast zoom");
            } else {
                baseline = Some(im.clone());
            }
            r.render().unwrap();
            let after = self.out.join(format!("ray-{zoom}-after.png"));
            r.save_screenshot(&after).unwrap();
            assert_eq!(im, image::open(&after).unwrap().to_rgb8());
            cases.push(serde_json::json!({"zoom":zoom,"blue_width_px":maxx-minx+1,"run_lengths_mm":[10,10],"gap_mm":10,"dash_probes":10,"ring_probes":4,"surface_export_equal":true}));
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
