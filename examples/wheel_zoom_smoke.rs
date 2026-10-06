//! Programmatic wheel calculation -> GPU zoom -> official PC symbol parity.
#[path = "../src/navigation.rs"]
mod navigation;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, PointInstruction, RenderContext, Viewport, WorldPoint,
};
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct App {
    out: PathBuf,
    pc: PathBuf,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(960, 640))
                    .with_title("Wheel zoom symbol parity"),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        let pc = PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(self.pc.join("Symbols"));
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(0., 0., 1., 1.));
        c.add_instruction(DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0.5, 0.5))
                .with_offset(5., 3.)
                .with_feature_id(7),
        ));
        r.begin_frame();
        r.add_instructions_with_symbols(&mut c, Some(&mut cache), Some(profile), None);
        assert_eq!(r.displayed_symbols().len(), 1);
        let p = c.scaler.world_to_screen(WorldPoint::new(0.5, 0.5));
        std::fs::create_dir_all(&self.out).unwrap();
        let mut baseline = None;
        let mut cases = Vec::new();
        for steps in [-8., -1., 0., 1., 8., 20., 100.] {
            let target = navigation::scroll_zoom_target(
                25.,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0., steps * 50. * density)),
                density,
            )
            .unwrap();
            for simulated_density in [1., 1.25, 1.5, 2., 3.] {
                let z = navigation::scroll_zoom_target(
                    25.,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                        0.,
                        steps * 50. * simulated_density,
                    )),
                    simulated_density,
                )
                .unwrap();
                assert!((z - target).abs() < 1e-10);
            }
            let zoom = (target / 25.) as f32;
            r.set_gpu_zoom(zoom, p.x, p.y);
            let path = self.out.join(format!("steps-{steps}.png"));
            r.save_screenshot(&path).unwrap();
            let im = image::open(path).unwrap().to_rgb8();
            let colored: Vec<_> = im
                .enumerate_pixels()
                .filter(|(_, _, p)| p.0 != [255, 255, 255])
                .map(|(x, y, _)| (x, y))
                .collect();
            assert!(!colored.is_empty(), "symbol must exist");
            let min_x = colored.iter().map(|p| p.0).min().unwrap();
            let max_x = colored.iter().map(|p| p.0).max().unwrap();
            let min_y = colored.iter().map(|p| p.1).min().unwrap();
            let max_y = colored.iter().map(|p| p.1).max().unwrap();
            if let Some(first) = &baseline {
                assert_eq!(
                    first, &im,
                    "screen symbol/LocalOffset changed with zoom input"
                );
            } else {
                baseline = Some(im.clone());
            }
            r.render().unwrap();
            let path = self.out.join(format!("steps-{steps}-after.png"));
            r.save_screenshot(&path).unwrap();
            assert_eq!(im, image::open(path).unwrap().to_rgb8());
            cases.push(serde_json::json!({"steps":steps,"target_zoom":target,"gpu_factor":zoom,"symbol_bbox":[min_x,min_y,max_x,max_y],"simulated_density_parity":true,"surface_export_equal":true}));
        }
        // Check a rebuilt view at the actual maximum as well as GPU fast zoom.
        r.set_gpu_zoom(1., p.x, p.y);
        c.set_bounds(GeoBounds::new(
            0.5 - 0.5 / 8.,
            0.5 - 0.5 / 8.,
            0.5 + 0.5 / 8.,
            0.5 + 0.5 / 8.,
        ));
        r.begin_frame();
        r.add_instructions_with_symbols(&mut c, Some(&mut cache), Some(profile), None);
        let rebuilt = self.out.join("rebuilt-200x.png");
        r.save_screenshot(&rebuilt).unwrap();
        assert_eq!(
            baseline.as_ref().unwrap(),
            &image::open(rebuilt).unwrap().to_rgb8(),
            "rebuilt 200x changed symbol size or local offset"
        );
        let displayed = r.displayed_symbols();
        assert_eq!(displayed.len(), 1);
        assert_eq!(displayed[0].1, Some(7));
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"native_density":density,"symbol":"ACHBRT07","local_offset_mm":[5,3],"all_zoom_images_equal":true,"maximum_zoom":navigation::MAX_ZOOM,"rebuilt_200x_matches":true,"native_os_input_verified":false,"execution_os":std::env::consts::OS,"windows_verified":cfg!(target_os="windows"),"cases":cases})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut args = std::env::args().skip(1);
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            out: args.next().unwrap().into(),
            pc: args.next().unwrap().into(),
        })
        .unwrap();
}
