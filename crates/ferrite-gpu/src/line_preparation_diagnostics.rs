//! Optional host-only line preparation clocks. Children overlap ancestors.
use crate::shared_cell::Shared;
use serde::Serialize;
use std::time::Instant;

pub const STAGE_NAMES: [&str; 10] = [
    "shared_admission",
    "owner_coverage_ranges",
    "suppression_bounds",
    "path_materialization",
    "path_gate_style",
    "offset_projection",
    "offset_solver",
    "dash_spans",
    "projection_clip_mesh",
    "anchor_rebase",
];
#[derive(Clone, Copy)]
pub enum Stage {
    Admission,
    OwnerCoverage,
    SuppressionBounds,
    Paths,
    GateStyle,
    OffsetProjection,
    OffsetSolver,
    Dash,
    ProjectionMesh,
    Anchor,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Work {
    pub host_ns: [u64; 10],
    pub calls: [u64; 10],
    pub line_ordinals_visited: u64,
    pub source_path_points: u64,
    pub emitted_vertices: u64,
    pub emitted_indices: u64,
    pub vertex_capacity_growth_bytes: u64,
    pub index_capacity_growth_bytes: u64,
}
impl Work {
    /// Same-renderer monotonic counters only; a reset/reversed snapshot is unavailable.
    pub fn checked_delta(&self, before: &Self) -> Option<Self> {
        let mut out = Self::default();
        for i in 0..10 {
            out.host_ns[i] = self.host_ns[i].checked_sub(before.host_ns[i])?;
            out.calls[i] = self.calls[i].checked_sub(before.calls[i])?;
        }
        out.line_ordinals_visited = self
            .line_ordinals_visited
            .checked_sub(before.line_ordinals_visited)?;
        out.source_path_points = self
            .source_path_points
            .checked_sub(before.source_path_points)?;
        out.emitted_vertices = self.emitted_vertices.checked_sub(before.emitted_vertices)?;
        out.emitted_indices = self.emitted_indices.checked_sub(before.emitted_indices)?;
        out.vertex_capacity_growth_bytes = self
            .vertex_capacity_growth_bytes
            .checked_sub(before.vertex_capacity_growth_bytes)?;
        out.index_capacity_growth_bytes = self
            .index_capacity_growth_bytes
            .checked_sub(before.index_capacity_growth_bytes)?;
        Some(out)
    }
}
pub type Collector = Shared<Work>;
pub struct Span {
    cell: Collector,
    stage: usize,
    start: Instant,
}
impl Drop for Span {
    fn drop(&mut self) {
        record(
            &mut self.cell.borrow_mut(),
            self.stage,
            self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        );
    }
}
fn record(work: &mut Work, stage: usize, elapsed: u64) {
    work.host_ns[stage] = work.host_ns[stage].saturating_add(elapsed);
    work.calls[stage] = work.calls[stage].saturating_add(1);
}
pub fn span(cell: Option<&Collector>, stage: Stage) -> Option<Span> {
    cell.map(|cell| Span {
        cell: Shared::clone(cell),
        stage: stage as usize,
        start: Instant::now(),
    })
}
pub fn enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_optin_disabled_has_no_clock_or_collector() {
        assert!(!enabled(None));
        for v in ["0", "true", " 1", "1 ", ""] {
            assert!(!enabled(Some(v.as_ref())));
        }
        assert!(enabled(Some("1".as_ref())));
        assert!(span(None, Stage::Admission).is_none());
    }
    #[test]
    fn children_are_independent_not_subtracted_from_parent() {
        let mut w = Work::default();
        record(&mut w, Stage::ProjectionMesh as usize, 100);
        record(&mut w, Stage::OffsetProjection as usize, 30);
        assert_eq!(w.host_ns[8], 100);
        assert_eq!(w.host_ns[5], 30);
        assert_eq!(w.calls[8], 1);
        assert_eq!(w.calls[5], 1);
        record(&mut w, Stage::ProjectionMesh as usize, u64::MAX);
        assert_eq!(w.host_ns[8], u64::MAX);
    }
    #[test]
    fn callback_delta_includes_all_rebuilds_and_rejects_reset() {
        let before = Work::default();
        let mut after = before.clone();
        for _ in 0..4 {
            record(&mut after, Stage::Dash as usize, 10);
        }
        let delta = after.checked_delta(&before).unwrap();
        assert_eq!(delta.host_ns[7], 40);
        assert_eq!(delta.calls[7], 4);
        assert!(before.checked_delta(&after).is_none());
    }
    #[test]
    fn early_return_span_records_and_no_nested_borrow_is_held() {
        let cell = Shared::new(Work::default());
        {
            let _parent = span(Some(&cell), Stage::Paths);
            {
                let _child = span(Some(&cell), Stage::GateStyle);
                cell.borrow_mut().source_path_points = 2;
            }
        }
        assert_eq!(cell.borrow().calls[3], 1);
        assert_eq!(cell.borrow().calls[4], 1);
        assert_eq!(cell.borrow().source_path_points, 2);
    }
}
