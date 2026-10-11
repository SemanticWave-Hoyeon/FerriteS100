//! Performance profiler for CPU and GPU timing
//!
//! Tracks hot-path timings and periodically logs a summary.
//! Also wraps wgpu-profiler for GPU-side timestamp queries.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Global flag to enable/disable profiling at runtime
static PROFILING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Check if profiling is enabled
#[inline]
pub fn is_profiling_enabled() -> bool {
    PROFILING_ENABLED.load(Ordering::Relaxed)
}

/// Enable or disable profiling
pub fn set_profiling_enabled(enabled: bool) {
    PROFILING_ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        tracing::info!("[PROFILER] Profiling ENABLED — timing data will be logged");
    } else {
        tracing::info!("[PROFILER] Profiling DISABLED");
    }
}

/// Accumulated timing statistics for a single scope
#[derive(Clone, Debug)]
struct ScopeStat {
    total: Duration,
    count: u64,
    min: Duration,
    max: Duration,
}

impl ScopeStat {
    fn new() -> Self {
        Self {
            total: Duration::ZERO,
            count: 0,
            min: Duration::from_secs(u64::MAX),
            max: Duration::ZERO,
        }
    }

    fn record(&mut self, elapsed: Duration) {
        self.total += elapsed;
        self.count += 1;
        if elapsed < self.min {
            self.min = elapsed;
        }
        if elapsed > self.max {
            self.max = elapsed;
        }
    }

    fn avg(&self) -> Duration {
        if self.count == 0 {
            Duration::ZERO
        } else {
            self.total / self.count as u32
        }
    }
}

/// Bounded recent CPU frame durations. Exact quantiles within this window only;
/// unavailable presentation timing must never be labelled physical display FPS.
struct FrameDistribution {
    ns: [u64; 4096],
    used: usize,
    next: usize,
    total: u64,
    over_60hz: u64,
    over_144hz: u64,
}
impl FrameDistribution {
    fn new() -> Self {
        Self {
            ns: [0; 4096],
            used: 0,
            next: 0,
            total: 0,
            over_60hz: 0,
            over_144hz: 0,
        }
    }
    fn record(&mut self, d: Duration) {
        let n = d.as_nanos().min(u64::MAX as u128) as u64;
        self.ns[self.next] = n;
        self.next = (self.next + 1) % self.ns.len();
        self.used = (self.used + 1).min(self.ns.len());
        self.total = self.total.saturating_add(1);
        // Compare to rational refresh budgets without rounding nanoseconds.
        self.over_60hz += u64::from(u128::from(n) * 60 > 1_000_000_000);
        self.over_144hz += u64::from(u128::from(n) * 144 > 1_000_000_000);
    }
    fn report(&self) -> String {
        if self.used == 0 {
            return "CPU frame distribution unavailable".into();
        }
        let mut values = self.ns; // fixed stack scratch, no per-frame allocation
        values[..self.used].sort_unstable();
        let q = |numerator: usize| {
            values[(self.used * numerator).div_ceil(100).saturating_sub(1)] as f64 / 1e6
        };
        format!("CPU frame_total recent_p95_ms={:.3} recent_p99_ms={:.3} recent_samples={} interval_samples={} overwritten={} over_60hz_budget_pct={:.2} over_144hz_budget_pct={:.2}; CPU wall scope only, not physical presentation FPS",
            q(95), q(99), self.used, self.total, self.total.saturating_sub(self.used as u64),
            self.over_60hz as f64 * 100.0 / self.total as f64,
            self.over_144hz as f64 * 100.0 / self.total as f64)
    }
}

/// CPU-side performance profiler
///
/// Accumulates timing data for named scopes and periodically logs summaries.
pub struct CpuProfiler {
    scopes: BTreeMap<&'static str, ScopeStat>,
    navigation_events: BTreeMap<&'static str, u64>,
    /// Cumulative stats that never reset (for exit report)
    cumulative: BTreeMap<&'static str, ScopeStat>,
    cumulative_frame_count: u64,
    session_start: Instant,
    last_report: Instant,
    report_interval: Duration,
    frame_count: u64,
    frame_start: Option<Instant>,
    frame_distribution: FrameDistribution,
    navigation_frame: bool,
    previous_navigation_entry: Option<Instant>,
    previous_navigation_wall: Option<Duration>,
    debug_window: crate::debug_metrics::Window,
    /// Per-frame timing log for detailed analysis (last N frames)
    #[allow(dead_code)]
    frame_log: Vec<FrameTimings>,
    #[allow(dead_code)]
    max_frame_log: usize,
}

/// Detailed timing for a single frame
#[derive(Clone, Debug)]
pub struct FrameTimings {
    pub frame_number: u64,
    pub total_ms: f64,
    pub sections: Vec<(&'static str, f64)>, // (name, ms)
}

impl Default for CpuProfiler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuProfiler {
    pub fn new() -> Self {
        Self {
            scopes: BTreeMap::new(),
            navigation_events: BTreeMap::new(),
            cumulative: BTreeMap::new(),
            cumulative_frame_count: 0,
            session_start: Instant::now(),
            last_report: Instant::now(),
            report_interval: Duration::from_secs(5), // Log every 5 seconds
            frame_count: 0,
            frame_start: None,
            frame_distribution: FrameDistribution::new(),
            navigation_frame: false,
            previous_navigation_entry: None,
            previous_navigation_wall: None,
            debug_window: crate::debug_metrics::Window::new(),
            frame_log: Vec::with_capacity(300),
            max_frame_log: 300, // Keep last 300 frames (~5s at 60fps)
        }
    }

    /// Start timing a named scope. Returns a guard that records on drop.
    #[inline]
    pub fn begin_scope(&mut self, name: &'static str) -> ScopeGuard<'_> {
        ScopeGuard {
            profiler: self,
            name,
            start: Instant::now(),
        }
    }

    /// Record a timing for a named scope (manual, without guard)
    #[inline]
    pub fn record(&mut self, name: &'static str, elapsed: Duration) {
        self.scopes
            .entry(name)
            .or_insert_with(ScopeStat::new)
            .record(elapsed);
        self.cumulative
            .entry(name)
            .or_insert_with(ScopeStat::new)
            .record(elapsed);
    }

    /// Navigation attempt counts are events, never fictitious duration samples.
    pub(crate) fn record_navigation_event(&mut self, name: &'static str) {
        self.debug_window.event(name);
        let count = self.navigation_events.entry(name).or_default();
        *count = count.saturating_add(1);
    }

    /// Monotonic totals for measuring individual frames independently of log resets.
    pub fn cumulative_snapshot(&self) -> BTreeMap<&'static str, (f64, u64)> {
        self.cumulative
            .iter()
            .map(|(&name, stat)| (name, (stat.total.as_secs_f64() * 1000.0, stat.count)))
            .collect()
    }

    pub fn begin_navigation_frame(&mut self, navigation: bool) {
        self.navigation_frame = navigation;
        self.begin_frame();
    }
    pub(crate) fn reset_debug_metrics(&mut self) {
        self.debug_window = crate::debug_metrics::Window::new();
        self.previous_navigation_entry = None;
        self.previous_navigation_wall = None;
        self.navigation_frame = false;
    }
    pub(crate) fn update_debug_stats(
        &mut self,
        target: &mut crate::debug_metrics::CpuDebugMetrics,
    ) {
        target.navigation_active = self.navigation_frame;
        if let Some(window) = self.debug_window.take_if_due(Instant::now()) {
            target.navigation = window.summary;
            target.window_seconds = window.seconds;
            target.fastpath_attempts = window.attempts;
            target.fastpath_accepted = window.accepted;
            target.fastpath_rejections = window.rejections;
        }
    }

    /// Mark the beginning of a frame
    pub fn begin_frame(&mut self) {
        let now = Instant::now();
        if self.navigation_frame {
            if let (Some(entry), Some(wall)) = (
                self.previous_navigation_entry,
                self.previous_navigation_wall.take(),
            ) {
                self.debug_window
                    .record_pair(wall, now.checked_duration_since(entry));
            }
            self.previous_navigation_entry = Some(now);
        } else {
            self.previous_navigation_entry = None;
            self.previous_navigation_wall = None;
        }
        self.frame_start = Some(now);
        self.frame_count += 1;
        self.cumulative_frame_count += 1;
    }

    /// Mark the end of a frame and check if we should log a report
    pub fn end_frame(&mut self) {
        if let Some(start) = self.frame_start.take() {
            let elapsed = start.elapsed();
            self.record("frame_total", elapsed);
            self.frame_distribution.record(elapsed);
            if self.navigation_frame {
                self.debug_window.record(elapsed, None);
                self.previous_navigation_wall = Some(elapsed);
            }
        }

        // Periodic report
        let now = Instant::now();
        if now.duration_since(self.last_report) >= self.report_interval {
            self.log_report();
            self.last_report = now;
        }
    }

    /// Log the accumulated profiling report
    pub fn log_report(&mut self) {
        if self.scopes.is_empty() && self.navigation_events.is_empty() {
            return;
        }

        let mut report = String::with_capacity(2048);
        report.push_str("\n╔══════════════════════════════════════════════════════════════════╗\n");
        report.push_str("║                    CPU PROFILER REPORT                          ║\n");
        report.push_str("╠══════════════════════════════════════════════════════════════════╣\n");
        report.push_str(&format!(
            "║ Frames: {} | Report interval: {:.1}s\n",
            self.frame_count,
            self.report_interval.as_secs_f64()
        ));
        report.push_str("╠══════════════════════════════════════════════════════════════════╣\n");
        report.push_str(&format!(
            "║ {:<35} {:>7} {:>7} {:>7} {:>5} ║\n",
            "Scope", "Avg", "Min", "Max", "Count"
        ));
        report.push_str("╠══════════════════════════════════════════════════════════════════╣\n");

        for (name, stat) in &self.scopes {
            let avg_us = stat.avg().as_micros();
            let min_us = if stat.min == Duration::from_secs(u64::MAX) {
                0
            } else {
                stat.min.as_micros()
            };
            let max_us = stat.max.as_micros();

            // Format: us for <1ms, ms for >=1ms
            let fmt = |us: u128| -> String {
                if us >= 1000 {
                    format!("{:.1}ms", us as f64 / 1000.0)
                } else {
                    format!("{}us", us)
                }
            };

            report.push_str(&format!(
                "║ {:<35} {:>7} {:>7} {:>7} {:>5} ║\n",
                name,
                fmt(avg_us),
                fmt(min_us),
                fmt(max_us),
                stat.count
            ));
        }

        report.push_str("╚══════════════════════════════════════════════════════════════════╝");

        tracing::info!("{}", report);
        tracing::info!("[PROFILER] {}", self.frame_distribution.report());
        self.frame_distribution = FrameDistribution::new();

        for (reason, count) in &self.navigation_events {
            tracing::info!("[PROFILER_NAVIGATION] scaler_attempt_reason={} count={} interval_scope=since_previous_cpu_report", reason, count);
        }
        self.navigation_events.clear();

        // Reset stats for next interval
        self.scopes.clear();
        self.frame_count = 0;
    }

    /// Force-log the final report (call on shutdown)
    /// Outputs both the last interval report and the cumulative session report.
    pub fn flush(&mut self) {
        // Log remaining interval data
        if !self.scopes.is_empty() {
            self.log_report();
        }

        // Log cumulative session report
        self.log_cumulative_report();
    }

    /// Log the cumulative (full session) profiling report
    fn log_cumulative_report(&self) {
        if self.cumulative.is_empty() {
            return;
        }

        let session_secs = self.session_start.elapsed().as_secs_f64();

        let mut report = String::with_capacity(2048);
        report.push_str(
            "\n╔══════════════════════════════════════════════════════════════════════════╗\n",
        );
        report.push_str(
            "║                  CUMULATIVE SESSION PROFILER REPORT                     ║\n",
        );
        report.push_str(
            "╠══════════════════════════════════════════════════════════════════════════╣\n",
        );
        report.push_str(&format!(
            "║ Total frames: {} | Session duration: {:.1}s | Avg FPS: {:.1}\n",
            self.cumulative_frame_count,
            session_secs,
            if session_secs > 0.0 {
                self.cumulative_frame_count as f64 / session_secs
            } else {
                0.0
            },
        ));
        report.push_str(
            "╠══════════════════════════════════════════════════════════════════════════╣\n",
        );
        report.push_str(&format!(
            "║ {:<30} {:>7} {:>7} {:>7} {:>8} {:>8} ║\n",
            "Scope", "Avg", "Min", "Max", "Count", "Total"
        ));
        report.push_str(
            "╠══════════════════════════════════════════════════════════════════════════╣\n",
        );

        for (name, stat) in &self.cumulative {
            let avg_us = stat.avg().as_micros();
            let min_us = if stat.min == Duration::from_secs(u64::MAX) {
                0
            } else {
                stat.min.as_micros()
            };
            let max_us = stat.max.as_micros();
            let total_ms = stat.total.as_secs_f64() * 1000.0;

            let fmt = |us: u128| -> String {
                if us >= 1000 {
                    format!("{:.1}ms", us as f64 / 1000.0)
                } else {
                    format!("{}us", us)
                }
            };

            let fmt_total = |ms: f64| -> String {
                if ms >= 1000.0 {
                    format!("{:.2}s", ms / 1000.0)
                } else {
                    format!("{:.1}ms", ms)
                }
            };

            report.push_str(&format!(
                "║ {:<30} {:>7} {:>7} {:>7} {:>8} {:>8} ║\n",
                name,
                fmt(avg_us),
                fmt(min_us),
                fmt(max_us),
                stat.count,
                fmt_total(total_ms),
            ));
        }

        report.push_str(
            "╚══════════════════════════════════════════════════════════════════════════╝",
        );

        tracing::info!("{}", report);
    }
}

/// RAII guard that records timing on drop
pub struct ScopeGuard<'a> {
    profiler: &'a mut CpuProfiler,
    name: &'static str,
    start: Instant,
}

impl<'a> Drop for ScopeGuard<'a> {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.profiler.record(self.name, elapsed);
    }
}

/// Standalone scope timer (doesn't borrow the profiler)
/// Use this when you can't hold a mutable borrow on the profiler
pub struct ScopeTimer {
    pub name: &'static str,
    pub start: Instant,
}

impl ScopeTimer {
    #[inline]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }

    #[inline]
    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    #[inline]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// GPU profiler wrapper around wgpu-profiler
pub struct GpuProfilerWrapper {
    pub profiler: wgpu_profiler::GpuProfiler,
    enabled: bool,
    supported: bool,
    last_report: Instant,
    timings: BTreeMap<String, ScopeStat>,
    completed_samples: u64,
    last_duration_ms: Option<f64>,
    last_sample_at: Option<Instant>,
    debug_chart_window: crate::debug_metrics::Window,
    retained_world_completed_samples: u64,
    retained_world_last_duration_ms: Option<f64>,
}

impl GpuProfilerWrapper {
    pub fn new(device: &wgpu::Device) -> Self {
        let supported = device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let profiler = wgpu_profiler::GpuProfiler::new(wgpu_profiler::GpuProfilerSettings {
            enable_timer_queries: supported,
            ..Default::default()
        })
        .unwrap_or_else(|e| {
            tracing::warn!(
                "[GPU_PROFILER] Failed to create: {}. Using disabled profiler.",
                e
            );
            wgpu_profiler::GpuProfiler::new(wgpu_profiler::GpuProfilerSettings {
                enable_timer_queries: false,
                ..Default::default()
            })
            .unwrap()
        });

        Self {
            profiler,
            enabled: false,
            supported,
            last_report: Instant::now(),
            timings: BTreeMap::new(),
            completed_samples: 0,
            last_duration_ms: None,
            last_sample_at: None,
            debug_chart_window: crate::debug_metrics::Window::new(),
            retained_world_completed_samples: 0,
            retained_world_last_duration_ms: None,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        let next = enabled && self.supported;
        if next != self.enabled {
            self.last_duration_ms = None;
            self.last_sample_at = None;
            self.debug_chart_window = crate::debug_metrics::Window::new();
        }
        self.enabled = next;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn update_debug_stats(
        &mut self,
        target: &mut crate::egui_integration::DebugGpuStats,
    ) {
        target.timestamp_supported = self.supported;
        target.timing_enabled = self.enabled;
        if let Some(window) = self.debug_chart_window.take_if_due(Instant::now()) {
            target.chart_ms = window.summary.as_ref().map(|s| s.mean_ms);
            target.chart_max_ms = window.summary.as_ref().map(|s| s.max_ms);
            target.chart_window_samples = window.summary.as_ref().map_or(0, |s| s.samples);
            target.chart_window_seconds = window.seconds;
        }
        if !self.enabled || self.last_sample_at.is_none() {
            target.chart_ms = None;
            target.chart_max_ms = None;
            target.chart_window_samples = 0;
        }
        target.sample_age_seconds = self.last_sample_at.map(|t| t.elapsed().as_secs_f64());
        target.completed_samples = self.completed_samples;
    }

    pub fn audit_value(&self) -> serde_json::Value {
        serde_json::json!({
            "timestamp_supported": self.supported,
            "enabled": self.enabled,
            "scope": "chart_pass",
            "completed_samples": self.completed_samples,
            "last_duration_ms": self.last_duration_ms,
            "includes_surface_wait_or_presentation": false,
            "retained_world_area_compute": { "completed_samples":self.retained_world_completed_samples,
                "last_duration_ms":self.retained_world_last_duration_ms,"separate_from_chart_pass":true },
        })
    }

    /// Process finished frames and log GPU timing data
    pub fn process_and_log(&mut self, queue: &wgpu::Queue) {
        if !self.enabled {
            return;
        }

        if let Some(entries) = self
            .profiler
            .process_finished_frame(queue.get_timestamp_period())
        {
            // Missing/invalid timestamps are unavailable, not zero GPU cost.
            for entry in entries {
                if let Some(milliseconds) = valid_gpu_duration_ms(entry.time.as_ref()) {
                    if entry.label == "chart_pass" {
                        self.completed_samples = self.completed_samples.saturating_add(1);
                        self.last_duration_ms = Some(milliseconds);
                        self.last_sample_at = Some(Instant::now());
                        self.debug_chart_window
                            .record(Duration::from_secs_f64(milliseconds / 1000.0), None);
                    } else if entry.label == "retained_world_area_compute" {
                        self.retained_world_completed_samples =
                            self.retained_world_completed_samples.saturating_add(1);
                        self.retained_world_last_duration_ms = Some(milliseconds);
                    }
                    self.timings
                        .entry(entry.label)
                        .or_insert_with(ScopeStat::new)
                        .record(Duration::from_secs_f64(milliseconds / 1000.0));
                }
            }
        }
        if self.last_report.elapsed() >= Duration::from_secs(5) {
            for (label, stat) in &self.timings {
                tracing::info!(
                    "[GPU_PROFILER] {} samples={} mean_ms={:.3} min_ms={:.3} max_ms={:.3}",
                    label,
                    stat.count,
                    stat.avg().as_secs_f64() * 1000.0,
                    stat.min.as_secs_f64() * 1000.0,
                    stat.max.as_secs_f64() * 1000.0,
                );
            }
            self.timings.clear();
            self.last_report = Instant::now();
        }
    }
}

fn valid_gpu_duration_ms(time: Option<&std::ops::Range<f64>>) -> Option<f64> {
    let time = time?;
    let seconds = time.end - time.start;
    // Bound before Duration conversion; invalid driver samples must not panic.
    (time.start.is_finite()
        && time.end.is_finite()
        && seconds.is_finite()
        && (0.0..=3600.0).contains(&seconds))
    .then_some(seconds * 1000.0)
}

#[cfg(test)]
mod gpu_timing_tests {
    use super::valid_gpu_duration_ms;

    #[test]
    fn unavailable_or_invalid_timestamps_do_not_become_zero_gpu_time() {
        assert_eq!(valid_gpu_duration_ms(None), None);
        for range in [2.0..1.0, f64::NAN..1.0, 0.0..f64::INFINITY, 0.0..3601.0] {
            assert_eq!(valid_gpu_duration_ms(Some(&range)), None);
        }
        assert_eq!(valid_gpu_duration_ms(Some(&(4.0..4.0))), Some(0.0));
        assert!((valid_gpu_duration_ms(Some(&(1.0..1.016))).unwrap() - 16.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod cpu_frame_distribution_tests {
    use super::*;
    #[test]
    fn budgets_quantiles_and_overflow_are_explicit() {
        let mut d = FrameDistribution::new();
        assert!(d.report().contains("unavailable"));
        for n in 1..=100 {
            d.record(Duration::from_millis(n));
        }
        let r = d.report();
        assert!(r.contains("recent_p95_ms=95.000"));
        assert!(r.contains("recent_p99_ms=99.000"));
        assert_eq!(d.over_60hz, 84);
        assert_eq!(d.over_144hz, 94);
        for _ in 0..5000 {
            d.record(Duration::from_nanos(1));
        }
        assert_eq!(d.used, 4096);
        assert_eq!(d.total, 5100);
        assert!(d.report().contains("overwritten=1004"));
        assert!(d.report().contains("recent_p99_ms=0.000"));
    }
}

#[cfg(test)]
mod navigation_debug_selection_tests {
    use super::*;
    #[test]
    fn idle_redraws_never_enter_navigation_frame_distribution() {
        let mut p = CpuProfiler::new();
        p.begin_navigation_frame(false);
        p.frame_start = Some(Instant::now() - Duration::from_millis(20));
        p.end_frame();
        p.begin_navigation_frame(true);
        p.frame_start = Some(Instant::now() - Duration::from_millis(10));
        p.end_frame();
        let s = p
            .debug_window
            .take_if_due(Instant::now() + crate::debug_metrics::PERIOD)
            .unwrap();
        assert_eq!(s.summary.unwrap().samples, 1);
        p.reset_debug_metrics();
        assert!(p
            .debug_window
            .take_if_due(Instant::now() + crate::debug_metrics::PERIOD)
            .unwrap()
            .summary
            .is_none());
        assert!(p.previous_navigation_entry.is_none());
        assert!(p.previous_navigation_wall.is_none());
    }
}
