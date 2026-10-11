//! Bounded device-pixel masks for product-neutral projected coverage regions.
//! A backend uploads these masks and applies them to every primitive, including
//! textured glyphs and symbols. This module does not select datasets or project
//! geography. Pixels are sampled at their centres with a half-open edge rule.
use crate::coverage_selection::Region;
use crate::coverage_selection::{CoverageFootprint, Selection};
use anyhow::{ensure, Result};
use geo::BoundingRect;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelMask {
    origin: [u32; 2],
    size: [u32; 2],
    /// Row-major R8 mask: 0 outside, 255 inside. Storage is cropped to bounds.
    pixels: Vec<u8>,
}
impl PixelMask {
    pub fn origin(&self) -> [u32; 2] {
        self.origin
    }
    pub fn size(&self) -> [u32; 2] {
        self.size
    }
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn contains_pixel(&self, x: u32, y: u32) -> bool {
        let Some(x) = x.checked_sub(self.origin[0]) else {
            return false;
        };
        let Some(y) = y.checked_sub(self.origin[1]) else {
            return false;
        };
        x < self.size[0]
            && y < self.size[1]
            && self.pixels[y as usize * self.size[0] as usize + x as usize] != 0
    }
    /// OR cropped immutable R8 masks once. Overlapping annotation owners must
    /// not blend the same pattern twice. Budget excludes callers' input masks.
    pub fn union_masks(masks: &[Self], byte_budget: usize) -> Result<Self> {
        let nonempty: Vec<_> = masks.iter().filter(|m| !m.size.contains(&0)).collect();
        if nonempty.is_empty() {
            return Ok(Self::empty());
        }
        let mut origin = [u32::MAX; 2];
        let mut end = [0; 2];
        for m in &nonempty {
            for axis in 0..2 {
                origin[axis] = origin[axis].min(m.origin[axis]);
                end[axis] = end[axis].max(
                    m.origin[axis]
                        .checked_add(m.size[axis])
                        .ok_or_else(|| anyhow::anyhow!("Mask union extent overflow"))?,
                );
            }
        }
        let size = [end[0] - origin[0], end[1] - origin[1]];
        let bytes = (size[0] as usize)
            .checked_mul(size[1] as usize)
            .ok_or_else(|| anyhow::anyhow!("Mask union storage overflow"))?;
        ensure!(bytes <= byte_budget, "Mask union byte budget exceeded");
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(bytes)?;
        pixels.resize(bytes, 0);
        for m in nonempty {
            for y in 0..m.size[1] as usize {
                let dst = (y + (m.origin[1] - origin[1]) as usize) * size[0] as usize
                    + (m.origin[0] - origin[0]) as usize;
                let src = y * m.size[0] as usize;
                for x in 0..m.size[0] as usize {
                    pixels[dst + x] |= m.pixels[src + x];
                }
            }
        }
        Ok(Self {
            origin,
            size,
            pixels,
        })
    }
    fn empty() -> Self {
        Self {
            origin: [0, 0],
            size: [0, 0],
            pixels: Vec::new(),
        }
    }
    /// Pixel set subtraction keeps shared mask boundaries exact. Rasterizing
    /// independent floating-point polygon differences can move an edge across
    /// a pixel centre after boolean-operation coordinate quantization.
    pub fn subtract(&mut self, other: &Self) {
        let lo = [
            self.origin[0].max(other.origin[0]),
            self.origin[1].max(other.origin[1]),
        ];
        let hi = [
            (self.origin[0] + self.size[0]).min(other.origin[0] + other.size[0]),
            (self.origin[1] + self.size[1]).min(other.origin[1] + other.size[1]),
        ];
        if lo[0] >= hi[0] || lo[1] >= hi[1] {
            return;
        }
        for y in lo[1]..hi[1] {
            let a = (y - self.origin[1]) as usize * self.size[0] as usize
                + (lo[0] - self.origin[0]) as usize;
            let b = (y - other.origin[1]) as usize * other.size[0] as usize
                + (lo[0] - other.origin[0]) as usize;
            for x in 0..(hi[0] - lo[0]) as usize {
                if other.pixels[b + x] != 0 {
                    self.pixels[a + x] = 0;
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct CoveragePixelMask {
    pub inventory_index: usize,
    pub mask: PixelMask,
}

/// Rasterize all components of selected datasets, then perform S-98 obscuring
/// as pixel set subtraction using the same source masks for every comparison.
/// This avoids cracks/overlaps caused by rounding independent polygon booleans.
/// The budget covers aggregate source plus output pixel storage at peak; edge
/// scratch is O(number of source edges), rather than O(number of pixels).
pub fn rasterize_selected_masks(
    inventory: &[CoverageFootprint],
    selection: &Selection,
    extent: [u32; 2],
    byte_budget: usize,
) -> Result<Vec<CoveragePixelMask>> {
    let mut datasets = BTreeSet::new();
    for selected in &selection.coverages {
        let coverage = inventory
            .get(selected.inventory_index)
            .ok_or_else(|| anyhow::anyhow!("Invalid selected coverage index"))?;
        datasets.insert(coverage.dataset_id);
    }
    let mut raw = Vec::new();
    let mut bytes = 0usize;
    for (index, coverage) in inventory.iter().enumerate() {
        if !datasets.contains(&coverage.dataset_id) {
            continue;
        }
        coverage.scales.validate()?;
        let mask = rasterize(&coverage.region, extent, byte_budget - bytes)?;
        bytes += mask.pixels.len();
        raw.push((index, mask));
    }
    let mut result = Vec::new();
    for (index, mask) in &raw {
        ensure!(
            mask.pixels.len() <= byte_budget - bytes,
            "Aggregate coverage masks exceed byte budget"
        );
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(mask.pixels.len())?;
        pixels.extend_from_slice(&mask.pixels);
        bytes += pixels.len();
        let mut visible = PixelMask {
            origin: mask.origin,
            size: mask.size,
            pixels,
        };
        let minimum = inventory[*index]
            .scales
            .minimum_denominator
            .unwrap_or(u32::MAX);
        for (other, obscuring) in &raw {
            if inventory[*other]
                .scales
                .minimum_denominator
                .unwrap_or(u32::MAX)
                < minimum
            {
                visible.subtract(obscuring);
            }
        }
        result.push(CoveragePixelMask {
            inventory_index: *index,
            mask: visible,
        });
    }
    Ok(result)
}

fn scanline_crossings(region: &Region, y: f64, crossings: &mut Vec<f64>) -> Result<()> {
    crossings.clear();
    for polygon in region.polygons() {
        for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
            for edge in ring.0.windows(2) {
                let (a, b) = if edge[0].y <= edge[1].y {
                    (edge[0], edge[1])
                } else {
                    (edge[1], edge[0])
                };
                // Horizontal edges and upper endpoints contribute no crossing.
                if (a.y > y) == (b.y > y) {
                    continue;
                }
                // Halved differences avoid overflowing opposite large values.
                let t = (y * 0.5 - a.y * 0.5) / (b.y * 0.5 - a.y * 0.5);
                let x = if a.x == b.x {
                    a.x
                } else {
                    a.x * (1. - t) + b.x * t
                };
                ensure!(x.is_finite(), "Non-finite coverage edge intersection");
                crossings.push(x);
            }
        }
    }
    crossings.sort_unstable_by(f64::total_cmp);
    ensure!(
        crossings.len().is_multiple_of(2),
        "Unpaired coverage scanline intersections"
    );
    Ok(())
}

/// Same centre/half-open row sampler as rasterize, without allocating a bitmap.
/// A coordinate addresses the physical pixel floor(x),floor(y), as GPU masks do.
pub fn sample_region_pixel(region: &Region, extent: [u32; 2], point: [f64; 2]) -> Result<bool> {
    ensure!(
        point.iter().all(|v| v.is_finite()),
        "Invalid annotation sample point"
    );
    if point[0] < 0.
        || point[1] < 0.
        || point[0] >= extent[0] as f64
        || point[1] >= extent[1] as f64
    {
        return Ok(false);
    }
    let pixel = [point[0].floor() as u32, point[1].floor() as u32];
    let mut crossings = Vec::new();
    scanline_crossings(region, pixel[1] as f64 + 0.5, &mut crossings)?;
    let clamp = |v: f64| v.clamp(0., extent[0] as f64) as u32;
    Ok(crossings.as_chunks::<2>().0.iter().any(|pair| {
        let start = clamp((pair[0] - 0.5).ceil());
        let stop = clamp((pair[1] - 0.5).ceil());
        pixel[0] >= start && pixel[0] < stop
    }))
}
/// Rasterize a normalized Region into the supplied device extent, with an
/// explicit allocation limit. No large full-viewport allocation is required
/// for a small coverage. Budget failures return an error; they never disable
/// obscuring masks or silently render hidden data.
pub fn rasterize(region: &Region, extent: [u32; 2], byte_budget: usize) -> Result<PixelMask> {
    if region.is_empty() || extent.contains(&0) {
        return Ok(PixelMask::empty());
    }
    let bounds = region
        .polygons()
        .iter()
        .filter_map(BoundingRect::bounding_rect)
        .fold(
            [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            |mut bounds, r| {
                bounds[0] = bounds[0].min(r.min().x);
                bounds[1] = bounds[1].min(r.min().y);
                bounds[2] = bounds[2].max(r.max().x);
                bounds[3] = bounds[3].max(r.max().y);
                bounds
            },
        );
    let clamp = |v: f64, maximum: u32| v.clamp(0., maximum as f64) as u32;
    let origin = [
        clamp(bounds[0].floor(), extent[0]),
        clamp(bounds[1].floor(), extent[1]),
    ];
    let end = [
        clamp(bounds[2].ceil(), extent[0]),
        clamp(bounds[3].ceil(), extent[1]),
    ];
    let size = [end[0] - origin[0], end[1] - origin[1]];
    if size.contains(&0) {
        return Ok(PixelMask::empty());
    }
    let bytes = (size[0] as usize)
        .checked_mul(size[1] as usize)
        .ok_or_else(|| anyhow::anyhow!("Coverage mask size overflow"))?;
    ensure!(
        bytes <= byte_budget,
        "Coverage mask exceeds byte budget: {bytes} > {byte_budget}"
    );
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(bytes)?;
    pixels.resize(bytes, 0);
    let mut crossings = Vec::new();
    for row in 0..size[1] {
        crossings.clear();
        let y = (origin[1] + row) as f64 + 0.5;
        scanline_crossings(region, y, &mut crossings)?;
        let offset = row as usize * size[0] as usize;
        for pair in crossings.as_chunks::<2>().0 {
            // A centre lies in [left,right), independent of ring winding.
            let start = clamp((pair[0] - 0.5).ceil(), extent[0]).max(origin[0]);
            let stop = clamp((pair[1] - 0.5).ceil(), extent[0]).min(end[0]);
            if start < stop {
                pixels[offset + (start - origin[0]) as usize..offset + (stop - origin[0]) as usize]
                    .fill(255);
            }
        }
    }
    Ok(PixelMask {
        origin,
        size,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Contains, Point};
    fn rectangle(a: [f64; 2], b: [f64; 2]) -> Region {
        Region::from_rings(&[a, [b[0], a[1]], b, [a[0], b[1]], a], &[]).unwrap()
    }
    #[test]
    fn concave_holes_disjoint_and_overlapping_components_match_geometry() {
        let outer = [
            [-3., -2.],
            [14., 1.],
            [14., 8.],
            [8., 8.],
            [8., 14.],
            [-3., 14.],
            [-3., -2.],
        ];
        let hole = vec![[1., 2.], [5., 2.], [5., 6.], [1., 6.], [1., 2.]];
        let region = Region::from_rings(&outer, &[hole])
            .unwrap()
            .union(&rectangle([12., 5.], [19., 11.]))
            .union(&rectangle([20., 16.], [23., 19.]));
        let mask = rasterize(&region, [24, 20], 480).unwrap();
        for y in 0..20 {
            for x in 0..24 {
                let point = Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let expected = region.polygons().iter().any(|p| p.contains(&point));
                assert_eq!(mask.contains_pixel(x, y), expected, "pixel {x},{y}");
            }
        }
    }
    #[test]
    fn complementary_regions_cover_boundaries_without_holes_or_double_pixels() {
        // Boundary goes through pixel centres: half-open ownership is consistent
        // for shared horizontal/vertical edges and both ring orientations.
        let left = rectangle([0., 0.], [3.5, 8.]);
        let right = rectangle([3.5, 0.], [8., 8.]);
        let a = rasterize(&left, [8, 8], 64).unwrap();
        let b = rasterize(&right, [8, 8], 64).unwrap();
        for y in 0..8 {
            for x in 0..8 {
                assert_ne!(a.contains_pixel(x, y), b.contains_pixel(x, y));
            }
        }
        let outer = rectangle([0., 0.], [8., 8.]);
        let fine = rectangle([1.5, 1.5], [6.5, 6.5]);
        let f = rasterize(&fine, [8, 8], 64).unwrap();
        let direct_difference = rasterize(&outer.difference(&fine), [8, 8], 64).unwrap();
        let mut c = rasterize(&outer, [8, 8], 64).unwrap();
        c.subtract(&f);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(
                    direct_difference.contains_pixel(x, y),
                    c.contains_pixel(x, y)
                );
            }
        }
        for y in 0..8 {
            for x in 0..8 {
                assert_ne!(
                    f.contains_pixel(x, y),
                    c.contains_pixel(x, y),
                    "pixel {x},{y}"
                );
            }
        }
    }
    #[test]
    fn selected_dataset_components_obscure_coarse_using_identical_pixel_sets() {
        use crate::coverage_selection::select_coverages;
        use crate::scale_policy::CoverageScaleRange;
        let scale = |min, opt, max| CoverageScaleRange {
            minimum_denominator: Some(min),
            optimum_denominator: opt,
            maximum_denominator: max,
        };
        let viewport = rectangle([0., 0.], [8., 8.]);
        let fine = rectangle([1.5, 1.5], [6.5, 6.5]);
        let inventory = vec![
            CoverageFootprint {
                dataset_id: 0,
                coverage_id: 1,
                scales: scale(45000, 12000, 6000),
                region: fine,
            },
            // This same-dataset component is already covered at selection time;
            // S-98 still requires its footprint to obscure the coarse dataset.
            CoverageFootprint {
                dataset_id: 0,
                coverage_id: 2,
                scales: scale(45000, 45000, 12000),
                region: rectangle([2., 2.], [3., 3.]),
            },
            CoverageFootprint {
                dataset_id: 1,
                coverage_id: 3,
                scales: scale(180000, 90000, 45000),
                region: viewport.clone(),
            },
        ];
        let selected = select_coverages(&inventory, 12000., &viewport).unwrap();
        let masks = rasterize_selected_masks(&inventory, &selected, [8, 8], 512).unwrap();
        assert_eq!(masks.len(), 3);
        for y in 0..8 {
            for x in 0..8 {
                let fine = masks
                    .iter()
                    .filter(|m| inventory[m.inventory_index].dataset_id == 0)
                    .any(|m| m.mask.contains_pixel(x, y));
                let coarse = masks[2].mask.contains_pixel(x, y);
                assert_ne!(fine, coarse, "pixel {x},{y}");
            }
        }
        assert!(rasterize_selected_masks(&inventory, &selected, [8, 8], 64).is_err());
    }
    #[test]
    fn allocation_is_cropped_bounded_and_empty_regions_allocate_nothing() {
        let small = rectangle([10., 20.], [12., 23.]);
        let mask = rasterize(&small, [u32::MAX, u32::MAX], 6).unwrap();
        assert_eq!(mask.origin, [10, 20]);
        assert_eq!(mask.size, [2, 3]);
        assert_eq!(mask.pixels.len(), 6);
        assert!(rasterize(&small, [100, 100], 5).is_err());
        assert!(!mask.contains_pixel(9, 20));
        assert!(!mask.contains_pixel(12, 20));
        assert!(!mask.contains_pixel(u32::MAX, u32::MAX));
        let empty = small.difference(&small);
        assert!(rasterize(&empty, [u32::MAX, u32::MAX], 0)
            .unwrap()
            .pixels
            .is_empty());
        assert!(rasterize(&small, [1, 1], 0).unwrap().pixels.is_empty());
        assert!(rasterize(&small, [0, 100], 0).unwrap().pixels.is_empty());
    }
}

/// A single device mask for every selected dataset. Component masks are united
/// as pixel sets after obscuring subtraction, without another polygon boolean
/// operation or rasterization. Holes and shared boundary samples are preserved.
#[derive(Debug)]
pub struct DatasetPixelMask {
    pub dataset_id: usize,
    pub mask: PixelMask,
}

/// Peak pixel storage includes all retained component masks plus all dataset
/// masks constructed so far. GPU allocations are a separate backend budget.
/// Runtime is O(component pixel storage + dataset bounding-box pixel storage).
pub fn rasterize_selected_dataset_masks(
    inventory: &[CoverageFootprint],
    selection: &Selection,
    extent: [u32; 2],
    byte_budget: usize,
) -> Result<Vec<DatasetPixelMask>> {
    let components = rasterize_selected_masks(inventory, selection, extent, byte_budget)?;
    let mut groups = std::collections::BTreeMap::<usize, Vec<PixelMask>>::new();
    let mut resident = 0usize;
    for component in components {
        resident = resident
            .checked_add(component.mask.pixels.len())
            .ok_or_else(|| anyhow::anyhow!("Dataset mask storage overflow"))?;
        let dataset = inventory
            .get(component.inventory_index)
            .ok_or_else(|| anyhow::anyhow!("Invalid component coverage index"))?
            .dataset_id;
        groups.entry(dataset).or_default().push(component.mask);
    }
    ensure!(
        resident <= byte_budget,
        "Component masks exceed byte budget"
    );
    let mut result = Vec::with_capacity(groups.len());
    for (dataset_id, mut masks) in groups {
        // The common single-component dataset needs no second bitmap allocation
        // or pixel copy: transfer the already cropped, obscured component.
        if masks.len() == 1 {
            result.push(DatasetPixelMask {
                dataset_id,
                mask: masks.pop().unwrap(),
            });
            continue;
        }
        let consumed_bytes = masks.iter().map(|m| m.pixels.len()).sum::<usize>();
        let mut lo = [u32::MAX; 2];
        let mut hi = [0u32; 2];
        for mask in &masks {
            if mask.pixels.is_empty() {
                continue;
            }
            for axis in 0..2 {
                lo[axis] = lo[axis].min(mask.origin[axis]);
                hi[axis] = hi[axis].max(
                    mask.origin[axis]
                        .checked_add(mask.size[axis])
                        .ok_or_else(|| anyhow::anyhow!("Dataset mask extent overflow"))?,
                );
            }
        }
        if lo[0] == u32::MAX {
            resident -= consumed_bytes;
            result.push(DatasetPixelMask {
                dataset_id,
                mask: PixelMask::empty(),
            });
            continue;
        }
        let size = [hi[0] - lo[0], hi[1] - lo[1]];
        let length = (size[0] as usize)
            .checked_mul(size[1] as usize)
            .ok_or_else(|| anyhow::anyhow!("Dataset mask size overflow"))?;
        ensure!(
            length <= byte_budget - resident,
            "Dataset masks exceed peak byte budget"
        );
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(length)?;
        pixels.resize(length, 0);
        resident += length;
        for mask in masks {
            if mask.pixels.is_empty() {
                continue;
            }
            let dx = (mask.origin[0] - lo[0]) as usize;
            let dy = (mask.origin[1] - lo[1]) as usize;
            let width = mask.size[0] as usize;
            for row in 0..mask.size[1] as usize {
                let target = (row + dy) * size[0] as usize + dx;
                let source = row * width;
                for col in 0..width {
                    pixels[target + col] |= mask.pixels[source + col];
                }
            }
        }
        // Consumed component buffers are now dropped; prior dataset output
        // and the unprocessed component groups remain included in the budget.
        resident -= consumed_bytes;
        result.push(DatasetPixelMask {
            dataset_id,
            mask: PixelMask {
                origin: lo,
                size,
                pixels,
            },
        });
    }
    Ok(result)
}

#[cfg(test)]
mod dataset_tests {
    use super::*;
    use crate::coverage_selection::SelectedCoverage;
    use crate::scale_policy::CoverageScaleRange;
    fn rect(x: f64, y: f64, w: f64, h: f64) -> Region {
        Region::from_rings(
            &[[x, y], [x + w, y], [x + w, y + h], [x, y + h], [x, y]],
            &[],
        )
        .unwrap()
    }
    fn footprint(
        dataset_id: usize,
        coverage_id: i64,
        min: u32,
        region: Region,
    ) -> CoverageFootprint {
        CoverageFootprint {
            dataset_id,
            coverage_id,
            region,
            scales: CoverageScaleRange {
                minimum_denominator: Some(min),
                optimum_denominator: min / 2,
                maximum_denominator: min / 4,
            },
        }
    }
    fn selection(indices: &[usize]) -> Selection {
        Selection {
            display_band: 10,
            coverages: indices
                .iter()
                .map(|&inventory_index| SelectedCoverage {
                    inventory_index,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                })
                .collect(),
            uncovered: rect(0., 0., 1., 1.),
        }
    }
    #[test]
    fn dataset_union_matches_every_component_sample_with_holes_and_obscuring() {
        let hole = Region::from_rings(
            &[[1., 1.], [9., 1.], [9., 9.], [1., 9.], [1., 1.]],
            &[vec![[3., 3.], [7., 3.], [7., 7.], [3., 7.], [3., 3.]]],
        )
        .unwrap();
        let inv = vec![
            footprint(8, 1, 180000, hole),
            footprint(8, 2, 180000, rect(8., 1., 4., 8.)),
            footprint(3, 3, 45000, rect(9., 2., 2., 4.)),
            footprint(99, 4, 180000, rect(0., 0., 16., 16.)),
        ];
        let s = selection(&[0, 2]);
        let components = rasterize_selected_masks(&inv, &s, [16, 16], 4096).unwrap();
        let datasets = rasterize_selected_dataset_masks(&inv, &s, [16, 16], 4096).unwrap();
        assert_eq!(
            datasets.iter().map(|d| d.dataset_id).collect::<Vec<_>>(),
            vec![3, 8]
        );
        for d in &datasets {
            for y in 0..16 {
                for x in 0..16 {
                    let expected = components
                        .iter()
                        .filter(|c| inv[c.inventory_index].dataset_id == d.dataset_id)
                        .any(|c| c.mask.contains_pixel(x, y));
                    assert_eq!(
                        d.mask.contains_pixel(x, y),
                        expected,
                        "dataset {} at {x},{y}",
                        d.dataset_id
                    );
                }
            }
        }
        let coarse = &datasets[1].mask;
        assert!(!coarse.contains_pixel(4, 4)); // preserved hole
        assert!(coarse.contains_pixel(11, 6)); // non-selected component of selected dataset
        assert!(!coarse.contains_pixel(9, 3)); // finer dataset obscures coarse
        assert!(datasets[0].mask.contains_pixel(9, 3));
    }
    #[test]
    fn sparse_components_cannot_allocate_unbounded_union_box_and_empty_remains_denied() {
        let inv = vec![
            footprint(0, 1, 180000, rect(0., 0., 1., 1.)),
            footprint(0, 2, 180000, rect(999., 999., 1., 1.)),
        ];
        let s = selection(&[0]);
        assert!(rasterize_selected_masks(&inv, &s, [1000, 1000], 10).is_ok());
        assert!(rasterize_selected_dataset_masks(&inv, &s, [1000, 1000], 10).is_err());
        let masks = rasterize_selected_dataset_masks(&inv, &s, [0, 0], 0).unwrap();
        assert_eq!(masks.len(), 1);
        assert_eq!(masks[0].dataset_id, 0);
        assert!(masks[0].mask.pixels().is_empty());
        assert!(
            rasterize_selected_dataset_masks(&inv, &selection(&[]), [1000, 1000], 0)
                .unwrap()
                .is_empty()
        );
    }
}
