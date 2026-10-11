//! Product-neutral visibility-aware suppression of coincident line segments.
#[path = "suppression_eligibility.rs"]
mod eligibility_program;
#[path = "suppression_owned_overlap.rs"]
mod owned_overlap_block;
use crate::{DrawingInstruction, FlatProjection, Scaler, ScreenPoint, WorldPoint};
pub use eligibility_program::Work as LineEligibilityWork;
use owned_overlap_block::{Bounds as OwnedBounds, Entry as OwnedEntry, OwnedOverlapBlock};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

// Exact bounded growth compilation is the default. Explicit values other than
// "1" retain the reference compiler; policy is sampled once per cache lifetime.
fn retained_growth_enabled(name: &str) -> bool {
    std::env::var_os(name)
        .as_deref()
        .is_none_or(|value| value == std::ffi::OsStr::new("1"))
}

pub fn instruction_visible(
    instruction: &DrawingInstruction,
    scale: u32,
    groups: Option<&HashSet<u32>>,
    override_group: Option<u32>,
) -> bool {
    !matches!(instruction,DrawingInstruction::Line(line) if !line.style.has_visible_stroke())
        && instruction.scale_range().is_visible_at(scale)
        && groups.is_none_or(|g| {
            instruction
                .viewing_groups()
                .all(|vg| g.contains(&vg.0) || override_group == Some(vg.0))
        })
}
fn original_eligibility(
    instructions: &[DrawingInstruction],
    scale: u32,
    groups: Option<&HashSet<u32>>,
    override_group: Option<u32>,
    visibility: Option<&[bool]>,
) -> Vec<bool> {
    instructions.iter().enumerate().map(|(i,item)| {
        visibility.is_none_or(|v|v.get(i).copied().unwrap_or(false))
            && instruction_visible(item,scale,groups,override_group)
            && matches!(item,DrawingInstruction::Line(l) if l.screen_ray.is_none() && l.portrayal_path.is_none())
    }).collect()
}
#[derive(Clone, Copy)]
struct Curve<'a> {
    points: &'a [WorldPoint],
    reversed: bool,
}
fn bits(p: WorldPoint) -> (u64, u64) {
    (
        if p.x == 0. { 0 } else { p.x.to_bits() },
        if p.y == 0. { 0 } else { p.y.to_bits() },
    )
}
impl<'a> Curve<'a> {
    fn new(points: &'a [WorldPoint]) -> Option<Self> {
        if points.len() < 2 || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
            return None;
        }
        let reversed = points
            .iter()
            .zip(points.iter().rev())
            .map(|(a, b)| bits(*a).cmp(&bits(*b)))
            .find(|c| !c.is_eq())
            .is_some_and(|c| c.is_gt());
        Some(Self { points, reversed })
    }
    fn ordered(self) -> impl Iterator<Item = WorldPoint> + 'a {
        (0..self.points.len()).map(move |i| {
            self.points[if self.reversed {
                self.points.len() - i - 1
            } else {
                i
            }]
        })
    }
}
impl Hash for Curve<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.points.len().hash(h);
        for p in self.ordered() {
            bits(p).hash(h);
        }
    }
}
impl PartialEq for Curve<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.points.len() == other.points.len()
            && self
                .ordered()
                .zip(other.ordered())
                .all(|(a, b)| bits(a) == bits(b))
    }
}
impl Eq for Curve<'_> {}
struct Fingerprint(Sha256);
impl Hasher for Fingerprint {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
    fn finish(&self) -> u64 {
        u64::from_le_bytes(self.0.clone().finalize()[..8].try_into().unwrap())
    }
}

/// A visible fraction of an original directed segment; source indices are retained
/// for hit testing and future patterned-stroke phase calculations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineSpan {
    pub segment: usize,
    pub start: f64,
    pub end: f64,
}
impl LineSpan {
    /// Interpolate the projected source edge. Fractions from dashing and
    /// suppression parameterize this straight edge, not latitude degrees.
    pub fn screen_endpoints(
        &self,
        points: &[WorldPoint],
        scaler: &Scaler,
    ) -> Option<(ScreenPoint, ScreenPoint)> {
        if !self.start.is_finite() || !self.end.is_finite() || self.start > self.end {
            return None;
        }
        let a = scaler.world_to_screen(*points.get(self.segment)?);
        let b = scaler.world_to_screen(*points.get(self.segment + 1)?);
        if ![a.x, a.y, b.x, b.y].iter().all(|v| v.is_finite()) {
            return None;
        }
        let at = |t: f64| {
            if t == 0. {
                a
            } else if t == 1. {
                b
            } else {
                ScreenPoint::new(
                    (a.x as f64 + (b.x as f64 - a.x as f64) * t) as f32,
                    (a.y as f64 + (b.y as f64 - a.y as f64) * t) as f32,
                )
            }
        };
        Some((at(self.start), at(self.end)))
    }
    /// Linear endpoints in the supplied coordinate system. Geographic callers
    /// using a nonlinear projection must use screen_endpoints instead.
    pub fn endpoints(&self, points: &[WorldPoint]) -> Option<(WorldPoint, WorldPoint)> {
        let a = *points.get(self.segment)?;
        let b = *points.get(self.segment + 1)?;
        let at = |t| {
            if t == 0. {
                a
            } else if t == 1. {
                b
            } else {
                WorldPoint::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
            }
        };
        Some((at(self.start), at(self.end)))
    }
}
#[derive(Debug, Default, PartialEq)]
pub struct LineSuppressionPlan {
    pub fully_suppressed: HashSet<usize>,
    pub partial: HashMap<usize, Vec<LineSpan>>,
}
impl LineSuppressionPlan {
    pub fn contains(&self, index: &usize) -> bool {
        self.fully_suppressed.contains(index)
    }
    pub fn is_empty(&self) -> bool {
        self.fully_suppressed.is_empty() && self.partial.is_empty()
    }
    /// None means the entire original line; an empty slice means fully suppressed.
    pub fn spans(&self, index: usize) -> Option<&[LineSpan]> {
        if self.fully_suppressed.contains(&index) {
            Some(&[])
        } else {
            self.partial.get(&index).map(Vec::as_slice)
        }
    }
}
#[derive(Clone, Copy)]
struct Segment {
    a: WorldPoint,
    b: WorldPoint,
}
impl rstar::RTreeObject for Segment {
    type Envelope = rstar::AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        rstar::AABB::from_corners(
            [self.a.x.min(self.b.x), self.a.y.min(self.b.y)],
            [self.a.x.max(self.b.x), self.a.y.max(self.b.y)],
        )
    }
}
impl Segment {
    fn new(a: WorldPoint, b: WorldPoint) -> Option<Self> {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        (a.x.is_finite()
            && a.y.is_finite()
            && b.x.is_finite()
            && b.y.is_finite()
            && dx.is_finite()
            && dy.is_finite()
            && (dx != 0. || dy != 0.))
            .then_some(Self { a, b })
    }
    fn key(self) -> ((u64, u64), (u64, u64)) {
        let (a, b) = (bits(self.a), bits(self.b));
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }
    fn overlap(self, other: Self) -> Option<(f64, f64)> {
        let c = |p: WorldPoint| robust::Coord { x: p.x, y: p.y };
        // Never hide nearby parallel lines based on a screen-space tolerance.
        if robust::orient2d(c(self.a), c(self.b), c(other.a)) != 0.
            || robust::orient2d(c(self.a), c(self.b), c(other.b)) != 0.
        {
            return None;
        }
        let dx = self.b.x - self.a.x;
        let dy = self.b.y - self.a.y;
        let t = |p: WorldPoint| {
            if dx.abs() >= dy.abs() {
                (p.x - self.a.x) / dx
            } else {
                (p.y - self.a.y) / dy
            }
        };
        let (a, b) = (t(other.a), t(other.b));
        let low = a.min(b).max(0.);
        let high = a.max(b).min(1.);
        (low < high && low.is_finite() && high.is_finite()).then_some((low, high))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuppressionRevision {
    Legacy(u64),
    Context(crate::StaticLineRelationEpoch),
}

/// Content validation costs O(V) even on a hit. Results share an Arc and contain
/// no borrowed vertices. Legacy spatial indexes are temporary; opt-in blocks are owned and bounded.
/// Optional diagnostics only. Child clocks are HOST wall time, not GPU time.
/// target_relations includes old relation cloning, all coincidence grouping
/// lookups, spatial queries, exact overlap arithmetic, endpoint sort and budget.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct GrowthCompilerWork {
    pub attempts: u64,
    pub admitted: u64,
    pub group_and_segment_build_ns: u64,
    pub two_index_build_ns: u64,
    pub projection_and_placeholder_ns: u64,
    pub target_relations_ns: u64,
    pub all_segments: u64,
    pub new_segments: u64,
    pub old_targets: u64,
    pub unchanged_old_target_checks: u64,
    pub unchanged_old_target_accepts: u64,
    pub unchanged_segment_windows_avoided: u64,
    pub new_targets: u64,
    pub index_block_attempts: u64,
    pub index_block_admitted: u64,
    pub index_block_declined: u64,
    pub index_segments_built: u64,
    pub index_blocks_reused: u64,
    pub index_block_payload_peak: u64,
    pub old_relation_elements_cloned: u64,
    pub old_relation_capacity_charged: u64,
}

const MAX_GROWTH_INDEX_BLOCKS: usize = 16;
struct OwnedGrowthIndex {
    blocks: Vec<Arc<OwnedOverlapBlock<IndexedSegment>>>,
    covered: Vec<bool>,
    bytes: usize,
}
impl OwnedGrowthIndex {
    fn stage(
        selected: &[(usize, &crate::LineInstruction, Curve<'_>)],
        covered: &[bool],
        new_covered: &[bool],
        prior: Option<&Self>,
        budget: usize,
        mut work: Option<&mut GrowthCompilerWork>,
    ) -> Option<(Self, usize)> {
        if covered.len() != new_covered.len()
            || covered
                .iter()
                .zip(new_covered)
                .any(|(old, new)| *old && !*new)
        {
            return None;
        }
        if prior
            .is_some_and(|old| old.covered != covered || old.blocks.len() > MAX_GROWTH_INDEX_BLOCKS)
        {
            return None;
        }
        let mut out = Self {
            blocks: Vec::new(),
            covered: Vec::new(),
            bytes: 0,
        };
        // Fixed maximum metadata allocation; actual capacities charged first.
        let requested = std::mem::size_of::<Self>()
            .checked_add(std::mem::size_of::<(
                SuppressionRevision,
                FlatProjection,
                usize,
            )>())?
            .checked_add(
                MAX_GROWTH_INDEX_BLOCKS
                    .checked_mul(std::mem::size_of::<Arc<OwnedOverlapBlock<IndexedSegment>>>())?,
            )?
            .checked_add(new_covered.len())?;
        if requested > budget {
            return None;
        }
        out.blocks.try_reserve_exact(MAX_GROWTH_INDEX_BLOCKS).ok()?;
        out.covered.try_reserve_exact(new_covered.len()).ok()?;
        out.bytes = std::mem::size_of::<Self>()
            .checked_add(std::mem::size_of::<(
                SuppressionRevision,
                FlatProjection,
                usize,
            )>())?
            .checked_add(
                out.blocks
                    .capacity()
                    .checked_mul(std::mem::size_of::<Arc<OwnedOverlapBlock<IndexedSegment>>>())?,
            )?
            .checked_add(out.covered.capacity())?;
        if out.bytes > budget {
            return None;
        }
        out.covered.extend_from_slice(new_covered);
        if let Some(old) = prior {
            for block in &old.blocks {
                let charge = block
                    .charged_payload_bytes()
                    .checked_add(2 * std::mem::size_of::<usize>())?;
                out.bytes = out.bytes.checked_add(charge)?;
                if out.bytes > budget {
                    return None;
                }
                out.blocks.push(Arc::clone(block));
            }
            if let Some(w) = work.as_mut() {
                w.index_blocks_reused = w
                    .index_blocks_reused
                    .saturating_add(old.blocks.len() as u64);
            }
        } else {
            out.capture(selected, covered, true, budget, work.as_deref_mut())?;
        }
        let new_start = out.blocks.len();
        out.capture(selected, covered, false, budget, work)?;
        Some((out, new_start))
    }
    fn capture(
        &mut self,
        selected: &[(usize, &crate::LineInstruction, Curve<'_>)],
        covered: &[bool],
        old: bool,
        budget: usize,
        mut work: Option<&mut GrowthCompilerWork>,
    ) -> Option<()> {
        let count = selected
            .iter()
            .filter(|(i, _, _)| covered[*i] == old)
            .try_fold(0usize, |sum, (_, line, _)| {
                sum.checked_add(line.points.len().saturating_sub(1))
            })?;
        if count == 0 {
            return Some(());
        }
        if self.blocks.len() == MAX_GROWTH_INDEX_BLOCKS {
            return None;
        }
        let arc_header = 2 * std::mem::size_of::<usize>();
        let remaining = budget.checked_sub(self.bytes)?.checked_sub(arc_header)?;
        if count
            .checked_mul(std::mem::size_of::<OwnedEntry<IndexedSegment>>())?
            .checked_add(std::mem::size_of::<OwnedOverlapBlock<IndexedSegment>>())?
            > remaining
        {
            return None;
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(count).ok()?;
        if entries
            .capacity()
            .checked_mul(std::mem::size_of::<OwnedEntry<IndexedSegment>>())?
            .checked_add(std::mem::size_of::<OwnedOverlapBlock<IndexedSegment>>())?
            > remaining
        {
            return None;
        }
        for (i, line, _) in selected.iter().filter(|(i, _, _)| covered[*i] == old) {
            let priority = (line.display_plane.order().get(), line.priority.0);
            for pair in line.points.windows(2) {
                if let Some(segment) = Segment::new(pair[0], pair[1]) {
                    use rstar::RTreeObject;
                    let b = segment.envelope();
                    entries.push(OwnedEntry {
                        bounds: OwnedBounds {
                            lower: b.lower(),
                            upper: b.upper(),
                        },
                        payload: IndexedSegment {
                            segment,
                            index: *i,
                            priority,
                        },
                    });
                }
            }
        }
        if entries.is_empty() {
            return Some(());
        }
        let block = OwnedOverlapBlock::try_build_owned(entries, remaining)?;
        if let Some(w) = work.as_mut() {
            w.index_segments_built = w.index_segments_built.saturating_add(block.len() as u64);
        }
        self.bytes = self
            .bytes
            .checked_add(block.charged_payload_bytes())?
            .checked_add(arc_header)?;
        if self.bytes > budget {
            return None;
        }
        self.blocks.push(Arc::new(block));
        Some(())
    }
    fn visit(
        &self,
        range: std::ops::Range<usize>,
        bounds: &rstar::AABB<[f64; 2]>,
        mut callback: impl FnMut(&IndexedSegment) -> Option<()>,
    ) -> Option<()> {
        let blocks = self.blocks.get(range)?;
        let envelope = OwnedBounds {
            lower: bounds.lower(),
            upper: bounds.upper(),
        };
        for block in blocks {
            for entry in block.query(envelope)? {
                callback(&entry.payload)?;
            }
        }
        Some(())
    }
    fn any(
        &self,
        range: std::ops::Range<usize>,
        bounds: &rstar::AABB<[f64; 2]>,
        mut predicate: impl FnMut(&IndexedSegment) -> bool,
    ) -> Option<bool> {
        let blocks = self.blocks.get(range)?;
        let envelope = OwnedBounds {
            lower: bounds.lower(),
            upper: bounds.upper(),
        };
        for block in blocks {
            for entry in block.query(envelope)? {
                if predicate(&entry.payload) {
                    return Some(true);
                }
            }
        }
        Some(false)
    }
}
#[derive(Default)]
pub struct LineSuppressionCache {
    last: Option<([u8; 32], Arc<LineSuppressionPlan>)>,
    immutable_prepared: Option<(
        SuppressionRevision,
        FlatProjection,
        Vec<bool>,
        Option<PreparedLineSuppression>,
    )>,
    immutable_last: Option<(Vec<bool>, Arc<LineSuppressionPlan>)>,
    immutable_current: Option<Arc<LineSuppressionPlan>>,
    // Existing default-on bounded prewarm; decline tried once per namespaced source epoch.
    eligibility_program: eligibility_program::Cache,
    eligibility_policy: Option<bool>,
    eligibility_diagnostics_policy: Option<bool>,
    eligibility_work: Option<LineEligibilityWork>,
    prewarm_policy: Option<bool>,
    growth_reuse_policy: Option<bool>,
    unchanged_target_policy: Option<bool>,
    index_blocks_policy: Option<bool>,
    immutable_index_blocks: Option<(SuppressionRevision, FlatProjection, usize, OwnedGrowthIndex)>,
    empty_curve_prewarm_policy: Option<bool>,
    // Optional bounded counters only; no retained source/geometry.
    tail_counters: Option<[u64; 10]>,
    growth_child_work: Option<GrowthCompilerWork>,
    prewarm_attempt: Option<(SuppressionRevision, FlatProjection, usize)>,
}
impl LineSuppressionCache {
    /// New exclusive cache with the same already captured policies, no source plans/indices.
    /// Unsampled Option policies remain unsampled and keep the original lazy resolution contract.
    pub fn fork_empty_with_same_policy(&self) -> Self {
        let mut next = Self {
            eligibility_policy: self.eligibility_policy,
            eligibility_diagnostics_policy: self.eligibility_diagnostics_policy,
            prewarm_policy: self.prewarm_policy,
            growth_reuse_policy: self.growth_reuse_policy,
            unchanged_target_policy: self.unchanged_target_policy,
            index_blocks_policy: self.index_blocks_policy,
            empty_curve_prewarm_policy: self.empty_curve_prewarm_policy,
            ..Self::default()
        };
        next.set_tail_diagnostics_enabled(self.tail_counters.is_some());
        next
    }

    /// Diagnostic only: does not reset or alter any suppression decision.
    pub fn set_tail_diagnostics_enabled(&mut self, enabled: bool) {
        self.tail_counters = enabled.then_some([0; 10]);
        self.growth_child_work = enabled.then_some(GrowthCompilerWork::default());
    }
    /// Order: prewarm attempt/success/decline, preparation branch,
    /// geometry reset, eligible growth, legacy fallback, eligibility-plan hit,
    /// compiled visibility query, lazy relation-compiler invocation.
    pub fn tail_diagnostics_counters(&self) -> [u64; 10] {
        self.tail_counters.unwrap_or([0; 10])
    }
    /// Optional cumulative HOST children; does not alter suppression decisions.
    pub fn growth_compiler_work(&self) -> Option<GrowthCompilerWork> {
        self.growth_child_work
    }
    /// Optional whole eligibility HOST boundary. None means timing/census unavailable.
    pub fn eligibility_work(&self) -> Option<LineEligibilityWork> {
        self.eligibility_work
    }
    fn context_eligibility(
        &mut self,
        context: &crate::RenderContext,
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
        visibility: Option<&[bool]>,
    ) -> Vec<bool> {
        let diagnostics = self.eligibility_diagnostics_policy == Some(true);
        let started = diagnostics.then(std::time::Instant::now);
        // Explicit group policy ALWAYS executes the complete original predicate.
        let attempt = self.eligibility_policy == Some(true) && groups.is_none();
        let (cached, summary) = if attempt {
            self.eligibility_program
                .evaluate(context, scale, visibility)
        } else {
            (None, eligibility_program::Summary::default())
        };
        let optimized = cached.is_some();
        let eligible = cached.unwrap_or_else(|| {
            original_eligibility(
                context.raw_instructions(),
                scale,
                groups,
                override_group,
                visibility,
            )
        });
        // Stop the whole-call timer BEFORE diagnostic-only eligible-bit counting.
        if let Some(started) = started {
            let elapsed = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            let w = self.eligibility_work.get_or_insert_with(Default::default);
            w.calls = w.calls.saturating_add(1);
            w.optimized_calls = w.optimized_calls.saturating_add(u64::from(optimized));
            w.fallback_calls = w.fallback_calls.saturating_add(u64::from(!optimized));
            w.cold = w.cold.saturating_add(u64::from(summary.cold));
            w.hits = w.hits.saturating_add(u64::from(summary.hit));
            w.declines = w.declines.saturating_add(u64::from(summary.decline));
            w.source_checks = w
                .source_checks
                .saturating_add(summary.source_checks)
                .saturating_add(if optimized {
                    0
                } else {
                    context.instruction_count() as u64
                });
            w.descriptor_checks = w.descriptor_checks.saturating_add(if optimized {
                summary.entries as u64
            } else {
                0
            });
            w.whole_eligibility_host_ns = w.whole_eligibility_host_ns.saturating_add(elapsed);
            w.eligible = w
                .eligible
                .saturating_add(eligible.iter().filter(|v| **v).count() as u64);
            w.retained_payload_bytes = self.eligibility_program.retained_bytes() as u64;
            w.mask_payload_bytes = (std::mem::size_of::<Vec<bool>>()
                + eligible.capacity() * std::mem::size_of::<bool>())
                as u64;
        }
        eligible
    }
    fn tail_count(&mut self, index: usize) {
        if let Some(counts) = &mut self.tail_counters {
            counts[index] = counts[index].saturating_add(1);
        }
    }

    fn static_prewarm_policy(value: Option<&str>) -> bool {
        value.is_none_or(|value| value == "1")
    }
    /// Override bounded static preparation and invalidate all cached relations.
    pub fn set_static_prewarm_enabled(&mut self, enabled: bool) {
        self.clear();
        self.prewarm_policy = Some(enabled);
    }
    pub fn clear(&mut self) {
        self.eligibility_program.clear();
        if let Some(w) = &mut self.eligibility_work {
            w.retained_payload_bytes = 0;
        }
        self.prewarm_attempt = None;
        self.immutable_index_blocks = None;
        self.last = None;
        self.immutable_prepared = None;
        self.immutable_last = None;
        self.immutable_current = None;
    }
    pub fn current(&self) -> Option<&LineSuppressionPlan> {
        self.immutable_current
            .as_deref()
            .or_else(|| self.last.as_ref().map(|(_, p)| p.as_ref()))
    }
    pub fn immutable_preparation_bytes(&self) -> Option<usize> {
        self.immutable_prepared
            .as_ref()?
            .3
            .as_ref()
            .map(PreparedLineSuppression::retained_bytes)
    }
    /// Jointly budgeted immutable index payload, not GPU/RSS/global memory.
    pub fn immutable_index_bytes(&self) -> usize {
        self.immutable_index_blocks
            .as_ref()
            .map_or(0, |(_, _, _, index)| index.bytes)
    }
    /// Binding-safe context entry: callers cannot supply a different source slice
    /// with an inherited epoch. Current permissions are still evaluated every call.
    pub fn plan_context_projected_with_visibility(
        &mut self,
        context: &crate::RenderContext,
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
        visibility: Option<&[bool]>,
    ) -> Arc<LineSuppressionPlan> {
        let reuse = *self.eligibility_policy.get_or_insert_with(|| {
            eligibility_program::enabled(
                std::env::var_os("FERRITE_LINE_ELIGIBILITY_PROGRAM").as_deref(),
            )
        });
        let diagnostics = *self.eligibility_diagnostics_policy.get_or_insert_with(|| {
            eligibility_program::enabled(
                std::env::var_os("FERRITE_LINE_ELIGIBILITY_DIAGNOSTICS").as_deref(),
            )
        });
        self.plan_immutable_projected_with_visibility_key(
            context.raw_instructions(),
            SuppressionRevision::Context(context.static_line_relation_epoch()),
            scale,
            groups,
            override_group,
            visibility,
            context.scaler.projection(),
            (reuse || diagnostics).then_some(context),
        )
    }
    /// Static overlap reuse for immutable RenderContext instruction geometry.
    /// The caller MUST advance revision when points, ordering, priority, planes,
    /// suppression or deferred geometry changes. Live stroke visibility, date,
    /// scale, groups and execution permissions are evaluated on every call.
    /// Ordinary plan methods continue to hash mutable slice contents.
    #[expect(
        clippy::too_many_arguments,
        reason = "Independent immutable geometry/projection identity and live scale, viewing-group and coverage permissions must remain separate inputs; preserve established callers and suppression proof boundaries."
    )]
    pub fn plan_immutable_projected_with_visibility(
        &mut self,
        instructions: &[DrawingInstruction],
        revision: u64,
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
        visibility: Option<&[bool]>,
        projection: FlatProjection,
    ) -> Arc<LineSuppressionPlan> {
        self.plan_immutable_projected_with_visibility_key(
            instructions,
            SuppressionRevision::Legacy(revision),
            scale,
            groups,
            override_group,
            visibility,
            projection,
            None,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Independent immutable geometry/projection identity and live scale, viewing-group and coverage permissions must remain separate inputs; preserve established callers and suppression proof boundaries."
    )]
    fn plan_immutable_projected_with_visibility_key(
        &mut self,
        instructions: &[DrawingInstruction],
        revision: SuppressionRevision,
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
        visibility: Option<&[bool]>,
        projection: FlatProjection,
        context: Option<&crate::RenderContext>,
    ) -> Arc<LineSuppressionPlan> {
        debug_assert!(visibility.is_none_or(|v| v.len() == instructions.len()));
        let prewarm = *self.prewarm_policy.get_or_insert_with(|| {
            Self::static_prewarm_policy(
                std::env::var("FERRITE_LINE_SUPPRESSION_PREWARM")
                    .ok()
                    .as_deref(),
            )
        });
        let source = (revision, projection, instructions.len());
        if prewarm && self.prewarm_attempt != Some(source) {
            self.prewarm_attempt = Some(source);
            self.tail_count(0);
            let accept_empty = *self.empty_curve_prewarm_policy.get_or_insert_with(|| {
                std::env::var("FERRITE_LINE_SUPPRESSION_EMPTY_CURVE_PREWARM").as_deref() == Ok("1")
            });
            let static_preparation = if accept_empty {
                Self::try_static_prewarm_covered_empty(
                    instructions,
                    projection,
                    64 * 1024 * 1024,
                    32 * 1024 * 1024,
                )
            } else {
                Self::try_static_prewarm(
                    instructions,
                    projection,
                    64 * 1024 * 1024,
                    32 * 1024 * 1024,
                )
            };
            if let Some((prepared, compiled)) = static_preparation {
                self.immutable_last = None;
                self.tail_count(1);
                self.immutable_index_blocks = None;
                self.immutable_prepared = Some((revision, projection, prepared, Some(compiled)));
            } else {
                self.tail_count(2);
            }
            // Decline changes no original prepared state: lazy admission and its
            // original mutable fallback below remain authoritative.
        }
        let eligible = if let Some(context) = context {
            self.context_eligibility(context, scale, groups, override_group, visibility)
        } else {
            original_eligibility(instructions, scale, groups, override_group, visibility)
        };
        let same_geometry = self
            .immutable_prepared
            .as_ref()
            .is_some_and(|(r, p, _, _)| *r == revision && *p == projection);
        if !self
            .immutable_prepared
            .as_ref()
            .is_some_and(|(r, p, prepared, _)| {
                *r == revision
                    && *p == projection
                    && eligible.iter().zip(prepared).all(|(e, p)| !*e || *p)
            })
        {
            self.tail_count(3);
            self.tail_count(if same_geometry { 5 } else { 4 });
            self.immutable_last = None;
            // Prepared candidates grow only when a newly eligible line appears.
            // All live visibility rules still run before applying relations.
            let prepared: Vec<_> = if same_geometry {
                eligible
                    .iter()
                    .zip(&self.immutable_prepared.as_ref().unwrap().2)
                    .map(|(e, p)| *e || *p)
                    .collect()
            } else {
                eligible.clone()
            };
            let base = instructions
                .len()
                .saturating_mul(std::mem::size_of::<DrawingInstruction>());
            let points = instructions
                .iter()
                .zip(&prepared)
                .map(|(i, enabled)| match i {
                    DrawingInstruction::Line(l)
                        if *enabled && l.screen_ray.is_none() && l.portrayal_path.is_none() =>
                    {
                        l.points.len()
                    }
                    _ => 0,
                })
                .sum::<usize>();
            let mut staged_index = None;
            let compiled = if base
                .saturating_add(points.saturating_mul(std::mem::size_of::<WorldPoint>()))
                <= 64 * 1024 * 1024
            {
                let projection_start = self
                    .growth_child_work
                    .as_ref()
                    .map(|_| std::time::Instant::now());
                let transformed: Vec<_> = instructions
                    .iter()
                    .zip(&prepared)
                    .map(|(item, enabled)| {
                        let mut out = crate::LineInstruction::new(Vec::new());
                        if let DrawingInstruction::Line(line) = item {
                            if *enabled
                                && line.screen_ray.is_none()
                                && line.portrayal_path.is_none()
                            {
                                out.points = line
                                    .points
                                    .iter()
                                    .map(|p| WorldPoint::new(p.x, projection.project_y(p.y)))
                                    .collect();
                                out.priority = line.priority;
                                out.display_plane = line.display_plane;
                                out.suppressible = line.suppressible;
                            }
                        }
                        // Ineligible candidates need no projected point allocation.
                        // Newly eligible curves trigger bounded preparation above.
                        DrawingInstruction::Line(out)
                    })
                    .collect();
                if let (Some(w), Some(start)) = (self.growth_child_work.as_mut(), projection_start)
                {
                    w.projection_and_placeholder_ns = w
                        .projection_and_placeholder_ns
                        .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
                }
                self.tail_count(9);
                let reuse = *self.growth_reuse_policy.get_or_insert_with(|| {
                    retained_growth_enabled("FERRITE_LINE_SUPPRESSION_GROWTH_REUSE")
                });
                let unchanged = *self.unchanged_target_policy.get_or_insert_with(|| {
                    retained_growth_enabled("FERRITE_LINE_SUPPRESSION_UNCHANGED_TARGETS")
                });
                let index_blocks = *self.index_blocks_policy.get_or_insert_with(|| {
                    retained_growth_enabled("FERRITE_LINE_SUPPRESSION_INDEX_BLOCKS")
                });
                let indexed = if reuse && same_geometry && index_blocks {
                    self.immutable_prepared
                        .as_ref()
                        .and_then(|(_, _, covered, compiled)| {
                            compiled.as_ref().and_then(|previous| {
                                let prior = self
                                    .immutable_index_blocks
                                    .as_ref()
                                    .filter(|(r, p, n, _)| {
                                        *r == revision
                                            && *p == projection
                                            && *n == instructions.len()
                                    })
                                    .map(|(_, _, _, index)| index);
                                PreparedLineSuppression::compile_growth_with_index_blocks(
                                    &transformed,
                                    32 * 1024 * 1024,
                                    previous,
                                    covered,
                                    &prepared,
                                    prior,
                                    self.growth_child_work.as_mut(),
                                    unchanged,
                                )
                            })
                        })
                } else {
                    None
                };
                if index_blocks && reuse && same_geometry && indexed.is_none() {
                    if let Some(w) = self.growth_child_work.as_mut() {
                        w.index_block_declined = w.index_block_declined.saturating_add(1);
                    }
                }
                let incremental = if let Some((plan, index)) = indexed {
                    staged_index = Some(index);
                    Some(plan)
                } else if reuse && same_geometry {
                    self.immutable_prepared
                        .as_ref()
                        .and_then(|(_, _, covered, compiled)| {
                            compiled.as_ref().and_then(|previous| {
                                PreparedLineSuppression::compile_growth_with_unchanged_targets(
                                    &transformed,
                                    32 * 1024 * 1024,
                                    previous,
                                    covered,
                                    self.growth_child_work.as_mut(),
                                    unchanged,
                                )
                            })
                        })
                } else {
                    None
                };
                incremental
                    .or_else(|| PreparedLineSuppression::compile(&transformed, 32 * 1024 * 1024))
            } else {
                None
            };
            self.immutable_index_blocks =
                staged_index.map(|index| (revision, projection, instructions.len(), index));
            self.immutable_prepared = Some((revision, projection, prepared, compiled));
        }
        if self.immutable_prepared.as_ref().unwrap().3.is_none() {
            self.tail_count(6);
            return self.plan_projected_with_visibility(
                instructions,
                scale,
                groups,
                override_group,
                visibility,
                projection,
            );
        }
        if let Some((old, plan)) = &self.immutable_last {
            if *old == eligible {
                let plan = Arc::clone(plan);
                self.tail_count(7);
                self.immutable_current = Some(Arc::clone(&plan));
                return plan;
            }
        }
        self.tail_count(8);
        let plan = Arc::new(
            self.immutable_prepared
                .as_ref()
                .unwrap()
                .3
                .as_ref()
                .unwrap()
                .plan(&eligible)
                .expect("matching immutable geometry dimension"),
        );
        // A separate bounded exact visibility plan; no source geometry is kept.
        let bytes = plan.fully_suppressed.capacity() * 32
            + plan.partial.capacity() * 64
            + plan
                .partial
                .values()
                .map(|v| v.capacity() * std::mem::size_of::<LineSpan>())
                .sum::<usize>()
            + eligible.capacity();
        self.immutable_last = if bytes <= 16 * 1024 * 1024 {
            Some((eligible, Arc::clone(&plan)))
        } else {
            None
        };
        self.immutable_current = Some(Arc::clone(&plan));
        plan
    }
    /// Compile a superset of possible geographic source curves, never visibility.
    /// Invalid prospective geometry declines the whole speculative operation.
    /// The existing lazy planner alone decides how that geometry is handled.
    fn try_static_prewarm(
        instructions: &[DrawingInstruction],
        projection: FlatProjection,
        transform_budget: usize,
        compiler_budget: usize,
    ) -> Option<(Vec<bool>, PreparedLineSuppression)> {
        let mut points = 0usize;
        let mut segments = 0usize;
        let mut lines = 0usize;
        for item in instructions {
            if let DrawingInstruction::Line(line) = item {
                if line.screen_ray.is_none() && line.portrayal_path.is_none() {
                    // Do not speculatively process malformed/incomplete sources.
                    Curve::new(&line.points)?;
                    points = points.checked_add(line.points.len())?;
                    segments = segments.checked_add(line.points.len() - 1)?;
                    lines = lines.checked_add(1)?;
                }
            }
        }
        let base = instructions
            .len()
            .checked_mul(std::mem::size_of::<DrawingInstruction>())?;
        if base.checked_add(points.checked_mul(std::mem::size_of::<WorldPoint>())?)?
            > transform_budget
            || segments.checked_mul(128)? > compiler_budget
            || lines.checked_mul(128)? > compiler_budget
        {
            return None;
        }
        let mut prepared = Vec::with_capacity(instructions.len());
        let mut transformed = Vec::with_capacity(instructions.len());
        for item in instructions {
            let mut out = crate::LineInstruction::new(Vec::new());
            let mut selected = false;
            if let DrawingInstruction::Line(line) = item {
                if line.screen_ray.is_none() && line.portrayal_path.is_none() {
                    selected = true;
                    // Identical original project_y expression and source order.
                    out.points = line
                        .points
                        .iter()
                        .map(|p| WorldPoint::new(p.x, projection.project_y(p.y)))
                        .collect();
                    Curve::new(&out.points)?;
                    // Extreme arithmetic is left to the original lazy planner.
                    if out
                        .points
                        .windows(2)
                        .any(|p| !(p[1].x - p[0].x).is_finite() || !(p[1].y - p[0].y).is_finite())
                    {
                        return None;
                    }
                    out.priority = line.priority;
                    out.display_plane = line.display_plane;
                    out.suppressible = line.suppressible;
                }
            }
            prepared.push(selected);
            transformed.push(DrawingInstruction::Line(out));
        }
        let compiled = PreparedLineSuppression::compile(&transformed, compiler_budget)?;
        Some((prepared, compiled))
    }
    fn try_static_prewarm_covered_empty(
        instructions: &[DrawingInstruction],
        projection: FlatProjection,
        transform_budget: usize,
        compiler_budget: usize,
    ) -> Option<(Vec<bool>, PreparedLineSuppression)> {
        let mut points = 0usize;
        let mut segments = 0usize;
        let mut lines = 0usize;
        for item in instructions {
            if let DrawingInstruction::Line(line) = item {
                if line.screen_ray.is_none() && line.portrayal_path.is_none() {
                    // Original Curve::new excludes <2 points before any relation.
                    // Admit only finite short placeholders; no nonfinite or
                    // zero-length two-point reinterpretation.
                    if line.points.len() < 2 {
                        if line
                            .points
                            .iter()
                            .any(|p| !p.x.is_finite() || !p.y.is_finite())
                        {
                            return None;
                        }
                        continue;
                    }
                    Curve::new(&line.points)?;
                    points = points.checked_add(line.points.len())?;
                    segments = segments.checked_add(line.points.len() - 1)?;
                    lines = lines.checked_add(1)?;
                }
            }
        }
        let base = instructions
            .len()
            .checked_mul(std::mem::size_of::<DrawingInstruction>())?;
        if base.checked_add(points.checked_mul(std::mem::size_of::<WorldPoint>())?)?
            > transform_budget
            || segments.checked_mul(128)? > compiler_budget
            || lines.checked_mul(128)? > compiler_budget
        {
            return None;
        }
        let mut prepared = Vec::with_capacity(instructions.len());
        let mut transformed = Vec::with_capacity(instructions.len());
        for item in instructions {
            let mut out = crate::LineInstruction::new(Vec::new());
            let mut selected = false;
            if let DrawingInstruction::Line(line) = item {
                if line.screen_ray.is_none() && line.portrayal_path.is_none() {
                    selected = true;
                    if line.points.len() < 2 {
                        // Covered because it remains absent in both legacy and
                        // compiled suppression. Raw emission/picking is untouched.
                        prepared.push(true);
                        transformed.push(DrawingInstruction::Line(out));
                        continue;
                    }
                    // Identical original project_y expression and source order.
                    out.points = line
                        .points
                        .iter()
                        .map(|p| WorldPoint::new(p.x, projection.project_y(p.y)))
                        .collect();
                    Curve::new(&out.points)?;
                    // Extreme arithmetic is left to the original lazy planner.
                    if out
                        .points
                        .windows(2)
                        .any(|p| !(p[1].x - p[0].x).is_finite() || !(p[1].y - p[0].y).is_finite())
                    {
                        return None;
                    }
                    out.priority = line.priority;
                    out.display_plane = line.display_plane;
                    out.suppressible = line.suppressible;
                }
            }
            prepared.push(selected);
            transformed.push(DrawingInstruction::Line(out));
        }
        let compiled = PreparedLineSuppression::compile(&transformed, compiler_budget)?;
        Some((prepared, compiled))
    }
    pub fn plan(
        &mut self,
        instructions: &[DrawingInstruction],
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
    ) -> Arc<LineSuppressionPlan> {
        self.plan_with_visibility(instructions, scale, groups, override_group, None)
    }
    /// Additional selector visibility applies before priority suppression.
    pub fn plan_with_visibility(
        &mut self,
        instructions: &[DrawingInstruction],
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
        visibility: Option<&[bool]>,
    ) -> Arc<LineSuppressionPlan> {
        self.plan_projected_with_visibility(
            instructions,
            scale,
            groups,
            override_group,
            visibility,
            FlatProjection::LocalGeographic,
        )
    }
    /// Suppression and returned segment fractions use the same projection as
    /// drawing and picking. Visibility is evaluated before any priority rule.
    /// Only point buffers are projected, on a cache miss; styles are not cloned.
    pub fn plan_projected_with_visibility(
        &mut self,
        instructions: &[DrawingInstruction],
        scale: u32,
        groups: Option<&HashSet<u32>>,
        override_group: Option<u32>,
        visibility: Option<&[bool]>,
        projection: FlatProjection,
    ) -> Arc<LineSuppressionPlan> {
        self.immutable_current = None;
        debug_assert!(visibility.is_none_or(|v| v.len() == instructions.len()));
        let mut digest = Fingerprint(Sha256::new());
        projection.hash(&mut digest);
        for (index, instruction) in instructions.iter().enumerate() {
            if !visibility.is_none_or(|v| v.get(index).copied().unwrap_or(false))
                || !instruction_visible(instruction, scale, groups, override_group)
            {
                continue;
            }
            let DrawingInstruction::Line(line) = instruction else {
                continue;
            };
            if line.screen_ray.is_some() || line.portrayal_path.is_some() {
                continue;
            }
            let Some(_) = Curve::new(&line.points) else {
                continue;
            };
            index.hash(&mut digest);
            line.points.len().hash(&mut digest);
            for p in &line.points {
                bits(*p).hash(&mut digest);
            }
            line.display_plane.order().hash(&mut digest);
            line.priority.0.hash(&mut digest);
            line.suppressible.hash(&mut digest);
        }
        let fingerprint = digest.0.finalize().into();
        if let Some((old, plan)) = &self.last {
            if *old == fingerprint {
                return Arc::clone(plan);
            }
        }
        // Allocate projected point buffers only after a cache miss. Legacy
        // coordinates borrow the source buffer; invalid projected paths never
        // suppress another line. A full instruction clone would also copy style
        // and deferred geometry that are irrelevant to this spatial query.
        let projected: Vec<Option<Cow<'_, [WorldPoint]>>> = instructions
            .iter()
            .enumerate()
            .map(|(index, instruction)| {
                if !visibility.is_none_or(|v| v.get(index).copied().unwrap_or(false))
                    || !instruction_visible(instruction, scale, groups, override_group)
                {
                    return None;
                }
                let DrawingInstruction::Line(line) = instruction else {
                    return None;
                };
                if line.screen_ray.is_some() || line.portrayal_path.is_some() {
                    return None;
                }
                Curve::new(&line.points)?;
                if projection == FlatProjection::LocalGeographic {
                    Some(Cow::Borrowed(line.points.as_slice()))
                } else {
                    let points: Vec<_> = line
                        .points
                        .iter()
                        .map(|p| WorldPoint::new(p.x, projection.project_y(p.y)))
                        .collect();
                    Curve::new(&points)?;
                    Some(Cow::Owned(points))
                }
            })
            .collect();
        let eligible: Vec<_> = instructions
            .iter()
            .enumerate()
            .filter_map(|(index, instruction)| {
                if !visibility.is_none_or(|v| v.get(index).copied().unwrap_or(false))
                    || !instruction_visible(instruction, scale, groups, override_group)
                {
                    return None;
                }
                let DrawingInstruction::Line(line) = instruction else {
                    return None;
                };
                if line.screen_ray.is_some() || line.portrayal_path.is_some() {
                    return None;
                }
                Some((
                    index,
                    Curve::new(projected[index].as_deref()?)?,
                    (line.display_plane.order().get(), line.priority.0),
                    line.suppressible,
                ))
            })
            .collect();
        let mut priorities = HashMap::new();
        for (_, curve, priority, _) in &eligible {
            priorities
                .entry(*curve)
                .and_modify(|v: &mut (i32, i32)| *v = (*v).max(*priority))
                .or_insert(*priority);
        }
        let mut plan = LineSuppressionPlan::default();
        for (index, curve, priority, suppressible) in &eligible {
            if *suppressible && *priority < priorities[curve] {
                plan.fully_suppressed.insert(*index);
            }
        }
        // Exact shared segments are deduplicated before spatial indexing. Each
        // priority has its own index, so equal/lower priorities are never queried.
        if let Some(min_priority) = eligible
            .iter()
            .filter(|(i, _, _, s)| *s && !plan.contains(i))
            .map(|(_, _, p, _)| *p)
            .min()
        {
            let mut segments = HashMap::new();
            for (_, curve, priority, _) in &eligible {
                for w in curve.points.windows(2) {
                    if let Some(segment) = Segment::new(w[0], w[1]) {
                        segments
                            .entry(segment.key())
                            .and_modify(|value: &mut (Segment, (i32, i32))| {
                                value.1 = value.1.max(*priority)
                            })
                            .or_insert((segment, *priority));
                    }
                }
            }
            let mut by_priority = std::collections::BTreeMap::<(i32, i32), Vec<Segment>>::new();
            for (segment, priority) in segments.values() {
                if *priority > min_priority {
                    by_priority.entry(*priority).or_default().push(*segment);
                }
            }
            let trees: std::collections::BTreeMap<_, _> = by_priority
                .into_iter()
                .map(|(p, s)| (p, rstar::RTree::bulk_load(s)))
                .collect();
            use rstar::RTreeObject;
            for (index, curve, priority, suppressible) in &eligible {
                if !suppressible
                    || plan.contains(index)
                    || trees
                        .range((
                            std::ops::Bound::Excluded(*priority),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                        .is_none()
                {
                    continue;
                }
                let mut visible = Vec::new();
                let mut changed = false;
                for (segment_index, w) in curve.points.windows(2).enumerate() {
                    let Some(segment) = Segment::new(w[0], w[1]) else {
                        continue;
                    };
                    if segments
                        .get(&segment.key())
                        .is_some_and(|(_, p)| p > priority)
                    {
                        changed = true;
                        continue;
                    }
                    let mut hidden = Vec::new();
                    for (_, tree) in trees.range((
                        std::ops::Bound::Excluded(*priority),
                        std::ops::Bound::Unbounded,
                    )) {
                        for higher in tree.locate_in_envelope_intersecting(&segment.envelope()) {
                            if let Some(overlap) = segment.overlap(*higher) {
                                hidden.push(overlap);
                            }
                        }
                    }
                    hidden.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
                    let mut cursor = 0.;
                    for (start, end) in hidden {
                        changed = true;
                        if start > cursor {
                            visible.push(LineSpan {
                                segment: segment_index,
                                start: cursor,
                                end: start,
                            });
                        }
                        cursor = cursor.max(end);
                        if cursor >= 1. {
                            break;
                        }
                    }
                    if cursor < 1. {
                        visible.push(LineSpan {
                            segment: segment_index,
                            start: cursor,
                            end: 1.,
                        });
                    }
                }
                if changed {
                    if visible.is_empty() {
                        plan.fully_suppressed.insert(*index);
                    } else {
                        plan.partial.insert(*index, visible);
                    }
                }
            }
        }
        let plan = Arc::new(plan);
        self.last = Some((fingerprint, Arc::clone(&plan)));
        plan
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LineInstruction, ScaleRange, ViewingGroup};
    fn line(priority: i32, reversed: bool) -> DrawingInstruction {
        let mut points = vec![
            WorldPoint::new(1., 2.),
            WorldPoint::new(3., 4.),
            WorldPoint::new(5., 7.),
        ];
        if reversed {
            points.reverse();
        }
        DrawingInstruction::Line(LineInstruction::new(points).with_priority(priority))
    }
    #[test]
    fn mercator_overlap_fractions_visibility_and_cache_follow_projection() {
        let low = DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(2., 60.), WorldPoint::new(2., 80.)])
                .with_priority(2),
        );
        let high = DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(2., 70.), WorldPoint::new(2., 80.)])
                .with_priority(9),
        );
        let lines = [low, high];
        let mut cache = LineSuppressionCache::default();
        let legacy = cache.plan(&lines, 0, None, None);
        assert_eq!(legacy.spans(0).unwrap()[0].end, 0.5);
        let merc = cache.plan_projected_with_visibility(
            &lines,
            0,
            None,
            None,
            Some(&[true, true]),
            FlatProjection::EllipsoidalMercator,
        );
        let y = |lat| {
            ferrite_kernel::geodesy::Mercator::World
                .project(ferrite_kernel::geodesy::GeographicPosition::new(lat, 2.).unwrap())
                .unwrap()[1]
        };
        let t = (y(70.) - y(60.)) / (y(80.) - y(60.));
        assert!((merc.spans(0).unwrap()[0].end - t).abs() < 1e-14);
        assert!((t - 0.5).abs() > 0.1);
        let hit = cache.plan_projected_with_visibility(
            &lines,
            0,
            None,
            None,
            Some(&[true, true]),
            FlatProjection::EllipsoidalMercator,
        );
        assert!(Arc::ptr_eq(&merc, &hit));
        let hidden = cache.plan_projected_with_visibility(
            &lines,
            0,
            None,
            None,
            Some(&[true, false]),
            FlatProjection::EllipsoidalMercator,
        );
        assert!(hidden.is_empty());
        let mut scaler = Scaler::new(
            crate::GeoBounds::new(0., 60., 4., 80.),
            crate::Viewport::new(1000., 1000.),
        );
        scaler.set_projection(FlatProjection::EllipsoidalMercator);
        assert!(crate::hit_geometry_visible(
            &lines[0],
            &scaler,
            scaler.world_to_screen(WorldPoint::new(2., 65.)),
            0.1,
            merc.spans(0)
        )
        .is_some());
        assert!(crate::hit_geometry_visible(
            &lines[0],
            &scaler,
            scaler.world_to_screen(WorldPoint::new(2., 75.)),
            0.1,
            merc.spans(0)
        )
        .is_none());
        let (a, b) = merc.spans(0).unwrap()[0]
            .screen_endpoints(
                match &lines[0] {
                    DrawingInstruction::Line(l) => &l.points,
                    _ => unreachable!(),
                },
                &scaler,
            )
            .unwrap();
        assert!((b.y - scaler.world_to_screen(WorldPoint::new(2., 70.)).y).abs() < 0.001);
        assert!(a.y > b.y);
        let legacy_again = cache.plan(&lines, 0, None, None);
        assert_eq!(legacy_again.spans(0).unwrap()[0].end, 0.5);
    }
    #[test]
    fn projected_screen_spans_preserve_straight_rhumb_and_dash_fraction() {
        let mut scaler = Scaler::new(
            crate::GeoBounds::new(-10., 60., 10., 80.),
            crate::Viewport::new(1000., 1000.),
        );
        scaler.set_projection(FlatProjection::EllipsoidalMercator);
        let points = [WorldPoint::new(-8., 60.), WorldPoint::new(8., 80.)];
        let span = LineSpan {
            segment: 0,
            start: 0.25,
            end: 0.75,
        };
        let (a, b) = span.screen_endpoints(&points, &scaler).unwrap();
        let p = scaler.world_to_screen(points[0]);
        let q = scaler.world_to_screen(points[1]);
        assert!((a.y - (p.y + (q.y - p.y) * 0.25)).abs() < 1e-4);
        assert!((b.x - (p.x + (q.x - p.x) * 0.75)).abs() < 1e-4);
        let old = scaler.world_to_screen(span.endpoints(&points).unwrap().0);
        assert!((old.y - a.y).abs() > 20.);
        let style = crate::LineStyle {
            dash_pattern: vec![30., 20.],
            ..Default::default()
        };
        let spans = crate::dash_line_spans(&points, &scaler, &style, None).unwrap();
        let first = spans[0];
        let (start, end) = first.screen_endpoints(&points, &scaler).unwrap();
        let length = ((end.x - start.x) as f64).hypot((end.y - start.y) as f64);
        assert!((length - 30.).abs() < 0.001);
    }
    #[test]
    fn only_drawn_higher_priority_curves_can_suppress_and_cache_tracks_settings() {
        let mut lines = vec![line(2, false), line(9, false)];
        if let DrawingInstruction::Line(high) = &mut lines[1] {
            high.scale_range = ScaleRange {
                scale_minimum: Some(10000),
                scale_maximum: None,
            };
            high.viewing_group = ViewingGroup(999);
        }
        let mut cache = LineSuppressionCache::default();
        assert!(cache.plan(&lines, 20000, None, None).is_empty());
        assert!(cache.plan(&lines, 5000, None, None).contains(&0));
        let shown = HashSet::from([lines[0].viewing_group().0]);
        assert!(cache.plan(&lines, 5000, Some(&shown), None).is_empty());
        assert!(cache.plan(&lines, 5000, None, None).contains(&0));
    }
    #[test]
    fn disabled_secondary_group_never_suppresses_visible_line() {
        let low = DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)])
                .with_priority(2)
                .with_viewing_groups(&[27070]),
        );
        let high = DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)])
                .with_priority(9)
                .with_viewing_groups(&[27070, 90020]),
        );
        let lines = [low, high];
        let mut cache = LineSuppressionCache::default();
        let primary = HashSet::from([27070]);
        let both = HashSet::from([27070, 90020]);
        assert!(cache.plan(&lines, 1000, Some(&primary), None).is_empty());
        assert!(cache.plan(&lines, 1000, Some(&both), None).contains(&0));
        assert!(cache.plan(&lines, 1000, Some(&primary), None).is_empty());
    }
    #[test]
    fn reversed_curves_equal_priorities_and_unsuppressed_lines() {
        let mut lines = vec![
            line(2, false),
            line(9, true),
            line(9, false),
            line(2, false),
        ];
        if let DrawingInstruction::Line(low) = &mut lines[3] {
            low.suppressible = false;
        }
        let plan = LineSuppressionCache::default().plan(&lines, 1000, None, None);
        assert_eq!(plan.fully_suppressed, HashSet::from([0]));
    }
    #[test]
    fn same_vector_address_mutations_invalidate_cache() {
        let mut lines = vec![line(2, false), line(9, false)];
        let address = lines.as_ptr();
        let mut cache = LineSuppressionCache::default();
        let a = cache.plan(&lines, 1000, None, None);
        let b = cache.plan(&lines, 1000, None, None);
        assert!(Arc::ptr_eq(&a, &b));
        if let DrawingInstruction::Line(high) = &mut lines[1] {
            high.points[1].y += 1.;
        }
        assert_eq!(address, lines.as_ptr());
        assert!(cache.plan(&lines, 1000, None, None).is_empty());
        if let DrawingInstruction::Line(high) = &mut lines[1] {
            high.points[1].y -= 1.;
            high.priority.0 = 1;
        }
        assert_eq!(
            cache.plan(&lines, 1000, None, None).fully_suppressed,
            HashSet::from([1])
        );
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;
    use crate::{GeoBounds, LineInstruction, Scaler, Viewport};
    fn line(priority: i32, points: &[(f64, f64)]) -> DrawingInstruction {
        DrawingInstruction::Line(
            LineInstruction::new(points.iter().map(|&(x, y)| WorldPoint::new(x, y)).collect())
                .with_priority(priority),
        )
    }
    #[test]
    fn differently_sampled_overlaps_keep_unshared_remainders_and_pick_geometry() {
        let lines = vec![
            line(2, &[(0., 5.), (10., 5.)]),
            line(8, &[(2., 5.), (3., 5.), (6., 5.)]),
            line(9, &[(8., 5.), (6., 5.)]),
        ];
        let plan = LineSuppressionCache::default().plan(&lines, 1000, None, None);
        assert_eq!(
            plan.spans(0).unwrap(),
            &[
                LineSpan {
                    segment: 0,
                    start: 0.,
                    end: 0.2
                },
                LineSpan {
                    segment: 0,
                    start: 0.8,
                    end: 1.
                }
            ]
        );
        let scaler = Scaler::new(
            GeoBounds::new(0., 0., 10., 10.),
            Viewport::new(1000., 1000.),
        );
        assert!(crate::hit_geometry_visible(
            &lines[0],
            &scaler,
            scaler.world_to_screen(WorldPoint::new(5., 5.)),
            1.,
            plan.spans(0)
        )
        .is_none());
        assert!(crate::hit_geometry_visible(
            &lines[0],
            &scaler,
            scaler.world_to_screen(WorldPoint::new(1., 5.)),
            1.,
            plan.spans(0)
        )
        .is_some());
    }
    #[test]
    fn reversed_same_address_line_invalidates_fraction_cache() {
        let mut lines = vec![
            line(2, &[(0., 0.), (10., 0.)]),
            line(8, &[(0., 0.), (4., 0.)]),
        ];
        let mut cache = LineSuppressionCache::default();
        let a = cache.plan(&lines, 1000, None, None);
        assert_eq!(a.spans(0).unwrap()[0].start, 0.4);
        if let DrawingInstruction::Line(low) = &mut lines[0] {
            low.points.reverse();
        }
        let b = cache.plan(&lines, 1000, None, None);
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(b.spans(0).unwrap()[0].end, 0.6);
    }
    #[test]
    fn crossing_touching_and_nearby_parallel_segments_do_not_suppress() {
        let lines = vec![
            line(1, &[(0., 0.), (10., 0.)]),
            line(8, &[(5., -1.), (5., 1.)]),
            line(9, &[(10., 0.), (20., 0.)]),
            line(9, &[(0., 1e-20), (10., 1e-20)]),
        ];
        assert!(LineSuppressionCache::default()
            .plan(&lines, 1000, None, None)
            .is_empty());
        let diagonal = vec![
            line(1, &[(0., 0.), (10., 20.)]),
            line(8, &[(6., 12.), (2., 4.)]),
        ];
        let p = LineSuppressionCache::default().plan(&diagonal, 1000, None, None);
        assert_eq!(p.spans(0).unwrap()[0].end, 0.2);
        assert_eq!(p.spans(0).unwrap()[1].start, 0.6);
    }
    #[test]
    fn endpoint_union_across_low_vertices_and_unsuppressed_exception() {
        let mut lines = vec![
            line(1, &[(0., 0.), (3., 0.), (7., 0.), (10., 0.)]),
            line(8, &[(2., 0.), (8., 0.)]),
        ];
        let p = LineSuppressionCache::default().plan(&lines, 1000, None, None);
        let spans = p.spans(0).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].segment, 0);
        assert_eq!(spans[1].segment, 2);
        if let DrawingInstruction::Line(low) = &mut lines[0] {
            low.suppressible = false;
        }
        assert!(LineSuppressionCache::default()
            .plan(&lines, 1000, None, None)
            .spans(0)
            .is_none());
    }
    #[test]
    fn indexed_result_matches_independent_interval_oracle() {
        let mut seed = 17u64;
        for _ in 0..128 {
            let mut lines = Vec::new();
            let mut data = Vec::new();
            for _ in 0..8 {
                let mut next = || {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    ((seed >> 32) % 20) as f64
                };
                let a = next();
                let mut b = next();
                if a == b {
                    b += 1.;
                }
                let priority = (next() as i32) % 5;
                data.push((a, b, priority));
                lines.push(line(priority, &[(a, 0.), (b, 0.)]));
            }
            let plan = LineSuppressionCache::default().plan(&lines, 1000, None, None);
            for (i, &(a, b, p)) in data.iter().enumerate() {
                for sample in 0..80 {
                    let x = (sample as f64 + 0.375) / 4.;
                    if x <= a.min(b) || x >= a.max(b) {
                        continue;
                    }
                    let expected = !data
                        .iter()
                        .any(|&(c, d, q)| q > p && x > c.min(d) && x < c.max(d));
                    let actual = if let Some(spans) = plan.spans(i) {
                        spans.iter().any(|s| {
                            let (c, d) = s
                                .endpoints(match &lines[i] {
                                    DrawingInstruction::Line(l) => &l.points,
                                    _ => unreachable!(),
                                })
                                .unwrap();
                            x > c.x.min(d.x) && x < c.x.max(d.x)
                        })
                    } else {
                        true
                    };
                    assert_eq!(
                        actual, expected,
                        "source={a}..{b}, priority={p}, x={x}, fixture={data:?}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod plane_suppression_tests {
    use super::*;
    use crate::{DisplayPlane, LineInstruction};
    #[test]
    fn higher_plane_wins_shared_curve_and_partial_segments_and_invalidates_cache() {
        let points = vec![WorldPoint::new(0., 0.), WorldPoint::new(10., 0.)];
        let plane = |n| DisplayPlane::Interoperability(std::num::NonZeroI32::new(n).unwrap());
        let mut lines = vec![
            DrawingInstruction::Line(
                LineInstruction::new(points.clone())
                    .with_priority(99)
                    .with_display_plane(plane(-10000)),
            ),
            DrawingInstruction::Line(
                LineInstruction::new(points)
                    .with_priority(0)
                    .with_display_plane(plane(-500)),
            ),
        ];
        let mut cache = LineSuppressionCache::default();
        let first = cache.plan(&lines, 1000, None, None);
        assert!(first.contains(&0));
        assert!(!first.contains(&1));
        if let DrawingInstruction::Line(line) = &mut lines[1] {
            line.display_plane = plane(-20000);
        }
        let second = cache.plan(&lines, 1000, None, None);
        assert!(!second.contains(&0));
        assert!(second.contains(&1));
        if let DrawingInstruction::Line(line) = &mut lines[1] {
            line.display_plane = plane(-500);
            line.points = vec![WorldPoint::new(2., 0.), WorldPoint::new(8., 0.)];
        }
        let partial = cache.plan(&lines, 1000, None, None);
        assert!(!partial.contains(&0));
        assert!(!partial.contains(&1));
        let spans = partial.spans(0).unwrap();
        assert_eq!(spans.len(), 2);
    }
}

/// Camera-independent overlap relations. The host compiles stable projected
/// geometry once and supplies current date/group/resource/dependency eligibility.
/// A bounded compilation can decline dense input; hosts then use the ordinary
/// spatial planner. No visibility or numeric tolerance is changed by this cache.
pub struct PreparedLineSuppression {
    dimension: usize,
    targets: Vec<PreparedTarget>,
    bytes: usize,
}
struct PreparedTarget {
    index: usize,
    full_higher: Vec<usize>,
    segments: Vec<PreparedSegment>,
}
struct PreparedSegment {
    index: usize,
    higher: Vec<(usize, f64, f64)>,
}
#[derive(Clone, Copy)]
struct IndexedSegment {
    segment: Segment,
    index: usize,
    priority: (i32, i32),
}
impl rstar::RTreeObject for IndexedSegment {
    type Envelope = rstar::AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        rstar::RTreeObject::envelope(&self.segment)
    }
}
impl PreparedLineSuppression {
    pub fn compile(instructions: &[DrawingInstruction], budget: usize) -> Option<Self> {
        use rstar::RTreeObject;
        let selected: Vec<_> = instructions
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let DrawingInstruction::Line(line) = item else {
                    return None;
                };
                if line.screen_ray.is_some()
                    || line.portrayal_path.is_some()
                    || !line.style.has_visible_stroke()
                {
                    return None;
                }
                Some((i, line, Curve::new(&line.points)?))
            })
            .collect();
        let segment_count: usize = selected
            .iter()
            .map(|(_, line, _)| line.points.len().saturating_sub(1))
            .sum();
        // The spatial index is temporary, but cap it before allocating as well.
        if segment_count.checked_mul(128)? > budget || selected.len().checked_mul(128)? > budget {
            return None;
        }
        type CoincidentSourceGroups<'a> = HashMap<Curve<'a>, Vec<(usize, (i32, i32))>>;
        let mut groups: CoincidentSourceGroups<'_> = HashMap::new();
        let mut segments = Vec::with_capacity(segment_count);
        for (i, line, curve) in &selected {
            let priority = (line.display_plane.order().get(), line.priority.0);
            groups.entry(*curve).or_default().push((*i, priority));
            for pair in line.points.windows(2) {
                if let Some(segment) = Segment::new(pair[0], pair[1]) {
                    segments.push(IndexedSegment {
                        segment,
                        index: *i,
                        priority,
                    });
                }
            }
        }
        let tree = rstar::RTree::bulk_load(segments);
        let mut targets = Vec::new();
        let mut bytes = std::mem::size_of::<Self>();
        for (i, line, curve) in selected {
            if !line.suppressible {
                continue;
            }
            let priority = (line.display_plane.order().get(), line.priority.0);
            let full_higher: Vec<_> = groups[&curve]
                .iter()
                .filter(|(_, p)| *p > priority)
                .map(|(i, _)| *i)
                .collect();
            bytes = bytes.checked_add(full_higher.capacity() * std::mem::size_of::<usize>())?;
            if bytes > budget {
                return None;
            }
            let mut prepared = Vec::new();
            let mut changed = !full_higher.is_empty();
            for (index, pair) in line.points.windows(2).enumerate() {
                let Some(segment) = Segment::new(pair[0], pair[1]) else {
                    continue;
                };
                let mut higher = Vec::new();
                for other in tree.locate_in_envelope_intersecting(&segment.envelope()) {
                    if other.priority <= priority || full_higher.contains(&other.index) {
                        continue;
                    }
                    if let Some((start, end)) = segment.overlap(other.segment) {
                        let old_capacity = higher.capacity();
                        higher.push((other.index, start, end));
                        bytes = bytes.checked_add(
                            (higher.capacity() - old_capacity)
                                * std::mem::size_of::<(usize, f64, f64)>(),
                        )?;
                        if bytes > budget {
                            return None;
                        }
                    }
                }
                changed |= !higher.is_empty();
                // Filtering this list by eligibility preserves the original
                // planner's endpoint sort, including exact signed-zero ordering.
                higher.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)));
                let old_capacity = prepared.capacity();
                prepared.push(PreparedSegment { index, higher });
                bytes = bytes.checked_add(
                    (prepared.capacity() - old_capacity) * std::mem::size_of::<PreparedSegment>(),
                )?;
                if bytes > budget {
                    return None;
                }
            }
            if changed {
                let old_capacity = targets.capacity();
                targets.push(PreparedTarget {
                    index: i,
                    full_higher,
                    segments: prepared,
                });
                bytes = bytes.checked_add(
                    (targets.capacity() - old_capacity) * std::mem::size_of::<PreparedTarget>(),
                )?;
                if bytes > budget {
                    return None;
                }
            } else {
                bytes -= full_higher.capacity() * std::mem::size_of::<usize>()
                    + prepared.capacity() * std::mem::size_of::<PreparedSegment>()
                    + prepared
                        .iter()
                        .map(|s| s.higher.capacity() * std::mem::size_of::<(usize, f64, f64)>())
                        .sum::<usize>();
            }
        }
        Some(Self {
            dimension: instructions.len(),
            targets,
            bytes,
        })
    }
    /// Private caller must prove same opaque context epoch/projection and union
    /// coverage. Only old-old relations are reused; all new relations use overlap.
    #[cfg(test)]
    fn compile_growth(
        instructions: &[DrawingInstruction],
        budget: usize,
        previous: &Self,
        covered: &[bool],
    ) -> Option<Self> {
        Self::compile_growth_with_work(instructions, budget, previous, covered, None)
    }
    #[cfg(test)]
    fn compile_growth_with_work(
        instructions: &[DrawingInstruction],
        budget: usize,
        previous: &Self,
        covered: &[bool],
        work: Option<&mut GrowthCompilerWork>,
    ) -> Option<Self> {
        Self::compile_growth_with_unchanged_targets(
            instructions,
            budget,
            previous,
            covered,
            work,
            false,
        )
    }
    fn compile_growth_with_unchanged_targets(
        instructions: &[DrawingInstruction],
        budget: usize,
        previous: &Self,
        covered: &[bool],
        mut work: Option<&mut GrowthCompilerWork>,
        reuse_unchanged: bool,
    ) -> Option<Self> {
        if let Some(w) = work.as_mut() {
            w.attempts = w.attempts.saturating_add(1);
        }
        if previous.dimension != instructions.len() || covered.len() != instructions.len() {
            return None;
        }
        use rstar::RTreeObject;
        let selected: Vec<_> = instructions
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let DrawingInstruction::Line(line) = item else {
                    return None;
                };
                if line.screen_ray.is_some()
                    || line.portrayal_path.is_some()
                    || !line.style.has_visible_stroke()
                {
                    return None;
                }
                Some((i, line, Curve::new(&line.points)?))
            })
            .collect();
        let segment_count: usize = selected
            .iter()
            .map(|(_, line, _)| line.points.len().saturating_sub(1))
            .sum();
        let new_segment_count: usize = selected
            .iter()
            .filter(|(i, _, _)| !covered[*i])
            .map(|(_, line, _)| line.points.len().saturating_sub(1))
            .sum();
        // Two temporary indexes share the ORIGINAL compiler scratch admission.
        if segment_count
            .checked_add(new_segment_count)?
            .checked_mul(128)?
            > budget
        {
            return None;
        }
        // The spatial index is temporary, but cap it before allocating as well.
        if segment_count.checked_mul(128)? > budget || selected.len().checked_mul(128)? > budget {
            return None;
        }
        if let Some(w) = work.as_mut() {
            w.all_segments = w.all_segments.saturating_add(segment_count as u64);
            w.new_segments = w.new_segments.saturating_add(new_segment_count as u64);
        }
        let group_start = work.as_ref().map(|_| std::time::Instant::now());
        type CoincidentSourceGroups<'a> = HashMap<Curve<'a>, Vec<(usize, (i32, i32))>>;
        let mut groups: CoincidentSourceGroups<'_> = HashMap::new();
        let mut segments = Vec::with_capacity(segment_count);
        let mut new_segments = Vec::with_capacity(new_segment_count);
        for (i, line, curve) in &selected {
            let priority = (line.display_plane.order().get(), line.priority.0);
            groups.entry(*curve).or_default().push((*i, priority));
            for pair in line.points.windows(2) {
                if let Some(segment) = Segment::new(pair[0], pair[1]) {
                    let entry = IndexedSegment {
                        segment,
                        index: *i,
                        priority,
                    };
                    segments.push(entry);
                    if !covered[*i] {
                        new_segments.push(entry);
                    }
                }
            }
        }
        if let (Some(w), Some(start)) = (work.as_mut(), group_start) {
            w.group_and_segment_build_ns = w
                .group_and_segment_build_ns
                .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
        let index_start = work.as_ref().map(|_| std::time::Instant::now());
        let tree = rstar::RTree::bulk_load(segments);
        let new_tree = rstar::RTree::bulk_load(new_segments);
        if let (Some(w), Some(start)) = (work.as_mut(), index_start) {
            w.two_index_build_ns = w
                .two_index_build_ns
                .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
        let target_start = work.as_ref().map(|_| std::time::Instant::now());
        let mut previous_target_cursor = 0usize;
        let mut targets = Vec::new();
        let mut bytes = std::mem::size_of::<Self>();
        for (i, line, curve) in selected {
            if !line.suppressible {
                continue;
            }
            while previous_target_cursor < previous.targets.len()
                && previous.targets[previous_target_cursor].index < i
            {
                previous_target_cursor += 1;
            }
            let old_target = if covered[i] {
                previous
                    .targets
                    .get(previous_target_cursor)
                    .filter(|t| t.index == i)
            } else {
                None
            };
            if let Some(w) = work.as_mut() {
                if covered[i] {
                    w.old_targets = w.old_targets.saturating_add(1);
                } else {
                    w.new_targets = w.new_targets.saturating_add(1);
                }
            }
            let query_tree = if covered[i] { &new_tree } else { &tree };
            let priority = (line.display_plane.order().get(), line.priority.0);
            let full_higher: Vec<_> = groups[&curve]
                .iter()
                .filter(|(_, p)| *p > priority)
                .map(|(i, _)| *i)
                .collect();
            bytes = bytes.checked_add(full_higher.capacity() * std::mem::size_of::<usize>())?;
            if bytes > budget {
                return None;
            }
            // Superset bbox test ONLY. The source epoch/projection and covered
            // union are the same private proof as the existing growth compiler.
            // Full-coincident new sources MUST be checked separately: a repeated
            // identical-point curve has no Segment::new entries but can still
            // participate in the ORIGINAL full-curve priority rule.
            if reuse_unchanged && covered[i] {
                if let Some(w) = work.as_mut() {
                    w.unchanged_old_target_checks = w.unchanged_old_target_checks.saturating_add(1);
                }
                let new_full_higher = full_higher.iter().any(|j| !covered[*j]);
                let first = line.points[0];
                let (mut lower, mut upper) = ([first.x, first.y], [first.x, first.y]);
                for point in &line.points[1..] {
                    lower[0] = lower[0].min(point.x);
                    lower[1] = lower[1].min(point.y);
                    upper[0] = upper[0].max(point.x);
                    upper[1] = upper[1].max(point.y);
                }
                let envelope = rstar::AABB::from_corners(lower, upper);
                let possible_partial = new_tree
                    .locate_in_envelope_intersecting(&envelope)
                    .any(|other| other.priority > priority && !full_higher.contains(&other.index));
                let old_is_exact = old_target.is_some_and(|old| old.full_higher == full_higher)
                    || (old_target.is_none() && full_higher.is_empty());
                if !new_full_higher && !possible_partial && old_is_exact {
                    if let Some(w) = work.as_mut() {
                        w.unchanged_old_target_accepts =
                            w.unchanged_old_target_accepts.saturating_add(1);
                        w.unchanged_segment_windows_avoided = w
                            .unchanged_segment_windows_avoided
                            .saturating_add(line.points.len().saturating_sub(1) as u64);
                    }
                    if let Some(old) = old_target {
                        // Preserve every old interval bit and ordinal, including
                        // temporarily hidden occluders. No shared mutable payload.
                        let mut segments = Vec::new();
                        let requested = old
                            .segments
                            .len()
                            .checked_mul(std::mem::size_of::<PreparedSegment>())?;
                        let old_higher_requested =
                            old.segments.iter().try_fold(0usize, |sum, part| {
                                sum.checked_add(
                                    part.higher.len().checked_mul(std::mem::size_of::<(
                                        usize,
                                        f64,
                                        f64,
                                    )>(
                                    ))?,
                                )
                            })?;
                        if bytes
                            .checked_add(requested)?
                            .checked_add(old_higher_requested)?
                            > budget
                        {
                            return None;
                        }
                        segments.try_reserve_exact(old.segments.len()).ok()?;
                        bytes = bytes.checked_add(
                            segments
                                .capacity()
                                .checked_mul(std::mem::size_of::<PreparedSegment>())?,
                        )?;
                        if bytes > budget {
                            return None;
                        }
                        for part in &old.segments {
                            let mut higher = Vec::new();
                            higher.try_reserve_exact(part.higher.len()).ok()?;
                            let charge = higher.capacity().checked_mul(std::mem::size_of::<(
                                usize,
                                f64,
                                f64,
                            )>(
                            ))?;
                            bytes = bytes.checked_add(charge)?;
                            if bytes > budget {
                                return None;
                            }
                            higher.extend_from_slice(&part.higher);
                            if let Some(w) = work.as_mut() {
                                w.old_relation_elements_cloned = w
                                    .old_relation_elements_cloned
                                    .saturating_add(higher.len() as u64);
                                w.old_relation_capacity_charged = w
                                    .old_relation_capacity_charged
                                    .saturating_add(charge as u64);
                            }
                            segments.push(PreparedSegment {
                                index: part.index,
                                higher,
                            });
                        }
                        let capacity = targets.capacity();
                        targets.push(PreparedTarget {
                            index: i,
                            full_higher,
                            segments,
                        });
                        bytes = bytes.checked_add(
                            (targets.capacity() - capacity)
                                .checked_mul(std::mem::size_of::<PreparedTarget>())?,
                        )?;
                        if bytes > budget {
                            return None;
                        }
                    } else {
                        // The original compiler would discard this unchanged
                        // relation-free target. Only metadata is cached; never raw
                        // drawing, selection, eligibility, or source visibility.
                        bytes -= full_higher.capacity() * std::mem::size_of::<usize>();
                    }
                    continue;
                }
            }
            let mut prepared = Vec::new();
            let mut changed = !full_higher.is_empty();
            for (index, pair) in line.points.windows(2).enumerate() {
                let Some(segment) = Segment::new(pair[0], pair[1]) else {
                    continue;
                };
                let mut higher = old_target
                    .and_then(|t| {
                        t.segments
                            .binary_search_by_key(&index, |s| s.index)
                            .ok()
                            .map(|slot| t.segments[slot].higher.clone())
                    })
                    .unwrap_or_default();
                if let Some(w) = work.as_mut() {
                    w.old_relation_elements_cloned = w
                        .old_relation_elements_cloned
                        .saturating_add(higher.len() as u64);
                    w.old_relation_capacity_charged =
                        w.old_relation_capacity_charged.saturating_add(
                            higher
                                .capacity()
                                .saturating_mul(std::mem::size_of::<(usize, f64, f64)>())
                                as u64,
                        );
                }
                bytes = bytes.checked_add(
                    higher
                        .capacity()
                        .checked_mul(std::mem::size_of::<(usize, f64, f64)>())?,
                )?;
                if bytes > budget {
                    return None;
                }
                for other in query_tree.locate_in_envelope_intersecting(&segment.envelope()) {
                    if other.priority <= priority || full_higher.contains(&other.index) {
                        continue;
                    }
                    if let Some((start, end)) = segment.overlap(other.segment) {
                        let old_capacity = higher.capacity();
                        higher.push((other.index, start, end));
                        bytes = bytes.checked_add(
                            (higher.capacity() - old_capacity)
                                * std::mem::size_of::<(usize, f64, f64)>(),
                        )?;
                        if bytes > budget {
                            return None;
                        }
                    }
                }
                changed |= !higher.is_empty();
                // Filtering this list by eligibility preserves the original
                // planner's endpoint sort, including exact signed-zero ordering.
                higher.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)));
                let old_capacity = prepared.capacity();
                prepared.push(PreparedSegment { index, higher });
                bytes = bytes.checked_add(
                    (prepared.capacity() - old_capacity) * std::mem::size_of::<PreparedSegment>(),
                )?;
                if bytes > budget {
                    return None;
                }
            }
            if changed {
                let old_capacity = targets.capacity();
                targets.push(PreparedTarget {
                    index: i,
                    full_higher,
                    segments: prepared,
                });
                bytes = bytes.checked_add(
                    (targets.capacity() - old_capacity) * std::mem::size_of::<PreparedTarget>(),
                )?;
                if bytes > budget {
                    return None;
                }
            } else {
                bytes -= full_higher.capacity() * std::mem::size_of::<usize>()
                    + prepared.capacity() * std::mem::size_of::<PreparedSegment>()
                    + prepared
                        .iter()
                        .map(|s| s.higher.capacity() * std::mem::size_of::<(usize, f64, f64)>())
                        .sum::<usize>();
            }
        }
        if let (Some(w), Some(start)) = (work.as_mut(), target_start) {
            w.target_relations_ns = w
                .target_relations_ns
                .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            w.admitted = w.admitted.saturating_add(1);
        }
        Some(Self {
            dimension: instructions.len(),
            targets,
            bytes,
        })
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Keep immutable old/current coverage witnesses, private index, original budget, diagnostics, and unchanged-target policy explicit; no public source authority."
    )]
    fn compile_growth_with_index_blocks(
        instructions: &[DrawingInstruction],
        budget: usize,
        previous: &Self,
        covered: &[bool],
        new_covered: &[bool],
        prior_index: Option<&OwnedGrowthIndex>,
        mut work: Option<&mut GrowthCompilerWork>,
        reuse_unchanged: bool,
    ) -> Option<(Self, OwnedGrowthIndex)> {
        if let Some(w) = work.as_mut() {
            w.index_block_attempts = w.index_block_attempts.saturating_add(1);
        }
        if let Some(w) = work.as_mut() {
            w.attempts = w.attempts.saturating_add(1);
        }
        if previous.dimension != instructions.len()
            || covered.len() != instructions.len()
            || new_covered.len() != instructions.len()
        {
            return None;
        }
        use rstar::RTreeObject;
        let selected: Vec<_> = instructions
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let DrawingInstruction::Line(line) = item else {
                    return None;
                };
                if line.screen_ray.is_some()
                    || line.portrayal_path.is_some()
                    || !line.style.has_visible_stroke()
                {
                    return None;
                }
                Some((i, line, Curve::new(&line.points)?))
            })
            .collect();
        let segment_count: usize = selected
            .iter()
            .map(|(_, line, _)| line.points.len().saturating_sub(1))
            .sum();
        let new_segment_count: usize = selected
            .iter()
            .filter(|(i, _, _)| !covered[*i])
            .map(|(_, line, _)| line.points.len().saturating_sub(1))
            .sum();
        // Two temporary indexes share the ORIGINAL compiler scratch admission.
        if segment_count
            .checked_add(new_segment_count)?
            .checked_mul(128)?
            > budget
        {
            return None;
        }
        // The spatial index is temporary, but cap it before allocating as well.
        if segment_count.checked_mul(128)? > budget || selected.len().checked_mul(128)? > budget {
            return None;
        }
        if let Some(w) = work.as_mut() {
            w.all_segments = w.all_segments.saturating_add(segment_count as u64);
            w.new_segments = w.new_segments.saturating_add(new_segment_count as u64);
        }
        let group_start = work.as_ref().map(|_| std::time::Instant::now());
        type CoincidentSourceGroups<'a> = HashMap<Curve<'a>, Vec<(usize, (i32, i32))>>;
        let mut groups: CoincidentSourceGroups<'_> = HashMap::new();

        for (i, line, curve) in &selected {
            let priority = (line.display_plane.order().get(), line.priority.0);
            groups.entry(*curve).or_default().push((*i, priority));
        }

        if let (Some(w), Some(start)) = (work.as_mut(), group_start) {
            w.group_and_segment_build_ns = w
                .group_and_segment_build_ns
                .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
        let index_start = work.as_ref().map(|_| std::time::Instant::now());
        let (owned_index, new_start) = OwnedGrowthIndex::stage(
            &selected,
            covered,
            new_covered,
            prior_index,
            budget,
            work.as_deref_mut(),
        )?;
        if let (Some(w), Some(start)) = (work.as_mut(), index_start) {
            w.two_index_build_ns = w
                .two_index_build_ns
                .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
        let target_start = work.as_ref().map(|_| std::time::Instant::now());
        let mut previous_target_cursor = 0usize;
        let mut targets = Vec::new();
        let mut bytes = std::mem::size_of::<Self>();
        for (i, line, curve) in selected {
            if !line.suppressible {
                continue;
            }
            while previous_target_cursor < previous.targets.len()
                && previous.targets[previous_target_cursor].index < i
            {
                previous_target_cursor += 1;
            }
            let old_target = if covered[i] {
                previous
                    .targets
                    .get(previous_target_cursor)
                    .filter(|t| t.index == i)
            } else {
                None
            };
            if let Some(w) = work.as_mut() {
                if covered[i] {
                    w.old_targets = w.old_targets.saturating_add(1);
                } else {
                    w.new_targets = w.new_targets.saturating_add(1);
                }
            }
            let query_start = if covered[i] { new_start } else { 0 };
            let priority = (line.display_plane.order().get(), line.priority.0);
            let full_higher: Vec<_> = groups[&curve]
                .iter()
                .filter(|(_, p)| *p > priority)
                .map(|(i, _)| *i)
                .collect();
            bytes = bytes.checked_add(full_higher.capacity() * std::mem::size_of::<usize>())?;
            if bytes > budget {
                return None;
            }
            // Superset bbox test ONLY. The source epoch/projection and covered
            // union are the same private proof as the existing growth compiler.
            // Full-coincident new sources MUST be checked separately: a repeated
            // identical-point curve has no Segment::new entries but can still
            // participate in the ORIGINAL full-curve priority rule.
            if reuse_unchanged && covered[i] {
                if let Some(w) = work.as_mut() {
                    w.unchanged_old_target_checks = w.unchanged_old_target_checks.saturating_add(1);
                }
                let new_full_higher = full_higher.iter().any(|j| !covered[*j]);
                let first = line.points[0];
                let (mut lower, mut upper) = ([first.x, first.y], [first.x, first.y]);
                for point in &line.points[1..] {
                    lower[0] = lower[0].min(point.x);
                    lower[1] = lower[1].min(point.y);
                    upper[0] = upper[0].max(point.x);
                    upper[1] = upper[1].max(point.y);
                }
                let envelope = rstar::AABB::from_corners(lower, upper);
                let possible_partial =
                    owned_index.any(new_start..owned_index.blocks.len(), &envelope, |other| {
                        other.priority > priority && !full_higher.contains(&other.index)
                    })?;
                let old_is_exact = old_target.is_some_and(|old| old.full_higher == full_higher)
                    || (old_target.is_none() && full_higher.is_empty());
                if !new_full_higher && !possible_partial && old_is_exact {
                    if let Some(w) = work.as_mut() {
                        w.unchanged_old_target_accepts =
                            w.unchanged_old_target_accepts.saturating_add(1);
                        w.unchanged_segment_windows_avoided = w
                            .unchanged_segment_windows_avoided
                            .saturating_add(line.points.len().saturating_sub(1) as u64);
                    }
                    if let Some(old) = old_target {
                        // Preserve every old interval bit and ordinal, including
                        // temporarily hidden occluders. No shared mutable payload.
                        let mut segments = Vec::new();
                        let requested = old
                            .segments
                            .len()
                            .checked_mul(std::mem::size_of::<PreparedSegment>())?;
                        let old_higher_requested =
                            old.segments.iter().try_fold(0usize, |sum, part| {
                                sum.checked_add(
                                    part.higher.len().checked_mul(std::mem::size_of::<(
                                        usize,
                                        f64,
                                        f64,
                                    )>(
                                    ))?,
                                )
                            })?;
                        if bytes
                            .checked_add(requested)?
                            .checked_add(old_higher_requested)?
                            > budget
                        {
                            return None;
                        }
                        segments.try_reserve_exact(old.segments.len()).ok()?;
                        bytes = bytes.checked_add(
                            segments
                                .capacity()
                                .checked_mul(std::mem::size_of::<PreparedSegment>())?,
                        )?;
                        if bytes > budget {
                            return None;
                        }
                        for part in &old.segments {
                            let mut higher = Vec::new();
                            higher.try_reserve_exact(part.higher.len()).ok()?;
                            let charge = higher.capacity().checked_mul(std::mem::size_of::<(
                                usize,
                                f64,
                                f64,
                            )>(
                            ))?;
                            bytes = bytes.checked_add(charge)?;
                            if bytes > budget {
                                return None;
                            }
                            higher.extend_from_slice(&part.higher);
                            if let Some(w) = work.as_mut() {
                                w.old_relation_elements_cloned = w
                                    .old_relation_elements_cloned
                                    .saturating_add(higher.len() as u64);
                                w.old_relation_capacity_charged = w
                                    .old_relation_capacity_charged
                                    .saturating_add(charge as u64);
                            }
                            segments.push(PreparedSegment {
                                index: part.index,
                                higher,
                            });
                        }
                        let capacity = targets.capacity();
                        targets.push(PreparedTarget {
                            index: i,
                            full_higher,
                            segments,
                        });
                        bytes = bytes.checked_add(
                            (targets.capacity() - capacity)
                                .checked_mul(std::mem::size_of::<PreparedTarget>())?,
                        )?;
                        if bytes > budget {
                            return None;
                        }
                    } else {
                        // The original compiler would discard this unchanged
                        // relation-free target. Only metadata is cached; never raw
                        // drawing, selection, eligibility, or source visibility.
                        bytes -= full_higher.capacity() * std::mem::size_of::<usize>();
                    }
                    continue;
                }
            }
            let mut prepared = Vec::new();
            let mut changed = !full_higher.is_empty();
            for (index, pair) in line.points.windows(2).enumerate() {
                let Some(segment) = Segment::new(pair[0], pair[1]) else {
                    continue;
                };
                let mut higher = old_target
                    .and_then(|t| {
                        t.segments
                            .binary_search_by_key(&index, |s| s.index)
                            .ok()
                            .map(|slot| t.segments[slot].higher.clone())
                    })
                    .unwrap_or_default();
                if let Some(w) = work.as_mut() {
                    w.old_relation_elements_cloned = w
                        .old_relation_elements_cloned
                        .saturating_add(higher.len() as u64);
                    w.old_relation_capacity_charged =
                        w.old_relation_capacity_charged.saturating_add(
                            higher
                                .capacity()
                                .saturating_mul(std::mem::size_of::<(usize, f64, f64)>())
                                as u64,
                        );
                }
                bytes = bytes.checked_add(
                    higher
                        .capacity()
                        .checked_mul(std::mem::size_of::<(usize, f64, f64)>())?,
                )?;
                if bytes > budget {
                    return None;
                }
                owned_index.visit(
                    query_start..owned_index.blocks.len(),
                    &segment.envelope(),
                    |other| {
                        if other.priority <= priority || full_higher.contains(&other.index) {
                            return Some(());
                        }
                        if let Some((start, end)) = segment.overlap(other.segment) {
                            let old_capacity = higher.capacity();
                            higher.push((other.index, start, end));
                            bytes = bytes.checked_add(
                                (higher.capacity() - old_capacity)
                                    * std::mem::size_of::<(usize, f64, f64)>(),
                            )?;
                            if bytes > budget {
                                return None;
                            }
                        }
                        Some(())
                    },
                )?;
                changed |= !higher.is_empty();
                // Filtering this list by eligibility preserves the original
                // planner's endpoint sort, including exact signed-zero ordering.
                higher.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)));
                let old_capacity = prepared.capacity();
                prepared.push(PreparedSegment { index, higher });
                bytes = bytes.checked_add(
                    (prepared.capacity() - old_capacity) * std::mem::size_of::<PreparedSegment>(),
                )?;
                if bytes > budget {
                    return None;
                }
            }
            if changed {
                let old_capacity = targets.capacity();
                targets.push(PreparedTarget {
                    index: i,
                    full_higher,
                    segments: prepared,
                });
                bytes = bytes.checked_add(
                    (targets.capacity() - old_capacity) * std::mem::size_of::<PreparedTarget>(),
                )?;
                if bytes > budget {
                    return None;
                }
            } else {
                bytes -= full_higher.capacity() * std::mem::size_of::<usize>()
                    + prepared.capacity() * std::mem::size_of::<PreparedSegment>()
                    + prepared
                        .iter()
                        .map(|s| s.higher.capacity() * std::mem::size_of::<(usize, f64, f64)>())
                        .sum::<usize>();
            }
        }
        if bytes.checked_add(owned_index.bytes)? > budget {
            return None;
        }
        if let Some(w) = work.as_mut() {
            w.index_block_admitted = w.index_block_admitted.saturating_add(1);
            w.index_block_payload_peak = w.index_block_payload_peak.max(owned_index.bytes as u64);
        }
        if let (Some(w), Some(start)) = (work.as_mut(), target_start) {
            w.target_relations_ns = w
                .target_relations_ns
                .saturating_add(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            w.admitted = w.admitted.saturating_add(1);
        }
        Some((
            Self {
                dimension: instructions.len(),
                targets,
                bytes,
            },
            owned_index,
        ))
    }
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }
    pub fn plan(&self, eligible: &[bool]) -> Result<LineSuppressionPlan, &'static str> {
        if eligible.len() != self.dimension {
            return Err("Invalid prepared suppression eligibility length");
        }
        let mut plan = LineSuppressionPlan::default();
        for target in &self.targets {
            if !eligible[target.index] {
                continue;
            }
            if target.full_higher.iter().any(|i| eligible[*i]) {
                plan.fully_suppressed.insert(target.index);
                continue;
            }
            let mut changed = false;
            let mut visible = Vec::new();
            for segment in &target.segments {
                let mut cursor = 0.;
                for &(higher, start, end) in &segment.higher {
                    if !eligible[higher] {
                        continue;
                    }
                    changed = true;
                    if start > cursor {
                        visible.push(LineSpan {
                            segment: segment.index,
                            start: cursor,
                            end: start,
                        });
                    }
                    cursor = cursor.max(end);
                    if cursor >= 1. {
                        break;
                    }
                }
                if cursor < 1. {
                    visible.push(LineSpan {
                        segment: segment.index,
                        start: cursor,
                        end: 1.,
                    });
                }
            }
            if changed {
                if visible.is_empty() {
                    plan.fully_suppressed.insert(target.index);
                } else {
                    plan.partial.insert(target.index, visible);
                }
            }
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod prepared_relation_tests {
    use super::*;
    use crate::{DisplayPriority, LineInstruction};
    fn line(points: &[(f64, f64)], priority: i32) -> DrawingInstruction {
        let mut line =
            LineInstruction::new(points.iter().map(|&(x, y)| WorldPoint::new(x, y)).collect());
        line.priority = DisplayPriority(priority);
        DrawingInstruction::Line(line)
    }
    #[test]
    fn all_visibility_combinations_match_cold_exact_partial_and_parallel_suppression() {
        let mut items = vec![
            line(&[(0., 0.), (10., 0.)], 1),
            line(&[(2., 0.), (4., 0.)], 2),
            line(&[(8., 0.), (3., 0.)], 3),
            line(&[(10., 0.), (0., 0.)], 4),
            line(&[(0., 1e-12), (10., 1e-12)], 9),
            line(&[(0., 0.), (10., 0.)], 99),
        ];
        if let DrawingInstruction::Line(l) = &mut items[1] {
            l.suppressible = false;
        }
        if let DrawingInstruction::Line(l) = &mut items[5] {
            l.style.width = 0.;
        }
        let prepared = PreparedLineSuppression::compile(&items, 1024 * 1024).unwrap();
        assert!(prepared.retained_bytes() <= 1024 * 1024);
        for bits in 0..(1 << items.len()) {
            let visible: Vec<_> = (0..items.len()).map(|i| bits & (1 << i) != 0).collect();
            let cold = LineSuppressionCache::default().plan_with_visibility(
                &items,
                0,
                None,
                None,
                Some(&visible),
            );
            assert_eq!(prepared.plan(&visible).unwrap(), *cold, "mask {bits}");
        }
        assert!(PreparedLineSuppression::compile(&items, 1).is_none());
        assert!(prepared.plan(&[]).is_err());
    }
    #[test]
    fn duplicate_zero_length_and_shared_subsegments_match_cold_planner() {
        let items = vec![
            line(&[(0., 0.), (0., 0.)], 1),
            line(&[(0., 0.), (0., 0.)], 3),
            line(&[(0., 0.), (0., 0.), (5., 0.), (10., 0.)], 1),
            line(&[(10., 0.), (5., 0.), (0., 0.), (0., 0.)], 3),
            line(&[(0., 0.), (5., 0.)], 2),
        ];
        let prepared = PreparedLineSuppression::compile(&items, 1024 * 1024).unwrap();
        for bits in 0..(1 << items.len()) {
            let visible: Vec<_> = (0..items.len()).map(|i| bits & (1 << i) != 0).collect();
            assert_eq!(
                prepared.plan(&visible).unwrap(),
                *LineSuppressionCache::default().plan_with_visibility(
                    &items,
                    0,
                    None,
                    None,
                    Some(&visible)
                ),
                "mask {bits}"
            );
        }
    }
}

#[cfg(test)]
mod immutable_projected_tests {
    use super::*;
    #[test]
    fn immutable_matches_mutable_masks_palette_and_revision() {
        let mut items = vec![
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![
                    WorldPoint::new(0., 60.),
                    WorldPoint::new(0., 80.),
                ])
                .with_priority(2),
            ),
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![
                    WorldPoint::new(0., 70.),
                    WorldPoint::new(0., 80.),
                ])
                .with_priority(9),
            ),
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![
                    WorldPoint::new(0., 80.),
                    WorldPoint::new(0., 60.),
                ])
                .with_priority(5),
            ),
        ];
        let mut warm = LineSuppressionCache::default();
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for bits in 0..8 {
                let visible: Vec<_> = (0..3).map(|i| bits & (1 << i) != 0).collect();
                let cold = LineSuppressionCache::default().plan_projected_with_visibility(
                    &items,
                    0,
                    None,
                    None,
                    Some(&visible),
                    projection,
                );
                let cached = warm.plan_immutable_projected_with_visibility(
                    &items,
                    1,
                    0,
                    None,
                    None,
                    Some(&visible),
                    projection,
                );
                assert_eq!(*cold, *cached);
                assert_eq!(warm.current(), Some(cached.as_ref()));
            }
        }
        if let DrawingInstruction::Line(l) = &mut items[1] {
            l.style.color.a = 0.;
        }
        let cold = LineSuppressionCache::default().plan_projected_with_visibility(
            &items,
            0,
            None,
            None,
            None,
            FlatProjection::EllipsoidalMercator,
        );
        assert_eq!(
            *cold,
            *warm.plan_immutable_projected_with_visibility(
                &items,
                1,
                0,
                None,
                None,
                None,
                FlatProjection::EllipsoidalMercator
            )
        );
        if let DrawingInstruction::Line(l) = &mut items[1] {
            l.style.color.a = 1.;
            l.points[0].x = 1.;
        }
        let cold = LineSuppressionCache::default().plan_projected_with_visibility(
            &items,
            0,
            None,
            None,
            None,
            FlatProjection::EllipsoidalMercator,
        );
        assert_eq!(
            *cold,
            *warm.plan_immutable_projected_with_visibility(
                &items,
                2,
                0,
                None,
                None,
                None,
                FlatProjection::EllipsoidalMercator
            )
        );
        assert_eq!(
            *cold,
            *warm.plan_projected_with_visibility(
                &items,
                0,
                None,
                None,
                None,
                FlatProjection::EllipsoidalMercator
            )
        );
        assert_eq!(warm.current(), Some(cold.as_ref()));
        warm.clear();
        assert!(warm.current().is_none());
    }
}

#[cfg(test)]
mod static_prewarm_contract_tests {
    use super::*;
    fn line(a: (f64, f64), b: (f64, f64), priority: i32) -> DrawingInstruction {
        DrawingInstruction::Line(
            crate::LineInstruction::new(vec![WorldPoint::new(a.0, a.1), WorldPoint::new(b.0, b.1)])
                .with_priority(priority),
        )
    }
    fn fixture() -> Vec<DrawingInstruction> {
        vec![
            line((0., 60.), (0., 80.), 2),
            line((0., 70.), (0., 80.), 9),
            line((0., 80.), (0., 60.), 5),
        ]
    }
    fn compare(
        cache: &mut LineSuppressionCache,
        items: &[DrawingInstruction],
        epoch: u64,
        projection: FlatProjection,
        scale: u32,
        groups: Option<&HashSet<u32>>,
        visible: &[bool],
    ) {
        let mut original = LineSuppressionCache::default();
        original.set_static_prewarm_enabled(false);
        let golden = original.plan_immutable_projected_with_visibility(
            items,
            epoch,
            scale,
            groups,
            None,
            Some(visible),
            projection,
        );
        let actual = cache.plan_immutable_projected_with_visibility(
            items,
            epoch,
            scale,
            groups,
            None,
            Some(visible),
            projection,
        );
        assert_eq!(*actual, *golden);
        // Independent ordinary planner, not a copy of speculative logic.
        let direct = LineSuppressionCache::default().plan_projected_with_visibility(
            items,
            scale,
            groups,
            None,
            Some(visible),
            projection,
        );
        assert_eq!(*actual, *direct);
    }
    #[test]
    fn growth_and_visibility_never_admit_an_ineligible_high_priority_source() {
        let items = fixture();
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(true);
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for mask in [1, 3, 7, 0, 5, 2, 7, 1] {
                let visible: Vec<_> = (0..3).map(|i| mask & (1 << i) != 0).collect();
                compare(&mut cache, &items, 1, projection, 0, None, &visible);
                assert!(cache
                    .immutable_prepared
                    .as_ref()
                    .unwrap()
                    .2
                    .iter()
                    .all(|p| *p));
                assert!(cache.immutable_preparation_bytes().unwrap() <= 32 * 1024 * 1024);
            }
        }
    }
    #[test]
    fn source_epoch_reorder_remove_and_projection_clear_original_relations() {
        let mut items = fixture();
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(true);
        for epoch in 1..5 {
            let visible = vec![true; items.len()];
            compare(
                &mut cache,
                &items,
                epoch,
                FlatProjection::EllipsoidalMercator,
                0,
                None,
                &visible,
            );
            if epoch == 1 {
                items.reverse();
            } else if epoch == 2 {
                items.pop();
            } else {
                if let DrawingInstruction::Line(l) = &mut items[0] {
                    l.points[0].x += 1.;
                }
            }
        }
        cache.clear();
        assert!(cache.prewarm_attempt.is_none());
        assert!(cache.current().is_none());
    }
    #[test]
    fn group_scale_alpha_and_temporal_coverage_permissions_stay_live() {
        let mut items = fixture();
        if let DrawingInstruction::Line(l) = &mut items[1] {
            l.scale_range = crate::ScaleRange {
                scale_minimum: Some(1000),
                scale_maximum: Some(100),
            };
            l.viewing_group = crate::ViewingGroup(999);
        }
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(true);
        for groups in [
            HashSet::new(),
            items
                .iter()
                .flat_map(|i| i.viewing_groups().map(|g| g.0))
                .chain([999])
                .collect::<HashSet<u32>>(),
        ] {
            for scale in [0, 99, 100, 1000, 1001] {
                for mask in [vec![true, true, true], vec![true, false, true]] {
                    compare(
                        &mut cache,
                        &items,
                        1,
                        FlatProjection::LocalGeographic,
                        scale,
                        Some(&groups),
                        &mask,
                    );
                }
            }
        }
        if let DrawingInstruction::Line(l) = &mut items[1] {
            l.style.color.a = 0.;
        }
        compare(
            &mut cache,
            &items,
            2,
            FlatProjection::LocalGeographic,
            100,
            None,
            &[true, true, true],
        );
    }
    #[test]
    fn invalid_prospective_curve_declines_and_preserves_lazy_result() {
        for bad in [f64::NAN, f64::INFINITY, 100.] {
            let mut items = fixture();
            if let DrawingInstruction::Line(l) = &mut items[1] {
                l.points[0].y = bad;
            }
            assert!(LineSuppressionCache::try_static_prewarm(
                &items,
                FlatProjection::EllipsoidalMercator,
                64 * 1024 * 1024,
                32 * 1024 * 1024
            )
            .is_none());
            let mut cache = LineSuppressionCache::default();
            cache.set_static_prewarm_enabled(true);
            compare(
                &mut cache,
                &items,
                1,
                FlatProjection::EllipsoidalMercator,
                0,
                None,
                &[true, false, true],
            );
            compare(
                &mut cache,
                &items,
                1,
                FlatProjection::EllipsoidalMercator,
                0,
                None,
                &[true, true, true],
            );
            assert_eq!(
                cache.prewarm_attempt,
                Some((
                    SuppressionRevision::Legacy(1),
                    FlatProjection::EllipsoidalMercator,
                    3
                ))
            );
        }
    }
    #[test]
    fn admission_bounds_decline_without_partial_state_and_zero_extra_retained_topology() {
        let items = fixture();
        assert!(LineSuppressionCache::try_static_prewarm(
            &items,
            FlatProjection::LocalGeographic,
            0,
            32 * 1024 * 1024
        )
        .is_none());
        assert!(LineSuppressionCache::try_static_prewarm(
            &items,
            FlatProjection::LocalGeographic,
            64 * 1024 * 1024,
            0
        )
        .is_none());
        let (_, compiled) = LineSuppressionCache::try_static_prewarm(
            &items,
            FlatProjection::LocalGeographic,
            64 * 1024 * 1024,
            32 * 1024 * 1024,
        )
        .unwrap();
        assert!(compiled.retained_bytes() <= 32 * 1024 * 1024);
    }
}

#[cfg(test)]
mod static_prewarm_numeric_tests {
    use super::*;
    #[test]
    fn projected_overlap_endpoints_preserve_fraction_bits_across_live_masks() {
        let items = vec![
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![
                    WorldPoint::new(0., 55.),
                    WorldPoint::new(0., 82.),
                ])
                .with_priority(2),
            ),
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![
                    WorldPoint::new(-0., 61.),
                    WorldPoint::new(0., 77.),
                ])
                .with_priority(4),
            ),
        ];
        let mut warm = LineSuppressionCache::default();
        warm.set_static_prewarm_enabled(true);
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for mask in [[true, false], [true, true], [false, true], [true, true]] {
                let actual = warm.plan_immutable_projected_with_visibility(
                    &items,
                    1,
                    0,
                    None,
                    None,
                    Some(&mask),
                    projection,
                );
                let golden = LineSuppressionCache::default().plan_projected_with_visibility(
                    &items,
                    0,
                    None,
                    None,
                    Some(&mask),
                    projection,
                );
                assert_eq!(actual.fully_suppressed, golden.fully_suppressed);
                for (index, spans) in &actual.partial {
                    let expected = golden.partial.get(index).unwrap();
                    assert_eq!(spans.len(), expected.len());
                    for (a, b) in spans.iter().zip(expected) {
                        assert_eq!(
                            (a.segment, a.start.to_bits(), a.end.to_bits()),
                            (b.segment, b.start.to_bits(), b.end.to_bits())
                        );
                    }
                }
                assert_eq!(actual.partial.len(), golden.partial.len());
            }
        }
    }
}

#[cfg(test)]
mod default_static_preparation_policy_tests {
    use super::*;
    #[test]
    fn default_and_explicit_controls_preserve_disable_and_unknown_values() {
        assert!(LineSuppressionCache::static_prewarm_policy(None));
        assert!(LineSuppressionCache::static_prewarm_policy(Some("1")));
        for value in ["0", "", "yes", "01", "true"] {
            assert!(!LineSuppressionCache::static_prewarm_policy(Some(value)));
        }
        let mut owner = LineSuppressionCache::default();
        owner.set_static_prewarm_enabled(false);
        assert_eq!(owner.prewarm_policy, Some(false));
        owner.clear();
        assert_eq!(owner.prewarm_policy, Some(false));
    }
}

#[cfg(test)]
mod context_prewarm_epoch_tests {
    use super::*;
    #[test]
    fn inherited_context_keeps_prewarm_attempt_and_cached_plan_but_legacy_does_not_alias() {
        let items = vec![
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![
                    WorldPoint::new(0., 0.),
                    WorldPoint::new(10., 0.),
                ])
                .with_priority(1),
            ),
            DrawingInstruction::Line(
                crate::LineInstruction::new(vec![WorldPoint::new(2., 0.), WorldPoint::new(8., 0.)])
                    .with_priority(9),
            ),
        ];
        let mut old = crate::RenderContext::new(crate::Viewport::new(800., 600.));
        old.set_instructions_from_cache(items.clone());
        old.get_sorted_instructions();
        let mut next = old.empty_for_rebuild();
        next.set_instructions_from_cache(items);
        next.get_sorted_instructions();
        assert!(next.inherit_static_line_relations_from(&old));
        assert_ne!(old.geometry_revision(), next.geometry_revision());
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(true);
        let first = cache.plan_context_projected_with_visibility(&old, 0, None, None, None);
        let attempt = cache.prewarm_attempt;
        assert!(matches!(
            attempt,
            Some((SuppressionRevision::Context(_), _, 2))
        ));
        let retry = cache.plan_context_projected_with_visibility(&next, 0, None, None, None);
        assert_eq!(cache.prewarm_attempt, attempt);
        assert!(Arc::ptr_eq(&first, &retry));
        let expected = LineSuppressionCache::default().plan_projected_with_visibility(
            next.raw_instructions(),
            0,
            None,
            None,
            None,
            next.scaler.projection(),
        );
        assert_eq!(*retry, *expected);
        cache.plan_immutable_projected_with_visibility(
            next.raw_instructions(),
            1,
            0,
            None,
            None,
            None,
            next.scaler.projection(),
        );
        assert!(matches!(
            cache.prewarm_attempt,
            Some((SuppressionRevision::Legacy(1), _, 2))
        ));
    }
}

#[cfg(test)]
mod tail_diagnostic_controls {
    use super::*;
    #[test]
    fn optional_counters_are_fixed_size_saturating_and_no_policy_change() {
        let mut c = LineSuppressionCache::default();
        c.tail_count(0);
        assert_eq!(c.tail_diagnostics_counters(), [0; 10]);
        c.set_tail_diagnostics_enabled(true);
        c.tail_counters.as_mut().unwrap()[0] = u64::MAX;
        c.tail_count(0);
        assert_eq!(c.tail_diagnostics_counters()[0], u64::MAX);
        c.set_tail_diagnostics_enabled(false);
        assert_eq!(c.tail_diagnostics_counters(), [0; 10]);
        assert!(c.immutable_prepared.is_none());
    }
}

#[cfg(test)]
mod growth_relation_controls {
    use super::*;
    fn line(points: &[(f64, f64)], priority: i32) -> DrawingInstruction {
        DrawingInstruction::Line(
            crate::LineInstruction::new(points.iter().map(|p| WorldPoint::new(p.0, p.1)).collect())
                .with_priority(priority),
        )
    }
    fn masked(items: &[DrawingInstruction], mask: &[bool]) -> Vec<DrawingInstruction> {
        items
            .iter()
            .zip(mask)
            .map(|(i, v)| {
                if *v {
                    i.clone()
                } else {
                    DrawingInstruction::Line(crate::LineInstruction::new(Vec::new()))
                }
            })
            .collect()
    }
    fn exact(a: &LineSuppressionPlan, b: &LineSuppressionPlan) {
        assert_eq!(a.fully_suppressed, b.fully_suppressed);
        assert_eq!(a.partial.len(), b.partial.len());
        for (i, spans) in &a.partial {
            let other = b.partial.get(i).unwrap();
            assert_eq!(spans.len(), other.len());
            for (x, y) in spans.iter().zip(other) {
                assert_eq!(
                    (x.segment, x.start.to_bits(), x.end.to_bits()),
                    (y.segment, y.start.to_bits(), y.end.to_bits())
                );
            }
        }
    }
    #[test]
    fn growing_old_targets_new_occluders_full_curves_and_all_visibility_masks_exact() {
        let items = vec![
            line(&[(0., 0.), (10., 0.)], 1),
            line(&[(2., 0.), (6., 0.)], 5),
            line(&[(10., 0.), (0., 0.)], 9),
            line(&[(5., -1.), (5., 1.)], 8),
        ];
        let mut covered = vec![true, false, false, false];
        let mut previous =
            PreparedLineSuppression::compile(&masked(&items, &covered), 32 * 1024 * 1024).unwrap();
        for add in [1, 3, 2] {
            covered[add] = true;
            let source = masked(&items, &covered);
            let incremental =
                PreparedLineSuppression::compile_growth(&source, 32 * 1024 * 1024, &previous, &{
                    let mut old = covered.clone();
                    old[add] = false;
                    old
                })
                .unwrap();
            let full = PreparedLineSuppression::compile(&source, 32 * 1024 * 1024).unwrap();
            for bits in 0..16 {
                let eligible: Vec<_> = (0..4)
                    .map(|i| covered[i] && (bits & (1 << i) != 0))
                    .collect();
                exact(
                    &incremental.plan(&eligible).unwrap(),
                    &full.plan(&eligible).unwrap(),
                );
                exact(
                    &incremental.plan(&eligible).unwrap(),
                    &LineSuppressionCache::default().plan_projected_with_visibility(
                        &source,
                        0,
                        None,
                        None,
                        Some(&eligible),
                        FlatProjection::LocalGeographic,
                    ),
                );
            }
            assert!(incremental.retained_bytes() <= 32 * 1024 * 1024);
            previous = incremental;
        }
    }
    #[test]
    fn finite_empty_prewarm_covered_but_invalid_and_budget_declines_unchanged() {
        let items = vec![
            line(&[], 3),
            line(&[(1., 2.)], 4),
            line(&[(0., 60.), (10., 60.)], 1),
            line(&[(2., 60.), (6., 60.)], 7),
        ];
        assert!(LineSuppressionCache::try_static_prewarm(
            &items,
            FlatProjection::EllipsoidalMercator,
            64 * 1024 * 1024,
            32 * 1024 * 1024
        )
        .is_none());
        let (covered, compiled) = LineSuppressionCache::try_static_prewarm_covered_empty(
            &items,
            FlatProjection::EllipsoidalMercator,
            64 * 1024 * 1024,
            32 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!(covered, vec![true; 4]);
        for bits in 0..16 {
            let mask: Vec<_> = (0..4).map(|i| bits & (1 << i) != 0).collect();
            exact(
                &compiled.plan(&mask).unwrap(),
                &LineSuppressionCache::default().plan_projected_with_visibility(
                    &items,
                    0,
                    None,
                    None,
                    Some(&mask),
                    FlatProjection::EllipsoidalMercator,
                ),
            );
        }
        assert!(LineSuppressionCache::try_static_prewarm_covered_empty(
            &items,
            FlatProjection::LocalGeographic,
            0,
            32 * 1024 * 1024
        )
        .is_none());
        assert!(LineSuppressionCache::try_static_prewarm_covered_empty(
            &items,
            FlatProjection::LocalGeographic,
            64 * 1024 * 1024,
            0
        )
        .is_none());
        let invalid = vec![line(&[(f64::NAN, 0.)], 4)];
        assert!(LineSuppressionCache::try_static_prewarm_covered_empty(
            &invalid,
            FlatProjection::LocalGeographic,
            64 * 1024 * 1024,
            32 * 1024 * 1024
        )
        .is_none());
    }
    #[test]
    fn reuse_growth_dimensions_decline_and_original_fullcompiler_remains_fallback() {
        let old = vec![line(&[(0., 0.), (10., 0.)], 1)];
        let compiled = PreparedLineSuppression::compile(&old, 4096).unwrap();
        assert!(PreparedLineSuppression::compile_growth(&old, 0, &compiled, &[true]).is_none());
        assert!(PreparedLineSuppression::compile_growth(&old, 4096, &compiled, &[]).is_none());
        let changed = vec![
            line(&[(0., 0.), (10., 0.)], 1),
            line(&[(0., 0.), (5., 0.)], 9),
        ];
        assert!(
            PreparedLineSuppression::compile_growth(&changed, 4096, &compiled, &[true, false])
                .is_none()
        );
        assert!(PreparedLineSuppression::compile(&changed, 4096).is_some());
    }
    #[test]
    fn context_revision_replacement_and_masks_do_not_reuse_old_geometry() {
        let a = vec![
            line(&[(0., 0.), (10., 0.)], 1),
            line(&[(0., 0.), (5., 0.)], 9),
        ];
        let b = vec![
            line(&[(0., 0.), (10., 0.)], 1),
            line(&[(5., 0.), (10., 0.)], 9),
        ];
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(false);
        cache.growth_reuse_policy = Some(true);
        cache.plan_immutable_projected_with_visibility(
            &a,
            1,
            0,
            None,
            None,
            Some(&[true, false]),
            FlatProjection::LocalGeographic,
        );
        exact(
            &cache.plan_immutable_projected_with_visibility(
                &a,
                1,
                0,
                None,
                None,
                Some(&[true, true]),
                FlatProjection::LocalGeographic,
            ),
            &LineSuppressionCache::default().plan(&a, 0, None, None),
        );
        exact(
            &cache.plan_immutable_projected_with_visibility(
                &b,
                2,
                0,
                None,
                None,
                Some(&[true, true]),
                FlatProjection::LocalGeographic,
            ),
            &LineSuppressionCache::default().plan(&b, 0, None, None),
        );
    }
    #[test]
    fn projection_source_priority_scale_group_and_time_masks_match_original() {
        let mut items = vec![
            line(&[], 1),
            line(&[(0., 60.), (10., 60.)], 2),
            line(&[(2., 60.), (6., 60.)], 7),
        ];
        if let DrawingInstruction::Line(l) = &mut items[2] {
            l.viewing_group = crate::ViewingGroup(33010);
            l.additional_viewing_groups = vec![crate::ViewingGroup(33011)].into_boxed_slice();
            l.scale_range.scale_minimum = Some(10_000);
            l.cell_index = Some(9);
            l.suppressible = false;
        }
        let mut cache = LineSuppressionCache {
            empty_curve_prewarm_policy: Some(true),
            growth_reuse_policy: Some(true),
            ..Default::default()
        };
        let sets = [
            None,
            Some(HashSet::from([33010])),
            Some(HashSet::from([33010, 33011])),
        ];
        for revision in [41, 42] {
            if revision == 42 {
                if let DrawingInstruction::Line(l) = &mut items[2] {
                    l.points.reverse();
                    l.priority = crate::DisplayPriority(1);
                    l.cell_index = Some(10);
                }
            }
            for projection in [
                FlatProjection::LocalGeographic,
                FlatProjection::EllipsoidalMercator,
            ] {
                for scale in [100, 20_000] {
                    for groups in &sets {
                        for override_group in [None, Some(33010)] {
                            for bits in 0..8 {
                                // Selector mask is the original temporal/coverage result;
                                // prewarm must not make a hidden contributor suppress.
                                let mask: Vec<_> = (0..3).map(|i| bits & (1 << i) != 0).collect();
                                exact(
                                    &cache.plan_immutable_projected_with_visibility(
                                        &items,
                                        revision,
                                        scale,
                                        groups.as_ref(),
                                        override_group,
                                        Some(&mask),
                                        projection,
                                    ),
                                    &LineSuppressionCache::default()
                                        .plan_projected_with_visibility(
                                            &items,
                                            scale,
                                            groups.as_ref(),
                                            override_group,
                                            Some(&mask),
                                            projection,
                                        ),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn old_empty_relation_new_lower_equal_plane_signed_zero_and_reappearing_sources_exact() {
        let mut items = vec![
            line(&[(-0., -0.), (10., 0.)], 2),
            line(&[(2., 0.), (6., 0.)], 8),
            line(&[(10., 0.), (0., -0.)], 9),
            line(&[(2., 0.), (6., 0.)], 1),
            line(&[(0., 0.), (10., 0.)], 2),
            line(&[(0., 0.), (10., 0.)], 2),
            line(&[(0., 0.), (10., 0.)], 99),
        ];
        if let DrawingInstruction::Line(l) = &mut items[5] {
            l.display_plane = crate::DisplayPlane::OverRadar;
        }
        if let DrawingInstruction::Line(l) = &mut items[6] {
            l.display_plane = crate::DisplayPlane::UnderRadar;
        }
        let mut covered = vec![true, false, false, false, false, false, false];
        let mut previous =
            PreparedLineSuppression::compile(&masked(&items, &covered), 32 * 1024 * 1024).unwrap();
        assert!(previous.targets.is_empty());
        for add in [1, 3, 4, 5, 6, 2] {
            let old = covered.clone();
            covered[add] = true;
            let source = masked(&items, &covered);
            let result =
                PreparedLineSuppression::compile_growth(&source, 32 * 1024 * 1024, &previous, &old)
                    .unwrap();
            for flags in 0..128 {
                let visible: Vec<_> = (0..7)
                    .map(|j| covered[j] && flags & (1 << j) != 0)
                    .collect();
                exact(
                    &result.plan(&visible).unwrap(),
                    &LineSuppressionCache::default().plan_projected_with_visibility(
                        &source,
                        0,
                        None,
                        None,
                        Some(&visible),
                        FlatProjection::LocalGeographic,
                    ),
                );
            }
            previous = result;
        }
        // One segment fits the old 128-byte scratch admission, whereas old+new
        // indexes with two segments must decline and use the original full compile.
        let old = vec![line(&[(0., 0.), (10., 0.)], 1)];
        let previous = PreparedLineSuppression::compile(&old, 4096).unwrap();
        let expanded = vec![old[0].clone(), line(&[(2., 0.), (6., 0.)], 8)];
        assert!(
            PreparedLineSuppression::compile_growth(&expanded, 256, &previous, &[true, false])
                .is_none()
        );
        assert!(PreparedLineSuppression::compile(&expanded, 4096).is_some());
    }
    #[test]
    fn optional_growth_children_preserve_plan_and_report_admission() {
        let items = vec![
            line(&[(0., 0.), (10., 0.)], 1),
            line(&[(2., 0.), (6., 0.)], 9),
        ];
        let previous =
            PreparedLineSuppression::compile(&masked(&items, &[true, false]), 4096).unwrap();
        let mut work = GrowthCompilerWork::default();
        let measured = PreparedLineSuppression::compile_growth_with_work(
            &items,
            4096,
            &previous,
            &[true, false],
            Some(&mut work),
        )
        .unwrap();
        let ordinary =
            PreparedLineSuppression::compile_growth(&items, 4096, &previous, &[true, false])
                .unwrap();
        for mask in [[false, false], [true, false], [false, true], [true, true]] {
            exact(
                &measured.plan(&mask).unwrap(),
                &ordinary.plan(&mask).unwrap(),
            );
        }
        assert_eq!(
            (
                work.attempts,
                work.admitted,
                work.all_segments,
                work.new_segments
            ),
            (1, 1, 2, 1)
        );
        assert!(PreparedLineSuppression::compile_growth_with_work(
            &items,
            0,
            &previous,
            &[true, false],
            Some(&mut work)
        )
        .is_none());
        assert_eq!((work.attempts, work.admitted), (2, 1));
    }
    #[test]
    fn unchanged_target_guard_all_masks_new_partial_full_far_cross_and_lower_exact() {
        let items = vec![
            line(&[(-0., -0.), (10., 0.)], 2),
            line(&[(2., 0.), (6., 0.)], 8),
            line(&[(100., 0.), (110., 0.)], 1),
            line(&[(200., 0.), (201., 0.)], 100),
            line(&[(4., -1.), (4., 1.)], 99),
            line(&[(6., 0.), (8., 0.)], 10),
            line(&[(10., 0.), (0., 0.)], 20),
            line(&[(2., 0.), (4., 0.)], 0),
        ];
        let mut covered = vec![true, true, true, false, false, false, false, false];
        let mut previous =
            PreparedLineSuppression::compile(&masked(&items, &covered), 32 * 1024 * 1024).unwrap();
        let mut work = GrowthCompilerWork::default();
        for add in [3, 4, 7, 5, 6] {
            let old = covered.clone();
            covered[add] = true;
            let source = masked(&items, &covered);
            let fast = PreparedLineSuppression::compile_growth_with_unchanged_targets(
                &source,
                32 * 1024 * 1024,
                &previous,
                &old,
                Some(&mut work),
                true,
            )
            .unwrap();
            let original_growth = PreparedLineSuppression::compile_growth_with_work(
                &source,
                32 * 1024 * 1024,
                &previous,
                &old,
                None,
            )
            .unwrap();
            for flags in 0..256 {
                let visible: Vec<_> = (0..8)
                    .map(|j| covered[j] && flags & (1 << j) != 0)
                    .collect();
                exact(
                    &fast.plan(&visible).unwrap(),
                    &original_growth.plan(&visible).unwrap(),
                );
                exact(
                    &fast.plan(&visible).unwrap(),
                    &LineSuppressionCache::default().plan_projected_with_visibility(
                        &source,
                        0,
                        None,
                        None,
                        Some(&visible),
                        FlatProjection::LocalGeographic,
                    ),
                );
            }
            assert!(fast.retained_bytes() <= 32 * 1024 * 1024);
            previous = fast;
        }
        assert!(work.unchanged_old_target_checks > 0);
        assert!(work.unchanged_old_target_accepts > 0);
        assert!(work.unchanged_segment_windows_avoided > 0);
    }
    #[test]
    fn unchanged_guard_must_not_drop_new_full_coincident_zero_edge_curve() {
        let items = vec![
            line(&[(0., 0.), (0., 0.)], 1),
            line(&[(0., 0.), (0., 0.)], 9),
        ];
        let old = PreparedLineSuppression::compile(&masked(&items, &[true, false]), 4096).unwrap();
        let mut work = GrowthCompilerWork::default();
        let new = PreparedLineSuppression::compile_growth_with_unchanged_targets(
            &items,
            4096,
            &old,
            &[true, false],
            Some(&mut work),
            true,
        )
        .unwrap();
        let full = PreparedLineSuppression::compile(&items, 4096).unwrap();
        for mask in [[false, false], [true, false], [false, true], [true, true]] {
            exact(&new.plan(&mask).unwrap(), &full.plan(&mask).unwrap());
        }
        assert!(new
            .plan(&[true, true])
            .unwrap()
            .fully_suppressed
            .contains(&0));
        assert_eq!(work.unchanged_old_target_accepts, 0);
    }
    #[test]
    fn unchanged_guard_private_cache_resets_source_projection_and_keeps_cap_fallback() {
        let a = vec![
            line(&[(0., 60.), (10., 60.)], 1),
            line(&[(2., 60.), (4., 60.)], 9),
        ];
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(false);
        cache.growth_reuse_policy = Some(true);
        cache.unchanged_target_policy = Some(true);
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for mask in [[true, false], [true, true], [true, false], [true, true]] {
                exact(
                    &cache.plan_immutable_projected_with_visibility(
                        &a,
                        400,
                        0,
                        None,
                        None,
                        Some(&mask),
                        projection,
                    ),
                    &LineSuppressionCache::default().plan_projected_with_visibility(
                        &a,
                        0,
                        None,
                        None,
                        Some(&mask),
                        projection,
                    ),
                );
            }
        }
        let b = vec![a[0].clone(), line(&[(6., 60.), (8., 60.)], 9)];
        exact(
            &cache.plan_immutable_projected_with_visibility(
                &b,
                401,
                0,
                None,
                None,
                Some(&[true, true]),
                FlatProjection::EllipsoidalMercator,
            ),
            &LineSuppressionCache::default().plan_projected_with_visibility(
                &b,
                0,
                None,
                None,
                Some(&[true, true]),
                FlatProjection::EllipsoidalMercator,
            ),
        );
        let old = PreparedLineSuppression::compile(&a, 4096).unwrap();
        assert!(
            PreparedLineSuppression::compile_growth_with_unchanged_targets(
                &a,
                0,
                &old,
                &[true, true],
                None,
                true
            )
            .is_none()
        );
    }
    #[test]
    fn index_blocks_growth_matches_original_all_masks_and_reuses_owned_blocks() {
        let items = vec![
            line(&[(-0., 0.), (10., 0.)], 2),
            line(&[(2., 0.), (6., 0.)], 8),
            line(&[(10., 0.), (0., -0.)], 9),
            line(&[(2., 0.), (6., 0.)], 1),
            line(&[(1., 1.), (1., 1.)], 20),
        ];
        let mut covered = vec![true, false, false, false, false];
        let mut previous =
            PreparedLineSuppression::compile(&masked(&items, &covered), 32 * 1024 * 1024).unwrap();
        let mut blocks = None;
        for add in [1, 3, 4, 2] {
            let old = covered.clone();
            covered[add] = true;
            let source = masked(&items, &covered);
            let (next, index) = PreparedLineSuppression::compile_growth_with_index_blocks(
                &source,
                32 * 1024 * 1024,
                &previous,
                &old,
                &covered,
                blocks.as_ref(),
                None,
                true,
            )
            .unwrap();
            if let Some(prior) = blocks.as_ref() {
                for (a, b) in prior.blocks.iter().zip(&index.blocks) {
                    assert!(Arc::ptr_eq(a, b));
                }
            }
            for flags in 0..32 {
                let visible: Vec<_> = (0..5)
                    .map(|j| covered[j] && flags & (1 << j) != 0)
                    .collect();
                exact(
                    &next.plan(&visible).unwrap(),
                    &LineSuppressionCache::default().plan_projected_with_visibility(
                        &source,
                        0,
                        None,
                        None,
                        Some(&visible),
                        FlatProjection::LocalGeographic,
                    ),
                );
            }
            assert!(index.bytes + next.bytes <= 32 * 1024 * 1024);
            previous = next;
            blocks = Some(index);
        }
    }
    #[test]
    fn index_blocks_cap_bitmap_mismatch_and_failed_stage_leave_prior_unchanged() {
        let items: Vec<_> = (0..18)
            .map(|i| line(&[(i as f64, 0.), (i as f64 + 1., 0.)], i))
            .collect();
        let mut covered = vec![false; 18];
        covered[0] = true;
        let mut previous =
            PreparedLineSuppression::compile(&masked(&items, &covered), 32 * 1024 * 1024).unwrap();
        let mut blocks = None;
        for add in 1..16 {
            let old = covered.clone();
            covered[add] = true;
            let (next, index) = PreparedLineSuppression::compile_growth_with_index_blocks(
                &masked(&items, &covered),
                32 * 1024 * 1024,
                &previous,
                &old,
                &covered,
                blocks.as_ref(),
                None,
                false,
            )
            .unwrap();
            previous = next;
            blocks = Some(index);
        }
        let prior = blocks.unwrap();
        assert_eq!(prior.blocks.len(), 16);
        let before = prior.bytes;
        let first = Arc::clone(&prior.blocks[0]);
        let old = covered.clone();
        covered[16] = true;
        assert!(PreparedLineSuppression::compile_growth_with_index_blocks(
            &masked(&items, &covered),
            32 * 1024 * 1024,
            &previous,
            &old,
            &covered,
            Some(&prior),
            None,
            false
        )
        .is_none());
        assert!(PreparedLineSuppression::compile_growth_with_index_blocks(
            &masked(&items, &covered),
            0,
            &previous,
            &old,
            &covered,
            Some(&prior),
            None,
            false
        )
        .is_none());
        let mut mismatch = old.clone();
        mismatch[0] = false;
        assert!(PreparedLineSuppression::compile_growth_with_index_blocks(
            &masked(&items, &covered),
            32 * 1024 * 1024,
            &previous,
            &mismatch,
            &covered,
            Some(&prior),
            None,
            false
        )
        .is_none());
        assert_eq!(prior.bytes, before);
        assert!(Arc::ptr_eq(&first, &prior.blocks[0]));
        assert!(PreparedLineSuppression::compile_growth(
            &masked(&items, &covered),
            32 * 1024 * 1024,
            &previous,
            &old
        )
        .is_some());
    }
    #[test]
    fn index_blocks_real_cache_source_projection_reset_matches_original() {
        let mut cache = LineSuppressionCache::default();
        cache.set_static_prewarm_enabled(false);
        cache.growth_reuse_policy = Some(true);
        cache.index_blocks_policy = Some(true);
        let mut items = vec![
            line(&[(0., 60.), (10., 60.)], 1),
            line(&[(2., 60.), (6., 60.)], 9),
            line(&[(6., 60.), (10., 60.)], 8),
        ];
        for revision in [100, 101] {
            if revision == 101 {
                if let DrawingInstruction::Line(l) = &mut items[1] {
                    l.points.reverse();
                    l.priority = crate::DisplayPriority(0);
                }
            }
            for projection in [
                FlatProjection::LocalGeographic,
                FlatProjection::EllipsoidalMercator,
            ] {
                for mask in [
                    [true, false, false],
                    [true, true, false],
                    [false, true, true],
                    [true, true, true],
                    [true, false, true],
                ] {
                    exact(
                        &cache.plan_immutable_projected_with_visibility(
                            &items,
                            revision,
                            0,
                            None,
                            None,
                            Some(&mask),
                            projection,
                        ),
                        &LineSuppressionCache::default().plan_projected_with_visibility(
                            &items,
                            0,
                            None,
                            None,
                            Some(&mask),
                            projection,
                        ),
                    );
                }
            }
        }
        cache.clear();
        assert_eq!(cache.immutable_index_bytes(), 0);
    }
}

#[cfg(test)]
mod eligibility_connected_controls {
    use super::*;
    use crate::{Color, LineInstruction, RenderContext, ScaleRange, Viewport};
    fn cache(enabled: bool) -> LineSuppressionCache {
        LineSuppressionCache {
            eligibility_policy: Some(enabled),
            eligibility_diagnostics_policy: Some(true),
            prewarm_policy: Some(false),
            ..Default::default()
        }
    }
    fn context() -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(100., 100.));
        for (priority, group, lo, hi) in [(1, 1, 0, 100), (9, 33010, 10, 20), (3, 2, 0, 100)] {
            let mut l =
                LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)])
                    .with_priority(priority)
                    .with_viewing_group(group);
            l.scale_range = ScaleRange {
                scale_maximum: Some(lo),
                scale_minimum: Some(hi),
            };
            l.color_token = Some("LINE".into());
            c.add_instruction(DrawingInstruction::Line(l));
        }
        c.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![])));
        c
    }
    #[test]
    fn connected_full_plan_masks_and_explicit_groups_follow_original() {
        let c = context();
        let mut off = cache(false);
        let mut on = cache(true);
        for bits in 0..16 {
            let mask: Vec<bool> = (0..4).map(|i| bits & (1 << i) != 0).collect();
            for scale in [0, 9, 10, 20, 21, 100, 101] {
                for groups in [None, Some(HashSet::new()), Some(HashSet::from([1, 2]))] {
                    for override_group in [None, Some(33010)] {
                        let a = off.plan_context_projected_with_visibility(
                            &c,
                            scale,
                            groups.as_ref(),
                            override_group,
                            Some(&mask),
                        );
                        let b = on.plan_context_projected_with_visibility(
                            &c,
                            scale,
                            groups.as_ref(),
                            override_group,
                            Some(&mask),
                        );
                        assert_eq!(a, b);
                    }
                }
            }
        }
        assert!(on.eligibility_work().unwrap().optimized_calls > 0);
        assert!(on.eligibility_work().unwrap().fallback_calls > 0);
    }
    #[test]
    fn connected_palette_new_source_and_clear_preserve_original() {
        let mut c = context();
        let mut off = cache(false);
        let mut on = cache(true);
        for alpha in [1., 0., f32::NAN, 1.] {
            c.remap_colors(&|_| Color::rgba(0., 0., 0., alpha));
            assert_eq!(
                off.plan_context_projected_with_visibility(&c, 15, None, None, None),
                on.plan_context_projected_with_visibility(&c, 15, None, None, None)
            );
        }
        on.clear();
        assert_eq!(
            off.plan_context_projected_with_visibility(&context(), 15, None, None, None),
            on.plan_context_projected_with_visibility(&context(), 15, None, None, None)
        );
    }
    #[test]
    fn diagnostics_unavailable_when_disabled_and_legacy_numeric_api_not_cached() {
        let mut c = cache(true);
        c.eligibility_diagnostics_policy = Some(false);
        let ctx = context();
        c.plan_context_projected_with_visibility(&ctx, 15, None, None, None);
        assert!(c.eligibility_work().is_none());
        let mut raw_only = cache(true);
        raw_only.plan_immutable_projected_with_visibility(
            ctx.raw_instructions(),
            1,
            15,
            None,
            None,
            None,
            ctx.scaler.projection(),
        );
        assert!(raw_only.eligibility_program.plan_for_test_is_absent());
        assert!(raw_only.eligibility_work().is_none());
    }
}

#[cfg(test)]
mod independent_suppression_cache_fork_tests {
    use super::*;
    #[test]
    fn cold_fork_retains_captured_policy_but_not_geometry_or_attempt_history() {
        let mut source = LineSuppressionCache {
            eligibility_policy: Some(true),
            eligibility_diagnostics_policy: Some(false),
            prewarm_policy: Some(false),
            growth_reuse_policy: Some(true),
            unchanged_target_policy: Some(false),
            index_blocks_policy: Some(true),
            empty_curve_prewarm_policy: Some(true),
            ..Default::default()
        };
        source.set_tail_diagnostics_enabled(true);
        source.tail_counters.as_mut().unwrap()[0] = 17;
        let cold = source.fork_empty_with_same_policy();
        assert_eq!(cold.eligibility_policy, source.eligibility_policy);
        assert_eq!(
            cold.eligibility_diagnostics_policy,
            source.eligibility_diagnostics_policy
        );
        assert_eq!(cold.prewarm_policy, source.prewarm_policy);
        assert_eq!(cold.growth_reuse_policy, source.growth_reuse_policy);
        assert_eq!(cold.unchanged_target_policy, source.unchanged_target_policy);
        assert_eq!(cold.index_blocks_policy, source.index_blocks_policy);
        assert_eq!(
            cold.empty_curve_prewarm_policy,
            source.empty_curve_prewarm_policy
        );
        assert_eq!(cold.tail_diagnostics_counters(), [0; 10]);
        assert_eq!(source.tail_diagnostics_counters()[0], 17);
        assert!(
            cold.last.is_none()
                && cold.immutable_prepared.is_none()
                && cold.immutable_last.is_none()
                && cold.immutable_current.is_none()
                && cold.immutable_index_blocks.is_none()
                && cold.prewarm_attempt.is_none()
        );
        assert!(LineSuppressionCache::default()
            .fork_empty_with_same_policy()
            .prewarm_policy
            .is_none());
    }
}
