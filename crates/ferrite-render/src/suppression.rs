//! Product-neutral visibility-aware suppression of coincident line segments.
use crate::{DrawingInstruction, FlatProjection, Scaler, ScreenPoint, WorldPoint};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

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
/// Content validation costs O(V) even on a hit. Results share an Arc and contain
/// no borrowed vertices. Spatial indexes are temporary and discarded after planning.
#[derive(Default)]
pub struct LineSuppressionCache {
    last: Option<([u8; 32], Arc<LineSuppressionPlan>)>,
    immutable_prepared: Option<(
        u64,
        FlatProjection,
        Vec<bool>,
        Option<PreparedLineSuppression>,
    )>,
    immutable_last: Option<(Vec<bool>, Arc<LineSuppressionPlan>)>,
    immutable_current: Option<Arc<LineSuppressionPlan>>,
    // Opt-in only. A declined prewarm is tried once per immutable source epoch.
    prewarm_policy: Option<bool>,
    prewarm_attempt: Option<(u64, FlatProjection, usize)>,
}
impl LineSuppressionCache {
    fn static_prewarm_policy(value: Option<&str>) -> bool {
        value.is_none_or(|value| value == "1")
    }
    /// Override bounded static preparation and invalidate all cached relations.
    pub fn set_static_prewarm_enabled(&mut self, enabled: bool) {
        self.clear();
        self.prewarm_policy = Some(enabled);
    }
    pub fn clear(&mut self) {
        self.prewarm_attempt = None;
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
    /// Static overlap reuse for immutable RenderContext instruction geometry.
    /// The caller MUST advance revision when points, ordering, priority, planes,
    /// suppression or deferred geometry changes. Live stroke visibility, date,
    /// scale, groups and execution permissions are evaluated on every call.
    /// Ordinary plan methods continue to hash mutable slice contents.
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
        debug_assert!(visibility.is_none_or(|v| v.len() == instructions.len()));
        let prewarm = *self.prewarm_policy.get_or_insert_with(|| {
            Self::static_prewarm_policy(std::env::var("FERRITE_LINE_SUPPRESSION_PREWARM").ok().as_deref())
        });
        let source = (revision, projection, instructions.len());
        if prewarm && self.prewarm_attempt != Some(source) {
            self.prewarm_attempt = Some(source);
            if let Some((prepared, compiled)) = Self::try_static_prewarm(
                instructions, projection, 64 * 1024 * 1024, 32 * 1024 * 1024,
            ) {
                self.immutable_last = None;
                self.immutable_prepared = Some((revision, projection, prepared, Some(compiled)));
            }
            // Decline changes no original prepared state: lazy admission and its
            // original mutable fallback below remain authoritative.
        }
        let eligible: Vec<_> = instructions.iter().enumerate().map(|(i,item)| {
            visibility.is_none_or(|v|v.get(i).copied().unwrap_or(false))
                && instruction_visible(item,scale,groups,override_group)
                && matches!(item,DrawingInstruction::Line(l) if l.screen_ray.is_none() && l.portrayal_path.is_none())
        }).collect();
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
            let compiled = if base
                .saturating_add(points.saturating_mul(std::mem::size_of::<WorldPoint>()))
                <= 64 * 1024 * 1024
            {
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
                PreparedLineSuppression::compile(&transformed, 32 * 1024 * 1024)
            } else {
                None
            };
            self.immutable_prepared = Some((revision, projection, prepared, compiled));
        }
        if self.immutable_prepared.as_ref().unwrap().3.is_none() {
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
                self.immutable_current = Some(Arc::clone(plan));
                return Arc::clone(plan);
            }
        }
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
        instructions: &[DrawingInstruction], projection: FlatProjection,
        transform_budget: usize, compiler_budget: usize,
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
        let base = instructions.len().checked_mul(std::mem::size_of::<DrawingInstruction>())?;
        if base.checked_add(points.checked_mul(std::mem::size_of::<WorldPoint>())?)? > transform_budget
            || segments.checked_mul(128)? > compiler_budget
            || lines.checked_mul(128)? > compiler_budget { return None; }
        let mut prepared = Vec::with_capacity(instructions.len());
        let mut transformed = Vec::with_capacity(instructions.len());
        for item in instructions {
            let mut out = crate::LineInstruction::new(Vec::new());
            let mut selected = false;
            if let DrawingInstruction::Line(line) = item {
                if line.screen_ray.is_none() && line.portrayal_path.is_none() {
                    selected = true;
                    // Identical original project_y expression and source order.
                    out.points = line.points.iter()
                        .map(|p| WorldPoint::new(p.x, projection.project_y(p.y))).collect();
                    Curve::new(&out.points)?;
                    // Extreme arithmetic is left to the original lazy planner.
                    if out.points.windows(2).any(|p| {
                        !(p[1].x-p[0].x).is_finite() || !(p[1].y-p[0].y).is_finite()
                    }) { return None; }
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
        let mut style = crate::LineStyle::default();
        style.dash_pattern = vec![30., 20.];
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
        let mut groups: HashMap<Curve<'_>, Vec<(usize, (i32, i32))>> = HashMap::new();
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
    fn line(a: (f64,f64), b: (f64,f64), priority:i32)->DrawingInstruction {
        DrawingInstruction::Line(crate::LineInstruction::new(vec![WorldPoint::new(a.0,a.1),WorldPoint::new(b.0,b.1)]).with_priority(priority))
    }
    fn fixture()->Vec<DrawingInstruction> {
        vec![line((0.,60.),(0.,80.),2),line((0.,70.),(0.,80.),9),line((0.,80.),(0.,60.),5)]
    }
    fn compare(cache:&mut LineSuppressionCache, items:&[DrawingInstruction], epoch:u64,
        projection:FlatProjection, scale:u32, groups:Option<&HashSet<u32>>, visible:&[bool]) {
        let mut original=LineSuppressionCache::default();
        original.set_static_prewarm_enabled(false);
        let golden=original.plan_immutable_projected_with_visibility(items,epoch,scale,groups,None,Some(visible),projection);
        let actual=cache.plan_immutable_projected_with_visibility(items,epoch,scale,groups,None,Some(visible),projection);
        assert_eq!(*actual,*golden);
        // Independent ordinary planner, not a copy of speculative logic.
        let direct=LineSuppressionCache::default().plan_projected_with_visibility(items,scale,groups,None,Some(visible),projection);
        assert_eq!(*actual,*direct);
    }
    #[test]
    fn growth_and_visibility_never_admit_an_ineligible_high_priority_source() {
        let items=fixture();let mut cache=LineSuppressionCache::default();cache.set_static_prewarm_enabled(true);
        for projection in [FlatProjection::LocalGeographic,FlatProjection::EllipsoidalMercator] {
            for mask in [1,3,7,0,5,2,7,1] {
                let visible:Vec<_>=(0..3).map(|i|mask&(1<<i)!=0).collect();
                compare(&mut cache,&items,1,projection,0,None,&visible);
                assert!(cache.immutable_prepared.as_ref().unwrap().2.iter().all(|p|*p));
                assert!(cache.immutable_preparation_bytes().unwrap()<=32*1024*1024);
            }
        }
    }
    #[test]
    fn source_epoch_reorder_remove_and_projection_clear_original_relations() {
        let mut items=fixture();let mut cache=LineSuppressionCache::default();cache.set_static_prewarm_enabled(true);
        for epoch in 1..5 {
            let visible=vec![true;items.len()];
            compare(&mut cache,&items,epoch,FlatProjection::EllipsoidalMercator,0,None,&visible);
            if epoch==1 {items.reverse();} else if epoch==2 {items.pop();} else {
                if let DrawingInstruction::Line(l)=&mut items[0] {l.points[0].x+=1.;}
            }
        }
        cache.clear();assert!(cache.prewarm_attempt.is_none());assert!(cache.current().is_none());
    }
    #[test]
    fn group_scale_alpha_and_temporal_coverage_permissions_stay_live() {
        let mut items=fixture();if let DrawingInstruction::Line(l)=&mut items[1] {
            l.scale_range=crate::ScaleRange{scale_minimum:Some(1000),scale_maximum:Some(100)};l.viewing_group=crate::ViewingGroup(999);
        }
        let mut cache=LineSuppressionCache::default();cache.set_static_prewarm_enabled(true);
        for groups in [HashSet::new(),items.iter().flat_map(|i|i.viewing_groups().map(|g|g.0)).chain([999]).collect::<HashSet<u32>>() ] {for scale in [0,99,100,1000,1001] {
            for mask in [vec![true,true,true],vec![true,false,true]] {compare(&mut cache,&items,1,FlatProjection::LocalGeographic,scale,Some(&groups),&mask);}
        }}
        if let DrawingInstruction::Line(l)=&mut items[1] {l.style.color.a=0.;}
        compare(&mut cache,&items,2,FlatProjection::LocalGeographic,100,None,&[true,true,true]);
    }
    #[test]
    fn invalid_prospective_curve_declines_and_preserves_lazy_result() {
        for bad in [f64::NAN,f64::INFINITY,100.] {
            let mut items=fixture();if let DrawingInstruction::Line(l)=&mut items[1] {l.points[0].y=bad;}
            assert!(LineSuppressionCache::try_static_prewarm(&items,FlatProjection::EllipsoidalMercator,64*1024*1024,32*1024*1024).is_none());
            let mut cache=LineSuppressionCache::default();cache.set_static_prewarm_enabled(true);
            compare(&mut cache,&items,1,FlatProjection::EllipsoidalMercator,0,None,&[true,false,true]);
            compare(&mut cache,&items,1,FlatProjection::EllipsoidalMercator,0,None,&[true,true,true]);
            assert_eq!(cache.prewarm_attempt,Some((1,FlatProjection::EllipsoidalMercator,3)));
        }
    }
    #[test]
    fn admission_bounds_decline_without_partial_state_and_zero_extra_retained_topology() {
        let items=fixture();
        assert!(LineSuppressionCache::try_static_prewarm(&items,FlatProjection::LocalGeographic,0,32*1024*1024).is_none());
        assert!(LineSuppressionCache::try_static_prewarm(&items,FlatProjection::LocalGeographic,64*1024*1024,0).is_none());
        let (_,compiled)=LineSuppressionCache::try_static_prewarm(&items,FlatProjection::LocalGeographic,64*1024*1024,32*1024*1024).unwrap();
        assert!(compiled.retained_bytes()<=32*1024*1024);
    }
}

#[cfg(test)]
mod static_prewarm_numeric_tests {
    use super::*;
    #[test]
    fn projected_overlap_endpoints_preserve_fraction_bits_across_live_masks() {
        let items=vec![
            DrawingInstruction::Line(crate::LineInstruction::new(vec![WorldPoint::new(0.,55.),WorldPoint::new(0.,82.)]).with_priority(2)),
            DrawingInstruction::Line(crate::LineInstruction::new(vec![WorldPoint::new(-0.,61.),WorldPoint::new(0.,77.)]).with_priority(4)),
        ];
        let mut warm=LineSuppressionCache::default();warm.set_static_prewarm_enabled(true);
        for projection in [FlatProjection::LocalGeographic,FlatProjection::EllipsoidalMercator] {for mask in [[true,false],[true,true],[false,true],[true,true]] {
            let actual=warm.plan_immutable_projected_with_visibility(&items,1,0,None,None,Some(&mask),projection);
            let golden=LineSuppressionCache::default().plan_projected_with_visibility(&items,0,None,None,Some(&mask),projection);
            assert_eq!(actual.fully_suppressed,golden.fully_suppressed);
            for (index,spans) in &actual.partial {
                let expected=golden.partial.get(index).unwrap();assert_eq!(spans.len(),expected.len());
                for (a,b) in spans.iter().zip(expected) {assert_eq!((a.segment,a.start.to_bits(),a.end.to_bits()),(b.segment,b.start.to_bits(),b.end.to_bits()));}
            }
            assert_eq!(actual.partial.len(),golden.partial.len());
        }}
    }
}

#[cfg(test)] mod default_static_preparation_policy_tests {
 use super::*;
 #[test] fn default_and_explicit_controls_preserve_disable_and_unknown_values() {
  assert!(LineSuppressionCache::static_prewarm_policy(None));
  assert!(LineSuppressionCache::static_prewarm_policy(Some("1")));
  for value in ["0", "", "yes", "01", "true"] {
   assert!(!LineSuppressionCache::static_prewarm_policy(Some(value)));
  }
  let mut owner=LineSuppressionCache::default();owner.set_static_prewarm_enabled(false);
  assert_eq!(owner.prewarm_policy,Some(false));owner.clear();assert_eq!(owner.prewarm_policy,Some(false));
 }
}
