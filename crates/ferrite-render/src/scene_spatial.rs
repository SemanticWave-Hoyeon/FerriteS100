//! Retained geographic area envelopes. Keep unsupported/invalid commands on
//! their original path; exact clipping and topology validation remain authoritative.
use crate::{AreaFillType, DrawingInstruction};
use ferrite_kernel::spatial_hierarchy::{SpatialBounds, SpatialHierarchy};
#[derive(Debug, Default)]
pub struct SceneSpatialIndex {
    pub areas: SpatialHierarchy<2>,
}
impl SceneSpatialIndex {
    pub fn compile(instructions: &[DrawingInstruction]) -> Self {
        Self::compile_with_limits(instructions, 131072, 16 * 1024 * 1024)
    }

    // Entries not admitted to the index remain on the original rendering path.
    // A failed allocation/budget check abandons the entire optimization, not data.
    fn compile_with_limits(
        instructions: &[DrawingInstruction],
        max_entries: usize,
        max_owned_bytes: usize,
    ) -> Self {
        let mut entries = Vec::new();
        for (id, item) in instructions.iter().enumerate() {
            if entries.len() >= max_entries {
                break;
            }
            let DrawingInstruction::Area(area) = item else {
                continue;
            };
            if !matches!(
                area.fill,
                AreaFillType::Solid(_)
                    | AreaFillType::Pattern { .. }
                    | AreaFillType::HatchFill { .. }
            ) || area.exterior.is_empty()
            {
                continue;
            }
            let mut b = SpatialBounds {
                min: [f64::INFINITY; 2],
                max: [f64::NEG_INFINITY; 2],
            };
            let mut valid = true;
            for p in area.exterior.iter().chain(area.interiors.iter().flatten()) {
                if !p.x.is_finite() || !p.y.is_finite() {
                    valid = false;
                    break;
                }
                b.min[0] = b.min[0].min(p.x);
                b.min[1] = b.min[1].min(p.y);
                b.max[0] = b.max[0].max(p.x);
                b.max[1] = b.max[1].max(p.y);
            }
            if valid && b.valid() {
                if entries.len() == entries.capacity() {
                    let target = entries
                        .capacity()
                        .max(32)
                        .saturating_mul(2)
                        .min(max_entries);
                    let bytes =
                        target.checked_mul(std::mem::size_of::<(usize, SpatialBounds<2>)>());
                    if bytes.is_none_or(|n| n > max_owned_bytes)
                        || entries.try_reserve_exact(target - entries.len()).is_err()
                        || entries
                            .capacity()
                            .checked_mul(std::mem::size_of::<(usize, SpatialBounds<2>)>())
                            .is_none_or(|n| n > max_owned_bytes)
                    {
                        return Self::default();
                    }
                }
                entries.push((id, b));
            }
        }
        Self {
            areas: SpatialHierarchy::build_bounded(entries, max_entries, max_owned_bytes)
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod bounded_scene_tests {
    use super::*;
    use crate::{AreaInstruction, WorldPoint};
    fn areas(n: usize) -> Vec<DrawingInstruction> {
        (0..n)
            .map(|i| {
                let x = i as f64;
                DrawingInstruction::Area(AreaInstruction::new(vec![
                    WorldPoint::new(x, 0.),
                    WorldPoint::new(x + 1., 0.),
                    WorldPoint::new(x, 1.),
                ]))
            })
            .collect()
    }
    #[test]
    fn whole_budget_decline_keeps_all_instructions_on_the_original_path() {
        let source = areas(17);
        let declined = SceneSpatialIndex::compile_with_limits(&source, 17, 0);
        assert_eq!(declined.areas.ids().count(), 0);
        assert_eq!(source.len(), 17);
        // Renderer initializes all candidates true and only clears indexed IDs.
        let mut admitted = vec![true; source.len()];
        for id in declined.areas.ids() {
            admitted[id] = false;
        }
        assert!(admitted.iter().all(|x| *x));
    }
    #[test]
    fn retained_capacity_is_bounded_and_unindexed_ordinals_remain_original() {
        let source = areas(33);
        let index = SceneSpatialIndex::compile_with_limits(&source, 17, 16384);
        assert!(index.areas.retained_bytes() <= 16384);
        let mut ids: Vec<_> = index.areas.ids().collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..17).collect::<Vec<_>>());
        let mut candidates = vec![true; source.len()];
        for id in index.areas.ids() {
            candidates[id] = false;
        }
        index.areas.query(
            |b| b.max[0] >= 8. && b.min[0] <= 9.,
            |id| candidates[id] = true,
        );
        assert!(candidates[17..].iter().all(|x| *x));
        assert_eq!(source.len(), 33);
    }
    #[test]
    fn builder_budget_failure_discards_the_whole_index_not_a_partial_tree() {
        let source = areas(9);
        // Captured entries fit, nodes do not. No indexed IDs may escape.
        let index = SceneSpatialIndex::compile_with_limits(
            &source,
            9,
            9 * std::mem::size_of::<(usize, SpatialBounds<2>)>(),
        );
        assert_eq!(index.areas.ids().count(), 0);
        assert_eq!(source.len(), 9);
    }
}
