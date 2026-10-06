//! Actual GPU selection overlay at the chosen longitude copy.
use ferrite_render::{Color, GeoBounds, RenderContext, Viewport, WorldPoint};
use ferrite_wgpu::{SelectedFeature, WgpuRenderer};
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
                    .with_title("Wrapped selection regression")
                    .with_inner_size(PhysicalSize::new(2400, 1600)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut r = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        r.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.output).unwrap();
        let mut probes = Vec::new();
        for shift in [-360., 0., 360.] {
            let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            ctx.set_bounds(GeoBounds::new(shift, 0., shift + 10., 10.));
            r.reset_pan_offset();
            r.begin_frame();
            r.add_instructions(&mut ctx);

            r.ui_state.selected_feature = Some(SelectedFeature {
                feature_type: "Line".into(),
                feature_id: 1,
                foid: Some("1:2:3".into()),
                cell_index: Some(0),
                primitive_type: "Curve".into(),
                source: None,
                attributes: Vec::new(),
                world_pos: (5., 3.),
                longitude_shift: shift,
                definition: None,
                symbol_name: None,
            });
            r.set_selection_geometry(
                vec![vec![WorldPoint::new(3., 3.), WorldPoint::new(7., 3.)]],
                &ctx.scaler,
            );
            r.render().unwrap();
            r.render().unwrap();
            for zoom in [0.5f32, 1., 2.] {
                r.set_pan_offset(15., -8.);
                r.set_gpu_zoom(zoom, size.width as f32 / 2., size.height as f32 / 2.);
                let path = self.output.join(format!("selection-{shift}-{zoom}.png"));
                r.save_screenshot(&path).unwrap();
                let image = image::open(&path).unwrap().to_rgb8();
                let p = ctx.scaler.world_to_screen(WorldPoint::new(shift + 4., 3.));
                let x = (p.x + 15. - size.width as f32 / 2.) * zoom + size.width as f32 / 2.;
                let y = (p.y - 8. - size.height as f32 / 2.) * zoom + size.height as f32 / 2.;
                let mut cyan = 0;
                for yy in (y as i32 - 8).max(0)..(y as i32 + 8).min(image.height() as i32) {
                    for xx in (x as i32 - 20).max(0)..(x as i32 + 20).min(image.width() as i32) {
                        let p = image.get_pixel(xx as u32, yy as u32).0;
                        if p[0] < 80 && p[1] > 150 && p[2] > 180 {
                            cyan += 1;
                        }
                    }
                }
                assert!(
                    cyan > 25,
                    "shift {shift} zoom {zoom}: highlight absent ({cyan})"
                );
                assert_eq!(
                    r.ui_state.selected_feature.as_ref().unwrap().world_pos,
                    (5., 3.)
                );
                assert_eq!(r.selected_geometry_vertex_count(), 2);
                r.render().unwrap();
                let after = self
                    .output
                    .join(format!("selection-{shift}-{zoom}-after.png"));
                r.save_screenshot(&after).unwrap();
                assert!(
                    image == image::open(after).unwrap().to_rgb8(),
                    "Export changed across surface render"
                );
                probes.push(serde_json::json!({"longitude_shift":shift,"zoom":zoom,"cyan_pixels":cyan,"source_position":[5.,3.]}));
            }
        }
        std::fs::write(self.output.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"probes":probes,"surface_export_pairs":9,"native_mouse_verified":false})).unwrap()).unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).unwrap());
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
