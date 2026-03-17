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

/// CPU-side performance profiler
///
/// Accumulates timing data for named scopes and periodically logs summaries.
pub struct CpuProfiler {
    scopes: BTreeMap<&'static str, ScopeStat>,
    /// Cumulative stats that never reset (for exit report)
    cumulative: BTreeMap<&'static str, ScopeStat>,
    cumulative_frame_count: u64,
    session_start: Instant,
    last_report: Instant,
    report_interval: Duration,
    frame_count: u64,
    frame_start: Option<Instant>,
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
            cumulative: BTreeMap::new(),
            cumulative_frame_count: 0,
            session_start: Instant::now(),
            last_report: Instant::now(),
            report_interval: Duration::from_secs(5), // Log every 5 seconds
            frame_count: 0,
            frame_start: None,
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

    /// Mark the beginning of a frame
    pub fn begin_frame(&mut self) {
        self.frame_start = Some(Instant::now());
        self.frame_count += 1;
        self.cumulative_frame_count += 1;
    }

    /// Mark the end of a frame and check if we should log a report
    pub fn end_frame(&mut self) {
        if let Some(start) = self.frame_start.take() {
            let elapsed = start.elapsed();
            self.record("frame_total", elapsed);
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
        if self.scopes.is_empty() {
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
}

impl GpuProfilerWrapper {
    pub fn new(_device: &wgpu::Device) -> Self {
        let profiler = wgpu_profiler::GpuProfiler::new(wgpu_profiler::GpuProfilerSettings {
            enable_timer_queries: true,
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
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Process finished frames and log GPU timing data
    pub fn process_and_log(&mut self, queue: &wgpu::Queue) {
        if !self.enabled {
            return;
        }

        if let Some(profiling_data) = self
            .profiler
            .process_finished_frame(queue.get_timestamp_period())
        {
            if !profiling_data.is_empty() {
                let mut report = String::with_capacity(1024);
                report.push_str("\n┌─────────────────────────────────────────────────┐\n");
                report.push_str("│              GPU PROFILER REPORT                │\n");
                report.push_str("├─────────────────────────────────────────────────┤\n");

                fn log_entries(
                    report: &mut String,
                    entries: &[wgpu_profiler::GpuTimerQueryResult],
                    depth: usize,
                ) {
                    for entry in entries {
                        let indent = "  ".repeat(depth);
                        let duration_ms = entry
                            .time
                            .as_ref()
                            .map(|t| (t.end - t.start) * 1000.0)
                            .unwrap_or(0.0);
                        report.push_str(&format!(
                            "│ {}{:<30} {:>8.3}ms │\n",
                            indent, entry.label, duration_ms
                        ));
                        if !entry.nested_queries.is_empty() {
                            log_entries(report, &entry.nested_queries, depth + 1);
                        }
                    }
                }

                log_entries(&mut report, &profiling_data, 0);
                report.push_str("└─────────────────────────────────────────────────┘");
                tracing::info!("{}", report);
            }
        }
    }
}
