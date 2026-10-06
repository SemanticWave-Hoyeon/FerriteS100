//! Product-neutral screen/geographic camera adapter. Rendering, navigation
//! ownership, portrayal and application menus stay outside this module.
use crate::{
    geodesy::{GeographicPosition, Mercator, WGS84_A},
};
use anyhow::{ensure, Result};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AngularProjection {
    Geographic,
    EllipsoidalMercator,
}
impl AngularProjection {
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
        [match self.projection { AngularProjection::Geographic => 0, AngularProjection::EllipsoidalMercator => 1 },
         self.origin[0].to_bits(), self.origin[1].to_bits(), self.scale[0].to_bits(), self.scale[1].to_bits(),
         self.offset[0].to_bits(), self.offset[1].to_bits()]
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
