//! Product-neutral immutable bounding-volume hierarchy. Visibility predicates
//! must be conservative and monotone under enclosure. Payload order is unchanged
//! by callers: this module selects candidates, never orders portrayal commands.
#[derive(Clone, Copy, Debug)]
pub struct SpatialBounds<const D: usize> {
    pub min: [f64; D],
    pub max: [f64; D],
}
impl<const D: usize> SpatialBounds<D> {
    pub fn valid(&self) -> bool {
        D > 0
            && (0..D).all(|i| {
                self.min[i].is_finite() && self.max[i].is_finite() && self.min[i] <= self.max[i]
            })
    }
    fn union(self, other: Self) -> Self {
        Self {
            min: std::array::from_fn(|i| self.min[i].min(other.min[i])),
            max: std::array::from_fn(|i| self.max[i].max(other.max[i])),
        }
    }
    fn center(&self, i: usize) -> f64 {
        self.min[i] * 0.5 + self.max[i] * 0.5
    }
}
impl SpatialBounds<3> {
    pub fn enclosing_sphere(&self) -> ([f64; 3], f64) {
        let c = std::array::from_fn(|i| self.center(i));
        let h: [f64; 3] = std::array::from_fn(|i| self.max[i] * 0.5 - self.min[i] * 0.5);
        let scale = self
            .min
            .iter()
            .chain(&self.max)
            .fold(1.0_f64, |s, x| s.max(x.abs()));
        (
            c,
            h[0].hypot(h[1]).hypot(h[2]) + 64. * f64::EPSILON * scale + 1e-6,
        )
    }
}
#[derive(Debug)]
struct Node<const D: usize> {
    bounds: SpatialBounds<D>,
    range: std::ops::Range<usize>,
    children: Option<[usize; 2]>,
}
/// Inside must guarantee every enclosed source is accepted. Intersecting keeps
/// the exact leaf predicate; Outside must conservatively reject the entire box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialRelation {
    Outside,
    Intersecting,
    Inside,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct SpatialQueryStats {
    pub nodes_tested: usize,
    pub leaves_tested: usize,
    pub selected: usize,
    pub inside_subtrees: usize,
    pub inside_entries: usize,
}
#[derive(Debug)]
pub struct SpatialHierarchy<const D: usize> {
    entries: Vec<(usize, SpatialBounds<D>)>,
    nodes: Vec<Node<D>>,
}
impl<const D: usize> Default for SpatialHierarchy<D> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            nodes: Vec::new(),
        }
    }
}
impl<const D: usize> SpatialHierarchy<D> {
    /// Invalid bounds are rejected rather than silently dropping their payload.
    /// Caller must keep such source objects on its ordinary exact-rendering path.
    pub fn build(entries: Vec<(usize, SpatialBounds<D>)>) -> Result<Self, &'static str> {
        if entries.iter().any(|(_, b)| !b.valid()) {
            return Err("Invalid spatial bound");
        }
        let mut tree = Self {
            entries,
            nodes: Vec::new(),
        };
        if !tree.entries.is_empty() {
            tree.build_node(0..tree.entries.len());
        }
        Ok(tree)
    }
    /// Fallible construction of the SAME tree. Caller separately bounds entry capture,
    /// query scratch and old/next leases; this bounds accepted owned Vec capacities only.
    pub fn build_bounded(
        entries: Vec<(usize, SpatialBounds<D>)>,
        max_entries: usize,
        max_owned_bytes: usize,
    ) -> Result<Self, &'static str> {
        Self::build_bounded_with_reserve(entries, max_entries, max_owned_bytes, |nodes, count| {
            nodes
                .try_reserve_exact(count)
                .map_err(|_| "Spatial node allocation unavailable")
        })
    }

    fn build_bounded_with_reserve(
        entries: Vec<(usize, SpatialBounds<D>)>,
        max_entries: usize,
        max_owned_bytes: usize,
        reserve: impl FnOnce(&mut Vec<Node<D>>, usize) -> Result<(), &'static str>,
    ) -> Result<Self, &'static str> {
        if entries.len() > max_entries {
            return Err("Spatial entry budget exceeded");
        }
        if entries.iter().any(|(_, bounds)| !bounds.valid()) {
            return Err("Invalid spatial bound");
        }
        let entry_bytes = entries
            .capacity()
            .checked_mul(std::mem::size_of::<(usize, SpatialBounds<D>)>())
            .ok_or("Spatial byte overflow")?;
        let node_count = exact_node_count(entries.len()).ok_or("Spatial node overflow")?;
        let predicted = owned_bytes::<D>(entry_bytes, node_count).ok_or("Spatial byte overflow")?;
        if predicted > max_owned_bytes {
            return Err("Spatial owned byte budget exceeded");
        }
        let mut nodes = Vec::new();
        // Even zero-count uses the same helper; no push occurs for empty input.
        reserve(&mut nodes, node_count)?;
        let actual =
            owned_bytes::<D>(entry_bytes, nodes.capacity()).ok_or("Spatial byte overflow")?;
        if nodes.capacity() < node_count || actual > max_owned_bytes {
            return Err("Spatial actual allocation exceeds budget");
        }
        let mut tree = Self { entries, nodes };
        if !tree.entries.is_empty() {
            tree.build_node(0..tree.entries.len());
        }
        debug_assert_eq!(tree.nodes.len(), node_count);
        debug_assert_eq!(tree.retained_bytes(), actual);
        Ok(tree)
    }

    fn build_node(&mut self, range: std::ops::Range<usize>) -> usize {
        let mut bounds = self.entries[range.start].1;
        for (_, b) in &self.entries[range.start + 1..range.end] {
            bounds = bounds.union(*b);
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            bounds,
            range: range.clone(),
            children: None,
        });
        if range.len() > 8 {
            let axis = (0..D)
                .max_by(|&a, &b| {
                    (bounds.max[a] - bounds.min[a]).total_cmp(&(bounds.max[b] - bounds.min[b]))
                })
                .unwrap();
            let mid = range.start + range.len() / 2;
            self.entries[range.clone()].select_nth_unstable_by(mid - range.start, |a, b| {
                a.1.center(axis)
                    .total_cmp(&b.1.center(axis))
                    .then(a.0.cmp(&b.0))
            });
            let left = self.build_node(range.start..mid);
            let right = self.build_node(mid..range.end);
            self.nodes[index].children = Some([left, right]);
        }
        index
    }
    pub fn ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.entries.iter().map(|x| x.0)
    }
    pub fn retained_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<(usize, SpatialBounds<D>)>()
            + self.nodes.capacity() * std::mem::size_of::<Node<D>>()
    }
    pub fn query(
        &self,
        mut intersects: impl FnMut(&SpatialBounds<D>) -> bool,
        mut selected: impl FnMut(usize),
    ) -> SpatialQueryStats {
        let mut stats = SpatialQueryStats::default();
        if !self.nodes.is_empty() {
            self.visit(0, &mut intersects, &mut selected, &mut stats);
        }
        stats
    }
    pub fn query_classified(
        &self,
        mut classify: impl FnMut(&SpatialBounds<D>) -> SpatialRelation,
        mut selected: impl FnMut(usize),
    ) -> SpatialQueryStats {
        let mut stats = SpatialQueryStats::default();
        if !self.nodes.is_empty() {
            self.visit_classified(0, &mut classify, &mut selected, &mut stats);
        }
        stats
    }
    fn visit_classified(
        &self,
        index: usize,
        classify: &mut impl FnMut(&SpatialBounds<D>) -> SpatialRelation,
        selected: &mut impl FnMut(usize),
        stats: &mut SpatialQueryStats,
    ) {
        let node = &self.nodes[index];
        stats.nodes_tested += 1;
        match classify(&node.bounds) {
            SpatialRelation::Outside => {}
            SpatialRelation::Inside => {
                stats.inside_subtrees += 1;
                stats.inside_entries += node.range.len();
                stats.selected += node.range.len();
                for (id, _) in &self.entries[node.range.clone()] {
                    selected(*id);
                }
            }
            SpatialRelation::Intersecting => {
                if let Some([a, b]) = node.children {
                    self.visit_classified(a, classify, selected, stats);
                    self.visit_classified(b, classify, selected, stats);
                } else {
                    for (id, b) in &self.entries[node.range.clone()] {
                        stats.leaves_tested += 1;
                        if classify(b) != SpatialRelation::Outside {
                            selected(*id);
                            stats.selected += 1;
                        }
                    }
                }
            }
        }
    }
    fn visit(
        &self,
        index: usize,
        intersects: &mut impl FnMut(&SpatialBounds<D>) -> bool,
        selected: &mut impl FnMut(usize),
        stats: &mut SpatialQueryStats,
    ) {
        let node = &self.nodes[index];
        stats.nodes_tested += 1;
        if !intersects(&node.bounds) {
            return;
        }
        if let Some([a, b]) = node.children {
            self.visit(a, intersects, selected, stats);
            self.visit(b, intersects, selected, stats);
        } else {
            for (id, b) in &self.entries[node.range.clone()] {
                stats.leaves_tested += 1;
                if intersects(b) {
                    selected(*id);
                    stats.selected += 1;
                }
            }
        }
    }
}
// Exact recurrence: T(0)=0; T(n<=8)=1; T(n>8)=1+T(floor(n/2))+T(ceil(n/2)).
// At a complete split depth, widths differ by at most one. Stop when the smaller
// width <=8. Only size9 nodes at that frontier need one more split. Count leaves,
// then use full binary-tree nodes=2*leaves-1. O(log n), no scratch allocation.
fn exact_node_count(n: usize) -> Option<usize> {
    if n == 0 {
        return Some(0);
    }
    let mut frontier = 1usize;
    while n / frontier > 8 {
        frontier = frontier.checked_mul(2)?;
    }
    let leaves = if n / frontier == 8 {
        frontier.checked_add(n % frontier)?
    } else {
        frontier
    };
    leaves.checked_mul(2)?.checked_sub(1)
}
fn owned_bytes<const D: usize>(entry_bytes: usize, node_capacity: usize) -> Option<usize> {
    node_capacity
        .checked_mul(std::mem::size_of::<Node<D>>())?
        .checked_add(entry_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhaustive_queries_match_linear_reference_and_prune() {
        let entries: Vec<_> = (0..100000)
            .map(|i| {
                let x = (i % 1000) as f64;
                let y = (i / 1000) as f64;
                (
                    i,
                    SpatialBounds {
                        min: [x, y],
                        max: [x + 0.5, y + 0.5],
                    },
                )
            })
            .collect();
        let tree = SpatialHierarchy::build(entries.clone()).unwrap();
        for q in [
            [5., 5., 6., 6.],
            [-1., -1., -0.5, -0.5],
            [0., 0., 1000., 100.],
            [999.5, 99.5, 1000., 100.],
            [350., -1., 370., 100.],
        ] {
            let test = |b: &SpatialBounds<2>| {
                b.max[0] >= q[0] && b.min[0] <= q[2] && b.max[1] >= q[1] && b.min[1] <= q[3]
            };
            let expected: Vec<_> = entries
                .iter()
                .filter(|(_, b)| test(b))
                .map(|(id, _)| *id)
                .collect();
            let mut actual = vec![];
            let stats = tree.query(test, |id| actual.push(id));
            actual.sort_unstable();
            assert_eq!(actual, expected);
            let mut classified = Vec::new();
            let cs = tree.query_classified(
                |b| {
                    if !test(b) {
                        SpatialRelation::Outside
                    } else if b.min[0] >= q[0]
                        && b.min[1] >= q[1]
                        && b.max[0] <= q[2]
                        && b.max[1] <= q[3]
                    {
                        SpatialRelation::Inside
                    } else {
                        SpatialRelation::Intersecting
                    }
                },
                |id| classified.push(id),
            );
            classified.sort_unstable();
            assert_eq!(classified, expected);
            if q == [0., 0., 1000., 100.] {
                assert_eq!(cs.nodes_tested, 1);
                assert_eq!(cs.leaves_tested, 0);
                assert_eq!(cs.inside_entries, 100000);
            }

            if q == [5., 5., 6., 6.] {
                assert!(stats.leaves_tested < 100);
                assert!(stats.nodes_tested < 100);
            }
        }
    }
    #[test]
    fn enclosure_contains_source_spheres_and_queries_preserve_candidates() {
        let entries: Vec<_> = (0..2000)
            .map(|i| {
                let c = [i as f64 * 1000., (i % 7) as f64 * 200., -6000000.];
                let radius = (i % 17 + 1) as f64;
                (
                    i,
                    SpatialBounds {
                        min: c.map(|x| x - radius),
                        max: c.map(|x| x + radius),
                    },
                )
            })
            .collect();
        let tree = SpatialHierarchy::build(entries.clone()).unwrap();
        let planes = [
            [1., 0., 0., -250000.],
            [-1., 0., 0., 300000.],
            [0., 1., 0., 0.],
        ];
        let test = |b: &SpatialBounds<3>| {
            let (c, r) = b.enclosing_sphere();
            planes
                .iter()
                .all(|p| p[0] * c[0] + p[1] * c[1] + p[2] * c[2] + p[3] >= -r)
        };
        let expected: Vec<_> = entries
            .iter()
            .filter(|(_, b)| test(b))
            .map(|(id, _)| *id)
            .collect();
        let mut actual = vec![];
        tree.query(test, |id| actual.push(id));
        actual.sort_unstable();
        assert_eq!(actual, expected);
        for n in &tree.nodes {
            let (c, r) = n.bounds.enclosing_sphere();
            for (_, b) in &tree.entries[n.range.clone()] {
                for corner in 0..8 {
                    let p: [f64; 3] = std::array::from_fn(|i| {
                        if corner & (1 << i) == 0 {
                            b.min[i]
                        } else {
                            b.max[i]
                        }
                    });
                    assert!((p[0] - c[0]).hypot(p[1] - c[1]).hypot(p[2] - c[2]) <= r);
                }
            }
        }
    }
    #[test]
    fn invalid_and_empty_are_explicit() {
        assert!(SpatialHierarchy::<2>::build(vec![(
            0,
            SpatialBounds {
                min: [f64::NAN, 0.],
                max: [1., 1.]
            }
        )])
        .is_err());
        assert_eq!(
            SpatialHierarchy::<2>::default()
                .query(|_| true, |_| panic!())
                .selected,
            0
        );
    }
}

#[cfg(test)]
mod bounded_builder_controls {
    use super::*;
    fn entries(n: usize) -> Vec<(usize, SpatialBounds<2>)> {
        (0..n)
            .map(|i| {
                let x = (i % 23) as f64 - 11.;
                let y = (i / 23) as f64 - 5.;
                // Repeated IDs intentionally remain repeated, never deduplicated.
                (
                    i % 7,
                    SpatialBounds {
                        min: [x, y],
                        max: [x + 0.5, y + 0.5],
                    },
                )
            })
            .collect()
    }
    fn budget(input: &Vec<(usize, SpatialBounds<2>)>) -> usize {
        owned_bytes::<2>(
            input.capacity() * std::mem::size_of::<(usize, SpatialBounds<2>)>(),
            exact_node_count(input.len()).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn exact_count_matches_independent_recurrence_and_large_boundaries() {
        let mut oracle = vec![0usize; 65537];
        for n in 1..oracle.len() {
            oracle[n] = if n <= 8 {
                1
            } else {
                1 + oracle[n / 2] + oracle[n - n / 2]
            };
            assert_eq!(exact_node_count(n), Some(oracle[n]));
        }
        assert_eq!(exact_node_count(0), Some(0));
        for shift in 0..usize::BITS - 3 {
            let width = 1usize.checked_shl(shift).unwrap();
            let n = width.checked_mul(8).unwrap();
            assert_eq!(
                exact_node_count(n),
                width.checked_mul(2).and_then(|v| v.checked_sub(1))
            );
            assert_eq!(
                exact_node_count(n + 1),
                width.checked_mul(2).and_then(|v| v.checked_add(1))
            );
        }
        assert!(exact_node_count(usize::MAX).is_some());
        assert!(owned_bytes::<2>(usize::MAX, 1).is_none());
        assert!(owned_bytes::<2>(0, usize::MAX).is_none());
    }
    #[test]
    fn bounded_build_has_identical_tree_order_bounds_and_query_results() {
        for n in [0, 1, 8, 9, 16, 17, 31, 32, 33, 257, 4097] {
            let input = entries(n);
            let legacy = SpatialHierarchy::build(input.clone()).unwrap();
            let tree = SpatialHierarchy::build_bounded(input, n, usize::MAX).unwrap();
            assert_eq!(tree.nodes.len(), legacy.nodes.len());
            assert_eq!(
                tree.ids().collect::<Vec<_>>(),
                legacy.ids().collect::<Vec<_>>()
            );
            for (a, b) in tree.nodes.iter().zip(&legacy.nodes) {
                assert_eq!(a.range, b.range);
                assert_eq!(a.children, b.children);
                assert_eq!(
                    a.bounds.min.map(f64::to_bits),
                    b.bounds.min.map(f64::to_bits)
                );
                assert_eq!(
                    a.bounds.max.map(f64::to_bits),
                    b.bounds.max.map(f64::to_bits)
                );
            }
            for q in [
                [-50., -50., 50., 500.],
                [-1., -1., 1., 1.],
                [500., 500., 501., 501.],
                [-11., -5., -11., -5.],
            ] {
                let test = |b: &SpatialBounds<2>| {
                    b.max[0] >= q[0] && b.min[0] <= q[2] && b.max[1] >= q[1] && b.min[1] <= q[3]
                };
                let mut old = Vec::new();
                let mut new = Vec::new();
                let os = legacy.query(test, |id| old.push(id));
                let ns = tree.query(test, |id| new.push(id));
                assert_eq!(new, old);
                assert_eq!(ns.nodes_tested, os.nodes_tested);
                assert_eq!(ns.leaves_tested, os.leaves_tested);
                let mut linear: Vec<_> = entries(n)
                    .into_iter()
                    .filter(|(_, b)| test(b))
                    .map(|(id, _)| id)
                    .collect();
                let mut selected = new;
                linear.sort_unstable();
                selected.sort_unstable();
                assert_eq!(selected, linear);
                let mut classified = Vec::new();
                tree.query_classified(
                    |b| {
                        if !test(b) {
                            SpatialRelation::Outside
                        } else if b.min[0] >= q[0]
                            && b.min[1] >= q[1]
                            && b.max[0] <= q[2]
                            && b.max[1] <= q[3]
                        {
                            SpatialRelation::Inside
                        } else {
                            SpatialRelation::Intersecting
                        }
                    },
                    |id| classified.push(id),
                );
                classified.sort_unstable();
                assert_eq!(classified, linear);
            }
        }
    }
    #[test]
    fn actual_capacity_not_length_and_exact_leaf_charge_are_admitted() {
        let mut input = Vec::new();
        input.try_reserve_exact(64).unwrap();
        input.extend(entries(9));
        let length_only =
            owned_bytes::<2>(9 * std::mem::size_of::<(usize, SpatialBounds<2>)>(), 3).unwrap();
        assert!(SpatialHierarchy::build_bounded(input, 9, length_only).is_err());
        let input = entries(17);
        let exact = budget(&input);
        let full_binary = owned_bytes::<2>(
            input.capacity() * std::mem::size_of::<(usize, SpatialBounds<2>)>(),
            2 * 17 - 1,
        )
        .unwrap();
        assert!(exact < full_binary);
        // Allocation capacity itself is platform-dependent: observed reservation is
        // still checked by builder, not assumed equal to the requested node count.
        let tree = SpatialHierarchy::build_bounded(input, 17, full_binary).unwrap();
        assert_eq!(tree.nodes.len(), 5);
        assert!(tree.retained_bytes() <= full_binary);
        assert!(tree.retained_bytes() >= exact);
        assert!(exact > 0);
    }
    #[test]
    fn preflight_failure_never_reaches_reserve_and_whole_failure_is_explicit() {
        let calls = std::cell::Cell::new(0);
        let reserve = |_: &mut Vec<Node<2>>, _| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert!(
            SpatialHierarchy::build_bounded_with_reserve(entries(9), 8, usize::MAX, reserve)
                .is_err()
        );
        assert_eq!(calls.get(), 0);
        let input = entries(9);
        let bytes = budget(&input);
        assert!(
            SpatialHierarchy::build_bounded_with_reserve(input, 9, bytes - 1, reserve).is_err()
        );
        assert_eq!(calls.get(), 0);
        let invalid = vec![(
            1,
            SpatialBounds {
                min: [f64::NAN, 0.],
                max: [1., 1.],
            },
        )];
        assert!(
            SpatialHierarchy::build_bounded_with_reserve(invalid, 1, usize::MAX, reserve).is_err()
        );
        assert_eq!(calls.get(), 0);
        assert!(
            SpatialHierarchy::build_bounded_with_reserve(entries(9), 9, usize::MAX, |_, _| Err(
                "injected allocation refusal"
            ))
            .is_err()
        );
        // A reservation that returns success but supplies insufficient capacity
        // cannot let recursive push silently grow the tree.
        assert!(SpatialHierarchy::build_bounded_with_reserve(
            entries(9),
            9,
            usize::MAX,
            |_, _| Ok(())
        )
        .is_err());
    }
    #[test]
    fn excess_actual_reservation_is_rejected_before_tree_partition() {
        let input = entries(17);
        let bytes = budget(&input);
        assert!(
            SpatialHierarchy::build_bounded_with_reserve(input, 17, bytes, |nodes, count| {
                nodes
                    .try_reserve_exact(count * 2)
                    .map_err(|_| "allocation unavailable")
            })
            .is_err()
        );
        let empty = SpatialHierarchy::<2>::build_bounded(Vec::new(), 0, 0).unwrap();
        assert_eq!(empty.retained_bytes(), 0);
        assert!(empty.nodes.is_empty());
    }
    #[test]
    fn signed_zero_closed_edges_nonfinite_and_invalid_dimensions_match_legacy() {
        let input = vec![(
            9,
            SpatialBounds {
                min: [-0., -0.],
                max: [0., 0.],
            },
        )];
        let old = SpatialHierarchy::build(input.clone()).unwrap();
        let new = SpatialHierarchy::build_bounded(input, 1, usize::MAX).unwrap();
        assert_eq!(
            old.nodes[0].bounds.min.map(f64::to_bits),
            new.nodes[0].bounds.min.map(f64::to_bits)
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let input = vec![(
                2,
                SpatialBounds {
                    min: [bad, 0.],
                    max: [1., 1.],
                },
            )];
            assert!(SpatialHierarchy::build(input.clone()).is_err());
            assert!(SpatialHierarchy::build_bounded(input, 1, usize::MAX).is_err());
        }
        let zero = vec![(0, SpatialBounds::<0> { min: [], max: [] })];
        assert!(SpatialHierarchy::build(zero.clone()).is_err());
        assert!(SpatialHierarchy::build_bounded(zero, 1, usize::MAX).is_err());
        assert!(SpatialHierarchy::<0>::build_bounded(Vec::new(), 0, 0).is_ok());
    }
}
