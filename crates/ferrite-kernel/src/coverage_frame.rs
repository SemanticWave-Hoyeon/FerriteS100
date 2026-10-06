//! One immutable coverage frame shared by rendering and picking. The product
//! adapter supplies its selected, device-projected inventory; no geographic
//! projection, product scale policy or GPU API is assumed here.
use crate::coverage_raster::{rasterize_selected_dataset_masks, PixelMask};
use crate::coverage_rendering::{coverage_render_policy, CoverageRenderPolicy, InstructionOrigin};
use crate::coverage_selection::{assign_masks, CoverageFootprint, Region, Selection};
use anyhow::{ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCoverageDecision {
    Unclipped,
    ClipDataset(usize),
    Hidden,
}
/// Exemption is explicit, for example a user route overlay. Missing product
/// identity must never silently become an exemption.
#[derive(Debug, Clone, Copy)]
pub enum CoverageSource {
    Exempt,
    Dataset {
        dataset_id: usize,
        origin: InstructionOrigin,
    },
}
#[derive(Debug)]
struct SelectedDataset {
    obscuring: Region,
    visible: PixelMask,
}
#[derive(Debug)]
pub struct CoverageFrame {
    known_datasets: BTreeSet<usize>,
    selected: BTreeMap<usize, SelectedDataset>,
}
impl CoverageFrame {
    /// Pixel budget covers construction peaks in the rasterizer. Region and
    /// metadata storage and any GPU copy require separate caller accounting.
    pub fn new(
        inventory: &[CoverageFootprint],
        selection: &Selection,
        viewport: &Region,
        extent: [u32; 2],
        pixel_budget: usize,
    ) -> Result<Self> {
        let mut identities = BTreeSet::new();
        let mut known_datasets = BTreeSet::new();
        for item in inventory {
            item.scales.validate()?;
            ensure!(
                identities.insert((item.dataset_id, item.coverage_id)),
                "Duplicate coverage identity"
            );
            known_datasets.insert(item.dataset_id);
        }
        let mut selected_indices = BTreeSet::new();
        for item in &selection.coverages {
            ensure!(
                item.inventory_index < inventory.len(),
                "Selected coverage outside inventory"
            );
            ensure!(
                selected_indices.insert(item.inventory_index),
                "Repeated selected coverage"
            );
        }
        // The view supplies its actual projection domain. Uncovered alone is
        // not a viewport: it may be empty even though the whole view is covered.
        let regions = assign_masks(inventory, selection, viewport)?;
        let masks = rasterize_selected_dataset_masks(inventory, selection, extent, pixel_budget)?;
        let mut selected = BTreeMap::new();
        for mask in masks {
            let obscuring = regions
                .dataset_obscuring
                .get(&mask.dataset_id)
                .context("Missing selected dataset obscuring region")?
                .clone();
            selected.insert(
                mask.dataset_id,
                SelectedDataset {
                    obscuring,
                    visible: mask.mask,
                },
            );
        }
        Ok(Self {
            known_datasets,
            selected,
        })
    }
    pub fn decision(&self, source: CoverageSource) -> Result<FrameCoverageDecision> {
        let CoverageSource::Dataset { dataset_id, origin } = source else {
            return Ok(FrameCoverageDecision::Unclipped);
        };
        ensure!(
            self.known_datasets.contains(&dataset_id),
            "Unknown coverage dataset {dataset_id}"
        );
        let Some(dataset) = self.selected.get(&dataset_id) else {
            return Ok(FrameCoverageDecision::Hidden);
        };
        Ok(match coverage_render_policy(&dataset.obscuring, origin)? {
            CoverageRenderPolicy::HidePoint => FrameCoverageDecision::Hidden,
            CoverageRenderPolicy::RenderPointUnclipped => FrameCoverageDecision::Unclipped,
            CoverageRenderPolicy::ClipNonPoint => FrameCoverageDecision::ClipDataset(dataset_id),
        })
    }
    pub fn mask(&self, dataset_id: usize) -> Option<&PixelMask> {
        self.selected.get(&dataset_id).map(|d| &d.visible)
    }
    /// Test the same device pixel accepted by the fragment mask. Point-origin
    /// instructions outside an obscuring region retain their whole extent.
    pub fn accepts_fragment(
        &self,
        decision: FrameCoverageDecision,
        point: [f64; 2],
    ) -> Result<bool> {
        ensure!(
            point.iter().all(|p| p.is_finite()),
            "Non-finite coverage fragment"
        );
        Ok(match decision {
            FrameCoverageDecision::Hidden => false,
            FrameCoverageDecision::Unclipped => true,
            FrameCoverageDecision::ClipDataset(id) => {
                let mask = self.mask(id).context("No mask for clipped dataset")?;
                point.iter().all(|p| *p >= 0. && *p < u32::MAX as f64)
                    && mask.contains_pixel(point[0].floor() as u32, point[1].floor() as u32)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage_selection::SelectedCoverage;
    use crate::scale_policy::CoverageScaleRange;
    fn square(x: f64, y: f64, w: f64) -> Region {
        Region::from_rings(
            &[[x, y], [x + w, y], [x + w, y + w], [x, y + w], [x, y]],
            &[],
        )
        .unwrap()
    }
    fn footprint(id: usize, min: u32, region: Region) -> CoverageFootprint {
        CoverageFootprint {
            dataset_id: id,
            coverage_id: id as i64,
            region,
            scales: CoverageScaleRange {
                minimum_denominator: Some(min),
                optimum_denominator: min / 2,
                maximum_denominator: min / 4,
            },
        }
    }
    fn source(id: usize, origin: InstructionOrigin) -> CoverageSource {
        CoverageSource::Dataset {
            dataset_id: id,
            origin,
        }
    }
    #[test]
    fn drawing_and_picking_share_selection_holes_and_point_extent_rules() {
        let finer = Region::from_rings(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
        .unwrap();
        let inventory = vec![
            footprint(0, 180000, square(0., 0., 10.)),
            footprint(1, 45000, finer),
            footprint(2, 180000, square(0., 0., 10.)),
        ];
        let selection = Selection {
            display_band: 10,
            coverages: vec![
                SelectedCoverage {
                    inventory_index: 0,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                },
                SelectedCoverage {
                    inventory_index: 1,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                },
            ],
            uncovered: Region::from_polygons(vec![]).unwrap(),
        };
        let frame =
            CoverageFrame::new(&inventory, &selection, &square(0., 0., 12.), [12, 12], 4096)
                .unwrap();
        let coarse = frame
            .decision(source(0, InstructionOrigin::NonPoint))
            .unwrap();
        assert_eq!(coarse, FrameCoverageDecision::ClipDataset(0));
        assert!(!frame.accepts_fragment(coarse, [1.5, 1.5]).unwrap());
        assert!(frame.accepts_fragment(coarse, [5.5, 5.5]).unwrap());
        assert!(!frame.accepts_fragment(coarse, [11.5, 1.5]).unwrap());
        assert_eq!(
            frame
                .decision(source(0, InstructionOrigin::Point([4., 5.])))
                .unwrap(),
            FrameCoverageDecision::Hidden
        );
        let point = frame
            .decision(source(0, InstructionOrigin::Point([5., 5.])))
            .unwrap();
        assert_eq!(point, FrameCoverageDecision::Unclipped);
        assert!(frame.accepts_fragment(point, [1.5, 1.5]).unwrap());
        assert!(frame.accepts_fragment(point, [11.5, 1.5]).unwrap());
        assert_eq!(
            frame
                .decision(source(2, InstructionOrigin::NonPoint))
                .unwrap(),
            FrameCoverageDecision::Hidden
        );
        assert!(frame
            .decision(source(99, InstructionOrigin::NonPoint))
            .is_err());
        assert_eq!(
            frame.decision(CoverageSource::Exempt).unwrap(),
            FrameCoverageDecision::Unclipped
        );
        assert!(frame.accepts_fragment(coarse, [f64::NAN, 1.]).is_err());
    }
    #[test]
    fn invalid_selection_and_exhausted_budget_cannot_enable_coverage_bypass() {
        let inventory = vec![footprint(0, 180000, square(0., 0., 10.))];
        let mut selection = Selection {
            display_band: 10,
            coverages: vec![SelectedCoverage {
                inventory_index: 0,
                selection_band: 10,
                selected_to_fill_gap: false,
            }],
            uncovered: Region::from_polygons(vec![]).unwrap(),
        };
        assert!(
            CoverageFrame::new(&inventory, &selection, &square(0., 0., 10.), [10, 10], 199)
                .is_err()
        );
        selection.coverages.push(selection.coverages[0]);
        assert!(
            CoverageFrame::new(&inventory, &selection, &square(0., 0., 10.), [10, 10], 4096)
                .is_err()
        );
        selection.coverages.clear();
        let frame =
            CoverageFrame::new(&inventory, &selection, &square(0., 0., 10.), [10, 10], 0).unwrap();
        assert_eq!(
            frame
                .decision(source(0, InstructionOrigin::NonPoint))
                .unwrap(),
            FrameCoverageDecision::Hidden
        );
    }
}
