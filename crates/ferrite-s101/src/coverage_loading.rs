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
    let band = display_scale_band(denominator)?;
    let mut bands = inventory
        .iter()
        .map(|c| c.scales.scale_bands())
        .collect::<Result<Vec<_>>>()?;
    let mut eligible = inventory.to_vec();
    let mut retained = Vec::new();
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
        eligible[index].region = region;
        if !eligible[index].region.is_empty() {
            bands[index] |= 1 << (band - 1);
            retained.push(index);
        }
    }
    let selection = select_with_band_membership(&eligible, denominator, viewport, &bands)?;
    let masks = assign_masks(&eligible, &selection, viewport)?;
    Ok(S101DisplayPlan {
        eligible_inventory: eligible,
        selection,
        masks,
        minimum_retention: retained,
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
