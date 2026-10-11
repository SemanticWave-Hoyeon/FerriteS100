//! Optional coarse emitter-wave HOST clocks; no geometry/authority result cache.
use crate::shared_cell::Shared;
use ferrite_render::DrawingInstruction;
use std::time::Instant;
pub(crate) const NAMES: [&str; 4] = ["area", "line", "point", "text"];
#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct Work {
    pub ordered_line_census_enabled: bool,
    pub admitted_line_helper_calls: u64,
    pub unsupported_line_helper_calls: u64,
    pub ordinary_jobs: u64,
    pub ordinary_source_points: u64,
    pub ordinary_waves: u64,
    pub ordinary_max_wave_jobs: u64,
    pub ordinary_max_job_points: u64,
    pub ordinary_jobs_with_output: u64,
    pub ordinary_failed_jobs: u64,
    pub ordinary_output_vertices: u64,
    pub ordinary_output_indices: u64,
    pub ordinary_active_capacity_growth_bytes: u64,
    pub ordinary_max_active_capacity_bytes: u64,
    pub calls: u64,
    pub dispatch_started_calls: u64,
    pub dispatch_finished_calls: u64,
    pub whole_emitter_host_ns: u64,
    pub pre_dispatch_host_ns: u64,
    pub dispatch_total_host_ns: u64,
    pub wave_host_ns: [u64; 4],
    pub post_dispatch_host_ns: u64,
    pub waves: [u64; 4],
    pub visited_ordinals: [u64; 4],
    pub authored_line_points_visited: u64,
    pub dynamic_line_ordinals_visited: u64,
    pub styled_line_ordinals_visited: u64,
    // Completed dispatch append counts; not allocations/GPU uploads.
    pub output_appends: [u64; 5],
}
pub(crate) type Collector = Shared<Work>;
pub(crate) fn enabled(v: Option<&std::ffi::OsStr>) -> bool {
    v == Some(std::ffi::OsStr::new("1"))
}
pub(crate) fn collector(v: Option<&std::ffi::OsStr>) -> Option<Collector> {
    enabled(v).then(|| Shared::new(Work::default()))
}
/// Reuses existing emitter-kind clocks. The new flag adds work counters, NOT
/// per-line/per-segment timers and NOT an additional instruction traversal.
pub(crate) fn collector_with_ordered_lines(
    wave: Option<&std::ffi::OsStr>,
    lines: Option<&std::ffi::OsStr>,
) -> Option<Collector> {
    if !enabled(lines) {
        return collector(wave);
    }
    Some(Shared::new(Work {
        ordered_line_census_enabled: true,
        ..Default::default()
    }))
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct OrdinaryPartitionKey {
    plane: i32,
    priority: i32,
    cell: Option<u32>,
    resource_revision: u64,
}
pub(crate) struct OrdinaryTicket {
    before: [usize; 4],
}
fn kind(i: &DrawingInstruction) -> usize {
    match i {
        DrawingInstruction::Area(_) => 0,
        DrawingInstruction::Line(_) => 1,
        DrawingInstruction::Point(_) => 2,
        DrawingInstruction::Text(_) => 3,
    }
}
fn ns(a: Instant, b: Instant) -> u64 {
    b.duration_since(a).as_nanos().min(u64::MAX as u128) as u64
}
fn add(target: &mut Work, v: &Work) {
    macro_rules! scalar {($($f:ident),*)=>{$(target.$f=target.$f.saturating_add(v.$f);)*};}
    scalar!(
        admitted_line_helper_calls,
        unsupported_line_helper_calls,
        ordinary_jobs,
        ordinary_source_points,
        ordinary_waves,
        ordinary_jobs_with_output,
        ordinary_failed_jobs,
        ordinary_output_vertices,
        ordinary_output_indices,
        ordinary_active_capacity_growth_bytes,
        calls,
        dispatch_started_calls,
        dispatch_finished_calls,
        whole_emitter_host_ns,
        pre_dispatch_host_ns,
        dispatch_total_host_ns,
        post_dispatch_host_ns,
        authored_line_points_visited,
        dynamic_line_ordinals_visited,
        styled_line_ordinals_visited
    );
    target.ordered_line_census_enabled |= v.ordered_line_census_enabled;
    target.ordinary_max_wave_jobs = target.ordinary_max_wave_jobs.max(v.ordinary_max_wave_jobs);
    target.ordinary_max_job_points = target
        .ordinary_max_job_points
        .max(v.ordinary_max_job_points);
    target.ordinary_max_active_capacity_bytes = target
        .ordinary_max_active_capacity_bytes
        .max(v.ordinary_max_active_capacity_bytes);
    for i in 0..4 {
        target.wave_host_ns[i] = target.wave_host_ns[i].saturating_add(v.wave_host_ns[i]);
        target.waves[i] = target.waves[i].saturating_add(v.waves[i]);
        target.visited_ordinals[i] =
            target.visited_ordinals[i].saturating_add(v.visited_ordinals[i]);
    }
    for i in 0..5 {
        target.output_appends[i] = target.output_appends[i].saturating_add(v.output_appends[i]);
    }
}
/// Owns one collector Rc per emitter call. No RefCell borrow crosses draw callbacks.
/// Drop closes on ANY early return/Result Err/unwind without changing readiness.
pub(crate) struct Call {
    collector: Collector,
    work: Work,
    whole: Instant,
    dispatch: Option<Instant>,
    active: Option<(usize, Instant)>,
    post: Option<Instant>,
    output_start: [usize; 5],
    last_ordinary: Option<(usize, OrdinaryPartitionKey)>,
    ordinary_wave_jobs: u64,
}
impl Call {
    pub(crate) fn new(collector: &Collector, output_start: [usize; 5]) -> Self {
        Self {
            collector: Shared::clone(collector),
            work: Work {
                calls: 1,
                ordered_line_census_enabled: collector.borrow().ordered_line_census_enabled,
                ..Default::default()
            },
            whole: Instant::now(),
            dispatch: None,
            active: None,
            post: None,
            output_start,
            last_ordinary: None,
            ordinary_wave_jobs: 0,
        }
    }
    pub(crate) fn begin_dispatch(&mut self) {
        let now = Instant::now();
        self.work.pre_dispatch_host_ns = ns(self.whole, now);
        self.work.dispatch_started_calls = 1;
        self.dispatch = Some(now);
    }
    pub(crate) fn visit(&mut self, instruction: &DrawingInstruction) {
        let k = kind(instruction);
        // No Instant call for an unchanged contiguous geometry kind.
        if self.active.as_ref().is_none_or(|(old, _)| *old != k) {
            let now = Instant::now();
            self.close_wave(now);
            self.active = Some((k, now));
            self.work.waves[k] = self.work.waves[k].saturating_add(1);
        }
        self.work.visited_ordinals[k] = self.work.visited_ordinals[k].saturating_add(1);
        if let DrawingInstruction::Line(l) = instruction {
            self.work.authored_line_points_visited = self
                .work
                .authored_line_points_visited
                .saturating_add(l.points.len() as u64);
            self.work.dynamic_line_ordinals_visited = self
                .work
                .dynamic_line_ordinals_visited
                .saturating_add(u64::from(
                    l.screen_ray.is_some() || l.portrayal_path.is_some(),
                ));
            self.work.styled_line_ordinals_visited = self
                .work
                .styled_line_ordinals_visited
                .saturating_add(u64::from(
                    l.style.offset_mm != 0.
                        || l.style.dash_cycle.is_some()
                        || !l.style.dash_pattern.is_empty(),
                ));
        }
    }
    pub(crate) fn ordered_lines_enabled(&self) -> bool {
        self.work.ordered_line_census_enabled
    }
    /// Called only AFTER original admission, owner and suppression gates. This
    /// eligibility is a diagnostic subset; it never grants execution or skips work.
    pub(crate) fn begin_ordinary_line(
        &mut self,
        ordinal: usize,
        line: &ferrite_render::LineInstruction,
        complete_unsuppressed: bool,
        resource_revision: u64,
        same_previous_coverage_decisions: bool,
        before: [usize; 4],
    ) -> Option<OrdinaryTicket> {
        if !self.ordered_lines_enabled() {
            return None;
        }
        self.work.admitted_line_helper_calls =
            self.work.admitted_line_helper_calls.saturating_add(1);
        if !complete_unsuppressed || !crate::source_line_projection_arena::eligible(line) {
            self.work.unsupported_line_helper_calls =
                self.work.unsupported_line_helper_calls.saturating_add(1);
            self.last_ordinary = None;
            return None;
        }
        let key = OrdinaryPartitionKey {
            plane: line.display_plane.order().get(),
            priority: line.priority.0,
            cell: line.cell_index,
            resource_revision,
        };
        let same = self.last_ordinary.is_some_and(|(old, previous)| {
            old.checked_add(1) == Some(ordinal)
                && previous == key
                && same_previous_coverage_decisions
        });
        if !same {
            self.work.ordinary_waves = self.work.ordinary_waves.saturating_add(1);
            self.ordinary_wave_jobs = 0;
        }
        self.last_ordinary = Some((ordinal, key));
        self.ordinary_wave_jobs = self.ordinary_wave_jobs.saturating_add(1);
        self.work.ordinary_max_wave_jobs = self
            .work
            .ordinary_max_wave_jobs
            .max(self.ordinary_wave_jobs);
        self.work.ordinary_jobs = self.work.ordinary_jobs.saturating_add(1);
        self.work.ordinary_source_points = self
            .work
            .ordinary_source_points
            .saturating_add(line.points.len() as u64);
        self.work.ordinary_max_job_points = self
            .work
            .ordinary_max_job_points
            .max(line.points.len() as u64);
        Some(OrdinaryTicket { before })
    }
    /// Record actual scalar append deltas, including partial output before Err.
    /// No clock, RefCell borrow, geometry copy or allocation occurs per job.
    pub(crate) fn finish_ordinary_line(
        &mut self,
        ticket: OrdinaryTicket,
        after: [usize; 4],
        succeeded: bool,
    ) {
        let vertices = after[0].saturating_sub(ticket.before[0]) as u64;
        let indices = after[1].saturating_sub(ticket.before[1]) as u64;
        self.work.ordinary_output_vertices =
            self.work.ordinary_output_vertices.saturating_add(vertices);
        self.work.ordinary_output_indices =
            self.work.ordinary_output_indices.saturating_add(indices);
        self.work.ordinary_jobs_with_output = self
            .work
            .ordinary_jobs_with_output
            .saturating_add(u64::from(indices != 0));
        self.work.ordinary_failed_jobs = self
            .work
            .ordinary_failed_jobs
            .saturating_add(u64::from(!succeeded));
        let growth = after[2]
            .saturating_sub(ticket.before[2])
            .saturating_add(after[3].saturating_sub(ticket.before[3]));
        self.work.ordinary_active_capacity_growth_bytes = self
            .work
            .ordinary_active_capacity_growth_bytes
            .saturating_add(growth as u64);
        self.work.ordinary_max_active_capacity_bytes = self
            .work
            .ordinary_max_active_capacity_bytes
            .max(after[2].saturating_add(after[3]) as u64);
    }
    fn close_wave(&mut self, now: Instant) {
        if let Some((kind, start)) = self.active.take() {
            self.work.wave_host_ns[kind] =
                self.work.wave_host_ns[kind].saturating_add(ns(start, now));
        }
    }
    pub(crate) fn end_dispatch(&mut self, output_end: [usize; 5]) {
        let now = Instant::now();
        self.close_wave(now);
        if let Some(start) = self.dispatch.take() {
            self.work.dispatch_total_host_ns = ns(start, now);
        }
        self.work.dispatch_finished_calls = 1;
        self.post = Some(now);
        for (i, end) in output_end.iter().enumerate() {
            self.work.output_appends[i] = end.saturating_sub(self.output_start[i]) as u64;
        }
    }
}
impl Drop for Call {
    fn drop(&mut self) {
        let now = Instant::now();
        self.close_wave(now);
        self.work.whole_emitter_host_ns = ns(self.whole, now);
        if let Some(start) = self.dispatch.take() {
            self.work.dispatch_total_host_ns = ns(start, now);
        } else if let Some(start) = self.post.take() {
            self.work.post_dispatch_host_ns = ns(start, now);
        } else {
            self.work.pre_dispatch_host_ns = self.work.whole_emitter_host_ns;
        }
        add(&mut self.collector.borrow_mut(), &self.work);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{LineInstruction, PointInstruction, WorldPoint};
    fn line() -> DrawingInstruction {
        DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ]))
    }
    #[test]
    fn exact_optin_disabled_has_no_collector() {
        for v in [None, Some("0"), Some("true"), Some(" 1"), Some("1 ")] {
            assert!(collector(v.map(std::ffi::OsStr::new)).is_none());
        }
        assert!(collector(Some("1".as_ref())).is_some());
    }
    #[test]
    fn contiguous_waves_preserve_visits_and_output_without_clock_per_line() {
        let c = collector(Some("1".as_ref())).unwrap();
        {
            let mut call = Call::new(&c, [4, 8, 12, 1, 2]);
            call.begin_dispatch();
            for _ in 0..500 {
                call.visit(&line());
            }
            call.visit(&DrawingInstruction::Point(PointInstruction::new(
                "x".into(),
                WorldPoint::new(0., 0.),
            )));
            call.visit(&line());
            call.end_dispatch([4, 12, 18, 2, 3]);
        }
        let w = c.borrow();
        assert_eq!(w.visited_ordinals, [0, 501, 1, 0]);
        assert_eq!(w.waves, [0, 2, 1, 0]);
        assert_eq!(w.authored_line_points_visited, 1002);
        assert_eq!(w.output_appends, [0, 4, 6, 1, 1]);
        assert_eq!(w.dispatch_finished_calls, 1);
    }
    #[test]
    fn early_return_and_predispatch_failure_record_without_borrow_held() {
        let c = collector(Some("1".as_ref())).unwrap();
        fn fail(c: &Collector) {
            let mut call = Call::new(c, [0; 5]);
            call.begin_dispatch();
            call.visit(&line());
            let _other = c.borrow();
        }
        fail(&c);
        {
            let _call = Call::new(&c, [0; 5]);
            let _other = c.borrow();
        }
        let w = c.borrow();
        assert_eq!(w.calls, 2);
        assert_eq!(w.dispatch_started_calls, 1);
        assert_eq!(w.dispatch_finished_calls, 0);
        assert_eq!(w.visited_ordinals[1], 1);
    }
    #[test]
    fn multiple_emitter_calls_accumulate_exact_work_and_saturate() {
        let c = collector(Some("1".as_ref())).unwrap();
        for _ in 0..4 {
            let mut call = Call::new(&c, [0; 5]);
            call.begin_dispatch();
            call.visit(&line());
            call.end_dispatch([0, 4, 6, 0, 0]);
        }
        let w = c.borrow().clone();
        assert_eq!(w.calls, 4);
        assert_eq!(w.output_appends[1], 16);
        let mut total = Work {
            calls: u64::MAX,
            ..Default::default()
        };
        add(&mut total, &w);
        assert_eq!(total.calls, u64::MAX);
    }
}

#[cfg(test)]
mod ordered_line_controls {
    use super::*;
    use ferrite_render::{LineInstruction, WorldPoint};
    fn line() -> LineInstruction {
        let mut line =
            LineInstruction::new(vec![WorldPoint::new(-0., 0.), WorldPoint::new(1., 2.)]);
        line.portrayal_origin = ferrite_render::PortrayalOrigin::NonPoint;
        line
    }
    #[test]
    fn exact_new_flag_and_original_flag_are_independent() {
        for flag in [None, Some("0"), Some("true"), Some(" 1"), Some("1 ")] {
            assert!(collector_with_ordered_lines(None, flag.map(std::ffi::OsStr::new)).is_none());
        }
        let old = collector_with_ordered_lines(Some("1".as_ref()), None).unwrap();
        assert!(!old.borrow().ordered_line_census_enabled);
        let new = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        assert!(new.borrow().ordered_line_census_enabled);
    }
    #[test]
    fn original_ordinal_and_partition_changes_split_work_without_reordering() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        let original = line();
        let bits: Vec<_> = original
            .points
            .iter()
            .map(|p| (p.x.to_bits(), p.y.to_bits()))
            .collect();
        {
            let mut call = Call::new(&c, [0; 5]);
            for (ordinal, revision) in [(0, 10), (1, 10), (3, 10), (4, 11)] {
                let t = call
                    .begin_ordinary_line(ordinal, &original, true, revision, true, [0; 4])
                    .unwrap();
                call.finish_ordinary_line(t, [4, 6, 96, 24], true);
            }
        }
        let w = c.borrow();
        assert_eq!(w.ordinary_jobs, 4);
        assert_eq!(w.ordinary_waves, 3);
        assert_eq!(w.ordinary_max_wave_jobs, 2);
        assert_eq!(w.ordinary_source_points, 8);
        assert_eq!(w.ordinary_output_vertices, 16);
        assert_eq!(w.ordinary_output_indices, 24);
        assert_eq!(w.ordinary_active_capacity_growth_bytes, 480);
        assert_eq!(w.ordinary_max_active_capacity_bytes, 120);
        assert_eq!(
            bits,
            original
                .points
                .iter()
                .map(|p| (p.x.to_bits(), p.y.to_bits()))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn original_coverage_decisions_not_locator_ordinals_define_coarse_continuity() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        {
            let mut call = Call::new(&c, [0; 5]);
            for (ordinal, same_coverage) in [(0, false), (1, true), (2, false), (3, true)] {
                let t = call
                    .begin_ordinary_line(ordinal, &line(), true, 7, same_coverage, [0; 4])
                    .unwrap();
                call.finish_ordinary_line(t, [4, 6, 0, 0], true);
            }
        }
        let w = c.borrow();
        assert_eq!(w.ordinary_jobs, 4);
        assert_eq!(w.ordinary_waves, 2);
        assert_eq!(w.ordinary_max_wave_jobs, 2);
    }
    #[test]
    fn coarse_partition_is_not_material_equality() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        let first = line();
        let mut second = line();
        second.style.width = first.style.width + 1.;
        assert_ne!(first.style.width.to_bits(), second.style.width.to_bits());
        {
            let mut call = Call::new(&c, [0; 5]);
            for (ordinal, l) in [(0, &first), (1, &second)] {
                let t = call
                    .begin_ordinary_line(ordinal, l, true, 7, true, [0; 4])
                    .unwrap();
                call.finish_ordinary_line(t, [4, 6, 0, 0], true);
            }
        }
        assert_eq!(c.borrow().ordinary_waves, 1);
        assert_eq!(
            second.style.width.to_bits(),
            (first.style.width + 1.).to_bits()
        );
    }
    #[test]
    fn unsupported_partial_suppression_and_failed_scalar_append_are_observed_only() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        {
            let mut call = Call::new(&c, [0; 5]);
            let original = line();
            assert!(call
                .begin_ordinary_line(0, &original, false, 1, true, [0; 4])
                .is_none());
            let mut styled = line();
            styled.style.offset_mm = 1.;
            assert!(call
                .begin_ordinary_line(1, &styled, true, 1, true, [0; 4])
                .is_none());
            let t = call
                .begin_ordinary_line(2, &original, true, 1, true, [4, 6, 96, 24])
                .unwrap();
            call.finish_ordinary_line(t, [8, 12, 96, 24], false);
        }
        let w = c.borrow();
        assert_eq!(w.admitted_line_helper_calls, 3);
        assert_eq!(w.unsupported_line_helper_calls, 2);
        assert_eq!(w.ordinary_jobs, 1);
        assert_eq!(w.ordinary_failed_jobs, 1);
        assert_eq!(w.ordinary_output_indices, 6);
        assert_eq!(w.ordinary_active_capacity_growth_bytes, 0);
        assert_eq!(w.dispatch_finished_calls, 0);
    }
}

#[cfg(test)]
mod ordered_line_boundary_controls {
    use super::*;
    fn ordinary() -> ferrite_render::LineInstruction {
        let mut l = ferrite_render::LineInstruction::new(vec![
            ferrite_render::WorldPoint::new(0., 0.),
            ferrite_render::WorldPoint::new(1., 1.),
        ]);
        l.portrayal_origin = ferrite_render::PortrayalOrigin::NonPoint;
        l
    }
    #[test]
    fn unspecified_origin_never_becomes_an_ordinary_job() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        {
            let mut call = Call::new(&c, [0; 5]);
            let mut l = ordinary();
            l.portrayal_origin = ferrite_render::PortrayalOrigin::Unspecified;
            assert!(call
                .begin_ordinary_line(0, &l, true, 1, true, [0; 4])
                .is_none());
        }
        assert_eq!(c.borrow().ordinary_jobs, 0);
        assert_eq!(c.borrow().unsupported_line_helper_calls, 1);
    }
    #[test]
    fn unsupported_job_breaks_consecutive_ordinary_partition() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        {
            let mut call = Call::new(&c, [0; 5]);
            for ordinal in 0..3 {
                let mut l = ordinary();
                if ordinal == 1 {
                    l.style.offset_mm = 1.;
                }
                if let Some(t) = call.begin_ordinary_line(ordinal, &l, true, 1, true, [0; 4]) {
                    call.finish_ordinary_line(t, [4, 6, 0, 0], true);
                }
            }
        }
        assert_eq!(c.borrow().ordinary_jobs, 2);
        assert_eq!(c.borrow().ordinary_waves, 2);
        assert_eq!(c.borrow().ordinary_max_wave_jobs, 1);
    }
    #[test]
    fn maximum_is_not_summed_and_partitions_never_cross_calls() {
        let c = collector_with_ordered_lines(None, Some("1".as_ref())).unwrap();
        for jobs in [2, 3] {
            let mut call = Call::new(&c, [0; 5]);
            for ordinal in 0..jobs {
                let t = call
                    .begin_ordinary_line(ordinal, &ordinary(), true, 1, true, [0; 4])
                    .unwrap();
                call.finish_ordinary_line(t, [4, 6, 0, 0], true);
            }
        }
        assert_eq!(c.borrow().ordinary_jobs, 5);
        assert_eq!(c.borrow().ordinary_waves, 2);
        assert_eq!(c.borrow().ordinary_max_wave_jobs, 3);
    }
}
