//! One immutable coverage frame shared by rendering and picking. The product
//! adapter supplies its selected, device-projected inventory; no geographic
//! projection, product scale policy or GPU API is assumed here.
use crate::coverage_raster::{rasterize_selected_dataset_masks, PixelMask};
use crate::coverage_rendering::{coverage_render_policy, CoverageRenderPolicy, InstructionOrigin};
use crate::coverage_selection::{assign_masks, CoverageFootprint, Region, Selection};
use crate::scale_policy::{CoverageScaleRange, OverscaleState};
use anyhow::{ensure, Context, Result};
use geo::CoordsIter;
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
/// Annotation rights retain the original coverage owner, not a whole-dataset mask.
#[derive(Debug)]
pub struct CoverageScaleAnnotation {
    pub dataset_id: usize,
    pub coverage_id: i64,
    pub scales: CoverageScaleRange,
    pub selection_band: Option<u8>,
    pub selected_to_fill_gap: bool,
    pub state: OverscaleState,
    /// Exact visible footprint, including holes and finer-data obscuring.
    pub visible: Region,
    source_region: Region,
}
#[derive(Debug)]
pub struct FrameScaleAnnotations {
    pub viewing_denominator: f64,
    /// Screen centre is the reference while no own-ship position is supplied.
    pub reference_point: [f64; 2],
    pub coverages: Vec<CoverageScaleAnnotation>,
    pub retained_logical_bytes: usize,
    reference_index: Option<usize>,
    extent: [u32; 2],
}
impl FrameScaleAnnotations {
    fn index_at(&self, point: [f64; 2]) -> Result<Option<usize>> {
        let sampled = self
            .coverages
            .iter()
            .map(|row| {
                crate::coverage_raster::sample_region_pixel(&row.source_region, self.extent, point)
            })
            .collect::<Result<Vec<_>>>()?;
        // Exactly rasterize_selected_masks' obscuring rights: lower minimum
        // denominator obscures every coarser footprint at this physical pixel.
        let finest = self
            .coverages
            .iter()
            .zip(&sampled)
            .filter(|(_, accepted)| **accepted)
            .map(|(row, _)| row.scales.minimum_denominator.unwrap_or(u32::MAX))
            .min();
        Ok(self
            .coverages
            .iter()
            .enumerate()
            .filter(|(index, row)| {
                sampled[*index]
                    && Some(row.scales.minimum_denominator.unwrap_or(u32::MAX)) == finest
            })
            .min_by_key(|(_, row)| {
                (
                    row.scales.optimum_denominator,
                    row.dataset_id,
                    row.coverage_id,
                )
            })
            .map(|(index, _)| index))
    }
    pub fn at(&self, point: [f64; 2]) -> Result<Option<&CoverageScaleAnnotation>> {
        Ok(self
            .index_at(point)?
            .and_then(|index| self.coverages.get(index)))
    }
    /// Physical extent retained by the same raster mask/annotation owner.
    pub fn physical_extent(&self) -> [u32; 2] {
        self.extent
    }
    /// S-98 12.3.3 annotation only. Reuse the original centre/half-open
    /// rasterization and strictly finer minimum subtraction, independently of
    /// dataset-wide masks, then OR eligible owners to avoid duplicate alpha.
    /// `None` means no eligible annotation; it never means unclipped drawing.
    pub fn overscale_pattern_mask(&self, byte_budget: usize) -> Result<Option<PixelMask>> {
        let eligible = |row: &CoverageScaleAnnotation| {
            row.state.pattern_required
                && self.viewing_denominator < f64::from(row.scales.maximum_denominator)
        };
        if !self.coverages.iter().any(eligible) {
            return Ok(None);
        }
        let mut raw = Vec::with_capacity(self.coverages.len());
        let mut bytes = 0usize;
        for row in &self.coverages {
            let mask = crate::coverage_raster::rasterize(
                &row.source_region,
                self.extent,
                byte_budget.saturating_sub(bytes),
            )?;
            bytes = bytes
                .checked_add(mask.pixels().len())
                .context("Overscale raw mask storage overflow")?;
            ensure!(bytes <= byte_budget, "Overscale mask budget exceeded");
            raw.push(mask);
        }
        let mut visible = Vec::new();
        for (index, row) in self
            .coverages
            .iter()
            .enumerate()
            .filter(|(_, row)| eligible(row))
        {
            let length = raw[index].pixels().len();
            ensure!(
                length <= byte_budget.saturating_sub(bytes),
                "Overscale visible mask budget exceeded"
            );
            let mut mask = raw[index].clone();
            bytes += length;
            let minimum = row.scales.minimum_denominator.unwrap_or(u32::MAX);
            for (other, obscuring) in self.coverages.iter().zip(&raw) {
                if other.scales.minimum_denominator.unwrap_or(u32::MAX) < minimum {
                    mask.subtract(obscuring);
                }
            }
            visible.push(mask);
        }
        Ok(Some(PixelMask::union_masks(
            &visible,
            byte_budget.saturating_sub(bytes),
        )?))
    }
    /// Disjoint immutable resource-owner masks. Union is identical to the
    /// original global annotation mask; underlying ENC draw/pick rights unchanged.
    pub fn overscale_pattern_masks_by_dataset(
        &self,
        byte_budget: usize,
    ) -> Result<Vec<(usize, PixelMask)>> {
        let eligible = |row: &CoverageScaleAnnotation| {
            row.state.pattern_required
                && self.viewing_denominator < f64::from(row.scales.maximum_denominator)
        };
        if !self.coverages.iter().any(eligible) {
            return Ok(Vec::new());
        }
        let mut raw = Vec::with_capacity(self.coverages.len());
        let mut bytes = 0usize;
        for row in &self.coverages {
            let mask = crate::coverage_raster::rasterize(
                &row.source_region,
                self.extent,
                byte_budget.saturating_sub(bytes),
            )?;
            bytes = bytes
                .checked_add(mask.pixels().len())
                .context("Overscale raw mask storage overflow")?;
            ensure!(bytes <= byte_budget, "Overscale mask budget exceeded");
            raw.push(mask);
        }
        let mut visible: Vec<(usize, PixelMask)> = Vec::new();
        let mut order: Vec<_> = self
            .coverages
            .iter()
            .enumerate()
            .filter(|(_, row)| eligible(row))
            .collect();
        order.sort_by_key(|(_, row)| {
            (
                row.scales.minimum_denominator.unwrap_or(u32::MAX),
                row.scales.optimum_denominator,
                row.dataset_id,
                row.coverage_id,
            )
        });
        for (index, row) in order {
            let length = raw[index].pixels().len();
            ensure!(
                length <= byte_budget.saturating_sub(bytes),
                "Overscale visible mask budget exceeded"
            );
            let mut mask = raw[index].clone();
            bytes += length;
            let minimum = row.scales.minimum_denominator.unwrap_or(u32::MAX);
            for (other, obscuring) in self.coverages.iter().zip(&raw) {
                if other.scales.minimum_denominator.unwrap_or(u32::MAX) < minimum {
                    mask.subtract(obscuring);
                }
            }
            // Same-minimum overlap receives one annotation resource owner;
            // deterministic existing reference tie fields, not iteration/hash order.
            for (_, prior) in &visible {
                mask.subtract(prior);
            }
            if mask.pixels().iter().any(|p| *p != 0) {
                visible.push((row.dataset_id, mask));
            }
        }
        Ok(visible)
    }
    pub fn reference(&self) -> Option<&CoverageScaleAnnotation> {
        self.reference_index
            .and_then(|index| self.coverages.get(index))
    }
}
#[derive(Debug)]
pub struct CoverageFrame {
    known_datasets: BTreeSet<usize>,
    selected: BTreeMap<usize, SelectedDataset>,
    scale_annotations: Option<FrameScaleAnnotations>,
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
        Self::build(inventory, selection, viewport, extent, pixel_budget, None)
    }
    /// Add device-frame scale indications without changing draw/pick decisions.
    /// The scale and reference belong to this same immutable view owner.
    pub fn new_with_scale_annotations(
        inventory: &[CoverageFootprint],
        selection: &Selection,
        viewport: &Region,
        extent: [u32; 2],
        pixel_budget: usize,
        viewing_denominator: f64,
        reference_point: [f64; 2],
    ) -> Result<Self> {
        ensure!(
            viewing_denominator.is_finite() && viewing_denominator > 0.,
            "Invalid annotation denominator"
        );
        ensure!(
            reference_point.iter().all(|v| v.is_finite()),
            "Invalid annotation reference"
        );
        Self::build(
            inventory,
            selection,
            viewport,
            extent,
            pixel_budget,
            Some((viewing_denominator, reference_point)),
        )
    }
    fn build(
        inventory: &[CoverageFootprint],
        selection: &Selection,
        viewport: &Region,
        extent: [u32; 2],
        pixel_budget: usize,
        annotation: Option<(f64, [f64; 2])>,
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
        let scale_annotations = if let Some((viewing_denominator, reference_point)) = annotation {
            ensure!(
                regions.coverages.len() <= 65_536,
                "Scale annotation owner budget exceeded"
            );
            let chosen: BTreeMap<_, _> = selection
                .coverages
                .iter()
                .map(|row| (row.inventory_index, row))
                .collect();
            // S-98 2.0.0 §12.3.3: when both portions are enlarged, the
            // larger-scale portion keeps its factor indication without OVERSC01.
            // Preserve the literal Appendix E stage-3b history separately. Empty
            // initial search bands do not make the primary visible data a smaller
            // scale gap fill. Only actually selected coverage establishes this
            // band; offscreen inventory cannot grant annotation rights.
            let primary_selected_band = selection
                .coverages
                .iter()
                .map(|row| row.selection_band)
                .max();
            let mut coverages = Vec::with_capacity(regions.coverages.len());
            let mut retained_logical_bytes = 0usize;
            for mask in &regions.coverages {
                let source = &inventory[mask.inventory_index];
                let selected = chosen.get(&mask.inventory_index);
                let selected_to_fill_gap = selected.is_some_and(|s| s.selected_to_fill_gap);
                let smaller_scale_gap = selected.is_some_and(|row| {
                    row.selected_to_fill_gap
                        && primary_selected_band.is_some_and(|primary| row.selection_band < primary)
                });
                let points = mask
                    .visible
                    .polygons()
                    .iter()
                    .chain(source.region.polygons())
                    .map(|p| p.coords_iter().count())
                    .sum::<usize>();
                retained_logical_bytes = retained_logical_bytes
                    .checked_add(
                        points
                            .checked_mul(16)
                            .context("Annotation coordinate overflow")?,
                    )
                    .and_then(|n| n.checked_add(std::mem::size_of::<CoverageScaleAnnotation>()))
                    .context("Annotation storage overflow")?;
                ensure!(
                    retained_logical_bytes <= 32 * 1024 * 1024,
                    "Scale annotation region budget exceeded"
                );
                coverages.push(CoverageScaleAnnotation {
                    dataset_id: source.dataset_id,
                    coverage_id: source.coverage_id,
                    scales: source.scales,
                    selection_band: selected.map(|s| s.selection_band),
                    selected_to_fill_gap,
                    state: source
                        .scales
                        .overscale(viewing_denominator, smaller_scale_gap)?,
                    visible: mask.visible.clone(),
                    source_region: source.region.clone(),
                });
            }
            let mut annotations = FrameScaleAnnotations {
                viewing_denominator,
                reference_point,
                coverages,
                retained_logical_bytes,
                reference_index: None,
                extent,
            };
            annotations.reference_index = annotations.index_at(reference_point)?;
            Some(annotations)
        } else {
            None
        };
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
            scale_annotations,
        })
    }
    pub fn scale_annotations(&self) -> Option<&FrameScaleAnnotations> {
        self.scale_annotations.as_ref()
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

#[cfg(test)]
mod scale_annotation_tests {
    use super::*;
    use crate::coverage_selection::SelectedCoverage;
    fn rect(x: f64, width: f64) -> Region {
        Region::from_rings(
            &[
                [x, 0.],
                [x + width, 0.],
                [x + width, 10.],
                [x, 10.],
                [x, 0.],
            ],
            &[],
        )
        .unwrap()
    }
    fn footprint(owner: usize, id: i64, region: Region) -> CoverageFootprint {
        CoverageFootprint {
            dataset_id: owner,
            coverage_id: id,
            region,
            scales: CoverageScaleRange {
                minimum_denominator: Some(45_000),
                optimum_denominator: 22_000,
                maximum_denominator: 12_000,
            },
        }
    }
    fn selected(index: usize, gap: bool) -> SelectedCoverage {
        SelectedCoverage {
            inventory_index: index,
            selection_band: if gap { 6 } else { 7 },
            selected_to_fill_gap: gap,
        }
    }
    fn make(
        inventory: &[CoverageFootprint],
        selected: Vec<SelectedCoverage>,
        scale: f64,
    ) -> CoverageFrame {
        CoverageFrame::new_with_scale_annotations(
            inventory,
            &Selection {
                display_band: 7,
                coverages: selected,
                uncovered: rect(0., 10.),
            },
            &rect(0., 10.),
            [10, 10],
            4096,
            scale,
            [5., 5.],
        )
        .unwrap()
    }
    #[test]
    fn pattern_mask_retains_gap_owner_only_and_strict_threshold() {
        let inv = [
            footprint(0, 10, rect(0., 5.)),
            footprint(0, 11, rect(5., 5.)),
        ];
        let f = make(&inv, vec![selected(0, true), selected(1, false)], 11999.);
        let mask = f
            .scale_annotations()
            .unwrap()
            .overscale_pattern_mask(4096)
            .unwrap()
            .unwrap();
        assert!(mask.contains_pixel(2, 2));
        assert!(!mask.contains_pixel(7, 2));
        assert!(
            make(&inv, vec![selected(0, true), selected(1, false)], 12000.)
                .scale_annotations()
                .unwrap()
                .overscale_pattern_mask(0)
                .unwrap()
                .is_none()
        );
        assert!(make(&inv, vec![selected(0, false)], 11000.)
            .scale_annotations()
            .unwrap()
            .overscale_pattern_mask(0)
            .unwrap()
            .is_none());
    }
    #[test]
    fn pattern_mask_holes_finer_obscuring_and_overlap_are_exact_r8() {
        let donut = Region::from_rings(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
        .unwrap();
        let mut fine = footprint(1, 20, rect(0., 3.));
        fine.scales.minimum_denominator = Some(40_000);
        let inv = [
            footprint(0, 10, donut.clone()),
            footprint(0, 11, donut),
            fine,
        ];
        let f = make(
            &inv,
            vec![selected(0, true), selected(1, true), selected(2, false)],
            11000.,
        );
        let a = f.scale_annotations().unwrap();
        let mask = a.overscale_pattern_mask(4096).unwrap().unwrap();
        assert!(!mask.contains_pixel(1, 2));
        assert!(!mask.contains_pixel(5, 5));
        assert!(mask.contains_pixel(7, 2));
        assert!(mask.pixels().iter().all(|v| *v == 0 || *v == 255));
        assert!(a.overscale_pattern_mask(1).is_err());
        // Annotation leaves original coverage draw/pick masks untouched.
        let original = f.mask(0).unwrap();
        assert!(original.contains_pixel(7, 2));
        assert!(!original.contains_pixel(1, 2));
    }
    #[test]
    fn strict_thresholds_and_gap_rights_do_not_spread_to_sibling_coverage() {
        let mut primary = footprint(1, 12, rect(0., 1.));
        primary.scales = CoverageScaleRange {
            minimum_denominator: Some(40_000),
            optimum_denominator: 20_000,
            maximum_denominator: 10_000,
        };
        let inv = [
            footprint(0, 10, rect(0., 5.)),
            footprint(0, 11, rect(5., 5.)),
            primary,
        ];
        for (scale, indicator, pattern) in [
            (22000., false, false),
            (21999., true, false),
            (12000., true, false),
            (11999., true, true),
        ] {
            let frame = make(&inv, vec![selected(0, true), selected(2, false)], scale);
            let a = frame.scale_annotations().unwrap();
            assert_eq!(a.coverages.len(), 3);
            assert_eq!(
                (
                    a.coverages[0].state.indicator_required,
                    a.coverages[0].state.pattern_required
                ),
                (indicator, pattern)
            );
            assert!(!a.coverages[1].state.pattern_required);
            assert_eq!(a.coverages[1].selection_band, None);
        }
        let frame = make(&inv, vec![selected(0, false)], 11999.);
        assert!(
            !frame.scale_annotations().unwrap().coverages[0]
                .state
                .pattern_required
        );
    }
    #[test]
    fn owner_partition_union_matches_legacy_with_holes_finer_and_same_min_overlap() {
        let donut = Region::from_rings(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
        .unwrap();
        let mut fine = footprint(2, 12, rect(0., 3.));
        fine.scales.minimum_denominator = Some(40000);
        let inv = [
            footprint(0, 10, donut),
            footprint(1, 11, rect(5., 5.)),
            fine,
        ];
        let f = make(
            &inv,
            vec![selected(0, true), selected(1, true), selected(2, false)],
            11000.,
        );
        let a = f.scale_annotations().unwrap();
        let partitions = a.overscale_pattern_masks_by_dataset(8192).unwrap();
        let masks: Vec<_> = partitions.iter().map(|(_, mask)| mask.clone()).collect();
        assert_eq!(
            PixelMask::union_masks(&masks, 8192).unwrap(),
            a.overscale_pattern_mask(8192).unwrap().unwrap()
        );
        for y in 0..10 {
            for x in 0..10 {
                assert!(
                    partitions
                        .iter()
                        .filter(|(_, mask)| mask.contains_pixel(x, y))
                        .count()
                        <= 1
                );
            }
        }
        assert!(partitions.iter().any(|(owner, _)| *owner == 0));
        assert!(partitions.iter().any(|(owner, _)| *owner == 1));
        assert!(!partitions.iter().any(|(owner, _)| *owner == 2));
        assert!(a.overscale_pattern_masks_by_dataset(1).is_err());
        let reversed = [inv[2].clone(), inv[1].clone(), inv[0].clone()];
        let reversed_frame = make(
            &reversed,
            vec![selected(0, false), selected(1, true), selected(2, true)],
            11000.,
        );
        assert_eq!(
            partitions,
            reversed_frame
                .scale_annotations()
                .unwrap()
                .overscale_pattern_masks_by_dataset(8192)
                .unwrap()
        );
    }
    #[test]
    fn owner_partition_keeps_strict_threshold_and_nongap_exclusion() {
        let inv = [footprint(0, 10, rect(0., 10.))];
        for (scale, gap) in [(12000., true), (11000., false)] {
            assert!(make(&inv, vec![selected(0, gap)], scale)
                .scale_annotations()
                .unwrap()
                .overscale_pattern_masks_by_dataset(8192)
                .unwrap()
                .is_empty());
        }
    }
    #[test]
    fn reference_uses_actual_visible_source_and_holes_without_inventing_factor() {
        let hole = Region::from_rings(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
        .unwrap();
        let inv = [footprint(0, 10, hole)];
        let frame = make(&inv, vec![selected(0, true)], 11000.);
        let a = frame.scale_annotations().unwrap();
        assert!(a.reference().is_none());
        assert!(a.at([11., 5.]).unwrap().is_none());
        assert!(a.at([f64::NAN, 5.]).is_err());
        let found = a.at([2., 2.]).unwrap().unwrap();
        assert_eq!((found.dataset_id, found.coverage_id), (0, 10));
        assert_eq!(found.state.factor, 2.);
    }
    #[test]
    fn annotation_does_not_change_source_fragment_masks() {
        let inv = [footprint(3, 99, rect(0., 10.))];
        let selection = Selection {
            display_band: 7,
            coverages: vec![selected(0, true)],
            uncovered: rect(0., 10.),
        };
        let plain = CoverageFrame::new(&inv, &selection, &rect(0., 10.), [10, 10], 4096).unwrap();
        let marked = make(&inv, selection.coverages, 11000.);
        assert_eq!(plain.mask(3), marked.mask(3));
        assert!(plain.scale_annotations().is_none());
        assert!(marked.scale_annotations().unwrap().retained_logical_bytes > 0);
    }
    #[test]
    fn shared_edge_reference_agrees_with_actual_raster_pixel_owner() {
        let mut right = footprint(1, 11, rect(5., 5.));
        right.scales = CoverageScaleRange {
            minimum_denominator: Some(90_000),
            optimum_denominator: 45_000,
            maximum_denominator: 22_000,
        };
        let inv = [footprint(0, 10, rect(0., 5.)), right];
        let frame = make(&inv, vec![selected(0, false), selected(1, true)], 11000.);
        let a = frame.scale_annotations().unwrap();
        let reference = a.reference().unwrap();
        assert_eq!(reference.dataset_id, 1);
        assert!(!frame.mask(0).unwrap().contains_pixel(5, 5));
        assert!(frame.mask(1).unwrap().contains_pixel(5, 5));
        for point in [[4.999, 5.], [5., 5.], [5.49, 5.]] {
            let row = a.at(point).unwrap().unwrap();
            assert!(frame
                .mask(row.dataset_id)
                .unwrap()
                .contains_pixel(point[0] as u32, point[1] as u32));
        }
    }
}
