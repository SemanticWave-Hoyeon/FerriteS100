//! Exact immutable-line AABB common subexpression, not a visibility heuristic.
use ferrite_render::{LineInstruction, StaticLineRelationEpoch, WorldPoint};
const CAP: usize = 8 * 1024 * 1024;
const ROWS: usize = 131072;
type Bounds = (f64, f64, f64, f64);
#[derive(Clone, Copy, Default)]
enum Slot {
    #[default]
    Empty,
    Ready(Bounds),
    Declined,
}
pub(crate) struct Cache {
    enabled: bool,
    epoch: Option<StaticLineRelationEpoch>,
    slots: Vec<Slot>,
    hits: u64,
    cold: u64,
    declined: u64,
    saved_points: u64,
    cold_points: u64,
}
impl Cache {
    /// Copy only the already sampled policy, not any published arena or GPU resource.
    pub(crate) fn fork_cold(&self) -> Self {
        Self::new(Some(std::ffi::OsStr::new(if self.enabled {
            "1"
        } else {
            "0"
        })))
    }

    pub(crate) fn new(flag: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: flag.and_then(|f| f.to_str()) == Some("1"),
            epoch: None,
            slots: Vec::new(),
            hits: 0,
            cold: 0,
            declined: 0,
            saved_points: 0,
            cold_points: 0,
        }
    }
    pub(crate) fn reset(&mut self) {
        self.epoch = None;
        self.slots = Vec::new();
    }
    pub(crate) fn bind(&mut self, epoch: StaticLineRelationEpoch, count: usize) {
        if !self.enabled || self.epoch == Some(epoch) {
            return;
        }
        self.reset();
        self.epoch = Some(epoch);
        if count > ROWS
            || !count
                .checked_mul(std::mem::size_of::<Slot>())
                .is_some_and(|n| n <= CAP)
        {
            self.declined = self.declined.saturating_add(1);
            return;
        }
        if self.slots.try_reserve_exact(count).is_err()
            || !self
                .slots
                .capacity()
                .checked_mul(std::mem::size_of::<Slot>())
                .is_some_and(|n| n <= CAP)
        {
            self.slots = Vec::new();
            self.declined = self.declined.saturating_add(1);
            return;
        }
        self.slots.resize(count, Slot::Empty);
    }
    pub(crate) fn prepare(&mut self, ordinal: usize, line: &LineInstruction) -> Option<Bounds> {
        if !self.enabled
            || line.screen_ray.is_some()
            || line.portrayal_path.is_some()
            || line.style.offset_mm != 0.0
            || line.points.len() < 2
        {
            return None;
        }
        let slot = self.slots.get_mut(ordinal)?;
        match *slot {
            Slot::Ready(b) => {
                self.hits = self.hits.saturating_add(1);
                self.saved_points = self.saved_points.saturating_add(line.points.len() as u64);
                Some(b)
            }
            Slot::Declined => None,
            Slot::Empty => {
                self.cold = self.cold.saturating_add(1);
                self.cold_points = self.cold_points.saturating_add(line.points.len() as u64);
                let b = legacy_finite_bounds(&line.points);
                *slot = b.map_or(Slot::Declined, Slot::Ready);
                b
            }
        }
    }
    pub(crate) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"slots":self.slots.len(),"charged_capacity_bytes":self.slots.capacity()*std::mem::size_of::<Slot>(),"cap":CAP,"hits":self.hits,"cold":self.cold,"declined":self.declined,"saved_point_scans":self.saved_points,"cold_point_scans":self.cold_points,"scope":"source bounds scans only; no suppressed/hidden objects or frame gain inferred"})
    }
}
fn legacy_finite_bounds(points: &[WorldPoint]) -> Option<Bounds> {
    let (mut ax, mut ay, mut bx, mut by) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in points {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        // Original strict comparisons preserve signed-zero/first-tie arithmetic.
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
    Some((ax, ay, bx, by))
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{DrawingInstruction, RenderContext, Viewport};
    fn context(points: Vec<WorldPoint>) -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(DrawingInstruction::Line(LineInstruction::new(points)));
        c.get_sorted_instructions();
        c
    }
    fn line(c: &RenderContext) -> &LineInstruction {
        let DrawingInstruction::Line(l) = &c.raw_instructions()[0] else {
            panic!()
        };
        l
    }
    #[test]
    fn signed_zero_original_bounds_bits_and_warm_scans() {
        let c = context(vec![
            WorldPoint::new(-0., -0.),
            WorldPoint::new(0., 0.),
            WorldPoint::new(10., 20.),
        ]);
        let mut cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        cache.bind(c.static_line_relation_epoch(), 1);
        let b = cache.prepare(0, line(&c)).unwrap();
        assert_eq!(b.0.to_bits(), (-0f64).to_bits());
        assert_eq!(b.1.to_bits(), (-0f64).to_bits());
        assert_eq!(b.2, 10.);
        assert_eq!(
            cache.prepare(0, line(&c)).unwrap().0.to_bits(),
            b.0.to_bits()
        );
        assert_eq!(cache.saved_points, 3);
    }
    #[test]
    fn source_mutation_and_sort_lifetime_rebuild() {
        let a = context(vec![WorldPoint::new(1., 1.), WorldPoint::new(2., 2.)]);
        let b = context(vec![WorldPoint::new(8., 8.), WorldPoint::new(9., 9.)]);
        let mut cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        cache.bind(a.static_line_relation_epoch(), 1);
        assert_eq!(cache.prepare(0, line(&a)).unwrap().0, 1.);
        cache.bind(b.static_line_relation_epoch(), 1);
        assert_eq!(cache.prepare(0, line(&b)).unwrap().0, 8.);
        assert_eq!(cache.hits, 0);
    }
    #[test]
    fn nonfinite_offset_empty_and_budget_decline_keep_cold_path() {
        assert!(legacy_finite_bounds(&[WorldPoint::new(f64::NAN, 2.)]).is_none());
        assert!(legacy_finite_bounds(&[WorldPoint::new(f64::INFINITY, 2.)]).is_none());
        let c = context(vec![WorldPoint::new(1., 1.), WorldPoint::new(2., 2.)]);
        let mut l = line(&c).clone();
        l.style.offset_mm = 1.;
        let mut cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        cache.bind(c.static_line_relation_epoch(), 1);
        assert!(cache.prepare(0, &l).is_none());
        cache.reset();
        cache.bind(c.static_line_relation_epoch(), ROWS + 1);
        assert!(cache.prepare(0, line(&c)).is_none());
        assert!(cache.slots.is_empty());
    }
    #[test]
    fn default_and_invalid_flag_do_not_allocate_or_prepare() {
        let c = context(vec![WorldPoint::new(1., 1.), WorldPoint::new(2., 2.)]);
        for flag in [
            None,
            Some(std::ffi::OsStr::new("0")),
            Some(std::ffi::OsStr::new("true")),
        ] {
            let mut cache = Cache::new(flag);
            cache.bind(c.static_line_relation_epoch(), 1);
            assert!(cache.prepare(0, line(&c)).is_none());
            assert_eq!(cache.slots.capacity(), 0);
        }
    }
    #[test]
    fn reowned_exact_context_reuses_only_opaque_validated_epoch() {
        let points = vec![WorldPoint::new(-179., -20.), WorldPoint::new(179., 40.)];
        let old = context(points.clone());
        let mut next = context(points);
        assert!(next.inherit_static_line_relations_from(&old));
        let mut cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        cache.bind(old.static_line_relation_epoch(), 1);
        let a = cache.prepare(0, line(&old)).unwrap();
        next.set_viewport(640., 480.);
        cache.bind(next.static_line_relation_epoch(), 1);
        let b = cache.prepare(0, line(&next)).unwrap();
        assert_eq!(
            [a.0.to_bits(), a.1.to_bits(), a.2.to_bits(), a.3.to_bits()],
            [b.0.to_bits(), b.1.to_bits(), b.2.to_bits(), b.3.to_bits()]
        );
        assert_eq!(cache.hits, 1);
    }
    #[test]
    fn independent_legacy_fold_many_finite_points_is_bit_exact() {
        let points = (0..2048)
            .map(|i| WorldPoint::new((i as f64 - 1024.) / 7., (i % 179) as f64 - 89.))
            .collect::<Vec<_>>();
        let mut legacy = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in &points {
            if p.x < legacy.0 {
                legacy.0 = p.x;
            }
            if p.y < legacy.1 {
                legacy.1 = p.y;
            }
            if p.x > legacy.2 {
                legacy.2 = p.x;
            }
            if p.y > legacy.3 {
                legacy.3 = p.y;
            }
        }
        let actual = legacy_finite_bounds(&points).unwrap();
        assert_eq!(
            [
                actual.0.to_bits(),
                actual.1.to_bits(),
                actual.2.to_bits(),
                actual.3.to_bits()
            ],
            [
                legacy.0.to_bits(),
                legacy.1.to_bits(),
                legacy.2.to_bits(),
                legacy.3.to_bits()
            ]
        );
    }
}
