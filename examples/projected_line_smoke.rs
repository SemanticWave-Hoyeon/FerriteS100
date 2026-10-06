//! Native GPU oracle: projected suppression must equal an explicitly clipped rhumb.
use ferrite_render::{
    Color, DrawingInstruction, FlatProjection, GeoBounds, LineInstruction, LineStyle,
    RenderContext, Viewport, WorldPoint,
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
impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Projected rhumb suppression oracle")
                    .with_inner_size(PhysicalSize::new(1000, 800)),
            )
            .unwrap(),
        );
        let size = w.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(w)).unwrap();
        renderer.background_color = Color::WHITE;
        std::fs::create_dir_all(&self.out).unwrap();
        let mut rows = Vec::new();
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for (case, base) in [
                ("high", GeoBounds::new(-10., 60., 10., 80.)),
                ("south", GeoBounds::new(-10., -80., 10., -60.)),
            ] {
                let lat = |v: f64| if case == "south" { -v } else { v };
                let line = |end: f64, priority, color, width| {
                    DrawingInstruction::Line(
                        LineInstruction::new(vec![
                            WorldPoint::new(0., lat(60.)),
                            WorldPoint::new(0., lat(end)),
                        ])
                        .with_priority(priority)
                        .with_style(LineStyle::solid(color, width)),
                    )
                };
                let high = DrawingInstruction::Line(
                    LineInstruction::new(vec![
                        WorldPoint::new(0., lat(70.)),
                        WorldPoint::new(0., lat(80.)),
                    ])
                    .with_priority(9)
                    .with_style(LineStyle::solid(Color::rgb(1., 0., 0.), 4.)),
                );
                let mut ctx =
                    RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
                ctx.scaler.set_projection(projection);
                ctx.set_bounds(base);
                ctx.add_instruction(line(80., 2, Color::rgb(0., 0., 1.), 12.));
                ctx.add_instruction(high.clone());
                renderer.reset_pan_offset();
                renderer.begin_frame();
                renderer.add_instructions(&mut ctx);
                let spans = renderer.displayed_line_spans(0).unwrap().to_vec();
                assert_eq!(spans.len(), 1);
                let y = |v| {
                    ferrite_kernel::geodesy::Mercator::World
                        .project(
                            ferrite_kernel::geodesy::GeographicPosition::new(lat(v), 0.).unwrap(),
                        )
                        .unwrap()[1]
                };
                let expected_fraction = if projection == FlatProjection::LocalGeographic {
                    0.5
                } else {
                    (y(70.) - y(60.)) / (y(80.) - y(60.))
                };
                assert!((spans[0].end - expected_fraction).abs() < 1e-14);
                let prefix = format!("{projection:?}-{case}");
                let actual_path = self.out.join(format!("{prefix}-actual.png"));
                renderer.save_screenshot(&actual_path).unwrap();
                let actual = image::open(actual_path).unwrap().to_rgb8();
                // Independent geometry oracle: replace the source's 60->80 edge with a
                // geographic 60->70 edge, retaining the separately authored 70->80 line.
                ctx.clear_instructions();
                ctx.add_instruction(line(70., 2, Color::rgb(0., 0., 1.), 12.));
                ctx.add_instruction(high);
                renderer.begin_frame();
                renderer.add_instructions(&mut ctx);
                let ref_path = self.out.join(format!("{prefix}-reference.png"));
                renderer.save_screenshot(&ref_path).unwrap();
                let reference = image::open(ref_path).unwrap().to_rgb8();
                let mut changed = 0;
                let mut mask_changed = 0;
                let mut max_channel = 0;
                for (a, b) in actual.pixels().zip(reference.pixels()) {
                    if a != b {
                        changed += 1;
                    }
                    let classify =
                        |p: &image::Rgb<u8>| (p[2] > 200 && p[0] < 50, p[0] > 200 && p[2] < 50);
                    if classify(a) != classify(b) {
                        mask_changed += 1;
                    }
                    for c in 0..3 {
                        max_channel = max_channel.max(a[c].abs_diff(b[c]));
                    }
                }
                assert_eq!(
                    mask_changed, 0,
                    "Projected partial line changes visible stroke footprint"
                );
                assert!(
                    max_channel <= 5,
                    "Geometry oracle differs beyond minor edge rounding"
                );
                rows.push(serde_json::json!({"projection":format!("{projection:?}"),"case":case,
      "span_end":spans[0].end,"independent_fraction":expected_fraction,
      "different_pixels":changed,"mask_differences":mask_changed,"max_channel_difference":max_channel}));
            }
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("Output directory"));
    EventLoop::new().unwrap().run_app(&mut App { out }).unwrap();
}
