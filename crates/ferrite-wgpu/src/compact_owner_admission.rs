//! Opt-in integration candidate: sealed-resource owned compact admission and original-ordinal schedule.
//! No camera, time, Parent, coverage, spatial, selection or geometry decisions are retained.
use ferrite_render::{DrawingInstruction, RenderContext, StaticInstructionOrderIdentity};
use std::{
    mem::size_of,
    sync::{Arc, Weak},
};
pub(crate) const RETAINED_CAP: usize = 3 * 1024 * 1024;
pub(crate) const FRAME_CAP: usize = 1024 * 1024;
const VALID_OWNER: u8 = 1;
const STROKE: u8 = 2;
const ORDINARY: u8 = 4;
const OVERRIDE_33010: u8 = 8;
#[derive(Clone, Copy)]
struct Entry {
    lower: u32,
    upper: u32,
    flags: u8,
}
pub(crate) struct Plan {
    source: Weak<StaticInstructionOrderIdentity>,
    entries: Vec<Entry>,
}
/// Must live inside exactly one immutable, sealed PortrayalResourceOwners instance.
/// Do not clone or export Plan: the cache keeps only one retained allocation.
#[derive(Default)]
pub(crate) struct Cache {
    plan: Option<Plan>,
}
pub(crate) struct Frame {
    /// Complete ordinal mask consumed by the ORIGINAL suppression path.
    pub(crate) mask: Vec<bool>,
    /// Original ordinals in strictly ascending order, never renumbered/grouped by owner.
    pub(crate) ordinals: Vec<u32>,
}
/// Sample once at Renderer creation. Absence, invalid and non-Unicode are OFF.
pub(crate) fn enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}
#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct Work {
    pub attempts: u64,
    pub cold: u64,
    pub hits: u64,
    pub declines: u64,
    pub prepare_host_ns: u64,
    pub evaluate_host_ns: u64,
    pub descriptor_visits: u64,
    pub original_owner_calls: u64,
    pub scheduled: u64,
    pub retained_payload_bytes: u64,
    pub frame_payload_bytes: u64,
}
impl Work {
    pub(crate) fn add(&mut self, other: Self) {
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.cold = self.cold.saturating_add(other.cold);
        self.hits = self.hits.saturating_add(other.hits);
        self.declines = self.declines.saturating_add(other.declines);
        self.prepare_host_ns = self.prepare_host_ns.saturating_add(other.prepare_host_ns);
        self.evaluate_host_ns = self.evaluate_host_ns.saturating_add(other.evaluate_host_ns);
        self.descriptor_visits = self
            .descriptor_visits
            .saturating_add(other.descriptor_visits);
        self.original_owner_calls = self
            .original_owner_calls
            .saturating_add(other.original_owner_calls);
        self.scheduled = self.scheduled.saturating_add(other.scheduled);
        self.retained_payload_bytes = other.retained_payload_bytes;
        self.frame_payload_bytes = other.frame_payload_bytes;
    }
}
impl Frame {
    pub(crate) fn charged_bytes(&self) -> usize {
        size_of::<Self>()
            + self.mask.capacity() * size_of::<bool>()
            + self.ordinals.capacity() * size_of::<u32>()
    }
}
/// No allocation, original indices and sequence unchanged. OFF is the full legacy range.
pub(crate) enum Ordinals<'a> {
    Legacy(std::ops::Range<usize>),
    Scheduled(std::slice::Iter<'a, u32>),
}
impl<'a> Ordinals<'a> {
    pub(crate) fn new(n: usize, frame: Option<&'a Frame>) -> Self {
        match frame {
            Some(f) => Self::Scheduled(f.ordinals.iter()),
            None => Self::Legacy(0..n),
        }
    }
}
impl Iterator for Ordinals<'_> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::Legacy(r) => r.next(),
            Self::Scheduled(i) => i.next().map(|i| *i as usize),
        }
    }
}
impl Cache {
    /// Callback is the existing sealed group compiler: None defers original owner validation.
    /// Returning a borrow prevents callers from retaining old plans during replacement.
    pub(crate) fn prepare(
        &mut self,
        context: &RenderContext,
        mut groups: impl FnMut(&DrawingInstruction) -> Option<(bool, bool)>,
    ) -> (Option<&Plan>, bool) {
        self.prepare_with_cap(context, &mut groups, RETAINED_CAP)
    }
    fn prepare_with_cap(
        &mut self,
        context: &RenderContext,
        mut groups: impl FnMut(&DrawingInstruction) -> Option<(bool, bool)>,
        cap: usize,
    ) -> (Option<&Plan>, bool) {
        let source = context.static_instruction_order_identity();
        let instructions = context.raw_instructions();
        let hit = self.plan.as_ref().is_some_and(|p| {
            p.entries.len() == instructions.len()
                && p.source.upgrade().is_some_and(|s| Arc::ptr_eq(&s, &source))
        });
        if hit {
            return (self.plan.as_ref(), true);
        }
        // Retained bytes released before the next allocation. No old-plan Arc escapes.
        self.plan = None;
        let Some(mut entries) = allocate_entries(instructions.len(), cap) else {
            return (None, false);
        };
        for instruction in instructions {
            let range = instruction.scale_range();
            let stroke = !matches!(instruction, DrawingInstruction::Line(l) if !l.style.has_visible_stroke());
            let flags = groups(instruction).map_or(0, |(ordinary, override33010)| {
                VALID_OWNER
                    | if stroke { STROKE } else { 0 }
                    | if ordinary { ORDINARY } else { 0 }
                    | if override33010 { OVERRIDE_33010 } else { 0 }
            });
            entries.push(Entry {
                lower: range.scale_maximum.unwrap_or(0),
                upper: range.scale_minimum.unwrap_or(u32::MAX),
                flags,
            });
        }
        self.plan = Some(Plan {
            source: Arc::downgrade(&source),
            entries,
        });
        (self.plan.as_ref(), false)
    }
}
fn allocate_entries(n: usize, cap: usize) -> Option<Vec<Entry>> {
    let header = size_of::<Plan>();
    if n.checked_mul(size_of::<Entry>())?.checked_add(header)? > cap {
        return None;
    }
    let mut entries = Vec::new();
    entries.try_reserve_exact(n).ok()?;
    if entries
        .capacity()
        .checked_mul(size_of::<Entry>())?
        .checked_add(header)?
        > cap
    {
        return None;
    }
    Some(entries)
}
fn allocate_frame(n: usize, cap: usize) -> Option<Frame> {
    if n > u32::MAX as usize {
        return None;
    }
    // Vec<bool> is an ordinary byte-per-bool Rust Vec, NOT a bit-packed container.
    let charge = |a: usize, b: usize| {
        a.checked_mul(size_of::<bool>())?
            .checked_add(b.checked_mul(size_of::<u32>())?)?
            .checked_add(size_of::<Frame>())
    };
    if charge(n, n)? > cap {
        return None;
    }
    let mut mask = Vec::new();
    mask.try_reserve_exact(n).ok()?;
    let mut ordinals = Vec::new();
    ordinals.try_reserve_exact(n).ok()?;
    if charge(mask.capacity(), ordinals.capacity())? > cap {
        return None;
    }
    Some(Frame { mask, ordinals })
}
impl Plan {
    pub(crate) fn charged_bytes(&self) -> usize {
        size_of::<Plan>() + self.entries.capacity() * size_of::<Entry>()
    }
    /// None means WHOLE legacy admission/emission, before any original callback ran.
    /// Err preserves original execution-true ordinal validation, even if its scale hides it.
    /// Pass the exact current context again; stale plan/length/unsupported override decline.
    pub(crate) fn evaluate<E>(
        &self,
        context: &RenderContext,
        execution: &[bool],
        scale: u32,
        override_group: Option<u32>,
        mut original: impl FnMut(usize) -> Result<bool, E>,
    ) -> Result<Option<Frame>, E> {
        self.evaluate_with_cap(
            context,
            execution,
            scale,
            override_group,
            &mut original,
            FRAME_CAP,
        )
    }
    fn evaluate_with_cap<E>(
        &self,
        context: &RenderContext,
        execution: &[bool],
        scale: u32,
        override_group: Option<u32>,
        mut original: impl FnMut(usize) -> Result<bool, E>,
        cap: usize,
    ) -> Result<Option<Frame>, E> {
        let bit = match override_group {
            None => ORDINARY,
            Some(33010) => OVERRIDE_33010,
            _ => return Ok(None),
        };
        let source = context.static_instruction_order_identity();
        if execution.len() != self.entries.len()
            || !self
                .source
                .upgrade()
                .is_some_and(|s| Arc::ptr_eq(&s, &source))
        {
            return Ok(None);
        }
        let Some(mut frame) = allocate_frame(execution.len(), cap) else {
            return Ok(None);
        };
        for (ordinal, (entry, execute)) in self.entries.iter().zip(execution).enumerate() {
            let visible = if !execute {
                false
            } else if entry.flags & VALID_OWNER == 0 {
                original(ordinal)?
            } else {
                entry.flags & STROKE != 0
                    && scale >= entry.lower
                    && scale <= entry.upper
                    && entry.flags & bit != 0
            };
            frame.mask.push(visible);
            if visible {
                frame.ordinals.push(ordinal as u32);
            }
        }
        Ok(Some(frame))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{
        LineInstruction, PointInstruction, ScaleRange, ViewingGroup, Viewport, WorldPoint,
    };
    fn context() -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(100., 100.));
        for (lower, upper, group) in [
            (None, None, 1),
            (Some(10), Some(20), 2),
            (Some(20), Some(10), 33010),
            (Some(0), Some(0), 33010),
        ] {
            let mut p = PointInstruction::new("x".into(), WorldPoint::new(0., 0.));
            p.scale_range = ScaleRange {
                scale_maximum: lower,
                scale_minimum: upper,
            };
            p.viewing_group = ViewingGroup(group);
            c.add_instruction(DrawingInstruction::Point(p));
        }
        for width in [0., -1., f32::NAN, f32::INFINITY, 1.] {
            let mut l =
                LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)]);
            l.style.width = width;
            l.viewing_group = ViewingGroup(1);
            c.add_instruction(DrawingInstruction::Line(l));
        }
        c
    }
    #[test]
    fn exhaustive_masks_scales_group_policy_preserve_order_and_original_gate() {
        let c = context();
        let n = c.raw_instructions().len();
        for policy in [vec![], vec![1], vec![1, 2], vec![33010], vec![1, 2, 33010]] {
            let groups: std::collections::HashSet<u32> = policy.into_iter().collect();
            let mut cache = Cache::default();
            let (p, _) = cache.prepare(&c, |i| {
                Some((
                    i.viewing_groups().all(|g| groups.contains(&g.0)),
                    i.viewing_groups()
                        .all(|g| groups.contains(&g.0) || g.0 == 33010),
                ))
            });
            let p = p.unwrap();
            for bits in 0..(1usize << n) {
                for scale in [0, 1, 9, 10, 20, 21, u32::MAX] {
                    for override_group in [None, Some(33010)] {
                        let execution: Vec<bool> = (0..n).map(|i| bits & (1 << i) != 0).collect();
                        let frame = p
                            .evaluate::<()>(&c, &execution, scale, override_group, |_| {
                                panic!("sealed owner")
                            })
                            .unwrap()
                            .unwrap();
                        let expected: Vec<bool> = c.raw_instructions().iter().enumerate().map(|(i,inst)| {
                    // Independent legacy body, not candidate descriptor evaluation.
                    execution[i] && !matches!(inst,DrawingInstruction::Line(l) if !l.style.has_visible_stroke())
                        && inst.scale_range().is_visible_at(scale)
                        && inst.viewing_groups().all(|g| groups.contains(&g.0)||override_group==Some(g.0))
                }).collect();
                        assert_eq!(frame.mask, expected);
                        assert_eq!(
                            frame.ordinals,
                            expected
                                .iter()
                                .enumerate()
                                .filter_map(|(i, v)| v.then_some(i as u32))
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn hidden_error_deferred_but_scale_hidden_error_not_skipped() {
        let c = context();
        let mut cache = Cache::default();
        let (p, _) = cache.prepare(&c, |_| None);
        let p = p.unwrap();
        let mut execution = vec![false; c.raw_instructions().len()];
        execution[2] = true;
        let mut calls = Vec::new();
        let result = p.evaluate(&c, &execution, 100, None, |i| {
            calls.push(i);
            Err::<bool, _>(i)
        });
        assert_eq!(result.err(), Some(2));
        assert_eq!(calls, vec![2]);
        execution[1] = true;
        assert_eq!(
            p.evaluate(&c, &execution, 100, None, |i| Err::<bool, _>(i))
                .err(),
            Some(1)
        );
    }
    #[test]
    fn resource_instances_identity_mask_and_override_cannot_grant_reuse() {
        let c = context();
        let other = context();
        let mut a = Cache::default();
        let mut b = Cache::default();
        assert!(!a.prepare(&c, |_| Some((true, true))).1);
        assert!(a.prepare(&c, |_| panic!("warm compile")).1);
        let (p, _) = a.prepare(&c, |_| unreachable!());
        let p = p.unwrap();
        let execution = vec![true; c.raw_instructions().len()];
        assert!(p
            .evaluate::<()>(&other, &execution, 1, None, |_| Ok(true))
            .unwrap()
            .is_none());
        assert!(p
            .evaluate::<()>(&c, &[], 1, None, |_| Ok(true))
            .unwrap()
            .is_none());
        assert!(p
            .evaluate::<()>(&c, &execution, 1, Some(7), |_| panic!(
                "decline before callbacks"
            ))
            .unwrap()
            .is_none());
        let (q, _) = b.prepare(&c, |_| Some((false, false)));
        assert!(q
            .unwrap()
            .evaluate::<()>(&c, &execution, 1, None, |_| Ok(true))
            .unwrap()
            .unwrap()
            .ordinals
            .is_empty());
    }
    #[test]
    fn cap_checks_precede_callbacks_and_charge_actual_capacity() {
        assert!(allocate_entries(usize::MAX, RETAINED_CAP).is_none());
        assert!(allocate_frame(usize::MAX, FRAME_CAP).is_none());
        assert!(allocate_entries(10, size_of::<Plan>() + 9 * size_of::<Entry>()).is_none());
        assert!(allocate_frame(
            10,
            size_of::<Frame>() + 9 * (size_of::<bool>() + size_of::<u32>())
        )
        .is_none());
        let c = context();
        let mut cache = Cache::default();
        let (p, _) = cache.prepare(&c, |_| Some((true, true)));
        assert!(p.unwrap().charged_bytes() <= RETAINED_CAP);
    }
}

#[cfg(test)]
mod integration_policy_controls {
    use super::*;
    #[test]
    fn policy_is_exact_default_off_and_nonunicode_off() {
        for value in [None, Some("0"), Some("true"), Some(" 1"), Some("")] {
            assert!(!enabled(value.map(std::ffi::OsStr::new)));
        }
        assert!(enabled(Some(std::ffi::OsStr::new("1"))));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(!enabled(Some(std::ffi::OsStr::from_bytes(&[255]))));
        }
    }
    #[test]
    fn cap_degrade_before_any_owner_callback_then_complete_legacy() {
        use ferrite_render::{PointInstruction, Viewport, WorldPoint};
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "a".into(),
            WorldPoint::new(0., 0.),
        )));
        let mut cache = Cache::default();
        assert!(cache
            .prepare_with_cap(&c, |_| panic!("retained denial before callback"), 0)
            .0
            .is_none());
        let (p, _) = cache.prepare(&c, |_| None);
        let p = p.unwrap();
        assert!(p
            .evaluate_with_cap::<()>(
                &c,
                &[true],
                1,
                None,
                |_| panic!("frame denial before callback"),
                0
            )
            .unwrap()
            .is_none());
        assert_eq!(Ordinals::new(1, None).collect::<Vec<_>>(), vec![0]);
        let frame = p
            .evaluate::<()>(&c, &[true], 1, None, |_| Ok(true))
            .unwrap()
            .unwrap();
        assert_eq!(Ordinals::new(1, Some(&frame)).collect::<Vec<_>>(), vec![0]);
    }
}
