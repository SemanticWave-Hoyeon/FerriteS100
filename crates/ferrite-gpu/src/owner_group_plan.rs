//! Group-only permissions: resource-instance owned, context identity bound.
use ferrite_render::{DrawingInstruction, RenderContext, StaticInstructionOrderIdentity};
use std::{
    mem::size_of,
    sync::{Arc, Weak},
};
const CAP: usize = 1024 * 1024;
const ERROR: u8 = 255;
pub(crate) struct Plan {
    source: Weak<StaticInstructionOrderIdentity>,
    bits: Vec<u8>,
}
#[derive(Default)]
pub(crate) struct Cache {
    plan: Option<Arc<Plan>>,
}
#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct Work {
    pub loops: u64,
    pub ordinals: u64,
    pub execution_skipped: u64,
    pub original_lookups: u64,
    pub planned_decisions: u64,
    pub admitted: u64,
    pub rejected: u64,
    pub loop_host_ns: u64,
    pub plan_prepare_host_ns: u64,
    pub plan_cold_host_ns: u64,
    pub plan_hit_host_ns: u64,
    pub plan_decline_host_ns: u64,
    pub plan_hits: u64,
    pub plan_cold: u64,
    pub plan_declines: u64,
    pub retained_payload_bytes: u64,
}
impl Cache {
    pub(crate) fn prepare(
        &mut self,
        context: &RenderContext,
        mut group: impl FnMut(&DrawingInstruction) -> Option<(bool, bool)>,
    ) -> (Option<Arc<Plan>>, bool) {
        let n = context.raw_instructions().len();
        let header = size_of::<Plan>() + 2 * size_of::<usize>();
        if n > CAP - header {
            self.plan = None;
            return (None, false);
        }
        let source = context.static_instruction_order_identity();
        if let Some(plan) = &self.plan {
            if plan.bits.len() == n
                && plan
                    .source
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, &source))
            {
                return (Some(plan.clone()), true);
            }
        }
        // Drop the previous payload before allocating the new bounded vector.
        self.plan = None;
        let Some(mut bits) = allocate(n) else {
            return (None, false);
        };
        for instruction in context.raw_instructions() {
            bits.push(match group(instruction) {
                Some((ordinary, override33010)) => {
                    u8::from(ordinary) | (u8::from(override33010) << 1)
                }
                None => ERROR,
            });
        }
        let plan = Arc::new(Plan {
            source: Arc::downgrade(&source),
            bits,
        });
        self.plan = Some(plan.clone());
        (Some(plan), false)
    }
}
fn allocate(n: usize) -> Option<Vec<u8>> {
    let header = size_of::<Plan>().checked_add(2 * size_of::<usize>())?;
    if n > CAP.checked_sub(header)? {
        return None;
    }
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    if v.capacity().checked_add(header)? > CAP {
        return None;
    }
    Some(v)
}
impl Plan {
    pub(crate) fn group_visible(
        &self,
        ordinal: usize,
        override_group: Option<u32>,
    ) -> Option<bool> {
        let bit = match override_group {
            None => 1,
            Some(33010) => 2,
            _ => return None,
        };
        let value = *self.bits.get(ordinal)?;
        if value == ERROR {
            None
        } else {
            Some(value & bit != 0)
        }
    }
    pub(crate) fn charged_bytes(&self) -> usize {
        self.bits.capacity() + size_of::<Plan>() + 2 * size_of::<usize>()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{
        DrawingInstruction, PointInstruction, ViewingGroup, Viewport, WorldPoint,
    };
    fn context(groups: &[u32]) -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(100., 100.));
        for &g in groups {
            let mut p = PointInstruction::new("a".into(), WorldPoint::new(0., 0.));
            p.viewing_group = ViewingGroup(g);
            c.add_instruction(DrawingInstruction::Point(p));
        }
        c
    }
    #[test]
    fn cap_declines_before_group_callback() {
        assert!(allocate(CAP).is_none());
        assert!(allocate(0).is_some());
    }
    #[test]
    fn identity_reorder_or_replacement_does_not_reuse() {
        let mut cache = Cache::default();
        let a = context(&[1, 2]);
        let (_, hit) = cache.prepare(&a, |i| Some((i.viewing_groups().all(|g| g.0 == 1), false)));
        assert!(!hit);
        assert!(cache.prepare(&a, |_| panic!("warm callback")).1);
        let b = context(&[2, 1]);
        let (p, hit) = cache.prepare(&b, |i| Some((i.viewing_groups().all(|g| g.0 == 1), false)));
        assert!(!hit);
        let p = p.unwrap();
        assert_eq!(p.group_visible(0, None), Some(false));
        assert_eq!(p.group_visible(1, None), Some(true));
    }
    #[test]
    fn deferred_owner_error_and_override_not_conflated() {
        let c = context(&[33010, 1]);
        let mut cache = Cache::default();
        let (p, _) = cache.prepare(&c, |i| {
            if i.viewing_groups().all(|g| g.0 == 33010) {
                Some((false, true))
            } else {
                None
            }
        });
        let p = p.unwrap();
        assert_eq!(p.group_visible(0, None), Some(false));
        assert_eq!(p.group_visible(0, Some(33010)), Some(true));
        assert_eq!(p.group_visible(1, None), None);
        assert_eq!(p.group_visible(0, Some(999)), None);
    }
    #[test]
    fn resource_instances_never_share_policy() {
        let c = context(&[1]);
        let mut a = Cache::default();
        let mut b = Cache::default();
        let pa = a.prepare(&c, |_| Some((true, true))).0.unwrap();
        let pb = b.prepare(&c, |_| Some((false, false))).0.unwrap();
        assert_eq!(pa.group_visible(0, None), Some(true));
        assert_eq!(pb.group_visible(0, None), Some(false));
    }
}
