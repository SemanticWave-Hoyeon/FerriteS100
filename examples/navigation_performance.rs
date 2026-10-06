//! Real instruction stream, moving cameras, actual GPU submissions. No synthetic FPS claim.
use ferrite_render::{FlatProjection, GeoBounds, RenderContext, ScreenPoint, Viewport, WorldPoint};
use ferrite_wgpu::{SymbolCache, WgpuRenderer};
use std::{path::PathBuf, sync::Arc, time::Instant};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct App {
    input: PathBuf,
    pc: PathBuf,
    out: PathBuf,
    gpu_projection: bool,
    cache_areas: bool,
    cache_curves: bool,
    cache_curve_bounds: bool,
    cache_dyadic_samples: bool,
    reuse_curve_scratch: bool,
    cache_dependencies: bool,
    eager_selection: bool,
    spatial_hierarchy: bool,
    partial_view: bool,
    state: Option<Audit>,
}
struct Audit {
    window: Arc<Window>,
    r: WgpuRenderer,
    c: RenderContext,
    cache: SymbolCache,
    groups: std::collections::HashSet<u32>,
    profile: ferrite_portrayal_catalog::ColorProfile,
    mode: usize,
    frame: i32,
    initial_ms: f64,
    rows: Vec<serde_json::Value>,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, e: &ActiveEventLoop) {
        std::fs::create_dir_all(&self.out).unwrap();
        let w = Arc::new(
            e.create_window(
                Window::default_attributes()
                    .with_title("Chart navigation performance audit")
                    .with_inner_size(PhysicalSize::new(3420, 2082)),
            )
            .unwrap(),
        );
        w.focus_window();
        let mut r = pollster::block_on(WgpuRenderer::new(w.clone())).unwrap();
        r.set_spatial_hierarchy_enabled(self.spatial_hierarchy);
        r.set_globe_area_cache_enabled(self.cache_areas);
        r.set_globe_curve_cache_enabled(self.cache_curves);
        r.set_globe_curve_bounds_cache_enabled(self.cache_curve_bounds);
        r.set_globe_dyadic_sample_cache_enabled(self.cache_dyadic_samples);
        r.set_globe_curve_scratch_reuse_enabled(self.reuse_curve_scratch);
        r.set_profiling_enabled(true);
        r.set_globe_gpu_projection_enabled(self.gpu_projection);
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&self.pc).unwrap();
        let profile = pc.color_profiles.profiles.get("Day").unwrap();
        let mut cache = SymbolCache::new(pc.root_path.join("Symbols"));
        let mut groups =
            ferrite_s101::viewing_groups_for_preset(&pc, ferrite_s101::DisplayPreset::Standard)
                .unwrap();
        groups.extend(ferrite_s101::viewing_groups_for_layers(&pc, ["101", "102"]).unwrap());
        let instructions = bincode::deserialize(&std::fs::read(&self.input).unwrap()).unwrap();
        let mut c = RenderContext::new(Viewport::with_origin(0., 80., 2780., 1934.));
        c.scaler.set_projection(FlatProjection::EllipsoidalMercator);
        c.settings.current_datetime = Some("2026-10-04T09:30:00+09:00".into());
        c.set_instructions_from_cache(instructions);
        c.set_dependency_plan_cache_enabled(self.cache_dependencies);
        let base = GeoBounds::new(-3.24799, 48.35, -1.32811, 50.15);
        c.set_bounds(base);
        self.state = Some(Audit {
            window: w.clone(),
            r,
            c,
            cache,
            groups,
            profile: profile.clone(),
            mode: 0,
            frame: -1,
            initial_ms: 0.,
            rows: vec![],
        });
        w.request_redraw();
    }
    fn window_event(&mut self, e: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(s) = self.state.as_mut() else {
            return;
        };
        if id != s.window.id() {
            return;
        }
        if matches!(event, WindowEvent::CloseRequested) {
            e.exit();
            return;
        }
        if !matches!(event, WindowEvent::RedrawRequested) {
            return;
        }
        let Audit {
            window,
            r,
            c,
            cache,
            groups,
            profile,
            mode,
            frame,
            initial_ms,
            rows,
        } = s;
        let label = ["flat", "globe"][*mode];
        let base = GeoBounds::new(-3.24799, 48.35, -1.32811, 50.15);
        let cpu_before = r.cpu_profiler.cumulative_snapshot();
        let start = Instant::now();
        if *frame == -1 {
            c.set_bounds(base);
            r.ui_state.globe_preview = *mode == 1;
            r.ui_state.globe_pose = None;
            r.begin_frame();
            if *mode == 1 {
                r.prepare_globe_with_symbols(c, Some(groups), cache, Some(profile))
                    .unwrap();
            } else {
                r.set_lon_wrap_pixels(360. * c.scaler.scale_x() as f32);
                r.add_instructions_with_symbols(c, Some(cache), Some(profile), Some(groups));
            }
            *initial_ms = start.elapsed().as_secs_f64() * 1000.;
        } else if *mode == 1 {
            let a = WorldPoint::new(-2.28805, 49.25825199486892);
            assert!(r.move_globe_anchor(
                a,
                ScreenPoint::new(1390., 1047.),
                if self.partial_view { 1.3 } else { 1.01 }
            ));
            r.begin_frame();
            r.prepare_globe_with_symbols(c, Some(groups), cache, Some(profile))
                .unwrap();
        } else {
            let bounds = FlatProjection::EllipsoidalMercator
                .view_bounds(
                    base,
                    (if self.partial_view { 1.3_f64 } else { 1.01_f64 }).powi(*frame + 1),
                    [0., 0.],
                )
                .unwrap();
            c.set_bounds(bounds);
            if !r.set_gpu_view_scaler(&c.scaler) {
                r.begin_frame();
                r.set_lon_wrap_pixels(360. * c.scaler.scale_x() as f32);
                r.add_instructions_with_symbols(c, Some(cache), Some(profile), Some(groups));
            }
        }
        // Controlled reference: reproduce the former eager picking-index build
        // using the same binary, camera, final geometry and suppression plan.
        if *mode == 0 && self.eager_selection {
            std::hint::black_box(r.selection_candidates_in_context(
                c,
                ScreenPoint::new(0., 0.),
                0.,
            ));
        }
        let camera_and_geometry_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        r.render().unwrap();
        r.state.device.poll(wgpu::Maintain::Wait);
        let submitted_frame_ms = start.elapsed().as_secs_f64() * 1000.;
        if *frame == -1 {
            r.save_screenshot(self.out.join(format!("{label}-initial.png")))
                .unwrap();
        } else {
            let cpu_scopes: std::collections::BTreeMap<_, _> = r
                .cpu_profiler
                .cumulative_snapshot()
                .into_iter()
                .map(|(name, (ms, calls))| {
                    let before = cpu_before.get(name).copied().unwrap_or_default();
                    (
                        name,
                        serde_json::json!({"ms": ms-before.0, "calls": calls-before.1}),
                    )
                })
                .collect();
            rows.push(serde_json::json!({"cpu_scopes":cpu_scopes,
                "dependency_iterations":r.dependency_render_status().iterations,
                "partial_view":self.partial_view,"spatial_hierarchy_enabled":self.spatial_hierarchy,"eager_selection_reference":self.eager_selection,"selection_index":r.selection_index_stats(),"mode":label,"frame":*frame,
                "initial_preparation_ms":*initial_ms,"camera_and_geometry_ms":camera_and_geometry_ms,
                "submitted_frame_ms_including_present_and_gpu_wait":submitted_frame_ms,
                "total_ms":camera_and_geometry_ms+submitted_frame_ms,
                "gpu_projection_requested":self.gpu_projection,"event_driven":true,"dependency_plan_cache_enabled":self.cache_dependencies,"flat_suppression_cache_bytes":r.flat_line_suppression_cache_bytes(),"text_buffers":r.chart_text_buffer_statistics(),
                "globe":r.globe_preview_diagnostics().map(|d|d.as_json())}));
        }
        *frame += 1;
        if *frame == 16 {
            r.save_screenshot(self.out.join(format!("{label}-final.png")))
                .unwrap();
            *mode += 1;
            *frame = -1;
            if *mode == 2 {
                std::fs::write(
                    self.out.join("timings.json"),
                    serde_json::to_vec_pretty(rows).unwrap(),
                )
                .unwrap();
                r.flush_profiler();
                e.exit();
                return;
            }
        }
        window.request_redraw();
    }
}
fn main() {
    let a: Vec<_> = std::env::args().collect();
    assert!(
        a.len() >= 4
            && a[4..].iter().all(|v| v == "--uncached-areas"
                || v == "--eager-selection"
                || v == "--uncached-curves"
                || v == "--uncached-curve-bounds"
                || v == "--uncached-dependencies"
                || v == "--no-spatial-hierarchy"
                || v == "--partial-view"
                || v == "--dyadic-curve-samples"
                || v == "--reuse-curve-scratch"
                || v == "--gpu-projection"),
        "instructions.bin PC output-directory [--uncached-areas] [--eager-selection] [--uncached-curves] [--uncached-dependencies] [--gpu-projection]"
    );
    EventLoop::new()
        .unwrap()
        .run_app(&mut App {
            input: PathBuf::from(&a[1]),
            pc: PathBuf::from(&a[2]),
            out: PathBuf::from(&a[3]),
            gpu_projection: a.iter().any(|v| v == "--gpu-projection"),
            cache_dependencies: !a.iter().any(|v| v == "--uncached-dependencies"),
            cache_curves: !a.iter().any(|v| v == "--uncached-curves"),
            cache_curve_bounds: !a.iter().any(|v| v == "--uncached-curve-bounds"),
            cache_dyadic_samples: a.iter().any(|v| v == "--dyadic-curve-samples"),
            reuse_curve_scratch: a.iter().any(|v| v == "--reuse-curve-scratch"),
            cache_areas: !a.iter().any(|v| v == "--uncached-areas"),
            eager_selection: a.iter().any(|v| v == "--eager-selection"),
            spatial_hierarchy: !a.iter().any(|v| v == "--no-spatial-hierarchy"),
            partial_view: a.iter().any(|v| v == "--partial-view"),
            state: None,
        })
        .unwrap();
}
