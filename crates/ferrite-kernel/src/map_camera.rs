//! Product-neutral screen/geographic camera adapter. Rendering, navigation
//! ownership, portrayal and application menus stay outside this module.
use crate::{
    geodesy::{GeographicPosition, Mercator, WGS84_A},
    globe_camera::GlobeCamera,
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
    Globe {
        camera: GlobeCamera,
        viewport_origin: [f64; 2],
    },
}
impl MapCamera {
    /// Returns None for points hidden by the ellipsoid or outside its depth range.
    /// Flat longitude copies are preserved; globe inputs use canonical WGS84.
    pub fn project(&self, point: [f64; 2]) -> Result<Option<[f64; 2]>> {
        match self {
            Self::Flat(c) => Ok(Some(c.project(point)?)),
            Self::Globe {
                camera,
                viewport_origin,
            } => {
                ensure!(
                    viewport_origin.iter().all(|v| v.is_finite()),
                    "Invalid viewport origin"
                );
                let geo = GeographicPosition::new(point[1], point[0])?;
                Ok(camera.project_visible(geo.to_ecef(0.)?)?.map(|p| {
                    [
                        p.screen_px[0] + viewport_origin[0],
                        p.screen_px[1] + viewport_origin[1],
                    ]
                }))
            }
        }
    }
    /// Returns None for a screen ray which misses the WGS84 ellipsoid. A miss is
    /// not converted into a latitude, a previous selection, or a flat coordinate.
    pub fn unproject(&self, point: [f64; 2]) -> Result<Option<[f64; 2]>> {
        match self {
            Self::Flat(c) => Ok(Some(c.unproject(point)?)),
            Self::Globe {
                camera,
                viewport_origin,
            } => {
                ensure!(
                    viewport_origin.iter().all(|v| v.is_finite()),
                    "Invalid viewport origin"
                );
                Ok(camera
                    .pick([point[0] - viewport_origin[0], point[1] - viewport_origin[1]])?
                    .map(|p| {
                        [
                            p.geodetic.surface.longitude(),
                            p.geodetic.surface.latitude(),
                        ]
                    }))
            }
        }
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
    #[test]
    fn globe_adapter_uses_viewport_origin_horizon_and_ray_misses() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(48.65, -2.05).unwrap(),
            20_000_000.,
            0.,
            0.,
            [1000., 800.],
            45.,
            1.,
            100_000_000.,
        )
        .unwrap();
        let view = MapCamera::Globe {
            camera: c,
            viewport_origin: [30., 80.],
        };
        let p = view.project([-2.05, 48.65]).unwrap().unwrap();
        assert!((p[0] - 530.).abs() < 1e-8 && (p[1] - 480.).abs() < 1e-8);
        let geo = view.unproject(p).unwrap().unwrap();
        assert!((geo[0] + 2.05).abs() < 1e-8 && (geo[1] - 48.65).abs() < 1e-8);
        assert!(view.project([177.95, -48.65]).unwrap().is_none());
        assert!(view.unproject([30., 80.]).unwrap().is_none());
    }
}
