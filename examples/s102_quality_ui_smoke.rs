//! Render the application's real coverage-query transcript in the information panel.
use ferrite_render::Color;
use ferrite_wgpu::WgpuRenderer;
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct Smoke {
    input: PathBuf,
    output: PathBuf,
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("S-102 quality information panel")
                    .with_inner_size(PhysicalSize::new(2400, 1600)),
            )
            .unwrap(),
        );
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&self.input).unwrap()).unwrap();
        let probes = data["probes"].as_array().unwrap();
        let probe = probes
            .iter()
            .find(|p| {
                p["displayed_information"]
                    .as_str()
                    .unwrap()
                    .contains("Source encoding warning")
            })
            .unwrap_or(&probes[0]);
        let information = probe["displayed_information"].as_str().unwrap().to_string();
        renderer.ui_state.bathymetry_count = 7;
        renderer.ui_state.zoom_level = 1.;
        renderer.ui_state.coverage_info = Some(information.clone());
        renderer.ui_state.security_status = "Dataset signatures verified".into();
        renderer.save_screenshot_with_ui(&self.output).unwrap();
        renderer.save_screenshot_with_ui(&self.output).unwrap();
        assert_eq!(
            renderer.ui_state.coverage_info.as_deref(),
            Some(information.as_str())
        );
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let mut args = std::env::args().skip(1);
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke {
            input: args.next().unwrap().into(),
            output: args.next().unwrap().into(),
        })
        .unwrap();
}
