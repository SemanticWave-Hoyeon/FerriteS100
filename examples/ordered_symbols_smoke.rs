//! Native overlapping A,B,A sprites must retain portrayal order through texture batching.
use ferrite_render::*;
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
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let dir = self.out.join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, color) in [
            ("ORDER_A", "#ff0000"),
            ("ORDER_B", "#00ff00"),
            ("ORDER_C", "#0000ff"),
        ] {
            std::fs::write(dir.join(format!("{name}.svg")),format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="10mm" height="10mm" viewBox="-5 -5 10 10"><rect x="-5" y="-5" width="10" height="10" fill="{color}"/></svg>"##)).unwrap();
        }
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Ordered sprite batching")
                    .with_inner_size(PhysicalSize::new(1200, 800)),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let density = w.scale_factor() as f32;
        let mm = (96. / 25.4) * density;
        let mut renderer = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        renderer.background_color = Color::WHITE;
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(dir);
        let mut cases = Vec::new();
        let mut baseline = None;
        for history in ["cold", "warmed", "texture-reset"] {
            if history == "warmed" {
                let mut warm =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                warm.set_bounds(GeoBounds::new(0., 0., 10., 10.));
                for (index, name) in ["ORDER_C", "ORDER_B", "ORDER_A"].iter().enumerate() {
                    warm.add_instruction(DrawingInstruction::Point(PointInstruction::new(
                        (*name).into(),
                        WorldPoint::new(1. + index as f64, 1.),
                    )));
                }
                renderer.begin_frame();
                renderer.add_instructions_with_symbols(
                    &mut warm,
                    Some(&mut cache),
                    Some(profile),
                    None,
                );
                renderer
                    .save_screenshot(self.out.join("warmup.png"))
                    .unwrap();
            }
            if history == "texture-reset" {
                renderer.clear_symbol_textures();
                cache.clear();
            }
            for zoom in [0.5, 1., 2.] {
                let mut ctx =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                ctx.set_bounds(GeoBounds::new(
                    5. - 5. / zoom,
                    5. - 5. / zoom,
                    5. + 5. / zoom,
                    5. + 5. / zoom,
                ));
                for (name, offset) in [("ORDER_A", 0.), ("ORDER_B", 0.), ("ORDER_A", 6.)] {
                    ctx.add_instruction(DrawingInstruction::Point(
                        PointInstruction::new(name.into(), WorldPoint::new(5., 5.))
                            .with_offset(offset, 0.)
                            .with_priority(24),
                    ));
                }
                renderer.begin_frame();
                renderer.add_instructions_with_symbols(
                    &mut ctx,
                    Some(&mut cache),
                    Some(profile),
                    None,
                );
                assert_eq!(renderer.displayed_symbols().len(), 3);
                let file = self.out.join(format!("{history}-{zoom}.png"));
                renderer.save_screenshot(&file).unwrap();
                let image = image::open(&file).unwrap().to_rgb8();
                assert_eq!(
                    renderer.symbol_draw_batch_count(),
                    3,
                    "A,B,A requires three ordered ranges"
                );
                let center = ctx.scaler.world_to_screen(WorldPoint::new(5., 5.));
                let sample = |dx: f32| {
                    image
                        .get_pixel((center.x + dx * mm).round() as u32, center.y.round() as u32)
                        .0
                };
                assert_eq!(
                    sample(-3.),
                    [0, 255, 0],
                    "A then B must leave green on the left"
                );
                assert_eq!(
                    sample(3.),
                    [255, 0, 0],
                    "A,B,A must leave the final red sprite above green on the right"
                );
                if let Some(ref previous) = baseline {
                    assert_eq!(
                        previous, &image,
                        "Zoom and texture history changed physical sprite pixels"
                    )
                } else {
                    baseline = Some(image.clone());
                }
                renderer.render().unwrap();
                let after = self.out.join(format!("{history}-{zoom}-surface.png"));
                renderer.save_screenshot(&after).unwrap();
                assert_eq!(image, image::open(after).unwrap().to_rgb8());
                cases.push(serde_json::json!({"history":history,"zoom":zoom,"left":[0,255,0],"right":[255,0,0],"surface_export_equal":true}));
            }
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(&cases).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
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
