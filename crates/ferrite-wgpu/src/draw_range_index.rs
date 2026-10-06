//! Bounded CPU dispatch index. It changes lookup cost, never drawing order.
use ferrite_kernel::CompositionPlane;

type Key = (CompositionPlane, i32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DrawKind {
    Area,
    Pattern,
    Line,
    Symbol,
    Text,
    Raster,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DrawRef {
    key: Key,
    kind: DrawKind,
    original_index: usize,
}

pub(crate) struct DrawRangeIndex {
    entries: Vec<DrawRef>,
}

impl DrawRangeIndex {
    /// Bound the logical allocation independently of dataset size. No retention.
    pub(crate) const BUDGET: usize = 8 * 1024 * 1024;

    pub(crate) fn new(total: usize) -> Option<Self> {
        if total.checked_mul(std::mem::size_of::<DrawRef>())? > Self::BUDGET {
            return None;
        }
        Some(Self {
            entries: Vec::with_capacity(total),
        })
    }

    pub(crate) fn push(&mut self, kind: DrawKind, key: Key, original_index: usize) {
        // Construction supplies exactly the counted ranges, including empty ones.
        assert!(self.entries.len() < self.entries.capacity());
        self.entries.push(DrawRef {
            key,
            kind,
            original_index,
        });
    }

    pub(crate) fn finish(&mut self) {
        // The original index breaks ties: priority sorting must never reorder
        // same-priority coverage sources, glyphs, or translucent primitives.
        self.entries
            .sort_unstable_by_key(|e| (e.key, e.kind, e.original_index));
    }

    fn select(&self, kind: DrawKind, key: Key) -> &[DrawRef] {
        let start = self
            .entries
            .partition_point(|e| (e.key, e.kind) < (key, kind));
        let end = self
            .entries
            .partition_point(|e| (e.key, e.kind) <= (key, kind));
        &self.entries[start..end]
    }
}

pub(crate) enum SelectedIndices<'a> {
    Indexed(std::slice::Iter<'a, DrawRef>),
    Full(std::ops::Range<usize>),
}

impl Iterator for SelectedIndices<'_> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::Indexed(it) => it.next().map(|e| e.original_index),
            Self::Full(it) => it.next(),
        }
    }
}

pub(crate) fn selected_indices(
    index: Option<&DrawRangeIndex>,
    kind: DrawKind,
    key: Key,
    full_length: usize,
) -> SelectedIndices<'_> {
    match index {
        Some(index) => SelectedIndices::Indexed(index.select(kind, key).iter()),
        None => SelectedIndices::Full(0..full_length),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane() -> CompositionPlane {
        // Avoid assumptions about enum spelling or catalogue plane numbering.
        CompositionPlane::new(
            ferrite_kernel::CompositionStage::Chart,
            std::num::NonZeroI32::new(1).unwrap(),
        )
    }

    #[test]
    fn dispatch_preserves_unsorted_repeated_priorities_and_primitive_order() {
        let keys = [
            (plane(), 9),
            (plane(), -1),
            (plane(), 9),
            (plane(), 0),
            (plane(), -1),
        ];
        let kinds = [
            DrawKind::Area,
            DrawKind::Pattern,
            DrawKind::Line,
            DrawKind::Symbol,
            DrawKind::Text,
            DrawKind::Raster,
        ];
        let mut index = DrawRangeIndex::new(keys.len() * kinds.len()).unwrap();
        for kind in kinds {
            for (i, key) in keys.iter().enumerate() {
                index.push(kind, *key, i);
            }
        }
        index.finish();
        for key in [(plane(), -1), (plane(), 0), (plane(), 9), (plane(), 99)] {
            for kind in kinds {
                let expected: Vec<_> = keys
                    .iter()
                    .enumerate()
                    .filter_map(|(i, k)| (*k == key).then_some(i))
                    .collect();
                assert_eq!(
                    selected_indices(Some(&index), kind, key, keys.len()).collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }

    #[test]
    fn plane_wrap_priority_schedule_keeps_sources_in_original_order() {
        let under = CompositionPlane::new(
            ferrite_kernel::CompositionStage::Chart,
            std::num::NonZeroI32::new(-2).unwrap(),
        );
        let over = CompositionPlane::new(
            ferrite_kernel::CompositionStage::Overlay,
            std::num::NonZeroI32::new(1).unwrap(),
        );
        let records = [
            (over, 5, DrawKind::Line),
            (under, 5, DrawKind::Symbol),
            (under, 5, DrawKind::Line),
            (under, 5, DrawKind::Line),
            (over, -1, DrawKind::Pattern),
            (under, 5, DrawKind::Text),
        ];
        let mut index = DrawRangeIndex::new(records.len()).unwrap();
        for (i, &(p, q, k)) in records.iter().enumerate() {
            index.push(k, (p, q), i);
        }
        index.finish();
        let keys = [(under, 5), (over, -1), (over, 5)];
        let primitives = [
            DrawKind::Area,
            DrawKind::Pattern,
            DrawKind::Line,
            DrawKind::Symbol,
        ];
        let old_schedule = keys
            .iter()
            .flat_map(|&key| {
                let mut schedule = Vec::new();
                for wrap in 0..3 {
                    for kind in primitives {
                        for (i, &(p, q, k)) in records.iter().enumerate() {
                            if (p, q) == key && kind == k {
                                schedule.push((key, wrap, kind, i));
                            }
                        }
                    }
                }
                for (i, &(p, q, k)) in records.iter().enumerate() {
                    if (p, q) == key && k == DrawKind::Text {
                        schedule.push((key, 255, k, i));
                    }
                }
                schedule
            })
            .collect::<Vec<_>>();
        let mut indexed_schedule = Vec::new();
        for key in keys {
            for wrap in 0..3 {
                for kind in primitives {
                    for i in selected_indices(Some(&index), kind, key, records.len()) {
                        indexed_schedule.push((key, wrap, kind, i));
                    }
                }
            }
            for i in selected_indices(Some(&index), DrawKind::Text, key, records.len()) {
                indexed_schedule.push((key, 255, DrawKind::Text, i));
            }
        }
        assert_eq!(indexed_schedule, old_schedule);
    }

    #[test]
    fn oversized_or_overflowed_dispatch_uses_complete_original_scan() {
        assert!(DrawRangeIndex::new(usize::MAX).is_none());
        assert!(
            DrawRangeIndex::new(DrawRangeIndex::BUDGET / std::mem::size_of::<DrawRef>() + 1)
                .is_none()
        );
        assert_eq!(
            selected_indices(None, DrawKind::Symbol, (plane(), 9), 4).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        let mut empty = DrawRangeIndex::new(0).unwrap();
        empty.finish();
        assert!(
            selected_indices(Some(&empty), DrawKind::Area, (plane(), 9), 0)
                .next()
                .is_none()
        );
    }
}
