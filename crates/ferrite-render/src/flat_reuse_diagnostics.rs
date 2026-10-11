//! Opt-in Flat navigation attribution. No cache, GPU access or state mutation.
//! Root integrates hooks after the Flat-only renderer source is frozen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlatReuseBlockers(pub u16);
impl FlatReuseBlockers {
    pub const MODE: u16 = 1;
    pub const DEPENDENCIES: u16 = 2;
    pub const VIEW_DEPENDENT: u16 = 4;
    pub const CLIPPED_PATTERN: u16 = 8;
    pub const NO_SOURCE_TRANSFORM: u16 = 16;
    pub const NO_AFFINE: u16 = 32;
    pub const INVALID_AFFINE: u16 = 64;
    pub const RETAINED_WORLD_AREA: u16 = 128;
    pub const OVERSCALE_ANNOTATION: u16 = 256;
    pub const COVERAGE_SCALE_SELECTION: u16 = 512;
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}
#[derive(Debug, Clone, Copy)]
pub struct NavigationAffine {
    pub scale: [f32; 2],
    pub pan: [f32; 2],
    pub pivot: [f32; 2],
}
impl NavigationAffine {
    /// Preserve the original f32 expression and short-circuit order.
    pub fn valid(self) -> bool {
        (0..2).all(|axis| {
            let (s, d, p) = (self.scale[axis], self.pan[axis], self.pivot[axis]);
            s.is_finite()
                && s > 0.
                && d.is_finite()
                && p.is_finite()
                && (1. / s).is_finite()
                && ((d - p) * s + p).is_finite()
                && (p - p / s - d).is_finite()
        })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct FlatNavigationInputs {
    /// Product mode support, not an instruction visibility filter.
    pub mode_supported: bool,
    pub retained_world_area: bool,
    pub overscale_annotation: bool,
    pub coverage_scale_selection: bool,
    pub dependency_iterations: usize,
    pub view_dependent_symbols: bool,
    pub view_clipped_patterns: bool,
    pub source_transform_present: bool,
    /// None only when the existing ScreenAffine conversion returned None.
    pub affine: Option<NavigationAffine>,
}
/// Reports all reasons evaluated at the FIRST original rejecting stage only.
/// Do not invoke later fallible affine/projection work to fill diagnostic fields.
pub fn classify_flat_reuse(input: FlatNavigationInputs) -> FlatReuseBlockers {
    let mut bits = 0;
    if !input.mode_supported {
        bits |= FlatReuseBlockers::MODE;
    }
    if input.dependency_iterations > 0 {
        bits |= FlatReuseBlockers::DEPENDENCIES;
    }
    if input.view_dependent_symbols {
        bits |= FlatReuseBlockers::VIEW_DEPENDENT;
    }
    if input.view_clipped_patterns {
        bits |= FlatReuseBlockers::CLIPPED_PATTERN;
    }
    if input.retained_world_area {
        bits |= FlatReuseBlockers::RETAINED_WORLD_AREA;
    }
    if input.overscale_annotation {
        bits |= FlatReuseBlockers::OVERSCALE_ANNOTATION;
    }
    if input.coverage_scale_selection {
        bits |= FlatReuseBlockers::COVERAGE_SCALE_SELECTION;
    }
    if bits != 0 {
        return FlatReuseBlockers(bits);
    }
    if !input.source_transform_present {
        return FlatReuseBlockers(FlatReuseBlockers::NO_SOURCE_TRANSFORM);
    }
    match input.affine {
        None => FlatReuseBlockers(FlatReuseBlockers::NO_AFFINE),
        Some(affine) if !affine.valid() => FlatReuseBlockers(FlatReuseBlockers::INVALID_AFFINE),
        Some(_) => FlatReuseBlockers(0),
    }
}
/// Span names are serial wall-time attribution. They are not GPU durations.
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum FlatFrameStage {
    Camera = 0,
    Coverage = 1,
    Visibility = 2,
    DependencyResolution = 3,
    AreaAndPattern = 4,
    Lines = 5,
    Points = 6,
    TextAndDeclutter = 7,
    PickIndex = 8,
    BufferUpload = 9,
    Overlay = 10,
    EncodeAndSubmit = 11,
    PresentCall = 12,
    CompletionWait = 13,
    SourceClassification = 14,
    SourceProjectionPreparation = 15,
    ExecutionVisibility = 16,
    CompactAdmission = 17,
    OwnerAdmission = 18,
    ViewDependencyClassification = 19,
}
pub const FLAT_FRAME_STAGE_COUNT: usize = 20;
/// Coarse measuring mode: only exact OS string "0" opts out. All other values
/// preserve existing diagnostics. Caller samples once when constructing Renderer.
pub fn stage_timing_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value != Some(std::ffi::OsStr::new("0"))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlatFrameWork {
    pub source_commands: u64,
    pub executed_commands: u64,
    pub dependency_iterations: u64,
    pub projected_vertices: u64,
    pub triangles: u64,
    pub clipped_triangles: u64,
    pub buffer_upload_calls: u64,
    pub buffer_upload_bytes: u64,
    pub triangulation_hits: u64,
    pub triangulation_cold: u64,
}
#[derive(Debug, Clone, Copy)]
pub struct FlatFrameSample {
    pub frame: u64,
    pub source_epoch: u64,
    pub view_epoch: u64,
    pub reuse_attempts: u64,
    pub reuse_accepted: u64,
    pub rejected_by_bit: [u64; 10],
    pub spans_ns: [u64; FLAT_FRAME_STAGE_COUNT],
    /// False means the internal span array is incomplete/unavailable, never zero cost.
    /// Overall service and work counters remain valid regardless of this flag.
    pub internal_stage_timing_available: bool,
    pub work: FlatFrameWork,
    /// Complete serialized wall service, readback/export outside this interval.
    pub service_ns: u64,
    pub hidden: bool,
    pub focused: bool,
}
impl FlatFrameSample {
    pub fn new(
        frame: u64,
        source_epoch: u64,
        view_epoch: u64,
        hidden: bool,
        focused: bool,
    ) -> Self {
        Self {
            frame,
            source_epoch,
            view_epoch,
            reuse_attempts: 0,
            reuse_accepted: 0,
            rejected_by_bit: [0; 10],
            spans_ns: [0; FLAT_FRAME_STAGE_COUNT],
            internal_stage_timing_available: true,
            work: FlatFrameWork::default(),
            service_ns: 0,
            hidden,
            focused,
        }
    }
    /// Export None as JSON null; a partially manually recorded array must not be
    /// mistaken for complete attribution when flat_span clocks were disabled.
    pub fn available_spans(&self) -> Option<&[u64; FLAT_FRAME_STAGE_COUNT]> {
        self.internal_stage_timing_available
            .then_some(&self.spans_ns)
    }
    pub fn record_reuse(&mut self, reason: FlatReuseBlockers) {
        self.reuse_attempts = self.reuse_attempts.saturating_add(1);
        if reason.is_empty() {
            self.reuse_accepted = self.reuse_accepted.saturating_add(1);
        }
        for bit in 0..10 {
            if reason.0 & (1 << bit) != 0 {
                self.rejected_by_bit[bit] = self.rejected_by_bit[bit].saturating_add(1);
            }
        }
    }
    pub fn record_span(&mut self, stage: FlatFrameStage, ns: u64) {
        let i = stage as usize;
        self.spans_ns[i] = self.spans_ns[i].saturating_add(ns);
    }
}
/// Fixed inline storage: recording cannot allocate or silently evict old samples.
/// Caller holds no collector/Instant when disabled. Export after the timed loop.
pub struct FlatFrameLedger<const N: usize> {
    rows: [Option<FlatFrameSample>; N],
    used: usize,
    dropped: u64,
}
impl<const N: usize> Default for FlatFrameLedger<N> {
    fn default() -> Self {
        Self {
            rows: [None; N],
            used: 0,
            dropped: 0,
        }
    }
}
impl<const N: usize> FlatFrameLedger<N> {
    pub fn record(&mut self, row: FlatFrameSample) {
        if self.used == N {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.rows[self.used] = Some(row);
        self.used += 1;
    }
    pub fn rows(&self) -> impl Iterator<Item = &FlatFrameSample> {
        self.rows[..self.used].iter().filter_map(Option::as_ref)
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn capacity_bytes() -> Option<usize> {
        N.checked_mul(std::mem::size_of::<Option<FlatFrameSample>>())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> FlatNavigationInputs {
        FlatNavigationInputs {
            mode_supported: true,
            retained_world_area: false,
            overscale_annotation: false,
            coverage_scale_selection: false,
            dependency_iterations: 0,
            view_dependent_symbols: false,
            view_clipped_patterns: false,
            source_transform_present: true,
            affine: Some(NavigationAffine {
                scale: [1., 1.],
                pan: [0., 0.],
                pivot: [0., 0.],
            }),
        }
    }
    #[test]
    fn first_guard_truth_table_matches_original_acceptance_and_no_later_work() {
        for state in 0u8..16 {
            let i = FlatNavigationInputs {
                mode_supported: state & 1 == 0,
                dependency_iterations: usize::from(state & 2 != 0),
                view_dependent_symbols: state & 4 != 0,
                view_clipped_patterns: state & 8 != 0,
                ..input()
            };
            let expected = state == 0;
            assert_eq!(classify_flat_reuse(i).is_empty(), expected);
            if !expected {
                assert_eq!(
                    classify_flat_reuse(FlatNavigationInputs {
                        source_transform_present: false,
                        affine: None,
                        ..i
                    }),
                    classify_flat_reuse(i)
                );
            }
        }
    }
    #[test]
    fn production_visibility_guards_are_never_misreported_as_missing_affine() {
        for state in 1u16..8 {
            let reason = classify_flat_reuse(FlatNavigationInputs {
                retained_world_area: state & 1 != 0,
                overscale_annotation: state & 2 != 0,
                coverage_scale_selection: state & 4 != 0,
                affine: None,
                ..input()
            });
            assert_eq!(reason.0, state << 7);
            let mut row = FlatFrameSample::new(0, 0, 0, true, false);
            row.record_reuse(reason);
            assert_eq!(row.rejected_by_bit[5], 0);
            assert_eq!(row.rejected_by_bit[7], u64::from(state & 1 != 0));
            assert_eq!(row.rejected_by_bit[8], u64::from(state & 2 != 0));
            assert_eq!(row.rejected_by_bit[9], u64::from(state & 4 != 0));
        }
    }
    #[test]
    fn affine_order_invalid_and_extreme_values_follow_original_expression() {
        for s in [
            0.,
            -1.,
            f32::INFINITY,
            f32::NAN,
            f32::MIN_POSITIVE,
            1.,
            200.,
        ] {
            for d in [0., f32::MAX, f32::NAN] {
                let a = NavigationAffine {
                    scale: [s, 1.],
                    pan: [d, 0.],
                    pivot: [10., 0.],
                };
                let reference = (0..2).all(|k| {
                    let (s, d, p) = (a.scale[k], a.pan[k], a.pivot[k]);
                    s.is_finite()
                        && s > 0.
                        && d.is_finite()
                        && p.is_finite()
                        && (1. / s).is_finite()
                        && ((d - p) * s + p).is_finite()
                        && (p - p / s - d).is_finite()
                });
                assert_eq!(
                    classify_flat_reuse(FlatNavigationInputs {
                        affine: Some(a),
                        ..input()
                    })
                    .is_empty(),
                    reference
                );
            }
        }
        assert_eq!(
            classify_flat_reuse(FlatNavigationInputs {
                source_transform_present: false,
                affine: None,
                ..input()
            }),
            FlatReuseBlockers(FlatReuseBlockers::NO_SOURCE_TRANSFORM)
        );
        assert_eq!(
            classify_flat_reuse(FlatNavigationInputs {
                affine: None,
                ..input()
            }),
            FlatReuseBlockers(FlatReuseBlockers::NO_AFFINE)
        );
    }
    #[test]
    fn ledger_is_bounded_truncation_and_counters_are_explicit() {
        let mut ledger = FlatFrameLedger::<2>::default();
        for frame in 0..3 {
            let mut s = FlatFrameSample::new(frame, 4, 5, true, false);
            s.record_reuse(FlatReuseBlockers(
                FlatReuseBlockers::VIEW_DEPENDENT | FlatReuseBlockers::CLIPPED_PATTERN,
            ));
            s.record_span(FlatFrameStage::Coverage, 10);
            ledger.record(s);
        }
        assert_eq!(ledger.rows().count(), 2);
        assert_eq!(ledger.dropped(), 1);
        let r = ledger.rows().next().unwrap();
        assert_eq!(r.reuse_attempts, 1);
        assert_eq!(r.rejected_by_bit[2], 1);
        assert_eq!(r.rejected_by_bit[3], 1);
        assert_eq!(r.spans_ns[1], 10);
        assert_eq!(
            FlatFrameLedger::<2>::capacity_bytes(),
            Some(2 * std::mem::size_of::<Option<FlatFrameSample>>())
        );
        let mut zero = FlatFrameLedger::<0>::default();
        zero.record(*r);
        assert_eq!(zero.dropped(), 1);
    }
}

#[cfg(test)]
mod coarse_clock_controls {
    use super::*;
    #[test]
    fn exact_zero_only_disables_and_default_is_unchanged() {
        assert!(!stage_timing_enabled(Some(std::ffi::OsStr::new("0"))));
        for value in [
            None,
            Some("1"),
            Some("false"),
            Some(" 0"),
            Some(""),
            Some("00"),
        ] {
            assert!(stage_timing_enabled(value.map(std::ffi::OsStr::new)));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(stage_timing_enabled(Some(std::ffi::OsStr::from_bytes(&[
                255
            ]))));
        }
    }
    #[test]
    fn default_span_array_preserves_original_cost_and_counter_recording() {
        let mut row = FlatFrameSample::new(0, 1, 2, true, false);
        row.record_span(FlatFrameStage::Lines, 123);
        row.record_reuse(FlatReuseBlockers(4));
        row.service_ns = 500;
        row.work.source_commands = 999;
        assert_eq!(row.available_spans().unwrap()[5], 123);
        assert_eq!(row.reuse_attempts, 1);
        assert_eq!(row.work.source_commands, 999);
        assert_eq!(row.service_ns, 500);
    }
    #[test]
    fn unavailable_internal_spans_are_null_not_partial_array_or_zero_cost() {
        let mut row = FlatFrameSample::new(0, 1, 2, true, false);
        row.internal_stage_timing_available = false;
        // External/manual coarse stamps still exist; internal per-instruction ones do not.
        row.record_span(FlatFrameStage::Camera, 12);
        row.work.buffer_upload_bytes = 1024;
        row.service_ns = 100;
        assert_eq!(
            serde_json::to_value(row.available_spans()).unwrap(),
            serde_json::Value::Null
        );
        assert_eq!(row.spans_ns[0], 12);
        assert_eq!(row.work.buffer_upload_bytes, 1024);
        assert_eq!(row.service_ns, 100);
    }
}
