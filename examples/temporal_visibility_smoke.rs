//! Actual GPU regression for partly coincident strokes and long-line clipping.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, RenderContext, Viewport,
    WorldPoint,
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
struct Smoke {
    output: PathBuf,
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Temporal visibility regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let pixels_per_mm = 96.0 / 25.4 * window.scale_factor() as f32;
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(1., 5.), WorldPoint::new(9., 5.)])
                .with_priority(2)
                .with_style(LineStyle::solid(Color::rgb(0., 0., 1.), 12.)),
        ));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![
                WorldPoint::new(3., 5.),
                WorldPoint::new(5., 5.),
                WorldPoint::new(7., 5.),
            ])
            .with_priority(8)
            .with_style(LineStyle::solid(Color::rgb(1., 0., 0.), 4.)),
        ));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(1., 7.), WorldPoint::new(9., 7.)])
                .with_priority(8)
                .with_style(LineStyle::solid_mm(Color::rgb(0., 1., 0.), 1.)),
        ));
        let interval = |lo: &str, hi: &str| {
            ferrite_kernel::TemporalInterval::new(
                Some(
                    ferrite_kernel::TemporalBounds::new(Some(lo.into()), Some(hi.into())).unwrap(),
                ),
                None,
                None,
                ferrite_kernel::IntervalClosure::Closed,
            )
            .unwrap()
        };
        ctx.set_time_intervals_from(1, &[interval("----1101", "----0331")]);
        // Replace the green line's condition with summer-only dates.
        let mut commands = ctx.raw_instructions().to_vec();
        commands[2].set_time_intervals(&[interval("----0601", "----0831")]);
        ctx.set_instructions_from_cache(commands);
        let overlap = ctx.scaler.world_to_screen(WorldPoint::new(5., 5.));
        let summer = ctx.scaler.world_to_screen(WorldPoint::new(5., 7.));
        std::fs::create_dir_all(&self.output).unwrap();
        let mut rows = Vec::new();
        for date in ["2026-01-15", "2026-07-15", "2026-10-15", "2026-01-15"] {
            ctx.settings.current_date = Some(date.into());
            renderer.begin_frame();
            renderer.add_instructions(&mut ctx);
            let file = format!("{date}.png");
            renderer.save_screenshot(self.output.join(&file)).unwrap();
            let shown = renderer.displayed_geometry();
            // These are the exact instruction indices used by application hit testing.
            let expected = match date {
                "2026-01-15" => vec![0, 1],
                "2026-07-15" => vec![0, 2],
                _ => vec![0],
            };
            assert_eq!(shown, expected);
            rows.push(serde_json::json!({"date":date,"file":file,"overlap":[overlap.x,overlap.y],"summer":[summer.x,summer.y],"displayed_geometry":shown,"temporal_visibility_counts":renderer.temporal_visibility_counts(),"line_visibility_counts":renderer.line_visibility_counts(),"mm_width_pixels":pixels_per_mm}));
        }
        std::fs::write(
            self.output.join("metadata.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).expect("Output directory"));
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke { output })
        .unwrap();
}
