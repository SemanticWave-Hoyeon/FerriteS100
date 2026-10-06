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
