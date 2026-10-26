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
    rasterize_selected_masks_with(inventory, selection, byte_budget, &|region, budget| {
        rasterize(region, extent, budget)
    })
}

/// `rasterize_selected_masks` over an overscan window (see `rasterize_window`).
/// Texels inside `hole` (a texel rectangle) stay empty.
/// Components are rasterized and obscured in parallel (same results); the
/// budget bounds each component and the aggregate after each stage.
pub fn rasterize_selected_masks_window(
    inventory: &[CoverageFootprint],
    selection: &Selection,
    window_origin: [i64; 2],
    size: [u32; 2],
    hole: Option<[u32; 4]>,
    byte_budget: usize,
) -> Result<Vec<CoveragePixelMask>> {
    use crate::par::prelude::*;
    let mut datasets = BTreeSet::new();
    for selected in &selection.coverages {
        let coverage = inventory
            .get(selected.inventory_index)
            .ok_or_else(|| anyhow::anyhow!("Invalid selected coverage index"))?;
        datasets.insert(coverage.dataset_id);
    }
    let mut indexes = Vec::new();
    for (index, coverage) in inventory.iter().enumerate() {
        if datasets.contains(&coverage.dataset_id) {
            coverage.scales.validate()?;
            indexes.push(index);
        }
    }
    let raw = indexes
        .par_iter()
        .map(|&index| {
            rasterize_window_with_hole(
                &inventory[index].region,
                window_origin,
                size,
                hole,
                byte_budget,
            )
            .map(|mask| (index, mask))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let raw_bytes = raw.iter().map(|(_, m)| m.pixels.len()).sum::<usize>();
    ensure!(
        raw_bytes <= byte_budget,
        "Aggregate coverage masks exceed byte budget"
    );
    let minimum = |index: usize| {
        inventory[index]
            .scales
            .minimum_denominator
            .unwrap_or(u32::MAX)
    };
    let result: Vec<CoveragePixelMask> = raw
        .par_iter()
        .map(|(index, mask)| {
            let mut visible = mask.clone();
            for (other, obscuring) in &raw {
                if minimum(*other) < minimum(*index) {
                    visible.subtract(obscuring);
                }
            }
            CoveragePixelMask {
                inventory_index: *index,
                mask: visible,
            }
        })
        .collect();
    ensure!(
        raw_bytes + result.iter().map(|m| m.mask.pixels.len()).sum::<usize>() <= byte_budget,
        "Aggregate coverage masks exceed byte budget"
    );
    Ok(result)
}

type RegionRaster<'a> = dyn Fn(&Region, usize) -> Result<PixelMask> + 'a;

fn rasterize_selected_masks_with(
    inventory: &[CoverageFootprint],
    selection: &Selection,
    byte_budget: usize,
    raster: &RegionRaster<'_>,
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
        let mask = raster(&coverage.region, byte_budget - bytes)?;
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

/// `rasterize` over a window of `size` texels whose texel (0, 0) is device
/// pixel `window_origin` (which may be negative, e.g. a navigation overscan
/// beyond the surface). Texel (i, j) samples device centre
/// (i + origin.x + 0.5, j + origin.y + 0.5) with the same half-open rule.
pub fn rasterize_window(
    region: &Region,
    window_origin: [i64; 2],
    size: [u32; 2],
    byte_budget: usize,
) -> Result<PixelMask> {
    rasterize_window_with_hole(region, window_origin, size, None, byte_budget)
}

/// `rasterize_window`, leaving the texels of `hole` empty (their owner is
/// another mask, e.g. the exact frame's chart pixels).
pub fn rasterize_window_with_hole(
    region: &Region,
    window_origin: [i64; 2],
    size: [u32; 2],
    hole: Option<[u32; 4]>,
    byte_budget: usize,
) -> Result<PixelMask> {
    if region.is_empty() || size.contains(&0) {
        return Ok(PixelMask::empty());
    }
    let offset = [window_origin[0] as f64, window_origin[1] as f64];
    ensure!(
        window_origin.iter().all(|v| v.unsigned_abs() < 1 << 40),
        "Coverage window origin out of range"
    );
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
        clamp((bounds[0] - offset[0]).floor(), size[0]),
        clamp((bounds[1] - offset[1]).floor(), size[1]),
    ];
    let end = [
        clamp((bounds[2] - offset[0]).ceil(), size[0]),
        clamp((bounds[3] - offset[1]).ceil(), size[1]),
    ];
    let extent = [end[0] - origin[0], end[1] - origin[1]];
    if extent.contains(&0) {
        return Ok(PixelMask::empty());
    }
    let bytes = (extent[0] as usize)
        .checked_mul(extent[1] as usize)
        .ok_or_else(|| anyhow::anyhow!("Coverage mask size overflow"))?;
    ensure!(
        bytes <= byte_budget,
        "Coverage mask exceeds byte budget: {bytes} > {byte_budget}"
    );
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(bytes)?;
    pixels.resize(bytes, 0);
    let mut crossings = Vec::new();
    let mut fill = |row_start: usize, start: u32, stop: u32| {
        if start < stop {
            pixels
                [row_start + (start - origin[0]) as usize..row_start + (stop - origin[0]) as usize]
                .fill(255);
        }
    };
    for row in 0..extent[1] {
        let texel_y = origin[1] + row;
        let y = texel_y as f64 + offset[1] + 0.5;
        scanline_crossings(region, y, &mut crossings)?;
        let start_of_row = row as usize * extent[0] as usize;
        let hole_x = hole
            .filter(|h| texel_y >= h[1] && texel_y < h[3])
            .map(|h| [h[0], h[2]]);
        for pair in crossings.as_chunks::<2>().0 {
            let start = clamp((pair[0] - offset[0] - 0.5).ceil(), size[0]).max(origin[0]);
            let stop = clamp((pair[1] - offset[0] - 0.5).ceil(), size[0]).min(end[0]);
            match hole_x {
                Some([h0, h1]) => {
                    fill(start_of_row, start, stop.min(h0));
                    fill(start_of_row, start.max(h1), stop);
                }
                None => fill(start_of_row, start, stop),
            }
        }
    }
    Ok(PixelMask {
        origin,
        size: extent,
        pixels,
    })
}

/// `compose_overscan_mask` taking the ring by value: when the ring's rectangle
/// already holds the exact part, the exact texels are written into it in place
/// (no second window-sized allocation or ring copy).
pub fn compose_overscan_mask_owned(
    exact: Option<&PixelMask>,
    ring: Option<PixelMask>,
    window_origin: [i64; 2],
    inner: [u32; 4],
    byte_budget: usize,
) -> Result<PixelMask> {
    let Some(mut ring) = ring.filter(|m| !m.size.contains(&0)) else {
        return compose_overscan_mask(exact, None, window_origin, inner, byte_budget);
    };
    let [ix0, iy0, ix1, iy1] = inner.map(i64::from);
    let r = [
        ring.origin[0] as i64,
        ring.origin[1] as i64,
        (ring.origin[0] + ring.size[0]) as i64,
        (ring.origin[1] + ring.size[1]) as i64,
    ];
    let a = exact.filter(|m| !m.size.contains(&0)).and_then(|m| {
        let x0 = (m.origin[0] as i64 - window_origin[0]).max(ix0);
        let y0 = (m.origin[1] as i64 - window_origin[1]).max(iy0);
        let x1 = (m.origin[0] as i64 + m.size[0] as i64 - window_origin[0]).min(ix1);
        let y1 = (m.origin[1] as i64 + m.size[1] as i64 - window_origin[1]).min(iy1);
        (x0 < x1 && y0 < y1).then_some((m, [x0, y0, x1, y1]))
    });
    if a.is_some_and(|(_, a)| a[0] < r[0] || a[1] < r[1] || a[2] > r[2] || a[3] > r[3]) {
        return compose_overscan_mask(exact, Some(&ring), window_origin, inner, byte_budget);
    }
    ensure!(
        ring.pixels.len() <= byte_budget,
        "Overscan mask byte budget exceeded"
    );
    let width = ring.size[0] as usize;
    // Ring texels inside the exact chart pixels are never its own.
    let (hx0, hx1) = (ix0.max(r[0]), ix1.min(r[2]));
    if hx0 < hx1 {
        for y in iy0.max(r[1])..iy1.min(r[3]) {
            let row = (y - r[1]) as usize * width;
            ring.pixels[row + (hx0 - r[0]) as usize..row + (hx1 - r[0]) as usize].fill(0);
        }
    }
    if let Some((m, a)) = a {
        let source_width = m.size[0] as usize;
        for y in a[1]..a[3] {
            let device_y = y + window_origin[1] - m.origin[1] as i64;
            let device_x = a[0] + window_origin[0] - m.origin[0] as i64;
            let s = device_y as usize * source_width + device_x as usize;
            let t = (y - r[1]) as usize * width + (a[0] - r[0]) as usize;
            let n = (a[2] - a[0]) as usize;
            ring.pixels[t..t + n].copy_from_slice(&m.pixels[s..s + n]);
        }
    }
    Ok(ring)
}

/// Navigation overscan composite in a window's texel space (device pixel =
/// texel + `window_origin`). Texels inside `inner` (texel rect `[x0, y0, x1,
/// y1)`) copy `exact`, a device-space mask, so that region is bit-identical to
/// the exact frame; every other texel copies `ring`, already in texel space.
pub fn compose_overscan_mask(
    exact: Option<&PixelMask>,
    ring: Option<&PixelMask>,
    window_origin: [i64; 2],
    inner: [u32; 4],
    byte_budget: usize,
) -> Result<PixelMask> {
    let [ix0, iy0, ix1, iy1] = inner.map(i64::from);
    // Exact part, clipped to the inner rectangle, in texel coordinates.
    let a = exact.filter(|m| !m.size.contains(&0)).and_then(|m| {
        let x0 = (m.origin[0] as i64 - window_origin[0]).max(ix0);
        let y0 = (m.origin[1] as i64 - window_origin[1]).max(iy0);
        let x1 = (m.origin[0] as i64 + m.size[0] as i64 - window_origin[0]).min(ix1);
        let y1 = (m.origin[1] as i64 + m.size[1] as i64 - window_origin[1]).min(iy1);
        (x0 < x1 && y0 < y1).then_some((m, [x0, y0, x1, y1]))
    });
    let b = ring.filter(|m| !m.size.contains(&0)).map(|m| {
        (
            m,
            [
                m.origin[0] as i64,
                m.origin[1] as i64,
                (m.origin[0] + m.size[0]) as i64,
                (m.origin[1] + m.size[1]) as i64,
            ],
        )
    });
    let rects: Vec<[i64; 4]> = a.iter().map(|x| x.1).chain(b.iter().map(|x| x.1)).collect();
    if rects.is_empty() {
        return Ok(PixelMask::empty());
    }
    let lo = [
        rects.iter().map(|r| r[0]).min().unwrap(),
        rects.iter().map(|r| r[1]).min().unwrap(),
    ];
    let hi = [
        rects.iter().map(|r| r[2]).max().unwrap(),
        rects.iter().map(|r| r[3]).max().unwrap(),
    ];
    ensure!(
        lo.iter().all(|v| *v >= 0) && hi.iter().all(|v| *v <= u32::MAX as i64),
        "Overscan mask outside its window"
    );
    let size = [(hi[0] - lo[0]) as u32, (hi[1] - lo[1]) as u32];
    let bytes = (size[0] as usize)
        .checked_mul(size[1] as usize)
        .ok_or_else(|| anyhow::anyhow!("Overscan mask size overflow"))?;
    ensure!(bytes <= byte_budget, "Overscan mask byte budget exceeded");
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(bytes)?;
    pixels.resize(bytes, 0);
    let width = size[0] as usize;
    if let Some((m, r)) = b {
        let source_width = m.size[0] as usize;
        for y in r[1]..r[3] {
            let source = (y - r[1]) as usize * source_width;
            let target = (y - lo[1]) as usize * width;
            let mut copy = |x0: i64, x1: i64| {
                if x0 < x1 {
                    let s = source + (x0 - r[0]) as usize;
                    let t = target + (x0 - lo[0]) as usize;
                    let n = (x1 - x0) as usize;
                    pixels[t..t + n].copy_from_slice(&m.pixels[s..s + n]);
                }
            };
            if y >= iy0 && y < iy1 {
                copy(r[0], r[2].min(ix0));
                copy(r[0].max(ix1), r[2]);
            } else {
                copy(r[0], r[2]);
            }
        }
    }
    if let Some((m, r)) = a {
        let source_width = m.size[0] as usize;
        for y in r[1]..r[3] {
            let device_y = y + window_origin[1] - m.origin[1] as i64;
            let device_x = r[0] + window_origin[0] - m.origin[0] as i64;
            let s = device_y as usize * source_width + device_x as usize;
            let t = (y - lo[1]) as usize * width + (r[0] - lo[0]) as usize;
            let n = (r[2] - r[0]) as usize;
            pixels[t..t + n].copy_from_slice(&m.pixels[s..s + n]);
        }
    }
    Ok(PixelMask {
        origin: [lo[0] as u32, lo[1] as u32],
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
    group_dataset_masks(inventory, components, byte_budget)
}

/// `rasterize_selected_dataset_masks` over an overscan window, `hole` empty.
pub fn rasterize_selected_dataset_masks_window(
    inventory: &[CoverageFootprint],
    selection: &Selection,
    window_origin: [i64; 2],
    size: [u32; 2],
    hole: Option<[u32; 4]>,
    byte_budget: usize,
) -> Result<Vec<DatasetPixelMask>> {
    let components = rasterize_selected_masks_window(
        inventory,
        selection,
        window_origin,
        size,
        hole,
        byte_budget,
    )?;
    group_dataset_masks(inventory, components, byte_budget)
}

fn group_dataset_masks(
    inventory: &[CoverageFootprint],
    components: Vec<CoveragePixelMask>,
    byte_budget: usize,
) -> Result<Vec<DatasetPixelMask>> {
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

#[cfg(test)]
mod overscan_window_tests {
    use super::*;
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Region {
        Region::from_rings(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]], &[]).unwrap()
    }
    fn concave() -> Region {
        Region::from_rings(
            &[
                [-6.3, -2.5],
                [13.7, 1.2],
                [13.7, 8.5],
                [8.5, 8.5],
                [8.5, 14.2],
                [-6.3, 14.2],
                [-6.3, -2.5],
            ],
            &[vec![[1., 2.5], [5.5, 2.5], [5.5, 6.], [1., 6.], [1., 2.5]]],
        )
        .unwrap()
    }
    #[test]
    fn zero_origin_window_is_the_surface_raster() {
        let region = concave();
        assert_eq!(
            rasterize_window(&region, [0, 0], [12, 10], 1000).unwrap(),
            rasterize(&region, [12, 10], 1000).unwrap()
        );
    }
    #[test]
    fn window_texels_sample_the_same_device_pixels_beyond_the_surface() {
        let region = concave();
        let origin = [-8i64, -4i64];
        let window = rasterize_window(&region, origin, [30, 24], 1000).unwrap();
        // The device raster of the region moved by +8/+4 (exact in f64).
        let moved = region.map_affine(|p| [p[0] + 8., p[1] + 4.]).unwrap();
        let shifted = rasterize(&moved, [30, 24], 1000).unwrap();
        for y in 0..24 {
            for x in 0..30 {
                assert_eq!(
                    window.contains_pixel(x, y),
                    shifted.contains_pixel(x, y),
                    "texel {x},{y}"
                );
            }
        }
        // Device pixel (-6, 0) is inside; texel = device - origin.
        assert!(window.contains_pixel(2, 4));
        assert!(rasterize_window(&region, origin, [30, 24], 10).is_err());
    }
    #[test]
    fn composite_copies_exact_inside_and_ring_outside() {
        // Exact mask in device space over the 10x10 surface, ring raster of a
        // different selection over the window [-5, 15).
        let exact = rasterize(&rect(2., 0., 10., 10.), [10, 10], 1000).unwrap();
        let ring = rasterize_window(&rect(-5., -5., 6., 15.), [-5, -5], [20, 20], 1000).unwrap();
        let inner = [5, 5, 15, 15];
        let composite =
            compose_overscan_mask(Some(&exact), Some(&ring), [-5, -5], inner, 1000).unwrap();
        for ty in 0..20u32 {
            for tx in 0..20u32 {
                let (dx, dy) = (tx as i64 - 5, ty as i64 - 5);
                let inside = (0..10).contains(&dx) && (0..10).contains(&dy);
                let expected = if inside {
                    exact.contains_pixel(dx as u32, dy as u32)
                } else {
                    ring.contains_pixel(tx, ty)
                };
                assert_eq!(
                    composite.contains_pixel(tx, ty),
                    expected,
                    "texel {tx},{ty}"
                );
            }
        }
        // Ring-only data never reaches the inner rectangle.
        let ring_only = compose_overscan_mask(None, Some(&ring), [-5, -5], inner, 1000).unwrap();
        for ty in 5..15 {
            for tx in 5..15 {
                assert!(!ring_only.contains_pixel(tx, ty));
            }
        }
        assert!(ring_only.contains_pixel(2, 2));
        // The owned variant gives the same texels, in place when it can.
        for exact in [Some(&exact), None] {
            let owned =
                compose_overscan_mask_owned(exact, Some(ring.clone()), [-5, -5], inner, 1000)
                    .unwrap();
            let borrowed =
                compose_overscan_mask(exact, Some(&ring), [-5, -5], inner, 1000).unwrap();
            for ty in 0..20u32 {
                for tx in 0..20u32 {
                    assert_eq!(
                        owned.contains_pixel(tx, ty),
                        borrowed.contains_pixel(tx, ty)
                    );
                }
            }
        }
        // A ring smaller than the exact part falls back to a new allocation.
        let small = rasterize_window(&rect(-5., -5., -1., 2.), [-5, -5], [20, 20], 1000).unwrap();
        let owned =
            compose_overscan_mask_owned(Some(&exact), Some(small.clone()), [-5, -5], inner, 1000)
                .unwrap();
        let borrowed =
            compose_overscan_mask(Some(&exact), Some(&small), [-5, -5], inner, 1000).unwrap();
        assert_eq!(owned, borrowed);
        // A hole leaves the exact chart pixels empty in the ring raster.
        let holed = rasterize_window_with_hole(
            &rect(-5., -5., 6., 15.),
            [-5, -5],
            [20, 20],
            Some(inner),
            1000,
        )
        .unwrap();
        for ty in 0..20u32 {
            for tx in 0..20u32 {
                let in_hole = (5..15).contains(&tx) && (5..15).contains(&ty);
                assert_eq!(
                    holed.contains_pixel(tx, ty),
                    ring.contains_pixel(tx, ty) && !in_hole
                );
            }
        }
        let empty = compose_overscan_mask(None, None, [-5, -5], inner, 0).unwrap();
        assert!(empty.pixels().is_empty() && empty.size() == [0, 0]);
        assert!(compose_overscan_mask(Some(&exact), Some(&ring), [-5, -5], inner, 10).is_err());
    }
}
