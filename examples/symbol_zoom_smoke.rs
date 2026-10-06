//! Render one official symbol at three animation zoom factors on the real GPU.
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, PointInstruction, RenderContext, Viewport, WorldPoint,
};
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
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
                    .with_title("Symbol zoom regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let dpi = window.scale_factor();
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let pc = PortrayalCatalogue::load("Catalogues/PC/S-101").unwrap();
        let profile = pc
            .color_profiles
            .profiles
            .get("Day")
            .or_else(|| {
                pc.color_profiles
                    .profiles
                    .values()
                    .find(|p| p.name.contains("Day"))
            })
            .unwrap();
        let mut cache = SymbolCache::new("Catalogues/PC/S-101/Symbols");
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0.0, 0.0, 1.0, 1.0));
        ctx.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "ACHBRT07".into(),
            WorldPoint::new(0.6, 0.5),
        )));
        renderer.begin_frame();
        renderer.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
        let anchor = ctx.scaler.world_to_screen(WorldPoint::new(0.6, 0.5));
        let pivot = (size.width as f32 / 2.0, size.height as f32 / 2.0);
        std::fs::create_dir_all(&self.output).unwrap();
        for (name, zoom) in [
            ("zoom-0.5.png", 0.5),
            ("zoom-1.png", 1.0),
            ("zoom-2.png", 2.0),
        ] {
            renderer.set_gpu_zoom(zoom, pivot.0, pivot.1);
            renderer.save_screenshot(self.output.join(name)).unwrap();
        }
        std::fs::write(self.output.join("metadata.json"),serde_json::to_string_pretty(&serde_json::json!({"dpi_scale":dpi,"size":[size.width,size.height],"anchor":[anchor.x,anchor.y],"pivot":[pivot.0,pivot.1],"symbol":"ACHBRT07"})).unwrap()).unwrap();
        // Keep LocalOffset fixed during animated zoom; its anchor stays geographic.
        ctx.clear_instructions();
        ctx.add_instruction(DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0.6, 0.5))
                .with_offset(5.0, 3.0)
                .with_feature_id(100),
        ));
        renderer.begin_frame();
        renderer.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
        let points = renderer.displayed_symbols();
        assert_eq!(points.len(), 1);
        assert!((points[0].3.x - anchor.x - 5.0 * 96.0 / 25.4 * dpi as f32).abs() < 0.01);
        assert!((points[0].3.y - anchor.y + 3.0 * 96.0 / 25.4 * dpi as f32).abs() < 0.01);
        for (name, zoom) in [
            ("offset-0.5.png", 0.5),
            ("offset-1.png", 1.0),
            ("offset-2.png", 2.0),
        ] {
            renderer.set_gpu_zoom(zoom, pivot.0, pivot.1);
            renderer.save_screenshot(self.output.join(name)).unwrap();
        }
        // Both input orders must retain only all digits of the shallow sounding.
        for reverse in [false, true] {
            ctx.clear_instructions();
            let mut soundings = vec![
                PointInstruction::new("SOUNDG08".into(), WorldPoint::new(0.55, 0.5))
                    .with_depth(8.0)
                    .with_feature_id(8),
                PointInstruction::new("SOUNDG02".into(), WorldPoint::new(0.551, 0.5))
                    .with_depth(2.0)
                    .with_feature_id(2),
                PointInstruction::new("SOUNDG00".into(), WorldPoint::new(0.551, 0.5))
                    .with_depth(2.0)
                    .with_offset(2.0, 0.0)
                    .with_feature_id(2),
            ];
            if reverse {
                soundings.reverse();
            }
            for p in soundings {
                ctx.add_instruction(DrawingInstruction::Point(p));
            }
            renderer.begin_frame();
            renderer.set_gpu_zoom(1.0, pivot.0, pivot.1);
            renderer.add_instructions_with_symbols(&mut ctx, Some(&mut cache), Some(profile), None);
            let shown = renderer.displayed_symbols();
            assert_eq!(
                shown.len(),
                2,
                "both digits of shallow champion must remain"
            );
            assert!(
                shown.iter().all(|p| p.1 == Some(2)),
                "deep digits must be removed"
            );
            renderer
                .save_screenshot(
                    self.output
                        .join(format!("soundings-reversed-{}.png", reverse)),
                )
                .unwrap();
        }
        std::fs::write(self.output.join("assertions.txt"), "PASS local offset physical coordinates\nPASS shallowest sounding in both input orders\nPASS all digits of shallow sounding retained\n").unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut app = Smoke {
        output: std::env::args()
            .nth(1)
            .map(PathBuf::from)
            .expect("output directory"),
    };
    EventLoop::new().unwrap().run_app(&mut app).unwrap();
}
