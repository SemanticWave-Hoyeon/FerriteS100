//! Exact source-only Mercator northing arena. No cached visibility or screen geometry.
use crate::shared_cell::Shared;
use ferrite_kernel::map_camera::PreparedFlatNorthing;
use ferrite_render::{
    DrawingInstruction, FlatProjection, LineInstruction, PortrayalOrigin, RenderContext, Scaler,
    ScreenPoint, StaticInstructionOrderIdentity, WorldPoint,
};
use std::{
    cell::Cell,
    sync::{Arc, Weak},
};
const CAP: usize = 32 * 1024 * 1024;
// Fixed control allocation allowance, not allocator metadata or RSS accounting.
const HEADERS: usize = 1024;
#[derive(Clone, Copy, Default)]
struct Range {
    start: u32,
    len: u32,
}
struct Arena {
    source: Weak<StaticInstructionOrderIdentity>,
    bounds: Option<Vec<[f64; 4]>>,
    ranges: Vec<Range>,
    bits: Vec<[u64; 2]>,
    tokens: Vec<Option<PreparedFlatNorthing>>,
    charge: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decline {
    Cardinality,
    RequestedCap,
}
impl Decline {
    fn label(self) -> &'static str {
        match self {
            Self::Cardinality => "cardinality_or_charge_overflow",
            Self::RequestedCap => "requested_cap",
        }
    }
}
// Constant-size source-bound count/negative certificate. No source data retained.
struct Admission {
    source: Weak<StaticInstructionOrderIdentity>,
    instruction_count: usize,
    cap: usize,
    outcome: Result<usize, Decline>,
}
#[cfg(test)]
fn preflight(instructions: &[DrawingInstruction], cap: usize) -> Result<usize, Decline> {
    preflight_with_bounds(instructions, cap, false)
}
fn preflight_with_bounds(
    instructions: &[DrawingInstruction],
    cap: usize,
    bounds: bool,
) -> Result<usize, Decline> {
    u32::try_from(instructions.len()).map_err(|_| Decline::Cardinality)?;
    let mut count = 0usize;
    for instruction in instructions {
        if let DrawingInstruction::Line(line) = instruction {
            if eligible(line) {
                count = count
                    .checked_add(line.points.len())
                    .ok_or(Decline::Cardinality)?;
            }
        }
    }
    u32::try_from(count).map_err(|_| Decline::Cardinality)?;
    let requested = charge(instructions.len(), count)
        .and_then(|base| {
            base.checked_add(if bounds {
                instructions
                    .len()
                    .checked_mul(std::mem::size_of::<[f64; 4]>())?
            } else {
                0
            })
        })
        .ok_or(Decline::Cardinality)?;
    if requested > cap {
        return Err(Decline::RequestedCap);
    }
    Ok(count)
}
#[derive(Default)]
struct Work {
    source_scans: u64,
    admission_hits: u64,
    negative_hits: u64,
    cold: u64,
    hits: u64,
    declines: u64,
    used_lines: u64,
    used_points: u64,
    bounds_hits: u64,
    saved_bounds_points: u64,
}
pub(crate) struct Cache {
    enabled: bool,
    bounds_enabled: bool,
    arena: Option<Arc<Arena>>,
    admission: Option<Admission>,
    work: Option<Shared<Work>>,
}
pub(crate) struct Frame {
    arena: Arc<Arena>,
    work: Shared<Work>,
    lines: Cell<u64>,
    points: Cell<u64>,
    bounds_hits: Cell<u64>,
    bounds_points: Cell<u64>,
}
/// An invocation-local proof that bounds belong to this currently borrowed context.
/// Both owners remain immutably borrowed; context retirement cannot happen while it lives.
pub(crate) struct SourceBounds<'a> {
    frame: &'a Frame,
    context: &'a RenderContext,
}
pub(crate) type SourceProjection<'a> = (&'a Frame, usize, Option<&'a SourceBounds<'a>>);
#[derive(Clone, Copy)]
pub(crate) struct Entry<'a> {
    arena: &'a Arena,
    ordinal: usize,
    range: Range,
}
pub(crate) fn eligible(line: &LineInstruction) -> bool {
    line.screen_ray.is_none()
        && line.portrayal_path.is_none()
        && matches!(line.portrayal_origin, PortrayalOrigin::NonPoint)
        && line.points.len() >= 2
        && line.style.offset_mm == 0.
        && line.style.dash_cycle.is_none()
        && line.style.dash_pattern.is_empty()
}
fn charge(n: usize, points: usize) -> Option<usize> {
    HEADERS
        .checked_add(n.checked_mul(std::mem::size_of::<Range>())?)?
        .checked_add(points.checked_mul(std::mem::size_of::<[u64; 2]>())?)?
        .checked_add(points.checked_mul(std::mem::size_of::<Option<PreparedFlatNorthing>>())?)
}
fn reserve<T>(n: usize) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    Some(v)
}
impl Cache {
    /// Copy only the already sampled policy, not any published arena or GPU resource.
    pub(crate) fn fork_cold(&self) -> Self {
        Self::new_with_bounds(
            Some(std::ffi::OsStr::new(if self.enabled { "1" } else { "0" })),
            Some(std::ffi::OsStr::new(if self.bounds_enabled {
                "1"
            } else {
                "0"
            })),
        )
    }

    pub(crate) fn new(flag: Option<&std::ffi::OsStr>) -> Self {
        let enabled = flag.and_then(|s| s.to_str()) == Some("1");
        Self {
            enabled,
            bounds_enabled: false,
            arena: None,
            admission: None,
            work: enabled.then(|| Shared::new(Work::default())),
        }
    }
    pub(crate) fn new_with_bounds(
        flag: Option<&std::ffi::OsStr>,
        bounds: Option<&std::ffi::OsStr>,
    ) -> Self {
        let mut cache = Self::new(flag);
        cache.bounds_enabled = cache.enabled && bounds == Some(std::ffi::OsStr::new("1"));
        cache
    }
    /// Mutually exclusive caches: arena mode owns the complete 32MiB allowance.
    /// Declines fall back to original projection, never a second retained cache.
    pub(crate) fn moving_flag<'a>(
        &self,
        flag: Option<&'a std::ffi::OsStr>,
    ) -> Option<&'a std::ffi::OsStr> {
        if self.enabled {
            None
        } else {
            flag
        }
    }
    pub(crate) fn prepare(&mut self, context: &RenderContext, scaler: &Scaler) -> Option<Frame> {
        self.prepare_with_cap(context, scaler, CAP)
    }
    fn prepare_with_cap(
        &mut self,
        context: &RenderContext,
        scaler: &Scaler,
        cap: usize,
    ) -> Option<Frame> {
        if !self.enabled || !prepared_projection_matches(scaler) {
            return None;
        }
        let identity = context.static_instruction_order_identity();
        let instructions = context.raw_instructions();
        let work = self.work.as_ref()?.clone();
        if let Some(arena) = &self.arena {
            if arena.charge <= cap
                && arena.ranges.len() == instructions.len()
                && arena
                    .source
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, &identity))
            {
                work.borrow_mut().hits += 1;
                return Some(Frame::new(arena.clone(), work));
            }
            // Never retain an old/new 32MiB arena chain while a caller holds a view.
            if Arc::strong_count(arena) != 1 {
                work.borrow_mut().declines += 1;
                return None;
            }
        }
        self.arena = None;
        let admission_matches = self.admission.as_ref().is_some_and(|a| {
            a.instruction_count == instructions.len()
                && a.cap == cap
                && a.source
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, &identity))
        });
        if admission_matches {
            work.borrow_mut().admission_hits += 1;
        } else {
            work.borrow_mut().source_scans += 1;
            self.admission = Some(Admission {
                source: Arc::downgrade(&identity),
                instruction_count: instructions.len(),
                cap,
                outcome: preflight_with_bounds(instructions, cap, self.bounds_enabled),
            });
        }
        let count = match self.admission.as_ref()?.outcome {
            Ok(count) => count,
            Err(_) => {
                let mut w = work.borrow_mut();
                w.declines += 1;
                if admission_matches {
                    w.negative_hits += 1;
                }
                return None;
            }
        };
        work.borrow_mut().cold += 1;
        // Reservation/actual-capacity failure is transient: retry allocations using
        // the sealed count, without rescanning N source instructions each frame.
        let built = Self::build_with_count(
            instructions,
            Arc::downgrade(&identity),
            scaler,
            cap,
            count,
            self.bounds_enabled,
        );
        let Some(arena) = built else {
            work.borrow_mut().declines += 1;
            return None;
        };
        let arena = Arc::new(arena);
        self.arena = Some(arena.clone());
        Some(Frame::new(arena, work))
    }
    #[cfg(test)]
    fn build(
        instructions: &[DrawingInstruction],
        source: Weak<StaticInstructionOrderIdentity>,
        scaler: &Scaler,
        cap: usize,
    ) -> Option<Arena> {
        Self::build_with_count(
            instructions,
            source,
            scaler,
            cap,
            preflight(instructions, cap).ok()?,
            false,
        )
    }
    fn build_with_count(
        instructions: &[DrawingInstruction],
        source: Weak<StaticInstructionOrderIdentity>,
        scaler: &Scaler,
        cap: usize,
        count: usize,
        bounds_enabled: bool,
    ) -> Option<Arena> {
        let mut ranges = reserve::<Range>(instructions.len())?;
        let mut bits = reserve::<[u64; 2]>(count)?;
        let mut tokens = reserve::<Option<PreparedFlatNorthing>>(count)?;
        let mut bounds = if bounds_enabled {
            Some(reserve::<[f64; 4]>(instructions.len())?)
        } else {
            None
        };
        let actual = HEADERS
            .checked_add(
                ranges
                    .capacity()
                    .checked_mul(std::mem::size_of::<Range>())?,
            )?
            .checked_add(
                bits.capacity()
                    .checked_mul(std::mem::size_of::<[u64; 2]>())?,
            )?
            .checked_add(
                tokens
                    .capacity()
                    .checked_mul(std::mem::size_of::<Option<PreparedFlatNorthing>>())?,
            )?;
        let actual = actual.checked_add(match &bounds {
            Some(b) => b.capacity().checked_mul(std::mem::size_of::<[f64; 4]>())?,
            None => 0,
        })?;
        if actual > cap {
            return None;
        }
        // All capacity checks precede projection/token construction; no vector grows.
        for instruction in instructions {
            let mut range = Range::default();
            let mut source_bounds = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            if let DrawingInstruction::Line(line) = instruction {
                if eligible(line) {
                    range = Range {
                        start: bits.len() as u32,
                        len: line.points.len() as u32,
                    };
                    for point in &line.points {
                        bits.push([point.x.to_bits(), point.y.to_bits()]);
                        // Invalid points remain legacy failures; no acceptance/truncation here.
                        tokens.push(scaler.prepare_flat_northing(point.y).ok());
                        if bounds_enabled {
                            update_original_bounds(&mut source_bounds, *point);
                        }
                    }
                }
            }
            ranges.push(range);
            if let Some(bounds) = &mut bounds {
                bounds.push(source_bounds);
            }
        }
        Some(Arena {
            source,
            bounds,
            ranges,
            bits,
            tokens,
            charge: actual,
        })
    }
    pub(crate) fn statistics(&self) -> serde_json::Value {
        let w = self.work.as_ref().map(|w| w.borrow());
        serde_json::json!({"enabled":self.enabled,"cap":CAP,
            "source_bounds_enabled":self.bounds_enabled,
            "source_bounds_capacity_bytes":self.arena.as_ref().and_then(|a|a.bounds.as_ref()).map_or(0,|b|b.capacity()*std::mem::size_of::<[f64;4]>()),
            "source_bounds_hits":w.as_ref().map_or(0,|w|w.bounds_hits),
            "saved_source_bounds_points":w.as_ref().map_or(0,|w|w.saved_bounds_points),
            "charged_retained_bytes":self.arena.as_ref().map_or(0, |a| a.charge),
            "source_points":self.arena.as_ref().map_or(0, |a| a.bits.len()),
            "cold":w.as_ref().map_or(0, |w| w.cold),"hits":w.as_ref().map_or(0, |w| w.hits),
            "declines":w.as_ref().map_or(0, |w| w.declines),
            "source_scans":w.as_ref().map_or(0, |w| w.source_scans),
            "admission_hits":w.as_ref().map_or(0, |w| w.admission_hits),
            "negative_hits":w.as_ref().map_or(0, |w| w.negative_hits),
            "negative_reason":self.admission.as_ref().and_then(|a| a.outcome.err().map(Decline::label)),
            "accepted_range_requests":w.as_ref().map_or(0, |w| w.used_lines),
            "requested_source_points":w.as_ref().map_or(0, |w| w.used_points),
            "scope":"owned source tokens/capacities; not screen vertices, global cache cap, allocator or RSS"})
    }
}
impl Frame {
    pub(crate) fn projection_input(&self) -> ProjectionInput<'_> {
        ProjectionInput { arena: &self.arena }
    }
    fn new(arena: Arc<Arena>, work: Shared<Work>) -> Self {
        Self {
            arena,
            work,
            lines: Cell::new(0),
            points: Cell::new(0),
            bounds_hits: Cell::new(0),
            bounds_points: Cell::new(0),
        }
    }
    /// Compare owning identities once before walking the emitter, never per line.
    /// A live old token/identical length/pointer alone does not authorize reuse.
    pub(crate) fn bind_source_bounds<'a>(
        &'a self,
        context: &'a RenderContext,
    ) -> Option<SourceBounds<'a>> {
        self.arena.bounds.as_ref()?;
        let identity = context.static_instruction_order_identity();
        if !self
            .arena
            .source
            .upgrade()
            .is_some_and(|held| Arc::ptr_eq(&held, &identity))
        {
            return None;
        }
        Some(SourceBounds {
            frame: self,
            context,
        })
    }
    #[cfg(test)]
    fn source_bounds(
        &self,
        context: &RenderContext,
        ordinal: usize,
        points: &[WorldPoint],
    ) -> Option<(f64, f64, f64, f64)> {
        self.bind_source_bounds(context)?
            .source_bounds(ordinal, points)
    }
    pub(crate) fn entry(&self, ordinal: usize, points: &[WorldPoint]) -> Option<Entry<'_>> {
        let range = *self.arena.ranges.get(ordinal)?;
        if range.len == 0 || range.len as usize != points.len() {
            return None;
        }
        self.lines.set(self.lines.get().saturating_add(1));
        self.points
            .set(self.points.get().saturating_add(points.len() as u64));
        Some(Entry {
            arena: &self.arena,
            ordinal,
            range,
        })
    }
}
impl SourceBounds<'_> {
    pub(crate) fn source_bounds(
        &self,
        ordinal: usize,
        points: &[WorldPoint],
    ) -> Option<(f64, f64, f64, f64)> {
        let bounds = self.peek_source_bounds(ordinal, points)?;
        self.frame
            .bounds_hits
            .set(self.frame.bounds_hits.get().saturating_add(1));
        self.frame.bounds_points.set(
            self.frame
                .bounds_points
                .get()
                .saturating_add(points.len() as u64),
        );
        Some(bounds)
    }
    /// Speculative preparation uses the same ownership checks without charging
    /// the original emitter's bounds lookup ledger.
    pub(crate) fn peek_source_bounds(
        &self,
        ordinal: usize,
        points: &[WorldPoint],
    ) -> Option<(f64, f64, f64, f64)> {
        let DrawingInstruction::Line(line) = self.context.raw_instructions().get(ordinal)? else {
            return None;
        };
        if !eligible(line) || !std::ptr::eq(points, line.points.as_slice()) {
            return None;
        }
        let [a, b, c, d] = *self.frame.arena.bounds.as_ref()?.get(ordinal)?;
        Some((a, b, c, d))
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        let mut w = self.work.borrow_mut();
        w.used_lines = w.used_lines.saturating_add(self.lines.get());
        w.used_points = w.used_points.saturating_add(self.points.get());
        w.bounds_hits = w.bounds_hits.saturating_add(self.bounds_hits.get());
        w.saved_bounds_points = w
            .saved_bounds_points
            .saturating_add(self.bounds_points.get());
    }
}

// Only immutable Arena storage crosses worker boundaries, never Frame Rc/Cell.
#[derive(Clone, Copy)]
pub(crate) struct ProjectionInput<'a> {
    arena: &'a Arena,
}
impl ProjectionInput<'_> {
    pub(crate) fn point_count(self) -> usize {
        self.arena.bits.len()
    }
    pub(crate) fn selected_points(self, mask: &[bool]) -> Option<usize> {
        self.arena
            .ranges
            .iter()
            .enumerate()
            .filter(|(i, _)| mask.get(*i) == Some(&true))
            .try_fold(0usize, |n, (_, range)| n.checked_add(range.len as usize))
    }
    pub(crate) fn project_chunk(
        self,
        start: usize,
        output: &mut [ScreenPoint],
        mask: &[bool],
        scaler: &Scaler,
    ) {
        let end = start + output.len();
        for (ordinal, range) in self.arena.ranges.iter().enumerate() {
            if mask.get(ordinal) != Some(&true) || range.len == 0 {
                continue;
            }
            let a = (range.start as usize).max(start);
            let b = (range.start as usize + range.len as usize).min(end);
            for slot in a..b {
                let bits = self.arena.bits[slot];
                let p = WorldPoint {
                    x: f64::from_bits(bits[0]),
                    y: f64::from_bits(bits[1]),
                };
                output[slot - start] = if let Some(token) = &self.arena.tokens[slot] {
                    scaler.world_to_screen_with_prepared_northing(p, token)
                } else {
                    scaler.world_to_screen(p)
                };
            }
        }
    }
}
impl Entry<'_> {
    pub(crate) fn batch_slot(
        &self,
        input: ProjectionInput<'_>,
        mask: &[bool],
        index: usize,
        point: WorldPoint,
    ) -> Option<usize> {
        if !std::ptr::eq(self.arena, input.arena)
            || mask.get(self.ordinal) != Some(&true)
            || index >= self.range.len as usize
        {
            return None;
        }
        let slot = self.range.start as usize + index;
        (self.arena.bits.get(slot)? == &[point.x.to_bits(), point.y.to_bits()]).then_some(slot)
    }
    pub(crate) fn project(&self, index: usize, point: WorldPoint, scaler: &Scaler) -> ScreenPoint {
        if prepared_projection_matches(scaler) && index < self.range.len as usize {
            let i = self.range.start as usize + index;
            if self.arena.bits[i] == [point.x.to_bits(), point.y.to_bits()] {
                if let Some(token) = &self.arena.tokens[i] {
                    // Original kernel validates model, latitude, finite input/output; same arithmetic order.
                    return scaler.world_to_screen_with_prepared_northing(point, token);
                }
            }
        }
        scaler.world_to_screen(point)
    }
}
// Exact original add_line_points comparisons, including first-tie signed zero and
// nonfinite behavior. No new validation, min/max reassociation or source normalization.
// A failed Scaler transform update can retain the prior actual camera. The
// encoded camera model (map_camera::FlatMapCamera::encoded_identity) uses 1
// for the fixed WGS84 EllipsoidalMercator model; source tokens require it.
pub(crate) fn prepared_projection_matches(scaler: &Scaler) -> bool {
    scaler.projection() == FlatProjection::EllipsoidalMercator
        && scaler.flat_encoded_identity().is_some_and(|id| id[0] == 1)
}

fn update_original_bounds(b: &mut [f64; 4], p: WorldPoint) {
    if p.x < b[0] {
        b[0] = p.x;
    }
    if p.y < b[1] {
        b[1] = p.y;
    }
    if p.x > b[2] {
        b[2] = p.x;
    }
    if p.y > b[3] {
        b[3] = p.y;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn scaler(b: [f64; 4], ratio: f64) -> Scaler {
        let mut s = Scaler::new(
            ferrite_render::GeoBounds::new(b[0], b[1], b[2], b[3]),
            ferrite_render::Viewport::new(1280., 808.),
        );
        s.set_projection(FlatProjection::EllipsoidalMercator);
        s.set_pixel_ratio(ratio);
        s
    }
    fn context(points: Vec<WorldPoint>) -> RenderContext {
        let mut c = RenderContext::new(ferrite_render::Viewport::new(1280., 808.));
        let mut line = LineInstruction::new(points);
        line.portrayal_origin = PortrayalOrigin::NonPoint;
        c.add_instruction(DrawingInstruction::Line(line));
        c
    }
    fn bits(p: ScreenPoint) -> [u32; 2] {
        [p.x.to_bits(), p.y.to_bits()]
    }
    #[test]
    fn invalid_initial_transform_declines_before_valid_camera_rebuild() {
        let points = vec![WorldPoint::new(-1., 49.), WorldPoint::new(1., 50.)];
        let c = context(points.clone());
        let mut s = c.scaler.clone();
        s.set_projection(FlatProjection::EllipsoidalMercator);
        assert_eq!(s.projection(), FlatProjection::EllipsoidalMercator);
        assert_eq!(s.flat_encoded_identity().unwrap()[0], 0);
        let mut cache = Cache::new(Some("1".as_ref()));
        assert!(cache.prepare(&c, &s).is_none());
        s.set_bounds(ferrite_render::GeoBounds::new(-2., 48., 2., 52.));
        assert_eq!(s.flat_encoded_identity().unwrap()[0], 1);
        let frame = cache.prepare(&c, &s).unwrap();
        let entry = frame.entry(0, &points).unwrap();
        for (i, point) in points.iter().enumerate() {
            let expected = s.world_to_screen(*point);
            assert!(expected.x.is_finite() && expected.y.is_finite());
            assert_eq!(bits(entry.project(i, *point, &s)), bits(expected));
        }
        let mut incompatible = c.scaler.clone();
        incompatible.set_projection(FlatProjection::EllipsoidalMercator);
        for (i, point) in points.iter().enumerate() {
            assert_eq!(
                bits(entry.project(i, *point, &incompatible)),
                bits(incompatible.world_to_screen(*point))
            );
        }
    }

    #[test]
    fn owned_source_arena_exact_camera_wrap_dpi_and_bad_points() {
        let points = vec![
            WorldPoint::new(-0., 48.),
            WorldPoint::new(359., -20.),
            WorldPoint::new(1., 90.),
            WorldPoint::new(f64::INFINITY, 40.),
            WorldPoint::new(1., f64::NAN),
        ];
        let c = context(points.clone());
        let mut cache = Cache::new(Some("1".as_ref()));
        let s = scaler([-10., 40., 10., 60.], 1.);
        let frame = cache.prepare(&c, &s).unwrap();
        let e = frame.entry(0, &points).unwrap();
        for s in [
            s,
            scaler([350., -30., 370., 10.], 2.),
            scaler([-1., 47., 1., 49.], 1.5),
        ] {
            for (i, p) in points.iter().enumerate() {
                assert_eq!(bits(e.project(i, *p, &s)), bits(s.world_to_screen(*p)));
            }
            let changed = WorldPoint::new(7., 41.);
            assert_eq!(
                bits(e.project(0, changed, &s)),
                bits(s.world_to_screen(changed))
            );
        }
    }
    #[test]
    fn replacement_while_view_held_declines_then_rebuilds_without_retention_chain() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let d = context(vec![WorldPoint::new(3., 40.), WorldPoint::new(4., 42.)]);
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        let f = cache.prepare(&c, &s).unwrap();
        let hit = cache.prepare(&c, &s).unwrap();
        assert!(Arc::ptr_eq(&f.arena, &hit.arena));
        drop(hit);
        assert!(cache.prepare(&d, &s).is_none());
        drop(f);
        assert!(cache.prepare(&d, &s).is_some());
        assert_eq!(cache.work.as_ref().unwrap().borrow().declines, 1);
    }
    #[test]
    fn cap_declines_before_tokens_and_styles_keep_legacy_path() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let s = scaler([0., 30., 5., 50.], 1.);
        assert!(Cache::build(
            c.raw_instructions(),
            Arc::downgrade(&c.static_instruction_order_identity()),
            &s,
            HEADERS
        )
        .is_none());
        let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
            panic!("line fixture")
        };
        let mut l = line.clone();
        assert!(eligible(&l));
        l.style.offset_mm = 1.;
        assert!(!eligible(&l));
        l.style.offset_mm = 0.;
        l.style.dash_pattern.push(1.);
        assert!(!eligible(&l));
        assert!(!Cache::new(None).enabled);
        assert!(!Cache::new(Some("true".as_ref())).enabled);
    }
    #[test]
    fn ordinal_count_beyond_old_entry_limit_and_new_source_token() {
        let mut c = RenderContext::new(ferrite_render::Viewport::new(100., 100.));
        for i in 0..4097 {
            let mut line = LineInstruction::new(vec![
                WorldPoint::new(i as f64, 40.),
                WorldPoint::new(i as f64 + 1., 42.),
            ]);
            line.portrayal_origin = PortrayalOrigin::NonPoint;
            c.add_instruction(DrawingInstruction::Line(line));
        }
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        let f = cache.prepare(&c, &s).unwrap();
        assert_eq!(f.arena.ranges.len(), 4097);
        assert!(f.arena.charge <= CAP);
        drop(f);
        let mut line =
            LineInstruction::new(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        line.portrayal_origin = PortrayalOrigin::NonPoint;
        c.add_instruction(DrawingInstruction::Line(line));
        assert_eq!(cache.prepare(&c, &s).unwrap().arena.ranges.len(), 4098);
    }
    #[test]
    fn palette_and_reorder_retire_arena_but_current_camera_does_not() {
        let mut c = context(vec![WorldPoint::new(-0., 40.), WorldPoint::new(2., 42.)]);
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        let f = cache.prepare(&c, &s).unwrap();
        let owner = Arc::downgrade(&f.arena);
        drop(f);
        c.set_bounds(ferrite_render::GeoBounds::new(-5., 20., 7., 55.));
        let f = cache.prepare(&c, &s).unwrap();
        assert!(Arc::ptr_eq(&owner.upgrade().unwrap(), &f.arena));
        drop(f);
        c.remap_colors(&|_| ferrite_render::Color::rgba(0., 0., 0., 0.));
        let f = cache.prepare(&c, &s).unwrap();
        assert!(owner.upgrade().is_none());
        drop(f);
        let old = Arc::downgrade(cache.arena.as_ref().unwrap());
        let mut reordered = c.raw_instructions().to_vec();
        reordered.reverse();
        c.set_instructions_from_cache(reordered);
        let f = cache.prepare(&c, &s).unwrap();
        assert!(old.upgrade().is_none());
        drop(f);
    }
    #[test]
    fn model_change_dynamic_style_and_wrong_point_fall_back_exactly() {
        let points = vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)];
        let c = context(points.clone());
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        let f = cache.prepare(&c, &s).unwrap();
        let e = f.entry(0, &points).unwrap();
        let local = Scaler::new(
            ferrite_render::GeoBounds::new(0., 30., 5., 50.),
            ferrite_render::Viewport::new(100., 100.),
        );
        assert_eq!(
            bits(e.project(0, points[0], &local)),
            bits(local.world_to_screen(points[0]))
        );
        assert!(cache.prepare(&c, &local).is_none());
        let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
            panic!("line fixture")
        };
        let mut l = line.clone();
        l.screen_ray = Some(ferrite_render::ScreenRay {
            direction: 0.,
            length_mm: 2.,
            geographic_direction: false,
        });
        assert!(!eligible(&l));
        l.screen_ray = None;
        l.portrayal_origin = PortrayalOrigin::CoverageExempt;
        assert!(!eligible(&l));
        assert_eq!(
            bits(e.project(99, points[0], &s)),
            bits(s.world_to_screen(points[0]))
        );
    }
    #[test]
    fn joint_budget_selects_only_one_retained_northing_strategy() {
        let arena = Cache::new(Some("1".as_ref()));
        let mut moving =
            crate::moving_line_northing::Cache::new(arena.moving_flag(Some("1".as_ref())));
        moving.bind_epoch(1);
        let s = scaler([0., 30., 5., 50.], 1.);
        let p = [WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)];
        assert!(moving.prepare(&p, &s).is_none());
        assert_eq!(moving.statistics()["charged_retained_bytes"], 0);
        assert_eq!(moving.statistics()["enabled"], false);
        let off = Cache::new(None);
        let mut moving =
            crate::moving_line_northing::Cache::new(off.moving_flag(Some("1".as_ref())));
        moving.bind_epoch(1);
        assert!(moving.prepare(&p, &s).is_some());
        assert_eq!(off.statistics()["charged_retained_bytes"], 0);
        assert!(
            moving.statistics()["charged_retained_bytes"]
                .as_u64()
                .unwrap()
                <= 16 * 1024 * 1024
        );
    }
    #[test]
    fn deterministic_decline_is_constant_size_and_does_not_rescan_500_frames() {
        let mut c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        for _ in 0..500 {
            assert!(cache.prepare_with_cap(&c, &s, HEADERS).is_none());
        }
        let w = cache.work.as_ref().unwrap().borrow();
        assert_eq!(w.source_scans, 1);
        assert_eq!(w.negative_hits, 499);
        assert_eq!(w.cold, 0);
        drop(w);
        assert_eq!(
            cache.admission.as_ref().unwrap().outcome,
            Err(Decline::RequestedCap)
        );
        c.remap_colors(&|_| ferrite_render::Color::WHITE);
        assert!(cache.prepare_with_cap(&c, &s, HEADERS).is_none());
        assert_eq!(cache.work.as_ref().unwrap().borrow().source_scans, 2);
        assert!(cache.prepare(&c, &s).is_some()); // New budget cannot reuse a negative certificate.
    }
    #[test]
    fn held_view_decline_never_installs_permanent_negative_for_new_source() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let d = context(vec![WorldPoint::new(3., 40.), WorldPoint::new(4., 42.)]);
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        let old = cache.prepare(&c, &s).unwrap();
        for _ in 0..10 {
            assert!(cache.prepare(&d, &s).is_none());
        }
        assert_eq!(cache.work.as_ref().unwrap().borrow().source_scans, 1);
        assert!(cache.admission.as_ref().unwrap().outcome.is_ok());
        drop(old);
        assert!(cache.prepare(&d, &s).is_some());
        assert_eq!(cache.work.as_ref().unwrap().borrow().source_scans, 2);
    }
    #[test]
    fn successful_count_certificate_retries_unretained_build_without_source_scan() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let s = scaler([0., 30., 5., 50.], 1.);
        let mut cache = Cache::new(Some("1".as_ref()));
        // Exact state left by transient build failure: no arena, valid sealed count.
        // This tests state retry, not an OS allocator failure injection.
        cache.admission = Some(Admission {
            source: Arc::downgrade(&c.static_instruction_order_identity()),
            instruction_count: c.raw_instructions().len(),
            cap: CAP,
            outcome: preflight(c.raw_instructions(), CAP),
        });
        cache.work.as_ref().unwrap().borrow_mut().source_scans = 1;
        assert!(cache.prepare(&c, &s).is_some());
        let w = cache.work.as_ref().unwrap().borrow();
        assert_eq!(w.source_scans, 1);
        assert_eq!(w.admission_hits, 1);
        assert_eq!(w.negative_hits, 0);
    }
}

#[cfg(test)]
mod source_bounds_controls {
    use super::*;
    fn context(points: Vec<WorldPoint>) -> RenderContext {
        let mut c = RenderContext::new(ferrite_render::Viewport::new(800., 600.));
        let mut l = LineInstruction::new(points);
        l.portrayal_origin = PortrayalOrigin::NonPoint;
        c.add_instruction(DrawingInstruction::Line(l));
        c
    }
    fn points(c: &RenderContext) -> &[WorldPoint] {
        let DrawingInstruction::Line(l) = &c.raw_instructions()[0] else {
            panic!("line")
        };
        &l.points
    }
    fn scaler() -> Scaler {
        let mut s = Scaler::new(
            ferrite_render::GeoBounds::new(-10., 30., 10., 60.),
            ferrite_render::Viewport::new(800., 600.),
        );
        s.set_projection(FlatProjection::EllipsoidalMercator);
        s
    }
    fn original(p: &[WorldPoint]) -> [u64; 4] {
        // Independent original renderer body. Do not call the candidate updater.
        let (mut ax, mut ay, mut bx, mut by) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in p {
            if p.x < ax {
                ax = p.x;
            }
            if p.y < ay {
                ay = p.y;
            }
            if p.x > bx {
                bx = p.x;
            }
            if p.y > by {
                by = p.y;
            }
        }
        [ax.to_bits(), ay.to_bits(), bx.to_bits(), by.to_bits()]
    }
    fn bits(b: (f64, f64, f64, f64)) -> [u64; 4] {
        [b.0.to_bits(), b.1.to_bits(), b.2.to_bits(), b.3.to_bits()]
    }
    #[test]
    fn cached_bounds_bit_exact_ieee_first_ties_and_warm_camera_changes() {
        for p in [
            vec![WorldPoint::new(-0., -0.), WorldPoint::new(0., 0.)],
            vec![WorldPoint::new(0., 0.), WorldPoint::new(-0., -0.)],
            vec![
                WorldPoint::new(f64::NAN, f64::NAN),
                WorldPoint::new(f64::NAN, f64::NAN),
            ],
            vec![
                WorldPoint::new(f64::INFINITY, -f64::INFINITY),
                WorldPoint::new(1., 40.),
            ],
            vec![
                WorldPoint::new(f64::from_bits(1), 40.),
                WorldPoint::new(359., 90.),
            ],
        ] {
            let c = context(p);
            let mut s = scaler();
            let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
            let expected = original(points(&c));
            for i in 0..20 {
                s.set_pixel_ratio(1. + i as f64 * 0.1);
                let f = cache.prepare(&c, &s).unwrap();
                assert_eq!(bits(f.source_bounds(&c, 0, points(&c)).unwrap()), expected);
                let e = f.entry(0, points(&c)).unwrap();
                for (i, p) in points(&c).iter().enumerate() {
                    let a = e.project(i, *p, &s);
                    let b = s.world_to_screen(*p);
                    assert_eq!(
                        [a.x.to_bits(), a.y.to_bits()],
                        [b.x.to_bits(), b.y.to_bits()]
                    );
                }
            }
            let w = cache.work.as_ref().unwrap().borrow();
            assert_eq!(w.cold, 1);
            assert_eq!(w.bounds_hits, 20);
            assert_eq!(w.saved_bounds_points, 40);
        }
    }
    #[test]
    fn wrong_slice_foreign_samebytes_and_retained_old_identity_never_grant_bounds() {
        let mut c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let other = context(points(&c).to_vec());
        let held_old_identity = c.static_instruction_order_identity();
        let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        let f = cache.prepare(&c, &scaler()).unwrap();
        assert!(f.source_bounds(&other, 0, points(&other)).is_none());
        assert!(f.source_bounds(&c, 0, &points(&c).to_vec()).is_none());
        assert!(f.source_bounds(&c, 1, points(&c)).is_none());
        c.remap_colors(&|_| ferrite_render::Color::WHITE);
        assert!(Arc::ptr_eq(
            &held_old_identity,
            &f.arena.source.upgrade().unwrap()
        ));
        assert!(f.source_bounds(&c, 0, points(&c)).is_none()); // weak upgrade alone is insufficient.
        assert!(cache.prepare(&c, &scaler()).is_none()); // held old frame prevents old+new chain.
        drop(f);
        assert!(cache.prepare(&c, &scaler()).is_some());
    }
    #[test]
    fn bounds_share_total_cap_negative_admission_and_default_off_has_no_extra_vec() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let s = scaler();
        let base = charge(c.raw_instructions().len(), points(&c).len()).unwrap();
        let mut on = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        for _ in 0..500 {
            assert!(on.prepare_with_cap(&c, &s, base).is_none());
        }
        assert_eq!(on.work.as_ref().unwrap().borrow().source_scans, 1);
        assert_eq!(on.work.as_ref().unwrap().borrow().negative_hits, 499);
        assert!(on.arena.is_none());
        let f = on.prepare(&c, &s).unwrap();
        assert!(f.arena.charge <= CAP);
        assert!(f.arena.bounds.is_some());
        let a = &f.arena;
        assert_eq!(
            a.charge,
            HEADERS
                + a.ranges.capacity() * std::mem::size_of::<Range>()
                + a.bits.capacity() * std::mem::size_of::<[u64; 2]>()
                + a.tokens.capacity() * std::mem::size_of::<Option<PreparedFlatNorthing>>()
                + a.bounds.as_ref().unwrap().capacity() * std::mem::size_of::<[f64; 4]>()
        );
        drop(f);
        assert!(on.fork_cold().bounds_enabled);
        for policy in [None, Some("0".as_ref()), Some("invalid".as_ref())] {
            let mut off = Cache::new_with_bounds(Some("1".as_ref()), policy);
            let f = off.prepare(&c, &s).unwrap();
            assert!(f.arena.bounds.is_none());
            assert!(f.source_bounds(&c, 0, points(&c)).is_none());
        }
    }
    #[test]
    fn unsupported_styles_and_localprojection_retain_original_paths() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        let f = cache.prepare(&c, &scaler()).unwrap();
        let mut local = scaler();
        local.set_projection(FlatProjection::LocalGeographic);
        assert!(cache.prepare(&c, &local).is_none());
        for mode in 0..4 {
            let mut d = context(points(&c).to_vec());
            let mut instructions = d.raw_instructions().to_vec();
            let DrawingInstruction::Line(l) = &mut instructions[0] else {
                panic!("line")
            };
            match mode {
                0 => l.style.offset_mm = 1.,
                1 => l.style.dash_pattern.push(1.),
                2 => {
                    l.screen_ray = Some(ferrite_render::ScreenRay {
                        direction: 0.,
                        length_mm: 1.,
                        geographic_direction: false,
                    })
                }
                _ => l.portrayal_origin = PortrayalOrigin::CoverageExempt,
            }
            d.set_instructions_from_cache(instructions);
            assert!(f.source_bounds(&d, 0, points(&d)).is_none());
            let mut fresh = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
            let frame = fresh.prepare(&d, &scaler()).unwrap();
            assert!(frame.source_bounds(&d, 0, points(&d)).is_none());
        }
    }
    #[test]
    fn speculative_bounds_peek_is_exact_without_emitter_ledger_charge() {
        let c = context(vec![WorldPoint::new(-0., 40.), WorldPoint::new(0., 42.)]);
        let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        let frame = cache.prepare(&c, &scaler()).unwrap();
        let witness = frame.bind_source_bounds(&c).unwrap();
        for _ in 0..4 {
            assert_eq!(
                bits(witness.peek_source_bounds(0, points(&c)).unwrap()),
                original(points(&c))
            );
        }
        assert_eq!(frame.bounds_hits.get(), 0);
        assert_eq!(frame.bounds_points.get(), 0);
        assert_eq!(
            bits(witness.source_bounds(0, points(&c)).unwrap()),
            original(points(&c))
        );
        assert_eq!(frame.bounds_hits.get(), 1);
        assert_eq!(frame.bounds_points.get(), 2);
        drop(frame);
        let work = cache.work.as_ref().unwrap().borrow();
        assert_eq!(work.bounds_hits, 1);
        assert_eq!(work.saved_bounds_points, 2);
    }
    #[test]
    fn speculative_bounds_peek_rejects_invalid_or_unbound_sources_without_charge() {
        let c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let foreign = context(points(&c).to_vec());
        let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        let frame = cache.prepare(&c, &scaler()).unwrap();
        assert!(frame.bind_source_bounds(&foreign).is_none());
        let witness = frame.bind_source_bounds(&c).unwrap();
        let copied = points(&c).to_vec();
        for (ordinal, slice) in [
            (0, copied.as_slice()),
            (1, points(&c)),
            (usize::MAX, points(&c)),
            (0, &points(&c)[..1]),
        ] {
            assert!(witness.peek_source_bounds(ordinal, slice).is_none());
            assert!(witness.source_bounds(ordinal, slice).is_none());
        }
        assert_eq!(frame.bounds_hits.get(), 0);
        assert_eq!(frame.bounds_points.get(), 0);
        let mut off = Cache::new_with_bounds(Some("1".as_ref()), Some("0".as_ref()));
        let off_frame = off.prepare(&c, &scaler()).unwrap();
        assert!(off_frame.bind_source_bounds(&c).is_none());
    }
    #[test]
    fn one_bound_witness_keeps_order_exact_ordinals_and_rejects_copied_slice() {
        let mut c = context(vec![WorldPoint::new(-0., 40.), WorldPoint::new(0., 42.)]);
        let mut second =
            LineInstruction::new(vec![WorldPoint::new(8., 40.), WorldPoint::new(9., 43.)]);
        second.portrayal_origin = PortrayalOrigin::NonPoint;
        c.add_instruction(DrawingInstruction::Line(second));
        let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        let frame = cache.prepare(&c, &scaler()).unwrap();
        {
            let witness = frame.bind_source_bounds(&c).unwrap();
            for ordinal in [1, 0, 1, 0] {
                let DrawingInstruction::Line(line) = &c.raw_instructions()[ordinal] else {
                    panic!("line")
                };
                assert_eq!(
                    bits(witness.source_bounds(ordinal, &line.points).unwrap()),
                    original(&line.points)
                );
                assert!(witness
                    .source_bounds(ordinal, &line.points.clone())
                    .is_none());
            }
            assert!(witness.source_bounds(2, points(&c)).is_none());
            assert!(witness.source_bounds(1, points(&c)).is_none());
        }
        drop(frame);
        let work = cache.work.as_ref().unwrap().borrow();
        assert_eq!(work.bounds_hits, 4);
        assert_eq!(work.saved_bounds_points, 8);
    }
    #[test]
    fn binding_rejects_foreign_or_retired_source_even_with_old_identity_kept_alive() {
        let mut c = context(vec![WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)]);
        let foreign = context(points(&c).to_vec());
        let old_identity = c.static_instruction_order_identity();
        let mut cache = Cache::new_with_bounds(Some("1".as_ref()), Some("1".as_ref()));
        let frame = cache.prepare(&c, &scaler()).unwrap();
        assert!(frame.bind_source_bounds(&foreign).is_none());
        assert!(frame.bind_source_bounds(&c).is_some());
        c.remap_colors(&|_| ferrite_render::Color::WHITE);
        assert!(Arc::ptr_eq(
            &old_identity,
            &frame.arena.source.upgrade().unwrap()
        ));
        assert!(frame.bind_source_bounds(&c).is_none());
        drop(frame);
        let next = cache.prepare(&c, &scaler()).unwrap();
        assert!(next.bind_source_bounds(&c).is_some());
        let next_identity = c.static_instruction_order_identity();
        let replacement = c.raw_instructions().to_vec();
        c.set_instructions_from_cache(replacement);
        assert!(Arc::ptr_eq(
            &next_identity,
            &next.arena.source.upgrade().unwrap()
        ));
        assert!(next.bind_source_bounds(&c).is_none()); // identical re-owned bytes are still a new source.
        let mut off = Cache::new_with_bounds(Some("1".as_ref()), Some("0".as_ref()));
        let off_frame = off.prepare(&c, &scaler()).unwrap();
        assert!(off_frame.bind_source_bounds(&c).is_none());
    }
}
