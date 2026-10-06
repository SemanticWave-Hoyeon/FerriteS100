//! PC/Lua -> S101 adapter -> native GPU proof for multiple stroke styles and transparency.
use ferrite_render::{Color, DrawingInstruction, GeoBounds, RenderContext, Viewport, WorldPoint};
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
    args: Vec<String>,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let out = PathBuf::from(&self.args[1]);
        std::fs::create_dir_all(&out).unwrap();
        let cell = ferrite_s100_core::S101Cell::load(&self.args[2]).unwrap();
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.args[3]).unwrap();
        let (&id, origin) = cell
            .features
            .iter()
            .find_map(|(id, f)| {
                f.spatial_associations.iter().find_map(|a| {
                    cell.points
                        .get(&a.spatial_id.key())
                        .map(|p| (id, WorldPoint::new(p.position.x, p.position.y)))
                })
            })
            .unwrap();
        let result=ferrite_lua::PortrayalResult::parse(&id.to_string(),"AugmentedRay:LocalCRS,90,LocalCRS,25;LineStyle:base,,1.28,LITRD;LineStyle:overlay,,0.32,LITGN,0.5;LineStyle:ghost,,10,LITYW,1;LineInstruction:base,overlay;DrawingPriority:99;LineInstruction:ghost","").unwrap();
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(960, 640))
                    .with_title("Stroke layers alpha"),
            )
            .unwrap(),
        );
        let density = w.scale_factor();
        let size = w.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        renderer.background_color = Color::WHITE;
        let mut context = RenderContext::new(Viewport::new(
            size.width as f32 - 1.,
            size.height as f32 - 1.,
        ));
        // Centre the horizontal stroke on a pixel centre so its narrow overlay
        // probes have full MSAA coverage, including native density 1.
        context.set_bounds(GeoBounds::new(
            origin.x - 1.,
            origin.y - 1.,
            origin.x + 1.,
            origin.y + 1.,
        ));
        ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut context, 0, "Day")
            .unwrap();
        assert_eq!(context.instruction_count(), 3);
        // Isolate compositing from catalogue colour conversion, retaining adapter-produced alpha and widths.
        let mut copy = context.raw_instructions().to_vec();
        let mut alpha = Vec::new();
        for (i, inst) in copy.iter_mut().enumerate() {
            let DrawingInstruction::Line(line) = inst else {
                panic!()
            };
            alpha.push(line.style.color.a);
            line.color_token = None;
            let a = line.style.color.a;
            line.style.color = if i == 0 {
                Color::BLUE.with_alpha(a)
            } else {
                Color::RED.with_alpha(a)
            };
        }
        assert_eq!(alpha, vec![1., 0.5, 0.]);
        context.clear_instructions();
        for inst in copy {
            context.add_instruction(inst);
        }
        renderer.begin_frame();
        renderer.add_instructions(&mut context);
        assert_eq!(renderer.displayed_geometry().len(), 2);
        let p = context.scaler.world_to_screen(origin);
        let mm = context.scaler.pixels_per_mm();
        let mut first = None;
        let mut rows = Vec::new();
        for zoom in [0.5, 1., 2.] {
            renderer.set_gpu_zoom(zoom, p.x, p.y);
            let path = out.join(format!("layers-{zoom}.png"));
            renderer.save_screenshot(&path).unwrap();
            let im = image::open(&path).unwrap().to_rgb8();
            for (y, expected) in [
                (0., [128, 0, 128]),
                (0.4, [0, 0, 255]),
                (0.8, [255, 255, 255]),
            ] {
                let actual = im
                    .get_pixel(
                        (p.x + 10. * mm as f32).floor() as u32,
                        (p.y + (y * mm) as f32).floor() as u32,
                    )
                    .0;
                for i in 0..3 {
                    assert!(
                        (actual[i] as i16 - expected[i] as i16).abs() <= 1,
                        "zoom{zoom} y{y}: {actual:?}"
                    );
                }
            }
            if let Some(first) = &first {
                assert_eq!(&im, first);
            } else {
                first = Some(im.clone());
            }
            renderer.render().unwrap();
            let path = out.join(format!("layers-{zoom}-after.png"));
            renderer.save_screenshot(&path).unwrap();
            assert_eq!(im, image::open(path).unwrap().to_rgb8());
            rows.push(serde_json::json!({"zoom":zoom,"probes":3,"displayed_strokes":2,"surface_export_equal":true}));
        }
        std::fs::write(out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"density":density,"adapter_alpha":alpha,"cases":rows,"all_zoom_images_equal":true,"native_os_input_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            args: std::env::args().collect(),
        })
        .unwrap();
}
