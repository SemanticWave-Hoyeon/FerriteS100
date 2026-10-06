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
    /// Resolve the typed source point for a globe coverage decision. Coordinates
    /// are local physical pane pixels; glyph LocalOffset remains independent.
    /// Local mm use device-parallel axes, and a hidden reference stays hidden.
    pub fn project_globe_source(
        origin: &PointOriginGeometry,
        camera: &ferrite_kernel::globe_camera::GlobeCamera,
        pixels_per_mm: f64,
    ) -> Result<Option<[f64; 2]>> {
        use ferrite_kernel::{
            geodesy::GeographicPosition,
            portrayal_position::{AugmentedPointPosition, PortrayalDevice, PositionError},
        };
        let authored = match origin {
            PointOriginGeometry::FeaturePoint(p) => AugmentedPointPosition::Geographic([p.x, p.y]),
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Geographic,
                coordinates,
            } => AugmentedPointPosition::Geographic(*coordinates),
            PointOriginGeometry::AugmentedLocalPoint {
                reference_point,
                millimetres,
            } => AugmentedPointPosition::Local {
                reference_point: [reference_point.x, reference_point.y],
                millimetres: *millimetres,
            },
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Portrayal,
                coordinates,
            } => AugmentedPointPosition::Portrayal {
                millimetres: *coordinates,
            },
            PointOriginGeometry::AugmentedPoint {
                crs: PointOriginCrs::Local,
                ..
            } => {
                return Err(RenderError::Transform(
                    "Local source lacks geographic reference point".into(),
                ))
            }
        };
        let device =
            PortrayalDevice::new([0., camera.viewport()[1]], [pixels_per_mm, pixels_per_mm])
                .map_err(|e| RenderError::Transform(e.to_string()))?;
        let mut projection_error = None;
        let resolved = device.resolve(authored, |p| {
            let projected = (|| -> Result<Option<[f64; 2]>> {
                if !p[0].is_finite() || p[0].abs() > 1e9 {
                    return Err(RenderError::Transform(
                        "Invalid geographic longitude".into(),
                    ));
                }
                let position = GeographicPosition::new(p[1], (p[0] + 180.).rem_euclid(360.) - 180.)
                    .map_err(|e| RenderError::Transform(e.to_string()))?;
                let ecef = position
                    .to_ecef(0.)
                    .map_err(|e| RenderError::Transform(e.to_string()))?;
                camera
                    .project_visible(ecef)
                    .map(|p| p.map(|p| p.screen_px))
                    .map_err(|e| RenderError::Transform(e.to_string()))
            })();
            match projected {
                Ok(p) => Ok(p),
                Err(error) => {
                    projection_error = Some(error);
                    Err(PositionError("Globe source projection failed"))
                }
            }
        });
        if let Some(error) = projection_error {
            return Err(error);
        }
        resolved.map_err(|e| RenderError::Transform(e.to_string()))
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
mod globe_source_tests {
    use super::*;
    use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera};
    fn camera(range: f64, heading: f64, tilt: f64) -> GlobeCamera {
        GlobeCamera::orbit(
            GeographicPosition::new(48., 179.99).unwrap(),
            range,
            heading,
            tilt,
            [640., 480.],
            45.,
            3.,
            1e9,
        )
        .unwrap()
    }
    #[test]
    fn physical_local_source_is_independent_of_zoom_heading_and_density() {
        let anchor = WorldPoint::new(179.99, 48.);
        for density in [1., 1.25, 2., 3.] {
            let ppm = 96. * density / 25.4;
            for range in [150., 30000., 3_000_000.] {
                for (heading, tilt) in [(0., 0.), (90., 70.), (135., 45.)] {
                    let c = camera(range, heading, tilt);
                    let source = PortrayalOrigin::project_globe_source(
                        &PointOriginGeometry::FeaturePoint(anchor),
                        &c,
                        ppm,
                    )
                    .unwrap()
                    .unwrap();
                    let local = PortrayalOrigin::project_globe_source(
                        &PointOriginGeometry::AugmentedLocalPoint {
                            reference_point: anchor,
                            millimetres: [3.2, -1.5],
                        },
                        &c,
                        ppm,
                    )
                    .unwrap()
                    .unwrap();
                    assert!((local[0] - source[0] - 3.2 * ppm).abs() < 1e-9);
                    assert!((local[1] - source[1] - 1.5 * ppm).abs() < 1e-9);
                }
            }
        }
    }
    #[test]
    fn device_coordinates_do_not_follow_camera_or_horizon() {
        let p = PointOriginGeometry::AugmentedPoint {
            crs: PointOriginCrs::Portrayal,
            coordinates: [5., 7.],
        };
        for ppm in [96. / 25.4, 192. / 25.4] {
            for (heading, tilt) in [(0., 0.), (90., 70.)] {
                assert_eq!(
                    PortrayalOrigin::project_globe_source(&p, &camera(30000., heading, tilt), ppm)
                        .unwrap(),
                    Some([5. * ppm, 480. - 7. * ppm])
                );
            }
        }
    }
    #[test]
    fn rear_local_remains_hidden_and_errors_keep_the_original_cause() {
        let c = camera(30000., 0., 0.);
        let rear = WorldPoint::new(0., -48.);
        assert_eq!(
            PortrayalOrigin::project_globe_source(
                &PointOriginGeometry::AugmentedLocalPoint {
                    reference_point: rear,
                    millimetres: [1e6, 1e6]
                },
                &c,
                4.
            )
            .unwrap(),
            None
        );
        let bad = PointOriginGeometry::FeaturePoint(WorldPoint::new(179., 100.));
        assert!(PortrayalOrigin::project_globe_source(&bad, &c, 4.)
            .unwrap_err()
            .to_string()
            .contains("Invalid WGS84 latitude"));
        let unsupported = PointOriginGeometry::AugmentedPoint {
            crs: PointOriginCrs::Local,
            coordinates: [0., 0.],
        };
        assert!(PortrayalOrigin::project_globe_source(&unsupported, &c, 4.).is_err());
        let overflow = PointOriginGeometry::AugmentedPoint {
            crs: PointOriginCrs::Portrayal,
            coordinates: [f64::MAX, 1.],
        };
        assert!(PortrayalOrigin::project_globe_source(&overflow, &c, 4.).is_err());
        for ppm in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(PortrayalOrigin::project_globe_source(&bad, &c, ppm).is_err());
        }
    }
    #[test]
    fn geographic_dateline_copies_share_one_globe_position() {
        let c = camera(30000., 35., 50.);
        let a = PointOriginGeometry::FeaturePoint(WorldPoint::new(179.99, 48.));
        let b = PointOriginGeometry::AugmentedPoint {
            crs: PointOriginCrs::Geographic,
            coordinates: [-180.01, 48.],
        };
        let a = PortrayalOrigin::project_globe_source(&a, &c, 4.)
            .unwrap()
            .unwrap();
        let b = PortrayalOrigin::project_globe_source(&b, &c, 4.)
            .unwrap()
            .unwrap();
        assert!((a[0] - b[0]).abs() < 1e-7 && (a[1] - b[1]).abs() < 1e-7);
    }
}
