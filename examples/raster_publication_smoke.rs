//! Native resource transaction regression; no continuous numerical certificate.
use ferrite_render::{Color, GeoBounds, RasterLayer, RasterMaterialLayer, RenderContext, Viewport};
use ferrite_wgpu::{PreparedRasterMaterialBatch, WgpuRenderer};
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
fn layer(color: [u8; 4]) -> RasterLayer {
    RasterLayer {
        viewing_groups: Vec::new(),
        draw_order: Default::default(),
        grid: None,
        id: "owned-publication-fixture".into(),
        width: 2,
        height: 2,
        rgba: color.repeat(4),
        bounds: GeoBounds::new(2., 2., 8., 8.),
    }
}
fn stage(r: &mut WgpuRenderer, c: &RenderContext) -> PreparedRasterMaterialBatch {
    r.stage_raster_material_batch(&c.scaler, |upload| -> Result<(), anyhow::Error> {
        upload(RasterMaterialLayer::Regular(layer([0, 0, 255, 255])))?;
        Ok(())
    })
    .unwrap()
}
fn capture(r: &mut WgpuRenderer, path: &std::path::Path) -> Vec<u8> {
    r.save_screenshot(path).unwrap();
    image::open(path).unwrap().to_rgba8().into_raw()
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        assert!(ferrite_wgpu::background_test::enabled());
        let attrs = ferrite_wgpu::background_test::window_attributes(
            Window::default_attributes().with_inner_size(PhysicalSize::new(960, 640)),
        );
        let window = Arc::new(el.create_window(attrs.clone()).unwrap());
        assert_eq!(window.is_visible(), Some(false));
        assert!(!window.has_focus());
        let size = window.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        r.background_color = Color::WHITE;
        let mut c = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        c.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        r.begin_frame();
        r.add_instructions(&mut c);
        r.add_raster_layer(layer([255, 0, 0, 255]), &c.scaler)
            .unwrap();
        std::fs::create_dir_all(&self.out).unwrap();
        let original = capture(&mut r, &self.out.join("before.png"));
        // Complete materials may be abandoned after any later producer failure.
        for replace in [false, true] {
            let batch = stage(&mut r, &c);
            let prepared = r
                .prepare_raster_material_publication(batch, replace, None)
                .unwrap();
            r.validate_raster_publication(&prepared).unwrap();
            drop(prepared);
            assert_eq!(
                original,
                capture(&mut r, &self.out.join(format!("dropped-{replace}.png")))
            );
        }
        let batch = stage(&mut r, &c);
        let prepared = r
            .prepare_raster_material_publication(batch, true, None)
            .unwrap();
        assert!(r.set_pan_offset(17., 9.));
        assert!(r.validate_raster_publication(&prepared).is_err());
        drop(prepared);
        r.reset_pan_offset();
        assert_eq!(original, capture(&mut r, &self.out.join("stale-view.png")));
        let batch = stage(&mut r, &c);
        let prepared = r
            .prepare_raster_material_publication(batch, true, None)
            .unwrap();
        r.update_raster_view(&c.scaler);
        assert!(r.validate_raster_publication(&prepared).is_err());
        drop(prepared);
        assert_eq!(
            original,
            capture(&mut r, &self.out.join("stale-inventory.png"))
        );
        let other_window = Arc::new(el.create_window(attrs).unwrap());
        let other = pollster::block_on(WgpuRenderer::new(other_window)).unwrap();
        let batch = stage(&mut r, &c);
        assert!(other
            .prepare_raster_material_publication(batch, true, None)
            .is_err());
        drop(other);
        assert_eq!(original, capture(&mut r, &self.out.join("wrong-owner.png")));
        // Fast gesture state is reset only at successful complete publication.
        assert!(r.set_pan_offset(31., 13.));
        let batch = stage(&mut r, &c);
        let mut raster = r
            .prepare_raster_material_publication(batch, true, None)
            .unwrap();
        r.reproject_raster_publication(&mut raster, &c.scaler)
            .unwrap();
        let scene = r
            .prepare_raster_scene_publication(raster)
            .unwrap();
        r.validate_raster_scene_publication(&scene).unwrap();
        r.commit_raster_scene_publication(scene);
        assert_eq!(r.get_pan_offset(), (0., 0.));
        let success = capture(&mut r, &self.out.join("success.png"));
        let rgba = image::open(self.out.join("success.png"))
            .unwrap()
            .to_rgba8();
        let p = c
            .scaler
            .world_to_screen(ferrite_render::WorldPoint::new(5., 5.));
        assert_eq!(rgba.get_pixel(p.x as u32, p.y as u32).0, [0, 0, 255, 255]);
        r.render().unwrap();
        assert_eq!(
            success,
            capture(&mut r, &self.out.join("after-surface.png"))
        );
        std::fs::write(self.out.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"hidden":true,"unfocused":true,"dropped_cases":2,"stale_view_rejected":true,"stale_inventory_rejected":true,"wrong_owner_rejected":true,"successful_camera_reset":true,"surface_export_equal":true,"continuous_qualification":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut builder = EventLoop::builder();
    ferrite_wgpu::background_test::configure_event_loop(&mut builder);
    builder
        .build()
        .unwrap()
        .run_app(&mut Smoke {
            out: std::env::args().nth(1).unwrap().into(),
        })
        .unwrap();
}
