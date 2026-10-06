//! Warm/cold GPU equivalence for physical pattern periods and UI profile changes.
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, DrawingInstruction, GeoBounds, RenderContext, ScreenPoint, Viewport,
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
struct App {
    out: PathBuf,
    pc: PathBuf,
}
fn instructions(c: &mut RenderContext, width: f32, height: f32) {
    c.clear_instructions();
    for (rect, w) in [
        ([20., 20., 480., 580.], width),
        ([520., 20., 980., 580.], width + 2.),
    ] {
        let points = [
            [rect[0], rect[1]],
            [rect[2], rect[1]],
            [rect[2], rect[3]],
            [rect[0], rect[3]],
        ]
        .map(|p| c.scaler.screen_to_world(ScreenPoint::new(p[0], p[1])))
        .to_vec();
        c.add_instruction(DrawingInstruction::Area(
            AreaInstruction::new(points).with_pattern_fill(
                "FOULAR01P".into(),
                (w, 0.),
                (2., height),
            ),
        ));
    }
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let make = || {
            Arc::new(
                el.create_window(
                    Window::default_attributes()
                        .with_inner_size(PhysicalSize::new(1000, 600))
                        .with_title("Pattern cache warm/cold oracle"),
                )
                .unwrap(),
            )
        };
        let mut warm = pollster::block_on(WgpuRenderer::new(make())).unwrap();
        warm.background_color = Color::WHITE;

        let pc = PortrayalCatalogue::load(&self.pc).unwrap();
        let mut wc = SymbolCache::new(self.pc.join("Symbols"));
        let mut cc = SymbolCache::new(self.pc.join("Symbols"));
        let mut c = RenderContext::new(Viewport::new(1000., 600.));
        c.set_bounds(GeoBounds::new(-1., 48., 1., 50.));
        let mut rows = Vec::new();
        let mut previous = "";
        for (index, (profile, width, height)) in [
            ("Day", 5., 6.),
            ("Day", 9., 6.),
            ("Day", 4., 9.),
            ("Day", 5., 9.),
            ("Night", 5., 6.),
            ("Night", 9., 6.),
            ("Dusk", 5., 6.),
            ("Day", 5., 6.),
        ]
        .into_iter()
        .enumerate()
        {
            if previous != profile {
                wc.clear();
                warm.clear_symbol_textures();
                previous = profile;
            }
            let mut cold = pollster::block_on(WgpuRenderer::new(make())).unwrap();
            cold.background_color = Color::WHITE;
            instructions(&mut c, width, height);
            let cp = pc.color_profiles.profiles.get(profile).unwrap();
            let mut images = Vec::new();
            for (label, r, cache) in [("warm", &mut warm, &mut wc), ("cold", &mut cold, &mut cc)] {
                if label == "cold" {
                    cache.clear();
                    r.clear_symbol_textures();
                }
                r.reset_pan_offset();
                r.begin_frame();
                r.add_instructions_with_symbols(&mut c, Some(cache), Some(cp), None);
                let p = self.out.join(format!("case{index}-{label}.png"));
                r.save_screenshot(&p).unwrap();
                images.push(image::open(p).unwrap().to_rgba8());
            }
            let changed = images[0]
                .pixels()
                .zip(images[1].pixels())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                changed, 0,
                "cache mismatch case{index}/{profile}/{width}/{height}"
            );
            assert!(
                images[0]
                    .pixels()
                    .filter(|p| p.0 != [255, 255, 255, 255])
                    .count()
                    > 1000
            );
            rows.push(serde_json::json!({"case":index,"profile":profile,"period_width_mm":width,"period_height_mm":height,"same_svg_two_lattices":true,"warm_cold_changed_pixels":changed,"actual_ui_cache_invalidation":true}));
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"rows":rows,"physical_input_verified":false}),
            )
            .unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        if matches!(e, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
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
