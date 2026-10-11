//! Private compiler-helper prototype. This index is NOT an authority/camera key.
//! Original Segment::overlap remains authoritative after inclusive bbox selection.
//! Connected opt-in growth index. Retention charge includes actual Vec capacities.
const LEAF_SIZE: usize = 16;
const NONE: usize = usize::MAX;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub lower: [f64; 2],
    pub upper: [f64; 2],
}
impl Bounds {
    fn valid(self) -> bool {
        (0..2).all(|i| {
            self.lower[i].is_finite() && self.upper[i].is_finite() && self.lower[i] <= self.upper[i]
        })
    }
    fn intersects(self, other: Self) -> bool {
        (0..2).all(|i| self.lower[i] <= other.upper[i] && other.lower[i] <= self.upper[i])
    }
    fn union(self, other: Self) -> Self {
        Self {
            lower: [
                self.lower[0].min(other.lower[0]),
                self.lower[1].min(other.lower[1]),
            ],
            upper: [
                self.upper[0].max(other.upper[0]),
                self.upper[1].max(other.upper[1]),
            ],
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Entry<T: Copy> {
    pub bounds: Bounds,
    pub payload: T,
}
#[derive(Clone, Copy)]
struct Node {
    bounds: Bounds,
    start: usize,
    end: usize,
    left: usize,
    right: usize,
}
pub struct OwnedOverlapBlock<T: Copy> {
    entries: Vec<Entry<T>>,
    nodes: Vec<Node>,
    charged: usize,
}
impl<T: Copy> OwnedOverlapBlock<T> {
    #[cfg(test)]
    pub fn try_build(source: &[Entry<T>], budget: usize) -> Option<Self> {
        let requested = source.len().checked_mul(std::mem::size_of::<Entry<T>>())?;
        if requested.checked_add(std::mem::size_of::<Self>())? > budget {
            return None;
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(source.len()).ok()?;
        if entries
            .capacity()
            .checked_mul(std::mem::size_of::<Entry<T>>())?
            .checked_add(std::mem::size_of::<Self>())?
            > budget
        {
            return None;
        }
        entries.extend_from_slice(source);
        Self::try_build_owned(entries, budget)
    }
    /// Consumes an already bounded input Vec: no second whole entry allocation.
    pub fn try_build_owned(mut entries: Vec<Entry<T>>, budget: usize) -> Option<Self> {
        if entries.iter().any(|e| !e.bounds.valid()) {
            return None;
        }
        let n = entries.len();
        let nodes = if n == 0 {
            0
        } else {
            n.div_ceil(LEAF_SIZE)
                .checked_next_power_of_two()?
                .checked_mul(2)?
                .checked_sub(1)?
        };
        let base = std::mem::size_of::<Self>();
        let entry_charge = entries
            .capacity()
            .checked_mul(std::mem::size_of::<Entry<T>>())?;
        let requested = base
            .checked_add(entry_charge)?
            .checked_add(nodes.checked_mul(std::mem::size_of::<Node>())?)?;
        if requested > budget {
            return None;
        }
        let mut tree_nodes = Vec::new();
        tree_nodes.try_reserve_exact(nodes).ok()?;
        let charged = base.checked_add(entry_charge)?.checked_add(
            tree_nodes
                .capacity()
                .checked_mul(std::mem::size_of::<Node>())?,
        )?;
        if charged > budget {
            return None;
        }
        entries.sort_unstable_by(|a, b| {
            a.bounds.lower[0]
                .total_cmp(&b.bounds.lower[0])
                .then(a.bounds.lower[1].total_cmp(&b.bounds.lower[1]))
                .then(a.bounds.upper[0].total_cmp(&b.bounds.upper[0]))
                .then(a.bounds.upper[1].total_cmp(&b.bounds.upper[1]))
        });
        let mut out = Self {
            entries,
            nodes: tree_nodes,
            charged,
        };
        if n != 0 {
            out.build_node(0, n);
        }
        debug_assert!(out.nodes.len() <= nodes);
        Some(out)
    }
    fn build_node(&mut self, start: usize, end: usize) -> usize {
        let slot = self.nodes.len();
        debug_assert!(slot < self.nodes.capacity());
        let initial = self.entries[start].bounds;
        self.nodes.push(Node {
            bounds: initial,
            start,
            end,
            left: NONE,
            right: NONE,
        });
        if end - start <= LEAF_SIZE {
            let bounds = self.entries[start + 1..end]
                .iter()
                .fold(initial, |b, e| b.union(e.bounds));
            self.nodes[slot].bounds = bounds;
        } else {
            let middle = start + (end - start) / 2;
            let left = self.build_node(start, middle);
            let right = self.build_node(middle, end);
            self.nodes[slot].bounds = self.nodes[left].bounds.union(self.nodes[right].bounds);
            self.nodes[slot].left = left;
            self.nodes[slot].right = right;
        }
        slot
    }
    pub fn charged_payload_bytes(&self) -> usize {
        self.charged
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn query(&self, bounds: Bounds) -> Option<Query<'_, T>> {
        if !bounds.valid() {
            return None;
        }
        let mut stack = [NONE; usize::BITS as usize];
        let active = (!self.nodes.is_empty()) as usize;
        if active != 0 {
            stack[0] = 0;
        }
        Some(Query {
            block: self,
            bounds,
            stack,
            active,
            leaf_next: 0,
            leaf_end: 0,
        })
    }
}
pub struct Query<'a, T: Copy> {
    block: &'a OwnedOverlapBlock<T>,
    bounds: Bounds,
    // Balanced recursion depth <=usize::BITS; fixed traversal scratch, no heap.
    stack: [usize; usize::BITS as usize],
    active: usize,
    leaf_next: usize,
    leaf_end: usize,
}
impl<'a, T: Copy> Iterator for Query<'a, T> {
    type Item = &'a Entry<T>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            while self.leaf_next < self.leaf_end {
                let entry = &self.block.entries[self.leaf_next];
                self.leaf_next += 1;
                if entry.bounds.intersects(self.bounds) {
                    return Some(entry);
                }
            }
            if self.active == 0 {
                return None;
            }
            self.active -= 1;
            let node = &self.block.nodes[self.stack[self.active]];
            if !node.bounds.intersects(self.bounds) {
                continue;
            }
            if node.left == NONE {
                self.leaf_next = node.start;
                self.leaf_end = node.end;
            } else {
                debug_assert!(self.active + 2 <= self.stack.len());
                self.stack[self.active] = node.right;
                self.stack[self.active + 1] = node.left;
                self.active += 2;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: usize, x: f64, y: f64) -> Entry<usize> {
        Entry {
            payload: id,
            bounds: Bounds {
                lower: [x, y],
                upper: [x + 2., y + 1.],
            },
        }
    }
    fn ids(block: &OwnedOverlapBlock<usize>, b: Bounds) -> Vec<usize> {
        let mut a: Vec<_> = block.query(b).unwrap().map(|x| x.payload).collect();
        a.sort_unstable();
        a
    }
    #[test]
    fn exact_linear_envelope_oracle_including_duplicates_and_signed_zero() {
        let mut source: Vec<_> = (0..257)
            .map(|i| entry(i, (i % 19) as f64, (i % 13) as f64))
            .collect();
        source.push(entry(257, -0., -0.));
        source.push(entry(258, 0., 0.));
        let block = OwnedOverlapBlock::try_build(&source, 1024 * 1024).unwrap();
        for x in -1..22 {
            for y in -1..16 {
                let b = Bounds {
                    lower: [x as f64, y as f64],
                    upper: [x as f64 + 1., y as f64 + 1.],
                };
                let mut old: Vec<_> = source
                    .iter()
                    .filter(|e| e.bounds.intersects(b))
                    .map(|e| e.payload)
                    .collect();
                old.sort_unstable();
                assert_eq!(ids(&block, b), old);
            }
        }
    }
    #[test]
    fn empty_zero_budget_invalid_and_actual_capacity_admission() {
        assert!(OwnedOverlapBlock::<usize>::try_build(&[], 0).is_none());
        let empty = OwnedOverlapBlock::<usize>::try_build(&[], 1024).unwrap();
        assert!(empty.is_empty());
        let b = Bounds {
            lower: [0., 0.],
            upper: [1., 1.],
        };
        assert_eq!(empty.query(b).unwrap().count(), 0);
        let source = [entry(0, 0., 0.)];
        let full = OwnedOverlapBlock::try_build(&source, 1024).unwrap();
        assert!(OwnedOverlapBlock::try_build(&source, full.charged_payload_bytes() - 1).is_none());
        assert!(OwnedOverlapBlock::try_build(
            &[Entry {
                payload: 0usize,
                bounds: Bounds {
                    lower: [f64::NAN, 0.],
                    upper: [1., 1.]
                }
            }],
            1024
        )
        .is_none());
    }
    #[test]
    fn split_owned_blocks_union_equals_one_linear_epoch_without_dedup() {
        let source: Vec<_> = (0..65)
            .map(|i| entry(i, (i % 11) as f64, (i % 7) as f64))
            .collect();
        let blocks: Vec<_> = source
            .chunks(17)
            .map(|s| OwnedOverlapBlock::try_build(s, 1024 * 1024).unwrap())
            .collect();
        for x in 0..15 {
            let b = Bounds {
                lower: [x as f64, 0.],
                upper: [x as f64 + 1., 9.],
            };
            let mut actual: Vec<_> = blocks
                .iter()
                .flat_map(|p| p.query(b).unwrap().map(|e| e.payload))
                .collect();
            actual.sort_unstable();
            let mut old: Vec<_> = source
                .iter()
                .filter(|e| e.bounds.intersects(b))
                .map(|e| e.payload)
                .collect();
            old.sort_unstable();
            assert_eq!(actual, old);
        }
    }
}
