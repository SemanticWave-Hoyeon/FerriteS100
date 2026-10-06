//! Native event-loop regression: no mouse input or manual redraw at time boundaries.
use ferrite_render::{
    Color, DrawingInstruction, GeoBounds, LineInstruction, LineStyle, RenderContext, Viewport,
    WorldPoint,
};
use ferrite_wgpu::WgpuRenderer;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};
struct Live {
    output: PathBuf,
    state: Option<(WgpuRenderer, RenderContext)>,
    rows: Vec<serde_json::Value>,
    watchdog: Instant,
}
impl Live {
    fn snapshot(&mut self) {
        let (renderer, ctx) = self.state.as_mut().unwrap();
        renderer.begin_frame();
        renderer.add_instructions(ctx);
        let ids: Vec<_> = renderer
            .displayed_geometry()
            .iter()
            .filter_map(|&index| {
                let inst = &ctx.raw_instructions()[index];
                let center = ctx.scaler.world_to_screen(WorldPoint::new(5., 5.));
                ferrite_render::hit_geometry_visible(
                    inst,
                    &ctx.scaler,
                    center,
                    8.,
                    renderer.displayed_line_spans(index),
                )
                .and_then(|_| inst.feature_id())
            })
            .collect();
        let stage = self.rows.len();
        assert_eq!(ids, if stage == 1 { vec![2] } else { vec![1] });
        let file = format!("live-{stage}.png");
        renderer.save_screenshot(self.output.join(&file)).unwrap();
        self.rows.push(serde_json::json!({"stage":stage,"file":file,"hit_feature_ids":ids,"observed_utc":chrono::Utc::now().to_rfc3339(),"temporal_visibility_counts":renderer.temporal_visibility_counts()}));
    }
}
impl ApplicationHandler for Live {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Live temporal regression")
                    .with_inner_size(PhysicalSize::new(960, 640)),
            )
            .unwrap(),
        );
        let size = window.inner_size();
        let mut renderer = pollster::block_on(WgpuRenderer::new(window)).unwrap();
        renderer.background_color = Color::WHITE;
        let mut ctx = RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(1., 5.), WorldPoint::new(9., 5.)])
                .with_feature_id(1)
                .with_priority(2)
                .with_style(LineStyle::solid(Color::rgb(0., 0., 1.), 12.)),
        ));
        ctx.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(3., 5.), WorldPoint::new(7., 5.)])
                .with_feature_id(2)
                .with_priority(8)
                .with_style(LineStyle::solid(Color::rgb(1., 0., 0.), 4.)),
        ));
        let now = chrono::Utc::now();
        let begin = (now + chrono::Duration::seconds(1)).to_rfc3339();
        let end = (now + chrono::Duration::seconds(2)).to_rfc3339();
        let condition = ferrite_kernel::TemporalInterval::new(
            None,
            None,
            Some(
                ferrite_kernel::TemporalBounds::new(Some(begin.clone()), Some(end.clone()))
                    .unwrap(),
            ),
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        ctx.set_time_intervals_from(1, &[condition]);
        std::fs::create_dir_all(&self.output).unwrap();
        std::fs::write(
            self.output.join("bounds.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"begin":begin,"end":end})).unwrap(),
        )
        .unwrap();
        self.state = Some((renderer, ctx));
        self.watchdog = Instant::now() + Duration::from_secs(6);
        self.snapshot();
    }
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        assert!(
            Instant::now() < self.watchdog,
            "live timer failed to reach all stages"
        );
        if self
            .state
            .as_ref()
            .is_some_and(|(r, c)| r.temporal_conditions_changed(c))
        {
            self.snapshot();
        }
        if self.rows.len() == 3 {
            std::fs::write(
                self.output.join("metadata.json"),
                serde_json::to_vec_pretty(&self.rows).unwrap(),
            )
            .unwrap();
            el.exit();
            return;
        }
        let deadline = self
            .state
            .as_ref()
            .and_then(|(_, c)| c.next_live_temporal_change());
        let wake = deadline
            .map(|d| {
                let delta = (d - chrono::Utc::now().fixed_offset())
                    .to_std()
                    .unwrap_or_default();
                Instant::now() + delta.max(Duration::from_nanos(1))
            })
            .unwrap_or(self.watchdog)
            .min(self.watchdog);
        el.set_control_flow(ControlFlow::WaitUntil(wake));
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    EventLoop::new()
        .unwrap()
        .run_app(&mut Live {
            output,
            state: None,
            rows: Vec::new(),
            watchdog: Instant::now() + Duration::from_secs(10),
        })
        .unwrap();
}
