//! Owned exact dispatch metadata only. Never a coverage/visibility or GPU resource cache.
use super::*;
use std::cell::{Cell, Ref, RefCell};
const CAP: usize = 16 * 1024 * 1024;
type Key = (CompositionPlane, i32);
type SymbolKey = (CompositionPlane, i32, usize, usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Row {
    kind: u8,
    key: Key,
    ordinal: usize,
    start: usize,
    end: usize,
    source: Option<usize>,
    extra: [usize; 6],
}
#[derive(Clone, Copy)]
struct Input<'a> {
    row: Row,
    name: &'a str,
}
struct Stored {
    row: Row,
    name: Vec<u8>,
}
pub(super) struct Plan {
    owner: Arc<()>,
    indexed: bool,
    rows: Vec<Stored>,
    pub(super) priorities: Vec<Key>,
    pub(super) index: Option<DrawRangeIndex>,
    symbols: Vec<(SymbolKey, usize)>,
    symbol_lookup_enabled: bool,
    charge: usize,
}
impl Plan {
    pub(super) fn symbol(&self, key: SymbolKey) -> Option<usize> {
        let i = self.symbols.partition_point(|entry| entry.0 < key);
        self.symbols
            .get(i)
            .filter(|entry| entry.0 == key)
            .map(|entry| entry.1)
    }
    pub(super) fn symbol_lookup_enabled(&self) -> bool {
        self.symbol_lookup_enabled
    }
}
pub(in crate::renderer) struct Cache {
    enabled: bool,
    plan: RefCell<Option<Plan>>,
    hits: Cell<u64>,
    builds: Cell<u64>,
    declines: Cell<u64>,
}
impl Cache {
    pub(in crate::renderer) fn new(value: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: value == Some(std::ffi::OsStr::new("1")),
            plan: RefCell::new(None),
            hits: Cell::new(0),
            builds: Cell::new(0),
            declines: Cell::new(0),
        }
    }
    pub(in crate::renderer) fn fork_cold(&self) -> Self {
        Self::new(Some(std::ffi::OsStr::new(if self.enabled {
            "1"
        } else {
            "0"
        })))
    }
    pub(in crate::renderer) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"hits":self.hits.get(),"builds":self.builds.get(),
          "declines":self.declines.get(),"charged_capacity_bytes":self.plan.borrow().as_ref().map_or(0,|p|p.charge),
          "cap":CAP,"scope":"exact dispatch metadata; not permission/GPU/RSS/frame-time"})
    }
    fn prepare_inputs<'a, 'b, I>(
        &'a self,
        owner: &Arc<()>,
        indexed: bool,
        count: usize,
        symbols: usize,
        inputs: I,
    ) -> Option<Ref<'a, Plan>>
    where
        I: Iterator<Item = Input<'b>> + Clone,
    {
        if !self.enabled {
            return None;
        }
        let matches = self.plan.borrow().as_ref().is_some_and(|p| {
            Arc::ptr_eq(&p.owner, owner)
                && p.indexed == indexed
                && p.rows.len() == count
                && p.rows
                    .iter()
                    .zip(inputs.clone())
                    .all(|(old, new)| old.row == new.row && old.name == new.name.as_bytes())
        });
        if matches {
            self.hits.set(self.hits.get().saturating_add(1));
        } else {
            // Drop old before any new allocation. No caller can retain/escape a Plan.
            self.plan.replace(None);
            let built = build(owner, indexed, count, symbols, inputs);
            if built.is_none() {
                self.declines.set(self.declines.get().saturating_add(1));
                return None;
            }
            self.builds.set(self.builds.get().saturating_add(1));
            self.plan.replace(built);
        }
        Some(Ref::map(self.plan.borrow(), |p| {
            p.as_ref().expect("installed plan")
        }))
    }
    pub(super) fn prepare<'a>(&'a self, scene: &VectorDrawScene<'_>) -> Option<Ref<'a, Plan>> {
        if !self.enabled {
            return None;
        }
        let c = &scene.emission.vector_frame.frame_cpu;
        let count = [
            c.area_priority_ranges.len(),
            c.pattern_ranges.len(),
            c.line_priority_ranges.len(),
            c.symbol_priority_ranges.len(),
            scene.text.len(),
            scene.raster_layers().len(),
            scene.symbols.len(),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)?;
        self.prepare_inputs(
            &c.owner,
            scene.draw_range_index_enabled,
            count,
            scene.symbols.len(),
            rows(scene),
        )
    }
}
fn row(kind: u8, key: Key, ordinal: usize, start: usize, end: usize, source: Option<usize>) -> Row {
    Row {
        kind,
        key,
        ordinal,
        start,
        end,
        source,
        extra: [0; 6],
    }
}
fn rows<'a>(scene: &'a VectorDrawScene<'a>) -> impl Iterator<Item = Input<'a>> + Clone + 'a {
    let c = &scene.emission.vector_frame.frame_cpu;
    c.area_priority_ranges
        .iter()
        .enumerate()
        .map(|(i, &(p, q, a, b, s))| Input {
            row: row(0, (p, q), i, a, b, s),
            name: "",
        })
        .chain(
            c.pattern_ranges
                .iter()
                .enumerate()
                .map(|(i, (p, q, a, b, name, wrap, s))| {
                    let mut r = row(1, (*p, *q), i, *a, *b, *s);
                    r.extra[0] = usize::from(*wrap);
                    Input { row: r, name }
                }),
        )
        .chain(
            c.line_priority_ranges
                .iter()
                .enumerate()
                .map(|(i, &(p, q, a, b, s))| Input {
                    row: row(2, (p, q), i, a, b, s),
                    name: "",
                }),
        )
        .chain(
            c.symbol_priority_ranges
                .iter()
                .enumerate()
                .map(|(i, &(p, q, a, b, s))| Input {
                    row: row(3, (p, q), i, a, b, s),
                    name: "",
                }),
        )
        .chain(scene.text.iter().enumerate().map(|(i, t)| {
            let mut r = row(
                4,
                (t.plane, t.priority),
                i,
                0,
                t.index_count as usize,
                t.source,
            );
            r.extra = [
                usize::from(t.wrap_pass),
                t.scissor[0] as usize,
                t.scissor[1] as usize,
                t.scissor[2] as usize,
                t.scissor[3] as usize,
                0,
            ];
            Input { row: r, name: "" }
        }))
        .chain(
            scene
                .raster_layers()
                .iter()
                .enumerate()
                .map(|(i, l)| Input {
                    row: row(
                        5,
                        l.draw_order.render_key(),
                        i,
                        0,
                        l.index_count as usize,
                        None,
                    ),
                    name: "",
                }),
        )
        .chain(
            scene
                .symbols
                .iter()
                .enumerate()
                .map(|(i, (p, q, a, b, _, _, ranges))| {
                    let mut r = row(6, (*p, *q), i, *a, *b, None);
                    r.extra[0] = ranges.len();
                    Input { row: r, name: "" }
                }),
        )
}
fn reserve<T>(n: usize) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    Some(v)
}
fn charge<T>(v: &Vec<T>) -> Option<usize> {
    v.capacity().checked_mul(std::mem::size_of::<T>())
}
fn build<'a, I>(
    owner: &Arc<()>,
    indexed: bool,
    count: usize,
    symbol_count: usize,
    inputs: I,
) -> Option<Plan>
where
    I: Iterator<Item = Input<'a>> + Clone,
{
    // A count-only impossible scene declines before walking any metadata or names.
    if count.checked_mul(std::mem::size_of::<Stored>())? > CAP {
        return None;
    }
    let index_count = count.checked_sub(symbol_count)?;
    let lookup = indexed && symbol_count <= 32768;
    let names = inputs
        .clone()
        .try_fold(0usize, |n, x| n.checked_add(x.name.len()))?;
    let requested = std::mem::size_of::<Plan>()
        .checked_add(count.checked_mul(std::mem::size_of::<Stored>())?)?
        .checked_add(count.checked_mul(std::mem::size_of::<Key>())?)?
        .checked_add(if lookup {
            symbol_count.checked_mul(std::mem::size_of::<(SymbolKey, usize)>())?
        } else {
            0
        })?
        .checked_add(names)?
        .checked_add(if indexed {
            DrawRangeIndex::retained_charge_bound(index_count).unwrap_or(0)
        } else {
            0
        })?;
    if requested > CAP {
        return None;
    }
    let mut records = reserve::<Stored>(count)?;
    let mut priorities = reserve::<Key>(count)?;
    let mut symbols = reserve::<(SymbolKey, usize)>(if lookup { symbol_count } else { 0 })?;
    // Original index declines independently of the retained plan's aggregate cap.
    let mut index = if indexed {
        DrawRangeIndex::try_new_retained(index_count)
    } else {
        None
    };
    let mut charged = std::mem::size_of::<Plan>()
        .checked_add(charge(&records)?)?
        .checked_add(charge(&priorities)?)?
        .checked_add(charge(&symbols)?)?
        .checked_add(index.as_ref().map_or(0, DrawRangeIndex::charged_bytes))?;
    if charged.checked_add(names)? > CAP {
        return None;
    }
    for input in inputs {
        if records.len() == count {
            return None;
        }
        let mut name = reserve::<u8>(input.name.len())?;
        charged = charged.checked_add(charge(&name)?)?;
        if charged > CAP {
            return None;
        }
        name.extend_from_slice(input.name.as_bytes());
        let r = input.row;
        if r.kind < 5 {
            priorities.push(r.key);
        } // Raster membership is always evaluated LIVE.
        if r.kind == 6 {
            if lookup && index.is_some() {
                symbols.push(((r.key.0, r.key.1, r.start, r.end), r.ordinal));
            }
        } else if let Some(index) = &mut index {
            let kind = match r.kind {
                0 => DrawKind::Area,
                1 => DrawKind::Pattern,
                2 => DrawKind::Line,
                3 => DrawKind::Symbol,
                4 => DrawKind::Text,
                5 => DrawKind::Raster,
                _ => return None,
            };
            index.push(kind, r.key, r.ordinal);
        }
        records.push(Stored { row: r, name });
    }
    if records.len() != count {
        return None;
    }
    if let Some(index) = &mut index {
        index.finish();
    }
    priorities.sort_unstable();
    priorities.dedup();
    symbols.sort_unstable(); // key then original ordinal => position() first match.
    let symbol_lookup_enabled = lookup && index.is_some();
    Some(Plan {
        owner: Arc::clone(owner),
        indexed,
        rows: records,
        priorities,
        index,
        symbols,
        symbol_lookup_enabled,
        charge: charged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(priority: i32) -> Key {
        (
            CompositionPlane::new(
                CompositionStage::Chart,
                std::num::NonZeroI32::new(1).unwrap(),
            ),
            priority,
        )
    }
    fn inputs(rows: &[Row]) -> impl Iterator<Item = Input<'_>> + Clone {
        rows.iter().copied().map(|row| Input { row, name: "" })
    }
    #[test]
    fn identical_owned_scene_reuses_actual_plan_without_rebuild() {
        let cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        let owner = Arc::new(());
        let rows = [
            row(2, key(9), 0, 0, 6, Some(1)),
            row(0, key(-1), 0, 0, 3, Some(2)),
        ];
        {
            let p = cache
                .prepare_inputs(&owner, true, rows.len(), 0, inputs(&rows))
                .unwrap();
            assert_eq!(p.priorities, vec![key(-1), key(9)]);
        }
        for _ in 0..500 {
            let p = cache
                .prepare_inputs(&owner, true, rows.len(), 0, inputs(&rows))
                .unwrap();
            assert_eq!(
                selected_indices(p.index.as_ref(), DrawKind::Line, key(9), 1).collect::<Vec<_>>(),
                vec![0]
            );
        }
        assert_eq!(cache.builds.get(), 1);
        assert_eq!(cache.hits.get(), 500);
        assert!(cache.plan.borrow().as_ref().unwrap().charge <= CAP);
    }
    #[test]
    fn same_length_changed_ranges_sources_text_and_owner_are_cold() {
        let cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        let owner = Arc::new(());
        let mut rows = [
            row(2, key(1), 0, 0, 6, Some(1)),
            row(4, key(2), 0, 0, 6, Some(2)),
        ];
        drop(
            cache
                .prepare_inputs(&owner, true, 2, 0, inputs(&rows))
                .unwrap(),
        );
        rows[0].start = 6;
        rows[0].end = 12;
        drop(
            cache
                .prepare_inputs(&owner, true, 2, 0, inputs(&rows))
                .unwrap(),
        );
        rows[0].source = Some(8);
        drop(
            cache
                .prepare_inputs(&owner, true, 2, 0, inputs(&rows))
                .unwrap(),
        );
        rows[1].extra = [2, 8, 9, 100, 101, 0];
        drop(
            cache
                .prepare_inputs(&owner, true, 2, 0, inputs(&rows))
                .unwrap(),
        );
        rows.swap(0, 1);
        drop(
            cache
                .prepare_inputs(&owner, true, 2, 0, inputs(&rows))
                .unwrap(),
        );
        drop(
            cache
                .prepare_inputs(&Arc::new(()), true, 2, 0, inputs(&rows))
                .unwrap(),
        );
        assert_eq!(cache.builds.get(), 6);
        assert_eq!(cache.hits.get(), 0);
    }
    #[test]
    fn duplicate_symbol_batches_keep_first_original_position_and_exact_priorities() {
        let cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        let rows = [
            row(3, key(8), 0, 0, 6, Some(7)),
            row(6, key(8), 0, 0, 6, None),
            row(6, key(8), 1, 0, 6, None),
            row(5, key(-10), 0, 0, 6, None),
        ];
        let owner = Arc::new(());
        let p = cache
            .prepare_inputs(&owner, true, 4, 2, inputs(&rows))
            .unwrap();
        assert_eq!(p.symbol((key(8).0, 8, 0, 6)), Some(0));
        assert_eq!(p.priorities, vec![key(8)]); // raster visibility is never cached.
        assert_eq!(
            selected_indices(p.index.as_ref(), DrawKind::Raster, key(-10), 1).collect::<Vec<_>>(),
            vec![0]
        );
    }
    #[test]
    fn cold_fork_and_cap_decline_cannot_retain_or_publish_old_plan() {
        let cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        let owner = Arc::new(());
        let rows = [row(2, key(1), 0, 0, 6, None)];
        drop(
            cache
                .prepare_inputs(&owner, true, 1, 0, inputs(&rows))
                .unwrap(),
        );
        let fork = cache.fork_cold();
        assert!(fork.plan.borrow().is_none());
        let count = CAP / std::mem::size_of::<Stored>() + 1;
        assert!(cache
            .prepare_inputs(&owner, true, count, 0, inputs(&[]))
            .is_none());
        assert!(cache.plan.borrow().is_none());
        assert_eq!(cache.declines.get(), 1);
        for flag in [
            None,
            Some(std::ffi::OsStr::new("0")),
            Some(std::ffi::OsStr::new("invalid")),
        ] {
            let off = Cache::new(flag);
            assert!(off
                .prepare_inputs(&owner, true, 1, 0, inputs(&rows))
                .is_none());
            assert!(off.plan.borrow().is_none());
            assert_eq!(off.builds.get(), 0);
        }
    }
    #[test]
    fn all_kinds_index_matches_original_linear_priority_order() {
        let owner = Arc::new(());
        let cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        let mut rows = Vec::new();
        for kind in 0..6 {
            for (i, priority) in [9, -1, 9, 0, -1].into_iter().enumerate() {
                rows.push(row(kind, key(priority), i, i * 6, i * 6 + 6, Some(i)));
            }
        }
        let p = cache
            .prepare_inputs(&owner, true, rows.len(), 0, inputs(&rows))
            .unwrap();
        for (kind, draw) in [
            (0, DrawKind::Area),
            (1, DrawKind::Pattern),
            (2, DrawKind::Line),
            (3, DrawKind::Symbol),
            (4, DrawKind::Text),
            (5, DrawKind::Raster),
        ] {
            for priority in [-1, 0, 9, 99] {
                let expected: Vec<_> = rows
                    .iter()
                    .filter(|r| r.kind == kind && r.key == key(priority))
                    .map(|r| r.ordinal)
                    .collect();
                assert_eq!(
                    selected_indices(p.index.as_ref(), draw, key(priority), 5).collect::<Vec<_>>(),
                    expected
                );
            }
        }
        assert_eq!(p.priorities, vec![key(-1), key(0), key(9)]);
    }
}

#[cfg(test)]
mod replacement_controls {
    use super::*;
    #[test]
    fn pattern_name_and_index_policy_replacement_and_empty_scene() {
        let owner = Arc::new(());
        let cache = Cache::new(Some(std::ffi::OsStr::new("1")));
        let key = (
            CompositionPlane::new(
                CompositionStage::Chart,
                std::num::NonZeroI32::new(1).unwrap(),
            ),
            5,
        );
        let data = row(1, key, 0, 0, 6, Some(2));
        drop(
            cache
                .prepare_inputs(
                    &owner,
                    true,
                    1,
                    0,
                    [Input {
                        row: data,
                        name: "PATTERN_A",
                    }]
                    .into_iter(),
                )
                .unwrap(),
        );
        drop(
            cache
                .prepare_inputs(
                    &owner,
                    true,
                    1,
                    0,
                    [Input {
                        row: data,
                        name: "PATTERN_B",
                    }]
                    .into_iter(),
                )
                .unwrap(),
        );
        drop(
            cache
                .prepare_inputs(
                    &owner,
                    false,
                    1,
                    0,
                    [Input {
                        row: data,
                        name: "PATTERN_B",
                    }]
                    .into_iter(),
                )
                .unwrap(),
        );
        assert_eq!(cache.builds.get(), 3);
        let empty = cache
            .prepare_inputs(&owner, true, 0, 0, std::iter::empty())
            .unwrap();
        assert!(empty.priorities.is_empty());
        assert!(empty.rows.is_empty());
    }
}
