//! Native GPU rendering of the IC Settings control and unchanged pending/active state.
//! Does not emulate mouse/menu events or verify OS accessibility.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, RenderContext, TextInstruction, Viewport, WorldPoint,
};
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
                    .with_title("Viewing time UI rendering")
                    .with_inner_size(PhysicalSize::new(2400, 1600)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let ppp = window.scale_factor() as f32;
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.add_instruction(DrawingInstruction::Text(
            TextInstruction::new("TEXT EXPORT 12.3".into(), WorldPoint::new(2., 5.))
                .with_color(Color::BLACK)
                .with_font_size(40.),
        ));
        renderer.begin_frame();
        renderer.add_instructions(&mut ctx);
        std::fs::create_dir_all(&self.output).unwrap();
        renderer
            .save_screenshot(self.output.join("chart-text.png"))
            .unwrap();
        renderer.ui_state.chart_area = (0., 0., size.width as f32 / ppp, size.height as f32 / ppp);
        renderer.ui_state.selected_feature = Some(SelectedFeature {
            feature_type: "Selection export".into(),
            feature_id: 1,
            foid: None,
            cell_index: None,
            primitive_type: "Line".into(),
            source: None,
            attributes: Vec::new(),
            world_pos: (5., 3.),
            longitude_shift: 0.,
            definition: None,
            symbol_name: None,
        });
        renderer.set_selection_geometry(
            vec![vec![WorldPoint::new(3., 3.), WorldPoint::new(7., 3.)]],
            &ctx.scaler,
        );
        renderer
            .save_screenshot(self.output.join("chart-selection.png"))
            .unwrap();
        let selected = image::open(self.output.join("chart-selection.png"))
            .unwrap()
            .to_rgb8();
        assert!(
            selected
                .pixels()
                .filter(|p| p.0[0] < 20 && p.0[1] > 180 && p.0[2] > 220)
                .count()
                > 200
        );
        renderer.ui_state.selected_feature = None;
        renderer.update_selection(&ctx.scaler);
        renderer.ui_state.version = "Time UI regression".into();
        renderer.ui_state.show_temporal = false;
        renderer.ui_state.show_settings = true;
        renderer.ui_state.interoperability_available = true;
        renderer.ui_state.interoperability_status =
            "Interoperability on: authenticated local test".into();
        let mut pending = renderer.settings().clone();
        pending.interoperability_enabled = false;
        renderer.ui_state.pending_settings = Some(pending);
        let file = self.output.join("interoperability-settings.png");
        renderer.save_screenshot_with_ui(&file).unwrap();
        renderer.save_screenshot_with_ui(&file).unwrap();
        assert!(
            renderer.settings().interoperability_enabled,
            "Draft changed active composition"
        );
        assert!(
            !renderer
                .ui_state
                .pending_settings
                .as_ref()
                .unwrap()
                .interoperability_enabled
        );
        assert!(!renderer.ui_state.settings_changed);
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
