//! Bounded hidden-surface attribution. No GPU waits, readbacks or disk writes.
use std::{ffi::OsStr, time::Instant};
pub(crate) const FRAMES: usize = 500;
const BUDGET: usize = 1024 * 1024;

pub(crate) fn requested_latency(value: Option<&OsStr>) -> u32 {
    if value == Some(OsStr::new("2")) {
        2
    } else {
        1
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Acquire,
    OverlayCpu,
    ChartTextHost,
    GeometryHost,
    SubmitHost,
    PresentCall,
}
impl Stage {
    fn index(self) -> usize {
        self as usize
    }
}
const NAMES: [&str; 6] = [
    "surface_acquire_wall_ns",
    "overlay_cpu_wall_ns",
    "chart_text_host_wall_ns",
    "geometry_host_wall_ns",
    "queue_submit_call_wall_ns",
    "present_call_wall_ns",
];
#[derive(Clone, Copy, Default)]
struct Row {
    frame: u64,
    source: u64,
    view: u64,
    stage: [Option<u64>; 6],
    acquire_calls: u32,
    acquire_errors: u32,
    presents: u32,
    finished: bool,
}
pub(crate) struct Capture {
    rows: Vec<Row>,
    active: Option<Row>,
    rejected: u64,
    abandoned: u64,
}
impl Capture {
    pub(crate) fn new(flag: Option<&OsStr>, hidden_background: bool) -> Option<Self> {
        if flag != Some(OsStr::new("1")) || !hidden_background {
            return None;
        }
        let mut rows = Vec::new();
        if rows.try_reserve_exact(FRAMES).is_err()
            || rows
                .capacity()
                .checked_mul(std::mem::size_of::<Row>())
                .is_none_or(|b| b > BUDGET)
        {
            return None;
        }
        Some(Self {
            rows,
            active: None,
            rejected: 0,
            abandoned: 0,
        })
    }
    pub(crate) fn arm(&mut self, frame: u64, source: u64, view: u64) {
        if self.active.take().is_some() {
            self.abandoned = self.abandoned.saturating_add(1);
        }
        if self.rows.len() == FRAMES || frame != self.rows.len() as u64 {
            self.rejected = self.rejected.saturating_add(1);
            return;
        }
        self.active = Some(Row {
            frame,
            source,
            view,
            ..Row::default()
        });
    }
    pub(crate) fn clock(&self) -> Option<Instant> {
        self.active.as_ref().map(|_| Instant::now())
    }
    pub(crate) fn record(&mut self, stage: Stage, elapsed_ns: u64) {
        let Some(row) = self.active.as_mut() else {
            return;
        };
        let slot = &mut row.stage[stage.index()];
        if slot.is_some() {
            self.rejected = self.rejected.saturating_add(1);
        }
        *slot = Some(elapsed_ns);
    }
    pub(crate) fn acquire(&mut self, error: bool) {
        if let Some(row) = self.active.as_mut() {
            row.acquire_calls = row.acquire_calls.saturating_add(1);
            row.acquire_errors = row.acquire_errors.saturating_add(u32::from(error));
        }
    }
    pub(crate) fn presented(&mut self) {
        if let Some(row) = self.active.as_mut() {
            row.presents = row.presents.saturating_add(1);
        }
    }
    pub(crate) fn finish(&mut self) {
        if let Some(mut row) = self.active.take() {
            row.finished = true;
            self.rows.push(row);
        }
    }
    pub(crate) fn snapshot(&self, latency: u32, extent: [u32; 2]) -> serde_json::Value {
        let rows: Vec<_> = self
            .rows
            .iter()
            .map(|row| {
                let mut out = serde_json::Map::new();
                out.insert("frame".into(), row.frame.into());
                out.insert("source_epoch".into(), row.source.into());
                out.insert("view_epoch".into(), row.view.into());
                for (name, value) in NAMES.iter().zip(row.stage) {
                    out.insert((*name).into(), serde_json::json!(value));
                }
                out.insert("surface_acquire_calls".into(), row.acquire_calls.into());
                out.insert("surface_acquire_errors".into(), row.acquire_errors.into());
                out.insert("present_calls".into(), row.presents.into());
                out.insert("normal_render_finished".into(), row.finished.into());
                serde_json::Value::Object(out)
            })
            .collect();
        let valid = self.rows.len() == FRAMES
            && self.active.is_none()
            && self.rejected == 0
            && self.abandoned == 0
            && self.rows.iter().enumerate().all(|(index, row)| {
                row.frame == index as u64
                    && row.finished
                    && row.acquire_calls == 1
                    && row.acquire_errors == 0
                    && row.presents == 1
                    && row.stage.iter().all(Option::is_some)
            });
        serde_json::json!({
            "requested_latency_hint":latency, "surface_extent":extent,
            "row_capacity_bytes":self.rows.capacity()*std::mem::size_of::<Row>(),
            "row_capacity_budget_bytes":BUDGET,
            "rejected":self.rejected,"abandoned":self.abandoned,"active":self.active.is_some(),
            "complete_500_normal_surface_frames":valid,"rows":rows,
            "scope":"Hidden normal render attribution only. Acquire wall includes backend/surface waits and CPU API work; not isolated GPU wait. Overlay CPU excludes explicit queue writes; chart text/geometry host include uploads. Queue submit wall is API work including encoder.finish, not GPU duration. Present call is not presentation completion. Backend may clamp latency; no physical FPS, input-to-display or total VRAM bound."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_two_only_default_one() {
        assert_eq!(requested_latency(None), 1);
        assert_eq!(requested_latency(Some(OsStr::new("2"))), 2);
        for value in ["1", "0", "3", "02", " 2", "2 ", "true", ""] {
            assert_eq!(requested_latency(Some(OsStr::new(value))), 1);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(requested_latency(Some(OsStr::from_bytes(&[255]))), 1);
        }
    }
    #[test]
    fn diagnostics_require_exact_one_and_hidden_background() {
        assert!(Capture::new(Some(OsStr::new("1")), true).is_some());
        assert!(Capture::new(Some(OsStr::new("1")), false).is_none());
        assert!(Capture::new(None, true).is_none());
        assert!(Capture::new(Some(OsStr::new("01")), true).is_none());
    }
    #[test]
    fn complete_means_actual_acquire_and_present_every_frame() {
        let mut c = Capture::new(Some(OsStr::new("1")), true).unwrap();
        for frame in 0..FRAMES {
            c.arm(frame as u64, 7, frame as u64);
            for stage in [
                Stage::Acquire,
                Stage::OverlayCpu,
                Stage::ChartTextHost,
                Stage::GeometryHost,
                Stage::SubmitHost,
                Stage::PresentCall,
            ] {
                c.record(stage, 1);
            }
            c.acquire(false);
            c.presented();
            c.finish();
        }
        assert_eq!(
            c.snapshot(2, [1920, 1080])["complete_500_normal_surface_frames"],
            true
        );
        c.arm(FRAMES as u64, 7, FRAMES as u64);
        assert_eq!(c.rows.len(), FRAMES);
        assert_eq!(c.rejected, 1);
    }
    #[test]
    fn offscreen_or_failed_or_duplicate_frame_is_not_success() {
        let mut c = Capture::new(Some(OsStr::new("1")), true).unwrap();
        c.arm(0, 1, 1);
        c.finish();
        assert_eq!(
            c.snapshot(1, [1, 1])["complete_500_normal_surface_frames"],
            false
        );
        c.arm(0, 1, 1);
        assert_eq!(c.rejected, 1);
        c.arm(1, 1, 1);
        c.acquire(true);
        c.arm(1, 1, 1);
        assert_eq!(c.abandoned, 1);
    }
}
