//! S-101 2.0.0 4.6 retention rules applied to S-98 Appendix E loading.
//! Producer attributes remain unchanged; only eligible device regions and
//! band membership are adjusted at/beyond minimum display scale.
use anyhow::Result;
use ferrite_kernel::coverage_selection::{
    assign_masks, select_with_band_membership, CoverageFootprint, DisplayMasks, Region, Selection,
};
use ferrite_kernel::scale_policy::display_scale_band;
#[derive(Debug, Clone)]
pub struct S101DisplayPlan {
    pub eligible_inventory: Vec<CoverageFootprint>,
    pub selection: Selection,
    pub masks: DisplayMasks,
    pub minimum_retention: Vec<usize>,
}
pub fn display_plan(
    inventory: &[CoverageFootprint],
    denominator: f64,
    viewport: &Region,
) -> Result<S101DisplayPlan> {
    display_plan_with_retention(
        inventory,
        denominator,
        viewport,
        &retention(inventory, denominator),
    )
}

/// Every retention decision is `denominator >= t` for a minimum or optimum
/// denominator `t`, so the count of thresholds passed identifies the result.
pub fn retention_key(inventory: &[CoverageFootprint], denominator: f64) -> usize {
    let mut thresholds: Vec<u32> = inventory
        .iter()
        .flat_map(|c| {
            [
                c.scales.minimum_denominator,
                Some(c.scales.optimum_denominator),
            ]
        })
        .flatten()
        .collect();
    thresholds.sort_unstable();
    thresholds.dedup();
    thresholds
        .iter()
        .filter(|t| denominator >= **t as f64)
        .count()
}

/// 4.6 retention: the region each coverage keeps at/beyond its minimum display
/// scale. Only polygon differences, so it commutes with an affine projection
/// and can be computed once per `retention_key` in a camera-independent plane.
pub fn retention(inventory: &[CoverageFootprint], denominator: f64) -> Vec<(usize, Region)> {
    let mut out = Vec::new();
    for (index, coverage) in inventory.iter().enumerate() {
        let Some(minimum) = coverage.scales.minimum_denominator else {
            continue;
        };
        if denominator < minimum as f64 {
            continue;
        }
        // 4.6: keep larger-scale coverage through a gap until MSVS reaches the
        // next coarser optimum. Where no coarser data exists, keep it visible.
        // Clip this spatially: selecting one uncovered part must not restore
        // finer data over an adjoining region already supplied by coarser data.
        let mut region = coverage.region.clone();
        for other in inventory {
            if other.scales.optimum_denominator > coverage.scales.optimum_denominator
                && denominator >= other.scales.optimum_denominator as f64
            {
                region = region.difference(&other.region);
            }
        }
        out.push((index, region));
    }
    out
}

/// `retained` is `retention(inventory, denominator)`, possibly computed in
/// another affine-equivalent plane and mapped into the inventory's space.
pub fn display_plan_with_retention(
    inventory: &[CoverageFootprint],
    denominator: f64,
    viewport: &Region,
    retained: &[(usize, Region)],
) -> Result<S101DisplayPlan> {
    let band = display_scale_band(denominator)?;
    let mut bands = inventory
        .iter()
        .map(|c| c.scales.scale_bands())
        .collect::<Result<Vec<_>>>()?;
    let mut eligible = inventory.to_vec();
    let mut minimum_retention = Vec::new();
    for (index, region) in retained {
        anyhow::ensure!(*index < inventory.len(), "Retention outside inventory");
        eligible[*index].region = region.clone();
        if !region.is_empty() {
            bands[*index] |= 1 << (band - 1);
            minimum_retention.push(*index);
        }
    }
    let selection = select_with_band_membership(&eligible, denominator, viewport, &bands)?;
    let masks = assign_masks(&eligible, &selection, viewport)?;
    Ok(S101DisplayPlan {
        eligible_inventory: eligible,
        selection,
        masks,
        minimum_retention,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::scale_policy::CoverageScaleRange;
    fn rect(x: f64, w: f64) -> Region {
        Region::from_rings(
            &[[x, 0.], [x + w, 0.], [x + w, 10.], [x, 10.], [x, 0.]],
            &[],
        )
        .unwrap()
    }
    fn c(id: usize, min: u32, opt: u32, max: u32, region: Region) -> CoverageFootprint {
        CoverageFootprint {
            dataset_id: id,
            coverage_id: id as i64,
            scales: CoverageScaleRange {
                minimum_denominator: Some(min),
                optimum_denominator: opt,
                maximum_denominator: max,
            },
            region,
        }
    }
    /// Retention computed once per key in another affine plane and mapped back
    /// gives the same plan as computing it in the device space.
    #[test]
    fn retention_key_and_affine_plane_reproduce_device_plan() {
        let inv = vec![
            c(0, 180000, 90000, 45000, rect(0., 5.)),
            c(1, 45000, 12000, 6000, rect(0., 10.)),
            c(2, 700000, 350000, 180000, rect(2., 3.)),
        ];
        // Flipped, scaled and translated plane, like (longitude, northing).
        let to_plane = |p: [f64; 2]| [p[0] * 0.25 - 3., 7. - p[1] * 0.5];
        let to_device = |p: [f64; 2]| [(p[0] + 3.) * 4., (7. - p[1]) * 2.];
        let plane: Vec<_> = inv
            .iter()
            .map(|f| CoverageFootprint {
                region: f.region.map_affine(to_plane).unwrap(),
                ..f.clone()
            })
            .collect();
        let mut seen = std::collections::BTreeMap::new();
        for d in [
            5000., 6000., 12000., 44999., 45000., 60000., 90000., 179999., 180000., 350000.,
            700000., 900000.,
        ] {
            let direct = display_plan(&inv, d, &rect(0., 10.)).unwrap();
            let key = retention_key(&inv, d);
            let retained = seen
                .entry(key)
                .or_insert_with(|| retention(&plane, d))
                .iter()
                .map(|(i, r)| (*i, r.map_affine(to_device).unwrap()))
                .collect::<Vec<_>>();
            let cached = display_plan_with_retention(&inv, d, &rect(0., 10.), &retained).unwrap();
            assert_eq!(cached.minimum_retention, direct.minimum_retention, "{d}");
            assert_eq!(
                cached.selection.coverages, direct.selection.coverages,
                "{d}"
            );
            for (a, b) in cached
                .eligible_inventory
                .iter()
                .zip(&direct.eligible_inventory)
            {
                assert!((a.region.area() - b.region.area()).abs() < 1e-9, "{d}");
            }
            for (a, b) in cached.masks.coverages.iter().zip(&direct.masks.coverages) {
                assert!((a.visible.area() - b.visible.area()).abs() < 1e-9, "{d}");
            }
        }
        assert!(seen.len() > 3, "thresholds must produce distinct keys");
    }
    #[test]
    fn no_coarser_data_remains_visible_at_and_beyond_minimum() {
        let inv = vec![c(0, 180000, 90000, 45000, rect(0., 10.))];
        for d in [179999., 180000., 180001., 300000.] {
            let plan = display_plan(&inv, d, &rect(0., 10.)).unwrap();
            assert!(plan.selection.uncovered.is_empty());
            assert_eq!(plan.selection.coverages.len(), 1);
            assert_eq!(plan.eligible_inventory[0].scales, inv[0].scales);
        }
    }
    #[test]
    fn scale_gap_switches_at_coarser_optimum_and_retains_only_missing_regions() {
        let inv = vec![
            c(0, 180000, 90000, 45000, rect(0., 5.)),
            c(1, 45000, 12000, 6000, rect(0., 10.)),
        ];
        let gap = display_plan(&inv, 60000., &rect(0., 10.)).unwrap();
        assert_eq!(
            gap.selection
                .coverages
                .iter()
                .map(|s| s.inventory_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(gap.masks.coverages[0].visible.area(), 100.);
        for d in [90000., 300000.] {
            let plan = display_plan(&inv, d, &rect(0., 10.)).unwrap();
            assert!(plan.selection.uncovered.is_empty());
            assert_eq!(plan.eligible_inventory[1].region.area(), 50.);
            assert_eq!(
                plan.masks
                    .coverages
                    .iter()
                    .map(|m| m.visible.area())
                    .collect::<Vec<_>>(),
                vec![50., 50.]
            );
        }
        assert_eq!(inv[1].region.area(), 100.);
    }
    #[test]
    fn ordinary_band_selection_and_overscale_pattern_flags_stay_unchanged() {
        let inv = vec![
            c(0, 180000, 90000, 45000, rect(0., 10.)),
            c(1, 45000, 22000, 12000, rect(0., 5.)),
        ];
        let plan = display_plan(&inv, 10000., &rect(0., 10.)).unwrap();
        assert!(plan.minimum_retention.is_empty());
        assert!(plan
            .selection
            .coverages
            .iter()
            .all(|s| s.selected_to_fill_gap));
        assert!(plan
            .selection
            .coverages
            .iter()
            .all(|s| inv[s.inventory_index]
                .scales
                .overscale(10000., s.selected_to_fill_gap)
                .unwrap()
                .pattern_required));
        assert_eq!(plan.masks.coverages[0].visible.area(), 50.);
        assert_eq!(plan.masks.coverages[1].visible.area(), 50.);
    }
}
