//! Hidden background-test diagnostic: drive a scripted drag, flick and
//! zoom-out through the production navigation path (`pan_camera_by`, motion
//! preview, background scene builds) at a 60 Hz wall-clock cadence, and
//! record per-frame timing and how much of the chart viewport the moving
//! preview showed from outside the prepared scene (drawn as the placeholder).
//! `FERRITE_NAV_MOTION_AUDIT=<dir>` with `FERRITE_BACKGROUND_TEST=1`.
//! `FERRITE_NAV_MOTION_SHOTS=0` skips the mid-motion screenshots (timing runs).
use super::*;

const FRAME: std::time::Duration = std::time::Duration::from_micros(16_667);

/// Audit-only A/B switch: prepare scenes without the navigation overscan.
static OVERSCAN_SUPPRESSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(crate) fn overscan_suppressed() -> bool {
    OVERSCAN_SUPPRESSED.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Default)]
pub(crate) struct SceneBuildStats {
    pub started: u64,
    pub installed: u64,
    pub discarded: u64,
    pub times_ms: Vec<f64>,
}
impl SceneBuildStats {
    pub fn record(&mut self, time: std::time::Duration, installed: bool) {
        if installed {
            self.installed += 1;
        } else {
            self.discarded += 1;
        }
        if self.times_ms.len() < 100_000 {
            self.times_ms.push(time.as_secs_f64() * 1e3);
        }
    }
}

#[derive(Clone, Copy)]
enum Step {
    /// Mouse motion in physical pixels this frame.
    Drag([f64; 2]),
    /// Zoom-out about the viewport centre by this factor this frame.
    ZoomOut(f64),
}

struct Phase {
    name: &'static str,
    steps: Vec<Step>,
    shots: &'static [usize],
}

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.)
        .unwrap_or(default)
}

impl ChartApp {
    fn motion_audit_frame(&mut self) -> Result<f64> {
        let start = std::time::Instant::now();
        self.poll_scene_build(false);
        anyhow::ensure!(
            self.flush_navigation_scene(),
            "Deferred navigation scene flush failed"
        );
        self.renderer.as_mut().context("No renderer")?.render()?;
        Ok(start.elapsed().as_secs_f64() * 1e3)
    }

    /// The release path: settle (background or UI-thread) until the shown
    /// frame is the exact scene of the stationary camera.
    fn motion_audit_settle(&mut self) -> Result<(usize, f64)> {
        let start = std::time::Instant::now();
        self.is_dragging = false;
        self.pan_velocity = (0., 0.);
        self.pan_rebuild_phase = 2;
        for frame in 0..1200 {
            let begin = std::time::Instant::now();
            self.poll_scene_build(false);
            if self.pan_rebuild_phase == 2 && self.settle_scene() {
                self.pan_rebuild_phase = 1;
            } else if self.pan_rebuild_phase == 1 && self.scene_build.is_none() {
                self.build_rendered_symbols();
                self.pan_rebuild_phase = 0;
            }
            self.renderer.as_mut().context("No renderer")?.render()?;
            let exact = self.pan_rebuild_phase == 0
                && self.scene_build.is_none()
                && self.renderer.as_ref().is_some_and(|r| {
                    let (pan, zoom, _) = r.fast_view_transform();
                    pan == (0., 0.)
                        && zoom == 1.
                        && !r.motion_preview_active()
                        && r.geometry_matches_view(&self.engine.render_context.scaler)
                });
            if exact {
                return Ok((frame, start.elapsed().as_secs_f64() * 1e3));
            }
            // The event loop's reconciliation of a scene left with no settle
            // scheduled (e.g. a background scene a rounding step off camera).
            if self.pan_rebuild_phase == 0 && self.scene_build.is_none() {
                if let Some(renderer) = &mut self.renderer {
                    renderer.reset_pan_offset();
                }
                self.update_view_ex(false, false);
            }
            if let Some(rest) = FRAME.checked_sub(begin.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        let r = self.renderer.as_ref().context("No renderer")?;
        anyhow::bail!(
            "Navigation did not settle: phase {} build {} transform {:?} preview {} matches {}",
            self.pan_rebuild_phase,
            self.scene_build.is_some(),
            r.fast_view_transform(),
            r.motion_preview_active(),
            r.geometry_matches_view(&self.engine.render_context.scaler)
        )
    }

    pub(super) fn audit_navigation_motion(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_ui::background_test::enabled(),
            "Navigation motion audit requires FERRITE_BACKGROUND_TEST=1"
        );
        std::fs::create_dir_all(output)?;
        let shots = std::env::var("FERRITE_NAV_MOTION_SHOTS").as_deref() != Ok("0");
        self.ensure_navigation_scene()?;
        // Reconcile the settled viewport, then capture the starting frame.
        for _ in 0..4 {
            self.renderer.as_mut().context("No renderer")?.render()?;
            if self.sync_chart_layout() {
                self.update_view();
            }
        }
        let mut idle_ms = Vec::new();
        for _ in 0..10 {
            idle_ms.push(self.motion_audit_frame()?);
        }
        // Settled (exact) scene work at the starting camera: coverage
        // preparation alone, and the whole UI-thread rebuild.
        let mut coverage_ms = Vec::new();
        let mut rebuild_ms = Vec::new();
        let (extent, density) = {
            let window = self.renderer.as_ref().context("No renderer")?.ui.window();
            (window.inner_size(), window.scale_factor())
        };
        let repeats = env_f64("FERRITE_NAV_MOTION_REBUILDS", 7.) as usize;
        // Interleaved A/B in this process: the same exact scene prepared
        // without the overscan (coverage ring, ring geometry).
        let mut coverage_off_ms = Vec::new();
        let mut rebuild_off_ms = Vec::new();
        let mut stats_off = None;
        if ferrite_gpu::navigation_overscan_fraction().is_some() {
            for _ in 0..repeats {
                OVERSCAN_SUPPRESSED.store(true, std::sync::atomic::Ordering::Relaxed);
                let mut fork = self.engine.render_context.fork_for_emission();
                fork.invalidate_coverage_view();
                let start = std::time::Instant::now();
                prepare_flat_coverage(
                    self.engine.s101.coverage_inventory.as_deref(),
                    &mut fork,
                    extent,
                    density,
                )?;
                coverage_off_ms.push(start.elapsed().as_secs_f64() * 1e3);
                let start = std::time::Instant::now();
                self.engine.render_context.invalidate_coverage_view();
                self.update_view_ex(false, false);
                rebuild_off_ms.push(start.elapsed().as_secs_f64() * 1e3);
                stats_off = self
                    .renderer
                    .as_ref()
                    .map(|r| format!("{:?}", r.statistics()));
                OVERSCAN_SUPPRESSED.store(false, std::sync::atomic::Ordering::Relaxed);
                let mut fork = self.engine.render_context.fork_for_emission();
                fork.invalidate_coverage_view();
                let start = std::time::Instant::now();
                prepare_flat_coverage(
                    self.engine.s101.coverage_inventory.as_deref(),
                    &mut fork,
                    extent,
                    density,
                )?;
                coverage_ms.push(start.elapsed().as_secs_f64() * 1e3);
                let start = std::time::Instant::now();
                self.engine.render_context.invalidate_coverage_view();
                self.update_view_ex(false, false);
                rebuild_ms.push(start.elapsed().as_secs_f64() * 1e3);
            }
        }
        for _ in 0..if rebuild_ms.is_empty() { repeats } else { 0 } {
            let mut fork = self.engine.render_context.fork_for_emission();
            fork.invalidate_coverage_view();
            let start = std::time::Instant::now();
            prepare_flat_coverage(
                self.engine.s101.coverage_inventory.as_deref(),
                &mut fork,
                extent,
                density,
            )?;
            coverage_ms.push(start.elapsed().as_secs_f64() * 1e3);
            let start = std::time::Instant::now();
            self.engine.render_context.invalidate_coverage_view();
            self.update_view_ex(false, false);
            rebuild_ms.push(start.elapsed().as_secs_f64() * 1e3);
        }
        let coverage_present = self
            .engine
            .render_context
            .prepared_coverage()
            .ok()
            .flatten()
            .is_some();
        let overscan_mask_bytes = self
            .engine
            .render_context
            .prepared_coverage()
            .ok()
            .flatten()
            .and_then(|p| p.pass(0).ok())
            .and_then(|pass| {
                pass.frame()
                    .overscan()
                    .map(|o| (o.pixel_bytes(), o.ring_only_count()))
            });
        let (_, _, unique_clips, logical_mask_bytes, unique_mask_bytes) = self
            .renderer
            .as_ref()
            .unwrap()
            .coverage_clip_cse_statistics();
        self.motion_audit_frame()?;
        if shots {
            self.renderer
                .as_mut()
                .unwrap()
                .save_screenshot(output.join("00-settled-start.png"))?;
        }
        let drag = env_f64("FERRITE_NAV_MOTION_DRAG_PX_S", 1500.) / 60.;
        let flick = env_f64("FERRITE_NAV_MOTION_FLICK_PX_S", 4000.) / 60.;
        let zoom = env_f64("FERRITE_NAV_MOTION_ZOOM_OUT", 3.).powf(1. / 12.);
        let fling = env_f64("FERRITE_NAV_MOTION_FLING_PX_S", 24000.) / 60.;
        let phases = [
            Phase {
                name: "drag",
                steps: vec![Step::Drag([-drag, 0.]); 60],
                shots: &[2, 8, 30, 59],
            },
            Phase {
                name: "flick",
                steps: vec![Step::Drag([flick * 0.8, flick * 0.6]); 20],
                shots: &[3, 10, 19],
            },
            Phase {
                name: "zoom_out",
                steps: vec![Step::ZoomOut(zoom); 12],
                shots: &[4, 11],
            },
            // Beyond any background build: the placeholder's cases.
            Phase {
                name: "fling",
                steps: vec![Step::Drag([-fling, 0.]); 8],
                shots: &[0, 1, 2, 7],
            },
            Phase {
                name: "zoom_out_fast",
                steps: vec![Step::ZoomOut(4f64.powf(1. / 3.)); 3],
                shots: &[0, 1, 2],
            },
        ];
        let stats_before = (
            self.scene_build_stats.started,
            self.scene_build_stats.installed,
            self.scene_build_stats.discarded,
            self.scene_build_stats.times_ms.len(),
        );
        let mut phase_rows = Vec::new();
        let mut frames = Vec::new();
        let phases = if std::env::var("FERRITE_NAV_MOTION_SETTLED_ONLY").as_deref() == Ok("1") {
            Vec::new()
        } else {
            phases.into_iter().collect()
        };
        let start_camera = (self.pan_offset, self.zoom_level);
        for (phase_index, phase) in phases.iter().enumerate() {
            // Every phase starts from the same settled camera.
            self.poll_scene_build(true);
            (self.pan_offset, self.zoom_level) = start_camera;
            self.zoom_target = self.zoom_level;
            let bounds = self
                .rebuild_camera_bounds()
                .context("Invalid start camera")?;
            self.engine.render_context.zoom_to_fit(bounds);
            if let Some(renderer) = &mut self.renderer {
                renderer.reset_pan_offset();
            }
            self.update_view_ex(false, false);
            self.motion_audit_settle()?;
            let builds_before = (
                self.scene_build_stats.started,
                self.scene_build_stats.installed,
                self.scene_build_stats.times_ms.len(),
            );
            self.is_dragging = true;
            let mut ms = Vec::new();
            let mut unprepared = Vec::new();
            let mut previewed = 0usize;
            let mut captured_large = false;
            for (index, step) in phase.steps.iter().enumerate() {
                let begin = std::time::Instant::now();
                match *step {
                    Step::Drag([dx, dy]) => {
                        let scaler = &self.engine.render_context.scaler;
                        let world = [-dx / scaler.scale_x(), dy / scaler.scale_y()];
                        self.pan_camera_by(world);
                    }
                    Step::ZoomOut(factor) => {
                        self.zoom_level = (self.zoom_level / factor)
                            .clamp(navigation::MIN_ZOOM, navigation::MAX_ZOOM);
                        self.zoom_target = self.zoom_level;
                        let bounds = self
                            .rebuild_camera_bounds()
                            .context("Invalid zoom camera")?;
                        self.engine.render_context.zoom_to_fit(bounds);
                        if !self.preview_or_background_build() {
                            self.update_view_ex(false, true);
                        }
                    }
                }
                let handler_ms = begin.elapsed().as_secs_f64() * 1e3;
                let frame_ms = self.motion_audit_frame()? + handler_ms;
                let renderer = self.renderer.as_ref().unwrap();
                let (margin, fraction) = renderer.navigation_preparation();
                let preview = renderer.motion_preview_active();
                previewed += preview as usize;
                ms.push(frame_ms);
                unprepared.push(fraction as f64);
                frames.push(serde_json::json!({
                    "phase": phase.name, "frame": index, "ms": frame_ms,
                    "handler_ms": handler_ms, "preview": preview,
                    "build_pending": self.scene_build.is_some(),
                    "overscan_margin": margin, "unprepared_fraction": fraction,
                    "builds_started": self.scene_build_stats.started,
                    "builds_installed": self.scene_build_stats.installed,
                }));
                // Also the first frame that shows a large unprepared area.
                let large = fraction > 0.08 && !captured_large;
                captured_large |= large;
                if shots && (phase.shots.contains(&index) || large) {
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .save_screenshot(output.join(format!(
                            "{:02}-{}-{index:02}.png",
                            phase_index + 1,
                            phase.name
                        )))?;
                }
                if let Some(rest) = FRAME.checked_sub(begin.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            let (settle_frames, settle_ms) = self.motion_audit_settle()?;
            if shots {
                self.renderer
                    .as_mut()
                    .unwrap()
                    .save_screenshot(output.join(format!(
                        "{:02}-{}-settled.png",
                        phase_index + 1,
                        phase.name
                    )))?;
            }
            let times = &self.scene_build_stats.times_ms[builds_before.2..];
            let revealed: Vec<f64> = unprepared.iter().copied().filter(|f| *f > 0.).collect();
            phase_rows.push(serde_json::json!({
                "phase": phase.name, "frames": phase.steps.len(),
                "frame_ms_p50": percentile(&ms, 0.5), "frame_ms_p95": percentile(&ms, 0.95),
                "frame_ms_max": percentile(&ms, 1.),
                "preview_frames": previewed,
                "frames_with_unprepared": revealed.len(),
                "unprepared_fraction_mean": unprepared.iter().sum::<f64>() / unprepared.len().max(1) as f64,
                "unprepared_fraction_max": percentile(&unprepared, 1.),
                "builds_started": self.scene_build_stats.started - builds_before.0,
                "builds_installed": self.scene_build_stats.installed - builds_before.1,
                "build_ms_mean": times.iter().sum::<f64>() / times.len().max(1) as f64,
                "build_ms_max": percentile(times, 1.),
                "settle_frames": settle_frames, "settle_ms": settle_ms,
            }));
        }
        let renderer = self.renderer.as_ref().unwrap();
        let summary = serde_json::json!({
            "scope": "Hidden background-test navigation motion audit; 60 Hz wall-clock cadence; frame ms = input handler + scene poll + render (CPU submit, not GPU duration)",
            "overscan_fraction": ferrite_gpu::navigation_overscan_fraction(),
            "prefetch_env": std::env::var("FERRITE_NAV_PREFETCH").ok(),
            "placeholder_env": std::env::var("FERRITE_NAV_PLACEHOLDER").ok(),
            "viewport": renderer.chart_viewport_pixels(),
            "idle_frame_ms_p50": percentile(&idle_ms, 0.5),
            "settled_coverage_prepare_ms": {"p50": percentile(&coverage_ms, 0.5), "min": percentile(&coverage_ms, 0.), "mean": coverage_ms.iter().sum::<f64>() / coverage_ms.len().max(1) as f64},
            "settled_without_overscan_ms": {
                "coverage_p50": percentile(&coverage_off_ms, 0.5),
                "rebuild_p50": percentile(&rebuild_off_ms, 0.5),
                "rebuild_mean": rebuild_off_ms.iter().sum::<f64>() / rebuild_off_ms.len().max(1) as f64,
            },
            "settled_rebuild_ms": {"p50": percentile(&rebuild_ms, 0.5), "min": percentile(&rebuild_ms, 0.), "mean": rebuild_ms.iter().sum::<f64>() / rebuild_ms.len().max(1) as f64},
            "coverage_prepared": coverage_present,
            "scene_statistics": {
                "without_overscan": stats_off,
                "with_overscan": self.renderer.as_ref().map(|r| format!("{:?}", r.statistics())),
            },
            "coverage_gpu": {"unique_clips": unique_clips, "logical_mask_bytes": logical_mask_bytes, "unique_mask_bytes": unique_mask_bytes},
            "overscan_masks": overscan_mask_bytes.map(|(bytes, ring_only)| serde_json::json!({"cpu_bytes_per_copy": bytes, "ring_only_datasets": ring_only})),
            "phases": phase_rows,
            "builds": {
                "started": self.scene_build_stats.started - stats_before.0,
                "installed": self.scene_build_stats.installed - stats_before.1,
                "discarded": self.scene_build_stats.discarded - stats_before.2,
                "ms": &self.scene_build_stats.times_ms[stats_before.3..],
            },
            "frames": frames,
        });
        std::fs::write(
            output.join("navigation-motion.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;
        info!("Navigation motion audit written to {}", output.display());
        Ok(())
    }
}
