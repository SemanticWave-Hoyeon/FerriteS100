//! Independent GPU pixels for ordinary product overlays and explicit base ordering.
use ferrite_kernel::CompositionStage;
use ferrite_render::{
    AreaInstruction, Color, DisplayPlane, DrawingInstruction, GeoBounds, RasterDrawOrder,
    RasterLayer, RenderContext, Viewport, WorldPoint,
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
fn rect(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    c: Color,
    p: i32,
    plane: DisplayPlane,
) -> DrawingInstruction {
    DrawingInstruction::Area(
        AreaInstruction::new(vec![
            WorldPoint::new(x, y),
            WorldPoint::new(x + w, y),
            WorldPoint::new(x + w, y + h),
            WorldPoint::new(x, y + h),
        ])
        .with_solid_fill(c)
        .with_priority(p)
        .with_display_plane(plane),
    )
}
fn raster(order: RasterDrawOrder, rgba: Vec<u8>, name: &str) -> RasterLayer {
    RasterLayer {
        viewing_groups: Vec::new(),
        draw_order: order,
        id: name.into(),
        bounds: GeoBounds::new(2., 2., 8., 8.),
        width: 2,
        height: 2,
        rgba,
        grid: None,
    }
}
fn sample(im: &image::RgbImage, c: &RenderContext, x: f64, y: f64) -> [u8; 3] {
    let p = c.scaler.world_to_screen(WorldPoint::new(x, y));
    im.get_pixel(p.x.round() as u32, p.y.round() as u32).0
}
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let pc =
            ferrite_portrayal_catalog::PortrayalCatalogue::load("Catalogues/PC/S-102").unwrap();
        let portrayal =
            ferrite_s102::BathymetryPortrayal::from_catalogue(&pc, "Day", Default::default())
                .unwrap();
        assert_eq!(portrayal.draw_order.stage, CompositionStage::Overlay);
        assert_eq!(portrayal.draw_order.display_plane, DisplayPlane::UnderRadar);
        assert_eq!(
            portrayal.draw_order.priority, 3,
            "Preserve official PC priority rather than old hardcoded 4"
        );
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(960, 640))
                    .with_title("Ordinary multi-product overlays"),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        r.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.out).unwrap();
        let mut cases = Vec::new();
        for zoom in [0.5, 1., 2.] {
            let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
            c.add_instruction(rect(
                0.,
                0.,
                10.,
                10.,
                Color::rgb(1., 0., 0.),
                8,
                DisplayPlane::UnderRadar,
            ));
            c.add_instruction(rect(
                4.,
                0.,
                2.,
                10.,
                Color::rgb(1., 1., 0.),
                0,
                DisplayPlane::OverRadar,
            ));
            r.set_pan_offset(0., 0.);
            r.set_gpu_zoom(1., 0., 0.);
            r.begin_frame();
            r.add_instructions(&mut c);
            r.set_gpu_zoom(zoom, size.width as f32 / 2., size.height as f32 / 2.);
            // At zoom2 stay inside exported extent; inverse transform is tested by resampling screen-space transformed coordinates.
            let original = raster(
                RasterDrawOrder {
                    stage: CompositionStage::Overlay,
                    display_plane: DisplayPlane::UnderRadar,
                    priority: 3,
                },
                vec![0, 255, 255, 255, 0, 0, 0, 0, 0, 255, 0, 128, 0, 0, 255, 255],
                "coverage",
            );
            r.add_raster_layer(original, &c.scaler).unwrap();
            let path = self.out.join(format!("overlay-{zoom}.png"));
            r.save_screenshot(&path).unwrap();
            let im = image::open(&path).unwrap().to_rgb8();
            // Renderer fast zoom pivot defaults to viewport centre; derive the physical query from the known transform.
            let probe = |x: f64, y: f64| {
                let p = c.scaler.world_to_screen(WorldPoint::new(x, y));
                let px = (p.x - size.width as f32 / 2.) * zoom + size.width as f32 / 2.;
                let py = (p.y - size.height as f32 / 2.) * zoom + size.height as f32 / 2.;
                im.get_pixel(px.round() as u32, py.round() as u32).0
            };
            let checks = [
                (3.8, 6.2, [0, 255, 255]),
                (5.5, 6.2, [255, 255, 0]),
                (3.8, 3.8, [127, 128, 0]),
                (5.5, 3.8, [0, 0, 255]),
            ];
            for (x, y, color) in checks {
                let got = probe(x, y);
                assert!(
                    got.iter()
                        .zip(color)
                        .all(|(a, b)| (*a as i16 - b as i16).abs() <= 1),
                    "zoom {zoom} ({x},{y}): {got:?} expected {color:?}"
                );
            }
            r.render().unwrap();
            let after = self.out.join(format!("overlay-{zoom}-after.png"));
            r.save_screenshot(&after).unwrap();
            assert_eq!(im, image::open(&after).unwrap().to_rgb8());
            cases.push(
                serde_json::json!({"zoom":zoom,"color_probes":4,"surface_export_equal":true}),
            );
            r.clear_raster_layers();
        }
        r.set_gpu_zoom(1., 0., 0.);
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        c.add_instruction(rect(
            0.,
            0.,
            10.,
            10.,
            Color::rgb(1., 0., 0.),
            8,
            DisplayPlane::UnderRadar,
        ));
        r.begin_frame();
        r.add_instructions(&mut c);
        // Upload reverse priority; composition must follow PC priority, not input order.
        for (p, col) in [(9, [0, 0, 255, 255]), (2, [0, 255, 0, 255])] {
            r.add_raster_layer(
                raster(
                    RasterDrawOrder {
                        stage: CompositionStage::Overlay,
                        display_plane: DisplayPlane::UnderRadar,
                        priority: p,
                    },
                    col.repeat(4),
                    "priority",
                ),
                &c.scaler,
            )
            .unwrap();
        }
        let p = self.out.join("reverse-input-order.png");
        r.save_screenshot(&p).unwrap();
        assert_eq!(
            sample(&image::open(&p).unwrap().to_rgb8(), &c, 5., 5.),
            [0, 0, 255]
        );
        r.clear_raster_layers();
        r.add_raster_layer(
            raster(
                RasterDrawOrder {
                    stage: CompositionStage::Chart,
                    display_plane: DisplayPlane::UnderRadar,
                    priority: 3,
                },
                [0, 255, 255, 255].repeat(4),
                "chart",
            ),
            &c.scaler,
        )
        .unwrap();
        let p = self.out.join("base-order.png");
        r.save_screenshot(&p).unwrap();
        assert_eq!(
            sample(&image::open(&p).unwrap().to_rgb8(), &c, 5., 5.),
            [255, 0, 0]
        );
        r.clear_raster_layers();
        let p = self.out.join("overlay-unloaded.png");
        r.save_screenshot(&p).unwrap();
        assert_eq!(
            sample(&image::open(&p).unwrap().to_rgb8(), &c, 5., 5.),
            [255, 0, 0]
        );
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"cases":cases,"official_s102_pc_priority":portrayal.draw_order.priority,"reverse_priority_upload":true,"base_priority_restored":true,"overlay_unload_restored":true,"native_os_input_verified":false,"s98_ic_processing_verified":false})).unwrap()).unwrap();
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
