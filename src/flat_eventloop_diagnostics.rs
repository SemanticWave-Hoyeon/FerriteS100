use ferrite_render::flat_reuse_diagnostics::{FlatFrameLedger, FlatFrameSample};
use std::{path::PathBuf, time::Instant};
pub const FRAME_COUNT: usize = 500;
pub struct Audit {
    pub path: PathBuf,
    pub bounds: [f64; 4],
    pub saved_zoom: f64,
    pub saved_pan: (f64, f64),
    pub next: usize,
    pub start: Option<Instant>,
    previous: Option<Instant>,
    render_start: Option<Instant>,
    ledger: FlatFrameLedger<512>,
    interval: [Option<u64>; FRAME_COUNT],
    pub overlap: [f64; FRAME_COUNT],
    pub gpu_coverage_host: [u64; FRAME_COUNT],
    prepare: [u64; FRAME_COUNT],
    render: [u64; FRAME_COUNT],
}
fn ns(d: std::time::Duration) -> u64 {
    d.as_nanos().min(u64::MAX as u128) as u64
}
impl Audit {
    pub fn new(path: PathBuf, bounds: [f64; 4], zoom: f64, pan: (f64, f64)) -> Self {
        Self {
            path,
            bounds,
            saved_zoom: zoom,
            saved_pan: pan,
            next: 0,
            start: None,
            previous: None,
            render_start: None,
            ledger: FlatFrameLedger::default(),
            interval: [None; FRAME_COUNT],
            overlap: [0.; FRAME_COUNT],
            gpu_coverage_host: [0; FRAME_COUNT],
            prepare: [0; FRAME_COUNT],
            render: [0; FRAME_COUNT],
        }
    }
    pub fn pose(&self) -> (f64, (f64, f64)) {
        let cycle = self.next / 100;
        let t = (self.next % 100) as f64 / 99.;
        let zoom = 200f64.powf(if cycle.is_multiple_of(2) { t } else { 1. - t });
        (
            zoom,
            crate::flat_service_trajectory::pan(self.bounds, t, cycle == 4),
        )
    }
    pub fn begin(&mut self, now: Instant) {
        self.interval[self.next] = self
            .previous
            .map(|old| ns(now.saturating_duration_since(old)));
        self.previous = Some(now);
        self.start = Some(now);
    }
    pub fn before_render(&mut self) {
        let now = Instant::now();
        self.prepare[self.next] = ns(now.saturating_duration_since(self.start.unwrap()));
        self.render_start = Some(now);
    }
    pub fn after_render(&mut self) {
        self.render[self.next] = ns(self.render_start.unwrap().elapsed());
    }
    pub fn complete(&mut self, row: FlatFrameSample) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.next < FRAME_COUNT && row.frame == self.next as u64 && row.hidden && !row.focused,
            "Eventloop sample ownership/visibility failed"
        );
        self.ledger.record(row);
        self.start = None;
        self.next += 1;
        Ok(())
    }
    pub fn export(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.next == FRAME_COUNT && self.ledger.dropped() == 0,
            "Incomplete eventloop proof"
        );
        let rows:Vec<_>=self.ledger.rows().map(|r|serde_json::json!({"frame":r.frame,"trajectory":if r.frame<400{"chart_relative"}else{"outside_wide"},"first_traversal":r.frame<100||r.frame>=400,"redraw_entry_interval_ns":self.interval[r.frame as usize],"handler_through_scheduling_wall_ns":r.service_ns,"prepare_wall_ns":self.prepare[r.frame as usize],"render_through_present_wall_ns":self.render[r.frame as usize],"chart_aabb_overlap_fraction":self.overlap[r.frame as usize],"hidden":r.hidden,"focused":r.focused,"spans_ns":r.spans_ns,"coverage_gpu_binding_plan_upload_host_wall_ns":self.gpu_coverage_host[r.frame as usize],"reuse_attempts":r.reuse_attempts,"reuse_accepted":r.reuse_accepted,"rejected_by_bit":r.rejected_by_bit,"source_iterations":r.work.source_commands,"executed_commands":r.work.executed_commands,"dependency_iterations":r.work.dependency_iterations,"buffer_upload_bytes_subset":r.work.buffer_upload_bytes,"triangulation_hits":r.work.triangulation_hits,"triangulation_cold":r.work.triangulation_cold})).collect();
        let values: Vec<_> = (100..400).filter_map(|i| self.interval[i]).collect();
        std::fs::create_dir_all(&self.path)?;
        std::fs::write(
            self.path.join("flat-eventloop.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"rows":rows,"warm_primary_interval_stats":summarize(&values),"scope":"Actual normal RedrawRequested/render/present callbacks driven by synthetic gesture macro; no device Wait/readback per frame. Redraw interval includes OS queue scheduling/acquire and previous GPU backpressure; submitted-frame cadence NOT display-refresh or foreground FPS or GPU-completed frame rate. Clock-instrumented diagnostic. No GPU-duration claim. Stage walls overlap; acquire waits only, residual completion unmeasured."}),
            )?,
        )?;
        Ok(())
    }
}
pub fn summarize(values: &[u64]) -> serde_json::Value {
    let mut v = values.to_vec();
    v.sort_unstable();
    let q = |f: f64| {
        if v.is_empty() {
            None
        } else {
            Some(v[((v.len() as f64 * f).ceil() as usize).saturating_sub(1)])
        }
    };
    serde_json::json!({"samples":v.len(),"p95_ns":q(0.95),"p99_ns":q(0.99),"over_16_7_ms":v.iter().filter(|&&n|n>16_700_000).count()})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_callback_owns_one_row_and_first_interval_is_absent() {
        let mut a = Audit::new(PathBuf::new(), [0., 0., 1., 1.], 1., (0., 0.));
        let now = Instant::now();
        a.begin(now);
        assert_eq!(a.interval[0], None);
        a.complete(FlatFrameSample::new(0, 1, 0, true, false))
            .unwrap();
        a.begin(now + std::time::Duration::from_millis(17));
        assert_eq!(a.interval[1], Some(17_000_000));
        assert!(a
            .complete(FlatFrameSample::new(8, 1, 8, true, false))
            .is_err());
        assert_eq!(a.next, 1);
    }
    #[test]
    fn hidden_ownership_and_bounds_are_bounded() {
        let mut a = Audit::new(PathBuf::new(), [0., 0., 1., 1.], 1., (0., 0.));
        assert!(a
            .complete(FlatFrameSample::new(0, 1, 0, false, false))
            .is_err());
        assert!(a
            .complete(FlatFrameSample::new(0, 1, 0, true, true))
            .is_err());
        assert!(std::mem::size_of::<Audit>() < 1024 * 1024);
    }
    #[test]
    fn percentile_is_nearest_rank_and_empty_does_not_zero_fill() {
        let q = summarize(&[10, 30, 20]);
        assert_eq!(q["p95_ns"], 30);
        assert_eq!(summarize(&[])["p99_ns"], serde_json::Value::Null);
    }
}
