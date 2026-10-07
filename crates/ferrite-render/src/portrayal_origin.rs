//! Persistent source geometry for S-98 E-1.5 coverage visibility.
//! The authored geometry survives sorting, expansion and instruction caching;
//! it is projected by the view, never inferred from a rendered symbol's extent.
use crate::error::Result;
use crate::{RenderError, WorldPoint};
use ferrite_kernel::coverage_rendering::{
    coverage_render_policy, CoverageRenderPolicy, InstructionOrigin,
};
use ferrite_kernel::coverage_selection::Region;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointOriginCrs {
    Geographic,
    Local,
    Portrayal,
}
impl PointOriginCrs {
    pub fn from_lua(value: &str) -> Result<Self> {
        Ok(match value {
            "GeographicCRS" => Self::Geographic,
            "LocalCRS" => Self::Local,
            "PortrayalCRS" => Self::Portrayal,
            _ => {
                return Err(RenderError::Transform(format!(
                    "Unsupported augmented point CRS: {value}"
                )))
            }
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PointOriginGeometry {
    FeaturePoint(WorldPoint),
    AugmentedPoint {
        crs: PointOriginCrs,
        coordinates: [f64; 2],
    },
    /// S-100 9-11.1.13: an augmented Local point requires the actual point
    /// feature anchor. Authored millimetres remain independent of glyph offsets.
    AugmentedLocalPoint {
        reference_point: WorldPoint,
        millimetres: [f64; 2],
    },
}
#[derive(Debug, Clone, Default, PartialEq)]
pub enum PortrayalOrigin {
    /// No product adapter has assigned a source. Coverage activation rejects it.
    #[default]
    Unspecified,
    NonPoint,
    /// Shared across the primitives produced by a command, rather than repeated
    /// heap allocations or coordinate copies for every expanded primitive.
    Point(Arc<PointOriginGeometry>),
    /// Explicit host overlay; never assigned by a product adapter.
    CoverageExempt,
}
#[derive(Serialize, Deserialize)]
enum WireOrigin {
    Unspecified,
    NonPoint,
    Point(PointOriginGeometry),
    CoverageExempt,
}
impl Serialize for PortrayalOrigin {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::CoverageExempt => WireOrigin::CoverageExempt.serialize(serializer),
            Self::Unspecified => WireOrigin::Unspecified.serialize(serializer),
            Self::NonPoint => WireOrigin::NonPoint.serialize(serializer),
            Self::Point(point) => WireOrigin::Point((**point).clone()).serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for PortrayalOrigin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let origin = match WireOrigin::deserialize(deserializer)? {
            WireOrigin::CoverageExempt => Self::CoverageExempt,
            WireOrigin::Unspecified => Self::Unspecified,
            WireOrigin::NonPoint => Self::NonPoint,
            WireOrigin::Point(PointOriginGeometry::FeaturePoint(point)) => {
                Self::feature_point(point).map_err(serde::de::Error::custom)?
            }
            WireOrigin::Point(PointOriginGeometry::AugmentedPoint { crs, coordinates }) => {
                Self::augmented_point(crs, coordinates).map_err(serde::de::Error::custom)?
            }
            WireOrigin::Point(PointOriginGeometry::AugmentedLocalPoint {
                reference_point,
                millimetres,
            }) => Self::augmented_local_point(reference_point, millimetres)
                .map_err(serde::de::Error::custom)?,
        };
        Ok(origin)
    }
}
impl PortrayalOrigin {
    /// Physical augmented origins cannot inherit a cached geographic affine
    /// visibility decision: zoom moves their geographic anchor, not their mm.
    pub fn requires_view_reprojection(&self) -> bool {
        matches!(self, Self::Point(point) if matches!(point.as_ref(),
            PointOriginGeometry::AugmentedLocalPoint { .. }
            | PointOriginGeometry::AugmentedPoint { crs: PointOriginCrs::Local | PointOriginCrs::Portrayal, .. }))
    }
    /// The host uses the chart viewport's lower-left corner as the explicit
    /// Portrayal origin. Authored axes are mm and +Y up; output pixels are +Y down.
    pub fn is_device_fixed(&self) -> bool {
        matches!(self, Self::Point(p) if matches!(p.as_ref(),
            PointOriginGeometry::AugmentedPoint { crs: PointOriginCrs::Portrayal, .. }))
    }
    /// Return authored device coordinates through a host-selected device.
    /// A non-device origin returns None without invoking a geographic projector.
    pub fn device_pixel_position(
        &self,
        device: ferrite_kernel::portrayal_position::PortrayalDevice,
    ) -> Result<Option<[f64; 2]>> {
        let Self::Point(p) = self else {
            return Ok(None);
        };
        let PointOriginGeometry::AugmentedPoint {
            crs: PointOriginCrs::Portrayal,
            coordinates,
        } = p.as_ref()
        else {
            return Ok(None);
        };
        device
            .resolve(
                ferrite_kernel::portrayal_position::AugmentedPointPosition::Portrayal {
                    millimetres: *coordinates,
                },
                |_| {
                    Err(ferrite_kernel::portrayal_position::PositionError(
                        "Device point must not invoke geographic projection",
                    ))
                },
            )
            .map_err(|e| RenderError::Transform(e.to_string()))
    }
    pub fn flat_glyph_anchor(
        &self,
        geographic: WorldPoint,
        scaler: &crate::Scaler,
    ) -> Result<crate::ScreenPoint> {
        if self.is_device_fixed() {
            self.flat_source_position(scaler, 0.)?
                .ok_or_else(|| RenderError::Transform("Missing Portrayal device anchor".into()))
        } else {
            let p = scaler.world_to_screen(geographic);
            if !p.x.is_finite() || !p.y.is_finite() {
                return Err(RenderError::Transform("Non-finite glyph anchor".into()));
            }
            Ok(p)
        }
    }
    /// Project the source point for coverage, independently from glyph offsets.
    /// Device points exist exactly once, never in +/-360 degree copies.
    pub fn flat_source_position(
        &self,
        scaler: &crate::Scaler,
        shift: f64,
    ) -> Result<Option<crate::ScreenPoint>> {
        let Self::Point(p) = self else {
            return Err(RenderError::Transform(
                "Point source resolver requires point origin".into(),
            ));
        };
        Self::project_flat_source(p, scaler, shift)
    }
    /// Borrow the persistent metadata without allocating a new Arc per view.
    pub fn project_flat_source(
        p: &PointOriginGeometry,
        scaler: &crate::Scaler,
        shift: f64,
    ) -> Result<Option<crate::ScreenPoint>> {
        Self::project_flat_source_with_northing(p, scaler, shift, None)
    }
    /// Same source conversion, optionally reusing an opaque exact projection result.
    pub fn project_flat_source_with_northing(
        p: &PointOriginGeometry,
        scaler: &crate::Scaler,
        shift: f64,
        prepared: Option<&ferrite_kernel::map_camera::PreparedFlatNorthing>,
    ) -> Result<Option<crate::ScreenPoint>> {
        use ferrite_kernel::portrayal_position::{AugmentedPointPosition, PortrayalDevice};
        if !shift.is_finite() {
            return Err(RenderError::Transform("Non-finite longitude shift".into()));
        }
        let authored = match p {
            PointOriginGeometry::FeaturePoint(p) => {
                AugmentedPointPosition::Geographic([p.x + shift, p.y])
            }
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Geographic,
                coordinates,
            } => AugmentedPointPosition::Geographic([coordinates[0] + shift, coordinates[1]]),
            PointOriginGeometry::AugmentedLocalPoint {
                reference_point,
                millimetres,
            } => AugmentedPointPosition::Local {
                reference_point: [reference_point.x + shift, reference_point.y],
                millimetres: *millimetres,
            },
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Portrayal,
                coordinates,
            } => {
                if shift != 0. {
                    return Ok(None);
                }
                AugmentedPointPosition::Portrayal {
                    millimetres: *coordinates,
                }
            }
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Local,
                ..
            } => {
                return Err(RenderError::Transform(
                    "Local source lacks geographic reference point".into(),
                ))
            }
        };
        let v = scaler.viewport;
        let density = scaler.pixels_per_mm();
        let device = PortrayalDevice::new(
            [v.x as f64, v.y as f64 + v.height as f64],
            [density, density],
        )
        .map_err(|e| RenderError::Transform(e.to_string()))?;
        let resolved = device
            .resolve(authored, |p| {
                let world = WorldPoint::new(p[0], p[1]);
                let p = match prepared {
                    Some(value) => scaler.world_to_screen_with_prepared_northing(world, value),
                    None => scaler.world_to_screen(world),
                };
                Ok(Some([p.x as f64, p.y as f64]))
            })
            .map_err(|e| RenderError::Transform(e.to_string()))?;
        resolved
            .map(|p| {
                let p = crate::ScreenPoint::new(p[0] as f32, p[1] as f32);
                if !p.x.is_finite() || !p.y.is_finite() {
                    return Err(RenderError::Transform(
                        "Physical point exceeds screen coordinate range".into(),
                    ));
                }
                Ok(p)
            })
            .transpose()
    }
    pub fn feature_point(point: WorldPoint) -> Result<Self> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(RenderError::InvalidCoordinate(
                "Non-finite feature origin".into(),
            ));
        }
        Ok(Self::Point(Arc::new(PointOriginGeometry::FeaturePoint(
            point,
        ))))
    }
    pub fn augmented_point(crs: PointOriginCrs, coordinates: [f64; 2]) -> Result<Self> {
        if !coordinates.iter().all(|v| v.is_finite()) {
            return Err(RenderError::InvalidCoordinate(
                "Non-finite augmented point origin".into(),
            ));
        }
        Ok(Self::Point(Arc::new(PointOriginGeometry::AugmentedPoint {
            crs,
            coordinates,
        })))
    }
    pub fn augmented_local_point(
        reference_point: WorldPoint,
        millimetres: [f64; 2],
    ) -> Result<Self> {
        if ![
            reference_point.x,
            reference_point.y,
            millimetres[0],
            millimetres[1],
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return Err(RenderError::InvalidCoordinate(
                "Non-finite augmented Local origin".into(),
            ));
        }
        Ok(Self::Point(Arc::new(
            PointOriginGeometry::AugmentedLocalPoint {
                reference_point,
                millimetres,
            },
        )))
    }
    /// A view supplies the CRS conversion and visibility/horizon test. None
    /// means the source point cannot be seen in that view (for example the far
    /// side of a globe). Physical symbol offsets never move the source point.
    pub fn render_policy(
        &self,
        obscuring: &Region,
        project: impl FnOnce(&PointOriginGeometry) -> Result<Option<[f64; 2]>>,
    ) -> Result<CoverageRenderPolicy> {
        match self {
            Self::CoverageExempt => Ok(CoverageRenderPolicy::RenderPointUnclipped),
            Self::Unspecified => Err(RenderError::Render(
                "Missing portrayal source geometry for coverage selection".into(),
            )),
            Self::NonPoint => coverage_render_policy(obscuring, InstructionOrigin::NonPoint)
                .map_err(|e| RenderError::Transform(e.to_string())),
            Self::Point(point) => match project(point)? {
                Some(position) => {
                    coverage_render_policy(obscuring, InstructionOrigin::Point(position))
                        .map_err(|e| RenderError::Transform(e.to_string()))
                }
                None => Ok(CoverageRenderPolicy::HidePoint),
            },
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn mask() -> Region {
        Region::from_rings(&[[0., 0.], [2., 0.], [2., 2.], [0., 2.], [0., 0.]], &[]).unwrap()
    }
    #[test]
    fn shared_geometry_survives_cache_and_uses_source_projection() {
        let origin = PortrayalOrigin::feature_point(WorldPoint::new(127., 35.)).unwrap();
        let clone = origin.clone();
        if let (PortrayalOrigin::Point(a), PortrayalOrigin::Point(b)) = (&origin, &clone) {
            assert!(Arc::ptr_eq(a, b));
        } else {
            panic!()
        }
        let decoded: PortrayalOrigin =
            bincode::deserialize(&bincode::serialize(&origin).unwrap()).unwrap();
        assert_eq!(decoded, origin);
        assert_eq!(
            origin
                .render_policy(&mask(), |_| Ok(Some([1., 1.])))
                .unwrap(),
            CoverageRenderPolicy::HidePoint
        );
        assert_eq!(
            origin
                .render_policy(&mask(), |_| Ok(Some([3., 1.])))
                .unwrap(),
            CoverageRenderPolicy::RenderPointUnclipped
        );
        assert_eq!(
            origin.render_policy(&mask(), |_| Ok(None)).unwrap(),
            CoverageRenderPolicy::HidePoint
        );
        assert!(std::mem::size_of::<PortrayalOrigin>() <= 16);
    }
    #[test]
    fn augmented_crs_remains_authored_and_missing_metadata_cannot_bypass_masking() {
        let origin = PortrayalOrigin::augmented_point(PointOriginCrs::Local, [3.2, 0.]).unwrap();
        assert_eq!(
            origin
                .render_policy(&mask(), |p| {
                    assert_eq!(
                        p,
                        &PointOriginGeometry::AugmentedPoint {
                            crs: PointOriginCrs::Local,
                            coordinates: [3.2, 0.]
                        }
                    );
                    Ok(Some([1., 1.]))
                })
                .unwrap(),
            CoverageRenderPolicy::HidePoint
        );
        assert!(PortrayalOrigin::Unspecified
            .render_policy(&mask(), |_| Ok(None))
            .is_err());
        assert_eq!(
            PortrayalOrigin::NonPoint
                .render_policy(&mask(), |_| panic!(
                    "nonpoint must not be projected as an anchor"
                ))
                .unwrap(),
            CoverageRenderPolicy::ClipNonPoint
        );
        assert!(
            PortrayalOrigin::augmented_point(PointOriginCrs::Geographic, [f64::NAN, 0.]).is_err()
        );
        assert!(PointOriginCrs::from_lua("inventedCRS").is_err());
    }
}

#[cfg(test)]
mod anchored_local_tests {
    use super::*;
    #[test]
    fn physical_local_origin_roundtrips_without_losing_reference_point() {
        let origin =
            PortrayalOrigin::augmented_local_point(WorldPoint::new(127., 35.), [3.2, -1.5])
                .unwrap();
        let decoded: PortrayalOrigin =
            bincode::deserialize(&bincode::serialize(&origin).unwrap()).unwrap();
        assert_eq!(decoded, origin);
        assert!(
            PortrayalOrigin::augmented_local_point(WorldPoint::new(f64::NAN, 35.), [0., 0.])
                .is_err()
        );
        assert!(PortrayalOrigin::augmented_local_point(
            WorldPoint::new(127., 35.),
            [f64::INFINITY, 0.]
        )
        .is_err());
        assert!(std::mem::size_of::<PortrayalOrigin>() <= 16);
    }
}

#[cfg(test)]
mod device_point_tests {
    use super::*;
    use crate::{GeoBounds, Scaler, Viewport};
    #[test]
    fn device_source_remains_fixed_across_zoom_pan_projection_density_and_viewport() {
        let origin = PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal, [8., 3.]).unwrap();
        assert!(origin.is_device_fixed());
        for density in [1., 1.25, 1.5, 2., 3.] {
            for zoom in [0.01_f64, 1., 4., 200.] {
                for pan in [-150., 0., 150.] {
                    for projection in [
                        crate::FlatProjection::LocalGeographic,
                        crate::FlatProjection::EllipsoidalMercator,
                    ] {
                        let viewport = Viewport {
                            x: 37.,
                            y: 83.,
                            width: 800.,
                            height: 600.,
                        };
                        let mut scaler = Scaler::new(
                            GeoBounds::new(
                                pan - 1. / zoom,
                                40. - 1. / zoom.min(1.),
                                pan + 1. / zoom,
                                40. + 1. / zoom.min(1.),
                            ),
                            viewport,
                        );
                        scaler.set_projection(projection);
                        scaler.set_pixel_ratio(density);
                        let p = origin.flat_source_position(&scaler, 0.).unwrap().unwrap();
                        assert!((p.x as f64 - (37. + 8. * scaler.pixels_per_mm())).abs() < 1e-4);
                        assert!((p.y as f64 - (683. - 3. * scaler.pixels_per_mm())).abs() < 1e-4);
                        assert_eq!(
                            origin
                                .flat_glyph_anchor(WorldPoint::new(f64::NAN, f64::NAN), &scaler)
                                .unwrap(),
                            p
                        );
                        assert!(origin
                            .flat_source_position(&scaler, -360.)
                            .unwrap()
                            .is_none());
                        assert!(origin
                            .flat_source_position(&scaler, 360.)
                            .unwrap()
                            .is_none());
                    }
                }
            }
        }
    }
    #[test]
    fn device_overflow_and_unanchored_local_are_rejected() {
        let s = Scaler::new(GeoBounds::new(-1., -1., 1., 1.), Viewport::new(800., 600.));
        let overflow =
            PortrayalOrigin::augmented_point(PointOriginCrs::Portrayal, [f64::MAX, 0.]).unwrap();
        assert!(overflow.flat_source_position(&s, 0.).is_err());
        let missing = PortrayalOrigin::augmented_point(PointOriginCrs::Local, [1., 2.]).unwrap();
        assert!(missing.flat_source_position(&s, 0.).is_err());
    }
}

#[cfg(test)]
mod northing_source_conversion_tests {
    use super::*;
    use crate::{FlatProjection, GeoBounds, Scaler, Viewport};
    fn result_bits(
        result: Result<Option<crate::ScreenPoint>>,
    ) -> std::result::Result<Option<[u32; 2]>, String> {
        result
            .map(|value| value.map(|p| [p.x.to_bits(), p.y.to_bits()]))
            .map_err(|e| e.to_string())
    }
    fn legacy_project_flat_source(
        p: &PointOriginGeometry,
        scaler: &crate::Scaler,
        shift: f64,
    ) -> Result<Option<crate::ScreenPoint>> {
        use ferrite_kernel::portrayal_position::{AugmentedPointPosition, PortrayalDevice};
        if !shift.is_finite() {
            return Err(RenderError::Transform("Non-finite longitude shift".into()));
        }
        let authored = match p {
            PointOriginGeometry::FeaturePoint(p) => {
                AugmentedPointPosition::Geographic([p.x + shift, p.y])
            }
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Geographic,
                coordinates,
            } => AugmentedPointPosition::Geographic([coordinates[0] + shift, coordinates[1]]),
            PointOriginGeometry::AugmentedLocalPoint {
                reference_point,
                millimetres,
            } => AugmentedPointPosition::Local {
                reference_point: [reference_point.x + shift, reference_point.y],
                millimetres: *millimetres,
            },
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Portrayal,
                coordinates,
            } => {
                if shift != 0. {
                    return Ok(None);
                }
                AugmentedPointPosition::Portrayal {
                    millimetres: *coordinates,
                }
            }
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Local,
                ..
            } => {
                return Err(RenderError::Transform(
                    "Local source lacks geographic reference point".into(),
                ))
            }
        };
        let v = scaler.viewport;
        let density = scaler.pixels_per_mm();
        let device = PortrayalDevice::new(
            [v.x as f64, v.y as f64 + v.height as f64],
            [density, density],
        )
        .map_err(|e| RenderError::Transform(e.to_string()))?;
        let resolved = device
            .resolve(authored, |p| {
                let p = scaler.world_to_screen(WorldPoint::new(p[0], p[1]));
                Ok(Some([p.x as f64, p.y as f64]))
            })
            .map_err(|e| RenderError::Transform(e.to_string()))?;
        resolved
            .map(|p| {
                let p = crate::ScreenPoint::new(p[0] as f32, p[1] as f32);
                if !p.x.is_finite() || !p.y.is_finite() {
                    return Err(RenderError::Transform(
                        "Physical point exceeds screen coordinate range".into(),
                    ));
                }
                Ok(p)
            })
            .transpose()
    }
    #[test]
    fn legacy_physical_crs_wrap_rounding_and_errors_remain_exact() {
        for projection in [
            FlatProjection::LocalGeographic,
            FlatProjection::EllipsoidalMercator,
        ] {
            for ratio in [1., 1.5, 2.] {
                let mut scaler = crate::RenderContext::new(Viewport::new(1280., 852.)).scaler;
                scaler.set_bounds(GeoBounds::new(-5., 45., 5., 55.));
                scaler.set_projection(projection);
                scaler.set_pixel_ratio(ratio);
                for lat in [
                    -90.,
                    90.,
                    f64::from_bits(90f64.to_bits() - 1),
                    f64::from_bits(90f64.to_bits() + 1),
                    f64::from_bits((-90f64).to_bits() - 1),
                    f64::from_bits((-90f64).to_bits() + 1),
                    -89.5,
                    -0.,
                    0.,
                    48.65,
                    89.5,
                    f64::NAN,
                    91.,
                ] {
                    for bounds in [
                        GeoBounds::new(-5., 45., 5., 55.),
                        GeoBounds::new(-4.2, 48.1, -4.1, 48.2),
                        GeoBounds::new(178., 45., 180., 50.),
                    ] {
                        scaler.zoom_to_fit(bounds);
                        let origins = [
                            PointOriginGeometry::FeaturePoint(crate::WorldPoint::new(179., lat)),
                            PointOriginGeometry::AugmentedPoint {
                                crs: PointOriginCrs::Geographic,
                                coordinates: [179., lat],
                            },
                            PointOriginGeometry::AugmentedLocalPoint {
                                reference_point: crate::WorldPoint::new(179., lat),
                                millimetres: [3.2, -1.5],
                            },
                            PointOriginGeometry::AugmentedPoint {
                                crs: PointOriginCrs::Portrayal,
                                coordinates: [3.2, -1.5],
                            },
                            PointOriginGeometry::AugmentedPoint {
                                crs: PointOriginCrs::Local,
                                coordinates: [3.2, -1.5],
                            },
                        ];
                        let prepared = scaler.prepare_flat_northing(lat).ok();
                        for origin in &origins {
                            for shift in [-360., 0., 360., f64::NAN] {
                                assert_eq!(
                                    result_bits(legacy_project_flat_source(origin, &scaler, shift)),
                                    result_bits(
                                        PortrayalOrigin::project_flat_source_with_northing(
                                            origin,
                                            &scaler,
                                            shift,
                                            prepared.as_ref()
                                        )
                                    )
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
