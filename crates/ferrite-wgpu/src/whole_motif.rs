//! One upright, independently removable motif, distinct from a repeating cell.
//!
//! The bitmap and geometric support are built from ONE parsed tree. This uses
//! the uncut painted-graphic extent, including graphics outside the SVG viewport.
//! That extent policy and combined curve/stroke/AA error still require the
//! whole-area renderer's conformance gate; this module does not authorize sites.
use std::sync::Arc;

use crate::svg_painted_support::{svg_painted_support, SvgPaintedSupport, SvgSupportLimits};
use resvg::{tiny_skia, usvg};

#[derive(Debug, Clone, Copy)]
pub struct MotifRasterLimits {
    pub max_edge: u32,
    pub max_rgba_bytes: usize,
    pub max_retained_payload_bytes: usize,
    /// Transparent guards for a future ClampToEdge, level-zero sampler.
    pub guard_pixels: u32,
}
impl Default for MotifRasterLimits {
    fn default() -> Self {
        Self {
            max_edge: 8192,
            max_rgba_bytes: 64 * 1024 * 1024,
            max_retained_payload_bytes: 64 * 1024 * 1024,
            guard_pixels: 2,
        }
    }
}

#[derive(Debug)]
pub struct NaturalMotifResource {
    /// Exact immutable-source revision, palette, physical scale and limits key.
    pub resource_key: String,
    pub support: Arc<SvgPaintedSupport>,
    /// sRGB premultiplied RGBA, one upright motif, never a prewarped Repeat cell.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Actual raster alpha. Nonempty geometric support may still rasterize empty.
    /// Site execution requires visible geometry as well, not just this flag.
    pub has_coverage: bool,
    /// Bitmap's top-left in physical pixels relative to the authored SVG origin.
    pub bitmap_origin_px: [f64; 2],
    /// Authored origin inside this bitmap; it may legitimately lie outside it.
    pub texture_pivot_px: [f64; 2],
    /// Bound only for the cast of the root raster affine parameters.
    /// Not a bound for combined path tessellation/stroke/AA/geo projection error.
    pub root_raster_cast_error_bound_px: f64,
    /// Logical owned pixels/geometry/key estimate, excluding parser/raster scratch.
    pub retained_payload_bytes: usize,
}

pub(crate) fn build_natural_motif(
    resource_key: String,
    tree: &usvg::Tree,
    pivot: [f64; 2],
    pixels_per_svg_px: f64,
    support_limits: SvgSupportLimits,
    raster_limits: MotifRasterLimits,
) -> Result<Option<NaturalMotifResource>, String> {
    if raster_limits.max_edge == 0
        || raster_limits.max_edge > 8192
        || !(2..=16).contains(&raster_limits.guard_pixels)
    {
        return Err("Invalid whole motif raster limits".into());
    }
    let Some(support) = svg_painted_support(tree, pivot, pixels_per_svg_px, support_limits)? else {
        return Ok(None);
    };
    // usvg's layer bounds include stroke extent. Union with the independently
    // tessellated support protects against either approximation being smaller.
    // The SVG viewport itself is deliberately not used as the graphic extent.
    let r = tree.root().abs_layer_bounding_box();
    let source_bounds = [
        f64::from(r.left()),
        f64::from(r.top()),
        f64::from(r.right()),
        f64::from(r.bottom()),
    ];
    let mapped = [
        (source_bounds[0] - pivot[0]) * pixels_per_svg_px,
        (source_bounds[1] - pivot[1]) * pixels_per_svg_px,
        (source_bounds[2] - pivot[0]) * pixels_per_svg_px,
        (source_bounds[3] - pivot[1]) * pixels_per_svg_px,
    ];
    let bounds = [
        mapped[0].min(support.bounds_px[0]),
        mapped[1].min(support.bounds_px[1]),
        mapped[2].max(support.bounds_px[2]),
        mapped[3].max(support.bounds_px[3]),
    ];
    if !bounds.iter().all(|c| c.is_finite()) {
        return Err("Whole motif graphic extent overflow".into());
    }
    let guard = f64::from(raster_limits.guard_pixels);
    let origin = [bounds[0].floor() - guard, bounds[1].floor() - guard];
    let extent = [
        bounds[2].ceil() + guard - origin[0],
        bounds[3].ceil() + guard - origin[1],
    ];
    if !origin.iter().all(|c| c.is_finite())
        || !extent
            .iter()
            .all(|c| c.is_finite() && *c > 0. && *c <= f64::from(raster_limits.max_edge))
    {
        return Err("Whole motif raster edge budget exceeded".into());
    }
    let [width, height] = extent.map(|c| c as u32);
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n <= raster_limits.max_rgba_bytes)
        .ok_or("Whole motif RGBA byte budget exceeded")?;
    let retained_payload_bytes = bytes
        .checked_add(support.retained_payload_bytes)
        .and_then(|n| n.checked_add(resource_key.capacity().checked_mul(2)?))
        .and_then(|n| n.checked_add(256))
        .ok_or("Whole motif retained payload overflow")?;
    if retained_payload_bytes > raster_limits.max_retained_payload_bytes {
        return Err("Whole motif retained payload budget exceeded".into());
    }
    let translation = [
        -pivot[0] * pixels_per_svg_px - origin[0],
        -pivot[1] * pixels_per_svg_px - origin[1],
    ];
    let scale32 = pixels_per_svg_px as f32;
    let translation32 = translation.map(|c| c as f32);
    let max_source_coordinate = source_bounds.iter().fold(0.0_f64, |n, c| n.max(c.abs()));
    let error = (f64::from(scale32) - pixels_per_svg_px).abs() * max_source_coordinate
        + translation
            .into_iter()
            .zip(translation32)
            .map(|(a, b)| (a - f64::from(b)).abs())
            .fold(0.0_f64, f64::max);
    if !scale32.is_finite()
        || !translation32.iter().all(|c| c.is_finite())
        || !error.is_finite()
        || error > f64::from(support_limits.configured_curve_tolerance_px) / 4.
    {
        return Err("Whole motif root raster affine precision budget exceeded".into());
    }
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).ok_or("Whole motif raster allocation failed")?;
    resvg::render(
        tree,
        tiny_skia::Transform::from_row(
            scale32,
            0.,
            0.,
            scale32,
            translation32[0],
            translation32[1],
        ),
        &mut pixmap.as_mut(),
    );
    let pixels = pixmap.take();
    if pixels.len() != bytes {
        return Err("Whole motif raster byte count mismatch".into());
    }
    // A nontransparent outer edge cannot safely be sampled ClampToEdge. Fail
    // the resource instead of cropping paint or silently accepting a bad guard.
    let w = width as usize;
    let h = height as usize;
    let alpha = |x: usize, y: usize| pixels[(y * w + x) * 4 + 3];
    if (0..w).any(|x| alpha(x, 0) != 0 || alpha(x, h - 1) != 0)
        || (1..h - 1).any(|y| alpha(0, y) != 0 || alpha(w - 1, y) != 0)
    {
        return Err("Whole motif transparent raster guard violated".into());
    }
    let has_coverage = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    Ok(Some(NaturalMotifResource {
        resource_key,
        support: Arc::new(support),
        pixels,
        width,
        height,
        has_coverage,
        bitmap_origin_px: origin,
        texture_pivot_px: origin.map(|c| -c),
        root_raster_cast_error_bound_px: error,
        retained_payload_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn build(
        body: &str,
        raster: MotifRasterLimits,
    ) -> Result<Option<NaturalMotifResource>, String> {
        let svg=format!("<svg xmlns='http://www.w3.org/2000/svg' width='100px' height='100px' viewBox='0 0 100 100'>{body}</svg>");
        let tree = usvg::Tree::from_str(&svg, &Default::default()).unwrap();
        build_natural_motif(
            "test".into(),
            &tree,
            [0., 0.],
            1.,
            Default::default(),
            raster,
        )
    }
    fn alpha(r: &NaturalMotifResource, p: [f64; 2]) -> u8 {
        let x = (p[0] - r.bitmap_origin_px[0]).floor() as usize;
        let y = (p[1] - r.bitmap_origin_px[1]).floor() as usize;
        r.pixels[(y * r.width as usize + x) * 4 + 3]
    }
    #[test]
    fn natural_motif_uses_uncut_graphic_not_viewport_or_repeat_cell() {
        let r = build(
            "<rect x='110' y='-10' width='8' height='4' fill='red'/>",
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!([r.width, r.height], [12, 8]);
        assert_eq!(r.bitmap_origin_px, [108., -12.]);
        assert_eq!(r.texture_pivot_px, [-108., 12.]);
        assert_eq!(r.support.bounds_px, [110., -10., 118., -6.]);
        assert_eq!(alpha(&r, [111., -9.]), 255);
        assert_eq!(alpha(&r, [108., -12.]), 0);
        assert!(r
            .pixels
            .chunks_exact(4)
            .all(|p| p[..3].iter().all(|c| *c <= p[3])));
    }
    #[test]
    fn natural_motif_retains_holes_disconnected_components_and_opacity() {
        let r=build("<path fill-rule='evenodd' fill='red' opacity='.5' d='M0 0H12V12H0Z M4 4H8V8H4Z'/><rect x='20' y='0' width='4' height='4' fill='blue'/>",Default::default()).unwrap().unwrap();
        assert_eq!(alpha(&r, [6., 6.]), 0);
        assert!((127..=128).contains(&alpha(&r, [2., 2.])));
        assert_eq!(alpha(&r, [16., 2.]), 0);
        assert_eq!(alpha(&r, [22., 2.]), 255);
        assert!(r.has_coverage);
    }
    #[test]
    fn geometric_support_does_not_authorize_an_empty_raster() {
        let r = build(
            "<rect x='10.2' y='10.2' width='.00001' height='.00001'/>",
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert!(r.support.triangle_count > 0);
        assert!(!r.has_coverage);
        assert!(r.pixels.chunks_exact(4).all(|p| p[3] == 0));
    }
    #[test]
    fn natural_motif_admission_rejects_whole_resource_without_partial_pixels() {
        let body = "<rect width='8' height='4'/>";
        assert!(build(
            body,
            MotifRasterLimits {
                max_edge: 11,
                ..Default::default()
            }
        )
        .is_err());
        assert!(build(
            body,
            MotifRasterLimits {
                max_rgba_bytes: 383,
                ..Default::default()
            }
        )
        .is_err());
        assert!(build(
            body,
            MotifRasterLimits {
                guard_pixels: 1,
                ..Default::default()
            }
        )
        .is_err());
        assert!(build(
            body,
            MotifRasterLimits {
                max_retained_payload_bytes: 1,
                ..Default::default()
            }
        )
        .is_err());
        assert!(build("<rect width='8' height='4' fill='url(#g)'/><defs><linearGradient id='g'><stop stop-color='red'/><stop offset='1' stop-color='blue'/></linearGradient></defs>",Default::default()).is_err());
        assert!(build(
            "<path d='M0 0L0 0' stroke='black' fill='none'/>",
            Default::default()
        )
        .unwrap()
        .is_none());
    }
}
