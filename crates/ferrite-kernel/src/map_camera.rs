//! Product-neutral screen/geographic camera adapter. Rendering, navigation
//! ownership, portrayal and application menus stay outside this module.
use crate::geodesy::{GeographicPosition, Mercator, WGS84_A};
use anyhow::{ensure, Result};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AngularProjection {
    Geographic,
    EllipsoidalMercator,
}
impl AngularProjection {
    fn northing_identity(self) -> [u64; 3] {
        // The only non-angular projection supported by this camera is fixed
        // Mercator::World on WGS84. No zone, custom ellipsoid or datum is implicit.
        [
            match self {
                Self::Geographic => 0,
                Self::EllipsoidalMercator => 1,
            },
            crate::geodesy::WGS84_A.to_bits(),
            crate::geodesy::WGS84_F.to_bits(),
        ]
    }
    pub fn northing(self, lat: f64) -> Result<f64> {
        ensure!(lat.is_finite(), "Non-finite camera latitude");
        if self == Self::Geographic {
            return Ok(lat);
        }
        Ok((Mercator::World.project(GeographicPosition::new(lat, 0.)?)?[1] / WGS84_A).to_degrees())
    }
    pub fn latitude(self, q: f64) -> Result<f64> {
        ensure!(q.is_finite(), "Non-finite camera northing");
        if self == Self::Geographic {
            return Ok(q);
        }
        Ok(Mercator::World
            .unproject([0., q.to_radians() * WGS84_A])?
            .latitude())
    }
}
/// Immutable output of the exact current projection, tied to authored latitude bits.
/// Not a coverage permission or numeric error certificate. Constructor is private.
#[derive(Debug, Clone, Copy)]
pub struct PreparedFlatNorthing {
    projection_identity: [u64; 3],
    latitude_bits: u64,
    northing: f64,
}
#[derive(Debug, Clone)]
pub struct FlatMapCamera {
    projection: AngularProjection,
    origin: [f64; 2],
    scale: [f64; 2],
    offset: [f64; 2],
}
impl FlatMapCamera {
    pub fn new(
        projection: AngularProjection,
        geographic_origin: [f64; 2],
        scale: [f64; 2],
        offset: [f64; 2],
    ) -> Result<Self> {
        ensure!(
            geographic_origin
                .iter()
                .chain(&scale)
                .chain(&offset)
                .all(|v| v.is_finite())
                && scale.iter().all(|v| *v > 0.),
            "Invalid flat camera transform"
        );
        Ok(Self {
            projection,
            origin: [
                geographic_origin[0],
                projection.northing(geographic_origin[1])?,
            ],
            scale,
            offset,
        })
    }
    /// Exact encoded camera identity, not a numerical error certificate.
    pub fn encoded_identity(&self) -> [u64; 7] {
        [
            match self.projection {
                AngularProjection::Geographic => 0,
                AngularProjection::EllipsoidalMercator => 1,
            },
            self.origin[0].to_bits(),
            self.origin[1].to_bits(),
            self.scale[0].to_bits(),
            self.scale[1].to_bits(),
            self.offset[0].to_bits(),
            self.offset[1].to_bits(),
        ]
    }
    fn project(&self, point: [f64; 2]) -> Result<[f64; 2]> {
        ensure!(
            point.iter().all(|v| v.is_finite()),
            "Non-finite camera position"
        );
        let p = [
            (point[0] - self.origin[0]) * self.scale[0] + self.offset[0],
            (self.origin[1] - self.projection.northing(point[1])?) * self.scale[1] + self.offset[1],
        ];
        ensure!(
            p.iter().all(|v| v.is_finite()),
            "Camera screen coordinate overflow"
        );
        Ok(p)
    }
    fn prepare_northing(&self, latitude: f64) -> Result<PreparedFlatNorthing> {
        let northing = self.projection.northing(latitude)?;
        ensure!(northing.is_finite(), "Non-finite prepared northing");
        Ok(PreparedFlatNorthing {
            projection_identity: self.projection.northing_identity(),
            latitude_bits: latitude.to_bits(),
            northing,
        })
    }
    fn project_prepared(
        &self,
        point: [f64; 2],
        prepared: &PreparedFlatNorthing,
    ) -> Result<[f64; 2]> {
        ensure!(
            prepared.projection_identity == self.projection.northing_identity()
                && prepared.latitude_bits == point[1].to_bits(),
            "Prepared northing projection/source mismatch"
        );
        ensure!(
            point.iter().all(|v| v.is_finite()),
            "Non-finite camera position"
        );
        // Exact same operation order as project; only northing evaluation is retained.
        let p = [
            (point[0] - self.origin[0]) * self.scale[0] + self.offset[0],
            (self.origin[1] - prepared.northing) * self.scale[1] + self.offset[1],
        ];
        ensure!(
            p.iter().all(|v| v.is_finite()),
            "Camera screen coordinate overflow"
        );
        Ok(p)
    }
    fn unproject(&self, point: [f64; 2]) -> Result<[f64; 2]> {
        ensure!(
            point.iter().all(|v| v.is_finite()),
            "Non-finite screen position"
        );
        let p = [
            (point[0] - self.offset[0]) / self.scale[0] + self.origin[0],
            self.projection
                .latitude(self.origin[1] - (point[1] - self.offset[1]) / self.scale[1])?,
        ];
        ensure!(
            p.iter().all(|v| v.is_finite()),
            "Camera geographic coordinate overflow"
        );
        Ok(p)
    }
}
#[derive(Debug, Clone)]
pub enum MapCamera {
    Flat(FlatMapCamera),
}
impl MapCamera {
    /// Preserve longitude copies using the selected 2D projection.
    pub fn project(&self, point: [f64; 2]) -> Result<Option<[f64; 2]>> {
        let Self::Flat(camera) = self;
        Ok(Some(camera.project(point)?))
    }
    pub fn prepare_flat_northing(&self, latitude: f64) -> Result<PreparedFlatNorthing> {
        let Self::Flat(camera) = self;
        camera.prepare_northing(latitude)
    }
    pub fn project_with_prepared_northing(
        &self,
        point: [f64; 2],
        prepared: &PreparedFlatNorthing,
    ) -> Result<Option<[f64; 2]>> {
        let Self::Flat(camera) = self;
        Ok(Some(camera.project_prepared(point, prepared)?))
    }
    pub fn unproject(&self, point: [f64; 2]) -> Result<Option<[f64; 2]>> {
        let Self::Flat(camera) = self;
        Ok(Some(camera.unproject(point)?))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flat_camera_keeps_longitude_copies_and_cached_northing_roundtrip() {
        for projection in [
            AngularProjection::Geographic,
            AngularProjection::EllipsoidalMercator,
        ] {
            let c = MapCamera::Flat(
                FlatMapCamera::new(projection, [-185., 80.], [123., 123.], [17., 83.]).unwrap(),
            );
            for lon in [-181., -2., 179., 539.] {
                for lat in [-80., 0., 48.65, 80.] {
                    let p = c.project([lon, lat]).unwrap().unwrap();
                    let back = c.unproject(p).unwrap().unwrap();
                    assert!((back[0] - lon).abs() < 1e-11 && (back[1] - lat).abs() < 1e-11);
                }
            }
        }
        assert!(FlatMapCamera::new(
            AngularProjection::EllipsoidalMercator,
            [0., 90.],
            [1., 1.],
            [0., 0.]
        )
        .is_err());
    }
}

#[cfg(test)]
mod retained_northing_tests {
    use super::*;
    fn bits(p: [f64; 2]) -> [u64; 2] {
        [p[0].to_bits(), p[1].to_bits()]
    }
    #[test]
    fn retained_northing_matches_independent_legacy_operation_order() {
        for projection in [
            AngularProjection::Geographic,
            AngularProjection::EllipsoidalMercator,
        ] {
            for lat in [-89.5, -80., -0., 0., 48.65, 80., 89.5] {
                // Independent original projector call; do not use retained token here.
                let q = projection.northing(lat).unwrap();
                for scale in [0.01, 1., 20., 200., 1e7] {
                    for offset in [[0., 0.], [117.25, -307.898], [-1e6, 1e6]] {
                        let c = FlatMapCamera::new(
                            projection,
                            [-185., 80.],
                            [scale, scale * 1.25],
                            offset,
                        )
                        .unwrap();
                        let n = c.prepare_northing(lat).unwrap();
                        for shift in [-360., 0., 360.] {
                            let point = [179. + shift, lat];
                            let expected = [
                                (point[0] - c.origin[0]) * c.scale[0] + c.offset[0],
                                (c.origin[1] - q) * c.scale[1] + c.offset[1],
                            ];
                            assert_eq!(bits(c.project(point).unwrap()), bits(expected));
                            assert_eq!(
                                bits(c.project_prepared(point, &n).unwrap()),
                                bits(expected)
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn source_bits_projection_and_malformed_inputs_cannot_rebind() {
        let merc = FlatMapCamera::new(
            AngularProjection::EllipsoidalMercator,
            [0., 80.],
            [100., 100.],
            [0., 0.],
        )
        .unwrap();
        let geo = FlatMapCamera::new(
            AngularProjection::Geographic,
            [0., 80.],
            [100., 100.],
            [0., 0.],
        )
        .unwrap();
        let n = merc.prepare_northing(0.).unwrap();
        assert!(merc.project_prepared([0., -0.], &n).is_err());
        assert!(merc.project_prepared([0., 1.], &n).is_err());
        assert!(geo.project_prepared([0., 0.], &n).is_err());
        let mut changed_model = n;
        changed_model.projection_identity[2] ^= 1;
        assert!(merc.project_prepared([0., 0.], &changed_model).is_err());
        assert!(std::mem::size_of::<Option<PreparedFlatNorthing>>() <= 64);
        for lat in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 91., -91.] {
            assert!(merc.prepare_northing(lat).is_err());
            assert!(merc.project([0., lat]).is_err());
        }
        // Overflow remains fallible despite a valid retained projection.
        let c = FlatMapCamera::new(
            AngularProjection::Geographic,
            [0., 0.],
            [f64::MAX, 1.],
            [0., 0.],
        )
        .unwrap();
        let n = c.prepare_northing(0.).unwrap();
        assert!(c.project_prepared([2., 0.], &n).is_err());
        assert!(c.project([2., 0.]).is_err());
    }
}
