//! S-98 Appendix E operates in a caller-supplied device projection. No product
//! encoding, geographic projection or renderer lives in this module.
use crate::scale_policy::{display_scale_band, CoverageScaleRange};
use anyhow::{ensure, Context, Result};
use geo::{Area, BooleanOps, BoundingRect, CoordsIter, MultiPolygon, Polygon, Validation};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// Validated planar region. Interiors/holes are preserved by polygon operations.
#[derive(Debug, Clone)]
pub struct Region(Arc<MultiPolygon<f64>>);
impl Region {
    pub fn from_polygons(polygons: Vec<Polygon<f64>>) -> Result<Self> {
        let mut result = MultiPolygon::<f64>(vec![]);
        for polygon in polygons {
            ensure!(
                polygon
                    .coords_iter()
                    .all(|p| p.x.is_finite() && p.y.is_finite()),
                "Non-finite projected coverage"
            );
            polygon
                .check_validation()
                .map_err(|e| anyhow::anyhow!("Invalid projected coverage: {e:?}"))?;
            let area = polygon.unsigned_area();
            ensure!(
                area.is_finite() && area > 0.,
                "Invalid projected coverage area"
            );
            // A dataset may contain touching/overlapping component surfaces;
            // normalize with set union, rather than parity across components.
            result = result.union(&polygon);
        }
        Ok(Self(Arc::new(result)))
    }
    /// Normalize a conforming projected triangle batch using one precision
    /// domain. Repeated pairwise overlays can round shared edges differently
    /// and open a narrow path from a hole to the exterior.
    pub(crate) fn from_polygon_batch(polygons: Vec<Polygon<f64>>) -> Result<Self> {
        use geo::{algorithm::orient::Direction, Orient};
        let mut oriented = Vec::with_capacity(polygons.len());
        for polygon in polygons {
            ensure!(
                polygon
                    .coords_iter()
                    .all(|p| p.x.is_finite() && p.y.is_finite()),
                "Non-finite projected coverage"
            );
            polygon
                .check_validation()
                .map_err(|e| anyhow::anyhow!("Invalid projected coverage: {e:?}"))?;
            let area = polygon.unsigned_area();
            ensure!(
                area.is_finite() && area > 0.,
                "Invalid projected coverage area"
            );
            oriented.push(polygon.orient(Direction::Default));
        }
        let result = geo::algorithm::unary_union(&oriented);
        for polygon in &result.0 {
            polygon
                .check_validation()
                .map_err(|e| anyhow::anyhow!("Invalid coverage union: {e:?}"))?;
            ensure!(
                polygon
                    .coords_iter()
                    .all(|p| p.x.is_finite() && p.y.is_finite()),
                "Non-finite coverage union"
            );
        }
        Ok(Self(Arc::new(result)))
    }
    /// Union already projected components in one precision domain, with a
    /// logical coordinate budget checked before copying and after overlay.
    pub fn union_projected(regions: &[Self], coordinate_budget: usize) -> Result<Self> {
        let count = regions
            .iter()
            .flat_map(|r| r.polygons())
            .try_fold(0usize, |n, p| n.checked_add(p.coords_iter().count()))
            .ok_or_else(|| anyhow::anyhow!("Coverage union coordinate overflow"))?;
        ensure!(
            count <= coordinate_budget,
            "Coverage union coordinate budget exceeded"
        );
        let region = Self::from_polygon_batch(
            regions
                .iter()
                .flat_map(|r| r.polygons().iter().cloned())
                .collect(),
        )?;
        let count = region
            .polygons()
            .iter()
            .try_fold(0usize, |n, p| n.checked_add(p.coords_iter().count()))
            .ok_or_else(|| anyhow::anyhow!("Coverage union coordinate overflow"))?;
        ensure!(
            count <= coordinate_budget,
            "Coverage union output coordinate budget exceeded"
        );
        Ok(region)
    }
    pub fn from_rings(exterior: &[[f64; 2]], holes: &[Vec<[f64; 2]>]) -> Result<Self> {
        let line = |p: &[[f64; 2]]| {
            geo::LineString::from(p.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>())
        };
        Self::from_polygons(vec![Polygon::new(
            line(exterior),
            holes.iter().map(|h| line(h)).collect(),
        )])
    }
    pub fn polygons(&self) -> &[Polygon<f64>] {
        &self.0 .0
    }
    pub fn area(&self) -> f64 {
        self.0.unsigned_area()
    }
    pub fn is_empty(&self) -> bool {
        self.0 .0.is_empty()
    }
    pub fn intersection(&self, other: &Self) -> Self {
        Self(Arc::new(self.0.as_ref().intersection(other.0.as_ref())))
    }
    pub fn difference(&self, other: &Self) -> Self {
        Self(Arc::new(self.0.as_ref().difference(other.0.as_ref())))
    }
    pub fn union(&self, other: &Self) -> Self {
        Self(Arc::new(self.0.as_ref().union(other.0.as_ref())))
    }
    pub fn overlaps_area(&self, other: &Self) -> bool {
        match (self.0.bounding_rect(), other.0.bounding_rect()) {
            (Some(a), Some(b))
                if a.min().x < b.max().x
                    && b.min().x < a.max().x
                    && a.min().y < b.max().y
                    && b.min().y < a.max().y =>
            {
                !self.intersection(other).is_empty()
            }
            _ => false,
        }
    }
    fn empty() -> Self {
        Self(Arc::new(MultiPolygon(vec![])))
    }
}
#[derive(Debug, Clone)]
pub struct CoverageFootprint {
    pub dataset_id: usize,
    pub coverage_id: i64,
    pub scales: CoverageScaleRange,
    pub region: Region,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedCoverage {
    pub inventory_index: usize,
    pub selection_band: u8,
    /// Selected after E-1.3 step 3.b; needed for 12.3.3 OVERSC01.
    pub selected_to_fill_gap: bool,
}
#[derive(Debug, Clone)]
pub struct Selection {
    pub display_band: u8,
    pub coverages: Vec<SelectedCoverage>,
    pub uncovered: Region,
}

/// E-1.3: inventory order is explicit and stable. Search finer display band
/// first, then progressively coarser bands, subtracting actual polygons each time.
pub fn select_coverages(
    inventory: &[CoverageFootprint],
    denominator: f64,
    viewport: &Region,
) -> Result<Selection> {
    let bands = inventory
        .iter()
        .map(|c| c.scales.scale_bands())
        .collect::<Result<Vec<_>>>()?;
    select_with_band_membership(inventory, denominator, viewport, &bands)
}
/// The Appendix E prerequisites supply band membership separately from source
/// scale values. Product adapters can apply documented loading rules without
/// changing the producer's scale attributes used for masks and warnings.
pub fn select_with_band_membership(
    inventory: &[CoverageFootprint],
    denominator: f64,
    viewport: &Region,
    bands: &[u16],
) -> Result<Selection> {
    let display_band = display_scale_band(denominator)?;
    ensure!(
        bands.len() == inventory.len() && bands.iter().all(|b| *b & !0x7fff == 0),
        "Invalid coverage band membership"
    );
    let mut identities = HashSet::new();
    for coverage in inventory {
        coverage.scales.validate()?;
        ensure!(
            identities.insert((coverage.dataset_id, coverage.coverage_id)),
            "Duplicate coverage identity"
        );
    }
    let mut remaining = viewport.clone();
    let mut coverages = Vec::new();
    let mut selected = vec![false; inventory.len()];
    for band in (1..=display_band).rev() {
        if remaining.is_empty() {
            break;
        }
        for (index, coverage) in inventory.iter().enumerate() {
            if selected[index]
                || bands[index] & (1 << (band - 1)) == 0
                || !coverage.region.overlaps_area(&remaining)
            {
                continue;
            }
            remaining = remaining.difference(&coverage.region);
            selected[index] = true;
            coverages.push(SelectedCoverage {
                inventory_index: index,
                selection_band: band,
                selected_to_fill_gap: band != display_band,
            });
        }
    }
    Ok(Selection {
        display_band,
        coverages,
        uncovered: remaining,
    })
}
#[derive(Debug, Clone)]
pub struct CoverageMask {
    pub inventory_index: usize,
    pub obscuring: Region,
    pub visible: Region,
}
#[derive(Debug, Clone)]
pub struct DisplayMasks {
    pub coverages: Vec<CoverageMask>,
    pub dataset_obscuring: BTreeMap<usize, Region>,
}

/// E-1.4 masks all coverages of selected datasets, including components that
/// did not directly fill remaining viewport pixels during E-1.3.
pub fn assign_masks(
    inventory: &[CoverageFootprint],
    selection: &Selection,
    viewport: &Region,
) -> Result<DisplayMasks> {
    let mut datasets = HashSet::new();
    for selected in &selection.coverages {
        let coverage = inventory
            .get(selected.inventory_index)
            .context("Selected coverage outside inventory")?;
        datasets.insert(coverage.dataset_id);
    }
    let indexes: Vec<_> = inventory
        .iter()
        .enumerate()
        .filter(|(_, c)| datasets.contains(&c.dataset_id))
        .map(|(i, _)| i)
        .collect();
    let mut masks = Vec::with_capacity(indexes.len());
    let mut dataset_obscuring = BTreeMap::new();
    for &index in &indexes {
        let coverage = &inventory[index];
        let minimum = coverage
            .scales
            .minimum_denominator
            .map(u64::from)
            .unwrap_or(u64::MAX);
        let mut mask = Region::empty();
        for &other in &indexes {
            let finer = &inventory[other];
            let other_minimum = finer
                .scales
                .minimum_denominator
                .map(u64::from)
                .unwrap_or(u64::MAX);
            if other != index
                && other_minimum < minimum
                && coverage.region.overlaps_area(&finer.region)
            {
                mask = mask.union(&finer.region.intersection(viewport));
            }
        }
        let visible = coverage.region.intersection(viewport).difference(&mask);
        dataset_obscuring
            .entry(coverage.dataset_id)
            .and_modify(|r: &mut Region| *r = r.union(&mask))
            .or_insert_with(|| mask.clone());
        masks.push(CoverageMask {
            inventory_index: index,
            obscuring: mask,
            visible,
        });
    }
    Ok(DisplayMasks {
        coverages: masks,
        dataset_obscuring,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{polygon, LineString};
    fn rectangle(x: f64, y: f64, w: f64, h: f64) -> Region {
        Region::from_polygons(vec![
            polygon![(x:x,y:y),(x:x+w,y:y),(x:x+w,y:y+h),(x:x,y:y+h),(x:x,y:y)],
        ])
        .unwrap()
    }
    #[test]
    fn projected_component_union_preserves_holes_and_enforces_coordinate_budget() {
        let shell = Region::from_rings(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
        .unwrap();
        let separate = rectangle(20., 0., 2., 2.);
        let regions = [shell.clone(), shell, separate];
        let result = Region::union_projected(&regions, 25).unwrap();
        assert_eq!(result.area(), 100.);
        assert_eq!(result.polygons().len(), 2);
        assert_eq!(
            result
                .polygons()
                .iter()
                .map(|p| p.interiors().len())
                .sum::<usize>(),
            1
        );
        assert!(Region::union_projected(&regions, 24).is_err());
        assert!(Region::union_projected(&[], 0).unwrap().is_empty());
    }
    fn c(
        dataset: usize,
        id: i64,
        min: u32,
        opt: u32,
        max: u32,
        region: Region,
    ) -> CoverageFootprint {
        CoverageFootprint {
            dataset_id: dataset,
            coverage_id: id,
            scales: CoverageScaleRange {
                minimum_denominator: Some(min),
                optimum_denominator: opt,
                maximum_denominator: max,
            },
            region,
        }
    }
    #[test]
    fn finer_coverage_hole_is_filled_by_coarser_fallback_with_pattern_state() {
        let hole = Polygon::new(
            LineString::from(vec![(0., 0.), (10., 0.), (10., 10.), (0., 10.), (0., 0.)]),
            vec![LineString::from(vec![
                (4., 4.),
                (6., 4.),
                (6., 6.),
                (4., 6.),
                (4., 4.),
            ])],
        );
        let view = rectangle(0., 0., 10., 10.);
        let inventory = vec![
            c(0, 1, 90000, 45000, 12000, rectangle(0., 0., 10., 10.)),
            c(
                1,
                2,
                45000,
                22000,
                8000,
                Region::from_polygons(vec![hole]).unwrap(),
            ),
        ];
        let selected = select_coverages(&inventory, 30000., &view).unwrap();
        assert_eq!(
            selected
                .coverages
                .iter()
                .map(|s| (s.inventory_index, s.selected_to_fill_gap))
                .collect::<Vec<_>>(),
            vec![(1, false), (0, true)]
        );
        assert!(selected.uncovered.is_empty());
        let masks = assign_masks(&inventory, &selected, &view).unwrap();
        assert_eq!(masks.coverages[0].visible.area(), 4.);
        assert_eq!(masks.coverages[1].visible.area(), 96.);
        let enlarged = select_coverages(&inventory, 6000., &view).unwrap();
        assert!(enlarged.coverages.iter().all(|s| s.selected_to_fill_gap
            && inventory[s.inventory_index]
                .scales
                .overscale(6000., s.selected_to_fill_gap)
                .unwrap()
                .pattern_required));
    }
    #[test]
    fn boundaries_do_not_count_as_area_and_missing_regions_stay_uncovered() {
        let view = rectangle(0., 0., 10., 10.);
        let inv = vec![c(0, 1, 45000, 22000, 8000, rectangle(10., 0., 10., 10.))];
        let selected = select_coverages(&inv, 30000., &view).unwrap();
        assert!(selected.coverages.is_empty());
        assert_eq!(selected.uncovered.area(), 100.);
    }
    #[test]
    fn every_component_of_selected_dataset_participates_and_equal_minimum_does_not_obscure() {
        let view = rectangle(0., 0., 10., 10.);
        let inv = vec![
            c(0, 1, 90000, 45000, 12000, rectangle(0., 0., 10., 10.)),
            c(1, 2, 45000, 22000, 8000, rectangle(0., 0., 4., 10.)),
            c(1, 3, 45000, 12000, 4000, rectangle(0., 0., 2., 10.)),
        ];
        let selected = select_coverages(&inv, 30000., &view).unwrap();
        assert_eq!(
            selected
                .coverages
                .iter()
                .map(|s| s.inventory_index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
        let masks = assign_masks(&inv, &selected, &view).unwrap();
        assert_eq!(masks.coverages.len(), 3);
        assert_eq!(masks.coverages[0].visible.area(), 60.);
        assert!(masks.coverages[1].obscuring.is_empty());
        assert!(masks.coverages[2].obscuring.is_empty());
    }
    #[test]
    fn invalid_topology_and_duplicate_identity_are_rejected() {
        let bow = polygon![(x:0.,y:0.),(x:10.,y:10.),(x:0.,y:10.),(x:10.,y:0.),(x:0.,y:0.)];
        assert!(Region::from_polygons(vec![bow]).is_err());
        let one = c(0, 1, 45000, 22000, 8000, rectangle(0., 0., 1., 1.));
        assert!(select_coverages(&[one.clone(), one], 30000., &rectangle(0., 0., 1., 1.)).is_err());
    }
}
