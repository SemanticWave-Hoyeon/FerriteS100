//! Bounded diagnostics, never portrayal authority or physical presentation FPS.
use std::time::{Duration, Instant};
pub const PERIOD: Duration = Duration::from_millis(500);
pub const REASONS: [&str; 10] = [
    "mode",
    "dependencies",
    "view_dependent",
    "clipped_pattern",
    "no_source_transform",
    "no_affine",
    "invalid_affine",
    "retained_world_area",
    "overscale_annotation",
    "coverage_scale_selection",
];
const CAPACITY: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub samples: u64,
    pub quantile_samples: usize,
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub over_60_percent: f64,
    pub over_144_percent: f64,
    /// Host redraw wall / consecutive active redraw-entry interval. Includes
    /// blocking waits, excludes independent input callbacks; NOT CPU utilization.
    pub redraw_wall_interval_percent: Option<f64>,
}
#[derive(Debug, Clone, Default)]
pub struct CpuDebugMetrics {
    pub navigation: Option<Summary>,
    pub navigation_active: bool,
    pub window_seconds: f64,
    pub fastpath_attempts: u64,
    pub fastpath_accepted: u64,
    pub fastpath_rejections: [u64; 10],
}
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub summary: Option<Summary>,
    pub seconds: f64,
    pub attempts: u64,
    pub accepted: u64,
    pub rejections: [u64; 10],
}
pub(crate) struct Window {
    start: Instant,
    ns: [u64; CAPACITY],
    used: usize,
    next: usize,
    samples: u64,
    sum: u128,
    max: u64,
    over: [u64; 2],
    paired_wall: u128,
    paired_interval: u128,
    events: [u64; 12],
}
impl Window {
    pub(crate) fn new() -> Self {
        Self::at(Instant::now())
    }
    fn at(start: Instant) -> Self {
        Self {
            start,
            ns: [0; CAPACITY],
            used: 0,
            next: 0,
            samples: 0,
            sum: 0,
            max: 0,
            over: [0; 2],
            paired_wall: 0,
            paired_interval: 0,
            events: [0; 12],
        }
    }
    pub(crate) fn record(&mut self, elapsed: Duration, interval: Option<Duration>) {
        let n = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.ns[self.next] = n;
        self.next = (self.next + 1) % CAPACITY;
        self.used = (self.used + 1).min(CAPACITY);
        self.samples = self.samples.saturating_add(1);
        self.sum += u128::from(n);
        self.max = self.max.max(n);
        self.over[0] += u64::from(u128::from(n) * 60 > 1_000_000_000);
        self.over[1] += u64::from(u128::from(n) * 144 > 1_000_000_000);
        self.record_pair(elapsed, interval);
    }
    pub(crate) fn record_pair(&mut self, wall: Duration, interval: Option<Duration>) {
        if let Some(interval) = interval.filter(|v| !v.is_zero()) {
            self.paired_wall += wall.as_nanos();
            self.paired_interval += interval.as_nanos();
        }
    }
    pub(crate) fn event(&mut self, name: &'static str) {
        let index = match name {
            "attempt" => Some(0),
            "accepted" => Some(1),
            _ => REASONS.iter().position(|r| *r == name).map(|i| i + 2),
        };
        if let Some(i) = index {
            self.events[i] = self.events[i].saturating_add(1);
        }
    }
    pub(crate) fn take_if_due(&mut self, now: Instant) -> Option<Snapshot> {
        let elapsed = now.checked_duration_since(self.start)?;
        if elapsed < PERIOD {
            return None;
        }
        let summary = if self.used == 0 {
            None
        } else {
            let mut ns = self.ns;
            ns[..self.used].sort_unstable();
            Some(Summary {
                samples: self.samples,
                quantile_samples: self.used,
                mean_ms: self.sum as f64 / self.samples as f64 / 1e6,
                p95_ms: ns[(self.used * 95).div_ceil(100) - 1] as f64 / 1e6,
                p99_ms: ns[(self.used * 99).div_ceil(100) - 1] as f64 / 1e6,
                max_ms: self.max as f64 / 1e6,
                over_60_percent: self.over[0] as f64 * 100.0 / self.samples as f64,
                over_144_percent: self.over[1] as f64 * 100.0 / self.samples as f64,
                redraw_wall_interval_percent: (self.paired_interval > 0)
                    .then(|| self.paired_wall as f64 / self.paired_interval as f64 * 100.0),
            })
        };
        let snapshot = Snapshot {
            summary,
            seconds: elapsed.as_secs_f64(),
            attempts: self.events[0],
            accepted: self.events[1],
            rejections: std::array::from_fn(|i| self.events[i + 2]),
        };
        *self = Self::at(now);
        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interval_population_empty_windows_and_exact_refresh_budgets() {
        let start = Instant::now();
        let mut w = Window::at(start);
        w.record(Duration::from_nanos(16_666_666), None);
        w.record(
            Duration::from_nanos(16_666_667),
            Some(Duration::from_millis(20)),
        );
        assert!(w
            .take_if_due(start + PERIOD - Duration::from_nanos(1))
            .is_none());
        let s = w.take_if_due(start + PERIOD).unwrap().summary.unwrap();
        assert_eq!(s.samples, 2);
        assert_eq!(s.over_60_percent, 50.0);
        assert_eq!(s.over_144_percent, 100.0);
        assert_eq!(s.p95_ms, 16.666667);
        assert_eq!(s.p99_ms, 16.666667);
        assert_eq!(s.max_ms, 16.666667);
        assert!((s.redraw_wall_interval_percent.unwrap() - 83.333335).abs() < 1e-6);
        assert!(w.take_if_due(start + PERIOD * 2).unwrap().summary.is_none());
    }
    #[test]
    fn p95_and_p99_use_distinct_nearest_rank_boundaries() {
        let start = Instant::now();
        let mut w = Window::at(start);
        for n in 1..=100 {
            w.record(Duration::from_nanos(n), None);
        }
        let s = w.take_if_due(start + PERIOD).unwrap().summary.unwrap();
        assert_eq!(s.p95_ms, 95.0 / 1e6);
        assert_eq!(s.p99_ms, 99.0 / 1e6);
        assert_eq!(s.max_ms, 100.0 / 1e6);
        assert_eq!(s.quantile_samples, 100);
    }
    #[test]
    fn bounded_quantile_population_and_actual_rejection_events_are_explicit() {
        let start = Instant::now();
        let mut w = Window::at(start);
        for _ in 0..600 {
            w.record(Duration::from_millis(1), None);
        }
        w.event("attempt");
        w.event("view_dependent");
        w.event("overscale_annotation");
        let s = w.take_if_due(start + PERIOD).unwrap();
        assert_eq!(s.summary.as_ref().unwrap().samples, 600);
        assert_eq!(s.summary.as_ref().unwrap().quantile_samples, 512);
        assert_eq!(s.attempts, 1);
        assert_eq!(s.accepted, 0);
        assert_eq!(s.rejections[2], 1);
        assert_eq!(s.rejections[8], 1);
    }
}
