//! S-98 2.0.0 E-1.5 distinguishes the instruction's source geometry from
//! its rendered primitive. A symbol or text placed on a curve is not thereby
//! a point-origin instruction. An augmented point overrides feature geometry.
use crate::coverage_selection::Region;
use anyhow::{ensure, Result};
use geo::{Intersects, Point};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InstructionOrigin {
    NonPoint,
    /// Device-projected augmented point or referenced feature point.
    Point([f64; 2]),
}
impl InstructionOrigin {
    /// `feature_point` is present only when the reference geometry is a point.
    /// Its absence must not be inferred from the rendering primitive's type.
    pub fn resolve(augmented_point: Option<[f64; 2]>, feature_point: Option<[f64; 2]>) -> Self {
        augmented_point
            .or(feature_point)
            .map_or(Self::NonPoint, Self::Point)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageRenderPolicy {
    /// Discard fragments intersecting the dataset's obscuring mask.
    ClipNonPoint,
    /// Retain the whole portrayal including its extent across mask boundaries.
    RenderPointUnclipped,
    HidePoint,
}
/// Boundary contact is intersection: a point on an obscuring polygon's exterior
/// or hole boundary is occluded. A point strictly inside a hole remains visible.
/// Use the exact projected dataset obscuring region, not a rounded pixel mask,
/// so subpixel source points cannot change visibility with mask raster rounding.
pub fn coverage_render_policy(
    obscuring: &Region,
    origin: InstructionOrigin,
) -> Result<CoverageRenderPolicy> {
    match origin {
        InstructionOrigin::NonPoint => Ok(CoverageRenderPolicy::ClipNonPoint),
        InstructionOrigin::Point([x, y]) => {
            ensure!(
                x.is_finite() && y.is_finite(),
                "Non-finite projected instruction origin"
            );
            let point = Point::new(x, y);
            Ok(
                if obscuring
                    .polygons()
                    .iter()
                    .any(|polygon| polygon.intersects(&point))
                {
                    CoverageRenderPolicy::HidePoint
                } else {
                    CoverageRenderPolicy::RenderPointUnclipped
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mask() -> Region {
        Region::from_rings(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
        .unwrap()
    }
    #[test]
    fn point_origin_controls_visibility_including_holes_and_boundaries() {
        let mask = mask();
        for point in [[2., 2.], [0., 5.], [4., 5.], [10., 10.]] {
            assert_eq!(
                coverage_render_policy(&mask, InstructionOrigin::Point(point)).unwrap(),
                CoverageRenderPolicy::HidePoint
            );
        }
        for point in [[5., 5.], [-0.00001, 5.], [10.00001, 5.]] {
            assert_eq!(
                coverage_render_policy(&mask, InstructionOrigin::Point(point)).unwrap(),
                CoverageRenderPolicy::RenderPointUnclipped
            );
        }
        assert_eq!(
            coverage_render_policy(&mask, InstructionOrigin::NonPoint).unwrap(),
            CoverageRenderPolicy::ClipNonPoint
        );
        assert!(coverage_render_policy(&mask, InstructionOrigin::Point([f64::NAN, 0.])).is_err());
    }
    #[test]
    fn augmented_point_overrides_reference_geometry_and_output_type_is_irrelevant() {
        let mask = mask();
        let origin = InstructionOrigin::resolve(Some([5., 5.]), Some([2., 2.]));
        assert_eq!(
            coverage_render_policy(&mask, origin).unwrap(),
            CoverageRenderPolicy::RenderPointUnclipped
        );
        assert_eq!(
            InstructionOrigin::resolve(None, Some([2., 2.])),
            InstructionOrigin::Point([2., 2.])
        );
        assert_eq!(
            InstructionOrigin::resolve(None, None),
            InstructionOrigin::NonPoint
        );
        assert_eq!(
            InstructionOrigin::resolve(Some([2., 2.]), None),
            InstructionOrigin::Point([2., 2.])
        );
    }
}
