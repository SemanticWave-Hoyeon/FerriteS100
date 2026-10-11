//! Immutable narrow metadata for the ORIGINAL static-line eligibility predicate.
//! Geometry, owner authority, live visibility and relation compilation are not cached here.
use crate::{DrawingInstruction, RenderContext, StaticInstructionOrderIdentity};
use std::{
    mem::size_of,
    sync::{Arc, Weak},
};
const RETAINED_CAP: usize = 1024 * 1024;
const MASK_CAP: usize = 1024 * 1024;
const CONTROL_CHARGE: usize = size_of::<StaticInstructionOrderIdentity>() + 2 * size_of::<usize>();
#[derive(Clone, Copy)]
struct Entry {
    ordinal: u32,
    lower: u32,
    upper: u32,
    stroke: bool,
}
struct Plan {
    source: Weak<StaticInstructionOrderIdentity>,
    count: usize,
    entries: Vec<Entry>,
}
#[derive(Default)]
pub(super) struct Cache {
    plan: Option<Plan>,
}
#[derive(Clone, Copy, Default)]
pub(super) struct Summary {
    pub hit: bool,
    pub cold: bool,
    pub decline: bool,
    pub entries: usize,
    pub source_checks: u64,
}
#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Work {
    pub calls: u64,
    pub optimized_calls: u64,
    pub fallback_calls: u64,
    pub cold: u64,
    pub hits: u64,
    pub declines: u64,
    pub source_checks: u64,
    pub descriptor_checks: u64,
    pub whole_eligibility_host_ns: u64,
    pub eligible: u64,
    pub retained_payload_bytes: u64,
    pub mask_payload_bytes: u64,
}
pub(super) fn enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}
fn static_line(i: &DrawingInstruction) -> bool {
    matches!(i,DrawingInstruction::Line(l) if l.screen_ray.is_none() && l.portrayal_path.is_none())
}
impl Cache {
    #[cfg(test)]
    pub(super) fn plan_for_test_is_absent(&self) -> bool {
        self.plan.is_none()
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.plan.as_ref().map_or(0, |p| {
            size_of::<Plan>() + CONTROL_CHARGE + p.entries.capacity() * size_of::<Entry>()
        })
    }
    pub(super) fn clear(&mut self) {
        self.plan = None;
    }
    pub(super) fn evaluate(
        &mut self,
        context: &RenderContext,
        scale: u32,
        visibility: Option<&[bool]>,
    ) -> (Option<Vec<bool>>, Summary) {
        self.evaluate_with_caps(context, scale, visibility, RETAINED_CAP, MASK_CAP)
    }
    fn evaluate_with_caps(
        &mut self,
        context: &RenderContext,
        scale: u32,
        visibility: Option<&[bool]>,
        retained_cap: usize,
        mask_cap: usize,
    ) -> (Option<Vec<bool>>, Summary) {
        let instructions = context.raw_instructions();
        let n = instructions.len();
        let mut summary = Summary::default();
        // Full output bounds and input dimensions checked BEFORE any allocation.
        if n > u32::MAX as usize
            || visibility.is_some_and(|v| v.len() != n)
            || n.checked_mul(size_of::<bool>())
                .and_then(|x| x.checked_add(size_of::<Vec<bool>>()))
                .is_none_or(|x| x > mask_cap)
        {
            summary.decline = true;
            return (None, summary);
        }
        let source = context.static_instruction_order_identity();
        let hit = self.plan.as_ref().is_some_and(|p| {
            p.count == n && p.source.upgrade().is_some_and(|s| Arc::ptr_eq(&s, &source))
        });
        if !hit {
            // No Plan or borrowed pointers escape; retire old charged Vec before reserving new.
            self.plan = None;
            summary.source_checks = n as u64;
            let count = instructions.iter().filter(|i| static_line(i)).count();
            let Some(request) = count
                .checked_mul(size_of::<Entry>())
                .and_then(|x| x.checked_add(size_of::<Plan>() + CONTROL_CHARGE))
            else {
                summary.decline = true;
                return (None, summary);
            };
            if request > retained_cap {
                summary.decline = true;
                return (None, summary);
            }
            let mut entries = Vec::new();
            if entries.try_reserve_exact(count).is_err()
                || entries
                    .capacity()
                    .checked_mul(size_of::<Entry>())
                    .and_then(|x| x.checked_add(size_of::<Plan>() + CONTROL_CHARGE))
                    .is_none_or(|x| x > retained_cap)
            {
                summary.decline = true;
                return (None, summary);
            }
            summary.source_checks = summary.source_checks.saturating_add(n as u64);
            for (ordinal, instruction) in instructions.iter().enumerate() {
                if let DrawingInstruction::Line(l) = instruction {
                    if l.screen_ray.is_none() && l.portrayal_path.is_none() {
                        entries.push(Entry {
                            ordinal: ordinal as u32,
                            lower: l.scale_range.scale_maximum.unwrap_or(0),
                            upper: l.scale_range.scale_minimum.unwrap_or(u32::MAX),
                            stroke: l.style.has_visible_stroke(),
                        });
                    }
                }
            }
            self.plan = Some(Plan {
                source: Arc::downgrade(&source),
                count: n,
                entries,
            });
            summary.cold = true;
        } else {
            summary.hit = true;
        }
        let plan = self.plan.as_ref().unwrap();
        summary.entries = plan.entries.len();
        let mut out = Vec::new();
        if out.try_reserve_exact(n).is_err()
            || out
                .capacity()
                .checked_mul(size_of::<bool>())
                .and_then(|x| x.checked_add(size_of::<Vec<bool>>()))
                .is_none_or(|x| x > mask_cap)
        {
            summary.decline = true;
            return (None, summary);
        }
        out.resize(n, false);
        for entry in &plan.entries {
            let ordinal = entry.ordinal as usize;
            out[ordinal] = visibility.is_none_or(|v| v[ordinal])
                && entry.stroke
                && scale >= entry.lower
                && scale <= entry.upper;
        }
        (Some(out), summary)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, LineInstruction, PointInstruction, ScaleRange, Viewport, WorldPoint};
    fn context() -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "a".into(),
            WorldPoint::new(0., 0.),
        )));
        for (width, alpha, lower, upper) in [
            (1., 1., None, None),
            (0., 1., None, None),
            (1., 0., None, None),
            (1., f32::NAN, None, None),
            (1., 1., Some(10), Some(20)),
            (1., 1., Some(20), Some(10)),
        ] {
            let mut l = LineInstruction::new(vec![]);
            l.style.width = width;
            l.style.color = Color::rgba(0., 0., 0., alpha);
            l.scale_range = ScaleRange {
                scale_maximum: lower,
                scale_minimum: upper,
            };
            c.add_instruction(DrawingInstruction::Line(l));
        }
        c
    }
    fn oracle(c: &RenderContext, scale: u32, v: Option<&[bool]>) -> Vec<bool> {
        c.raw_instructions().iter().enumerate().map(|(i,item)|v.is_none_or(|v|v.get(i).copied().unwrap_or(false)) && super::super::instruction_visible(item,scale,None,None)
            && matches!(item,DrawingInstruction::Line(l) if l.screen_ray.is_none() && l.portrayal_path.is_none())).collect()
    }
    #[test]
    fn exhaustive_full_original_mask_zero_empty_curve_and_scale_boundaries() {
        let c = context();
        let n = c.instruction_count();
        let mut cache = Cache::default();
        for bits in 0..1usize << n {
            for scale in [0, 1, 9, 10, 20, 21, u32::MAX] {
                let v: Vec<bool> = (0..n).map(|i| bits & (1 << i) != 0).collect();
                let (out, _) = cache.evaluate(&c, scale, Some(&v));
                assert_eq!(out.unwrap(), oracle(&c, scale, Some(&v)));
            }
        }
        assert_eq!(cache.evaluate(&c, 1, None).0.unwrap(), oracle(&c, 1, None));
    }
    #[test]
    fn same_context_palette_retirement_new_context_and_clear_recompile() {
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        let mut l = LineInstruction::new(vec![]);
        l.color_token = Some("LINE".into());
        c.add_instruction(DrawingInstruction::Line(l));
        let mut cache = Cache::default();
        assert!(cache.evaluate(&c, 1, None).1.cold);
        assert!(cache.evaluate(&c, 2, None).1.hit);
        for alpha in [0., f32::NAN, 1.] {
            c.remap_colors(&|_| Color::rgba(0., 0., 0., alpha));
            let (out, work) = cache.evaluate(&c, 1, None);
            assert!(work.cold);
            assert_eq!(out.unwrap(), oracle(&c, 1, None));
        }
        c.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "x".into(),
            WorldPoint::new(0., 0.),
        )));
        assert!(cache.evaluate(&c, 1, None).1.cold);
        cache.clear();
        assert!(cache.evaluate(&c, 1, None).1.cold);
        assert!(cache.evaluate(&context(), 1, None).1.cold);
    }
    #[test]
    fn bounded_declines_leave_whole_legacy_fallback_and_actual_capacity_charge() {
        let c = context();
        let mut cache = Cache::default();
        assert!(cache
            .evaluate_with_caps(&c, 1, None, 0, MASK_CAP)
            .0
            .is_none());
        assert!(cache
            .evaluate_with_caps(&c, 1, None, RETAINED_CAP, 0)
            .0
            .is_none());
        assert!(cache.evaluate(&c, 1, Some(&[true])).0.is_none());
        let (out, _) = cache.evaluate(&c, 1, None);
        let out = out.unwrap();
        assert!(cache.retained_bytes() <= RETAINED_CAP);
        assert!(size_of::<Vec<bool>>() + out.capacity() * size_of::<bool>() <= MASK_CAP);
        assert_eq!(out, oracle(&c, 1, None));
    }
    #[test]
    fn exact_optin_default_and_invalid_off() {
        assert!(enabled(Some(std::ffi::OsStr::new("1"))));
        for v in [None, Some("0"), Some("true"), Some(" 1")] {
            assert!(!enabled(v.map(std::ffi::OsStr::new)));
        }
    }
}
