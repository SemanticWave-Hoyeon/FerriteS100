//! Render actual object-details panels in native GPU windows. Fixture data only;
//! pointer click behavior is tested in object_details unit tests, not OS automation.
use ferrite_render::{Color, GeoBounds, RenderContext, Viewport};
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
                    .with_title("Object details UI regression")
                    .with_inner_size(PhysicalSize::new(2400, 2000)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let ppp = window.scale_factor() as f32;
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::rgb(0.72, 0.88, 0.94);
        let mut chart = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        chart.set_bounds(GeoBounds::new(-1.2, 50.7, -1.0, 50.9));
        renderer.begin_frame();
        renderer.add_instructions(&mut chart);
        std::fs::create_dir_all(&self.output).unwrap();
        renderer.ui_state.version = "UI layout fixture".into();
        renderer.ui_state.reduced_motion = true;
        renderer.ui_state.chart_area = (0., 0., size.width as f32 / ppp, size.height as f32 / ppp);
        renderer.ui_state.selected_feature=Some(SelectedFeature {
   feature_type:"Wreck".into(),feature_id:4294967311,cell_index:Some(0),foid:Some("LOCAL-UI-TEST:15:1".into()),primitive_type:"Point".into(),
   source:Some("LOCAL-UI-TEST/Portsmouth-object-detail-fixture.000".into()),
   attributes:vec![("Depth".into(),"12.34 m".into()),("Survey status".into(),"Unsurveyed; value retained from fixture".into()),("Information".into(),"Long descriptions wrap within the panel. This is a UI fixture, not an official chart feature.".into())],
   world_pos:(-1.1,50.8),longitude_shift:0.,definition:Some("The wreck of a vessel; catalogue definition is kept separate from its instance attributes.".into()),symbol_name:Some("ISODGR01".into()) });
        renderer.ui_state.security_status = "Local UI fixture".into();
        renderer.ui_state.security_details =
            "No operational certificate claim. UI layout data only.".into();
        renderer.ui_state.coverage_info=Some("Depth: 12.34 m\nUncertainty: 0.42 m\nSurvey quality: fixture record\nSource: local UI fixture".into());
        let mut rows = Vec::new();
        for profile in ["Day", "Dusk", "Night"] {
            renderer.ui_state.color_profile = profile.into();
            for mode in ["collapsed", "expanded", "filtered"] {
                renderer
                    .ui_state
                    .object_detail_sections
                    .set_all(mode != "collapsed");
                renderer.ui_state.object_attribute_query = if mode == "filtered" {
                    "12.34".into()
                } else {
                    String::new()
                };
                let path = self.output.join(format!("{profile}-{mode}.png"));
                renderer.save_screenshot_with_ui(&path).unwrap();
                renderer.save_screenshot_with_ui(&path).unwrap();
                let feature = renderer.ui_state.selected_feature.as_ref().unwrap();
                assert_eq!(feature.attributes.len(), 3);
                assert_eq!(
                    renderer.ui_state.object_attribute_query,
                    if mode == "filtered" { "12.34" } else { "" }
                );
                let image = image::open(&path).unwrap().to_rgba8();
                assert!(image.pixels().any(|p| p.0[0] != p.0[1]));
                rows.push(serde_json::json!({"profile":profile,"mode":mode,"file":path.file_name().unwrap().to_string_lossy(),"source_attributes_retained":feature.attributes.len(),"query":renderer.ui_state.object_attribute_query,"native_pixels_per_point":ppp,"physical_size":[size.width,size.height],"os_pointer_automation":false}));
            }
        }
        let preferences = renderer.ui_state.object_detail_sections.clone();
        renderer.ui_state.object_attribute_query = "NO_MATCH_ON_NEW_OBJECT".into();
        renderer
            .ui_state
            .selected_feature
            .as_mut()
            .unwrap()
            .feature_id = 4294967312;
        renderer
            .save_screenshot_with_ui(self.output.join("selection-change.png"))
            .unwrap();
        assert!(renderer.ui_state.object_attribute_query.is_empty());
        assert_eq!(renderer.ui_state.object_detail_sections, preferences);
        std::fs::write(
            self.output.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("output folder");
    let event_loop = EventLoop::new().unwrap();
    event_loop.run_app(&mut Smoke { output }).unwrap();
}
