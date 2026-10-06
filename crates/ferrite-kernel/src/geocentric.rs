//! EPSG:4978 -> EPSG:4979 WGS84 inverse. Adapted from GeographicLib's
//! Geocentric::IntReverse, Charles Karney (2008-2022), MIT/X11.
//! Full license: ../LICENSE-GeographicLib.txt. Closest ellipsoid footpoint
//! is chosen; at the ECEF origin the conventional north pole is returned.
use crate::geodesy::{GeographicPosition, WGS84_A, WGS84_F};
use anyhow::{ensure, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeodeticPosition {
    pub surface: GeographicPosition,
    /// Height above the WGS84 ellipsoid, not chart datum or sea level.
    pub ellipsoidal_height_m: f64,
}
impl GeodeticPosition {
    pub fn to_ecef(self) -> Result<[f64; 3]> {
        self.surface.to_ecef(self.ellipsoidal_height_m)
    }
}
/// The result is geodetic latitude, canonical longitude, and ellipsoidal height.
/// Multiple interior solutions use the nearest ellipsoid footpoint convention.
pub fn from_ecef(xyz: [f64; 3]) -> Result<GeodeticPosition> {
    ensure!(
        xyz.iter().all(|x| x.is_finite()),
        "Non-finite ECEF coordinate"
    );
    let [x, y, z] = xyz;
    let radial = x.hypot(y);
    let mut height = radial.hypot(z);
    ensure!(height.is_finite(), "ECEF magnitude overflow");
    let mut longitude = if radial > 0. {
        y.atan2(x).to_degrees()
    } else {
        0.
    };
    let e2 = WGS84_F * (2. - WGS84_F);
    let e2m = 1. - e2;
    let e4 = e2 * e2;
    let (sin_lat, cos_lat) = if height > 2. * WGS84_A / f64::EPSILON {
        // Scaled normalization avoids intermediate overflow for remote points.
        let r = (x / 2.).hypot(y / 2.);
        let h = (z / 2.).hypot(r);
        longitude = if r > 0. {
            (y / 2.).atan2(x / 2.).to_degrees()
        } else {
            0.
        };
        ((z / 2.) / h, r / h)
    } else {
        let p = (radial / WGS84_A).powi(2);
        let q = e2m * (z / WGS84_A).powi(2);
        let r = (p + q - e4) / 6.;
        if !(e4 * q == 0. && r <= 0.) {
            let s = e4 * p * q / 4.;
            let r2 = r * r;
            let r3 = r * r2;
            let disc = s * (2. * r3 + s);
            let mut u = r;
            if disc >= 0. {
                let t3 = s + r3;
                let t = (t3 + disc.sqrt().copysign(t3)).cbrt();
                u += t + if t != 0. { r2 / t } else { 0. };
            } else {
                let angle = (-disc).sqrt().atan2(-(s + r3));
                u += 2. * r * (angle / 3.).cos();
            }
            let v = (u * u + e4 * q).sqrt();
            let uv = if u < 0. { e4 * q / (v - u) } else { u + v };
            let w = (e2 * (uv - q) / (2. * v)).max(0.);
            let k = uv / ((uv + w * w).sqrt() + w);
            let k2 = k + e2;
            let d = k * radial / k2;
            let h = (z / k).hypot(radial / k2);
            height = (1. - e2m / k) * d.hypot(z);
            ((z / k) / h, (radial / k2) / h)
        } else {
            let zz = ((e4 - p) / e2m).sqrt();
            let xx = p.sqrt();
            let h = zz.hypot(xx);
            height = -WGS84_A * e2m * h / e2;
            (if z < 0. { -zz / h } else { zz / h }, xx / h)
        }
    };
    let latitude = sin_lat.atan2(cos_lat).to_degrees();
    ensure!(height.is_finite(), "Geocentric inverse failed");
    Ok(GeodeticPosition {
        surface: GeographicPosition::new(latitude, longitude)?,
        ellipsoidal_height_m: height,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::geodesy::WGS84_B;
    #[test]
    fn axes_origin_and_nonfinite_coordinates() {
        for (xyz, lat, lon, h) in [
            ([WGS84_A + 123., 0., 0.], 0., 0., 123.),
            ([0., WGS84_A, 0.], 0., 90., 0.),
            ([0., 0., -WGS84_B - 500.], -90., 0., 500.),
            ([0., 0., 0.], 90., 0., -WGS84_B),
        ] {
            let p = from_ecef(xyz).unwrap();
            assert!((p.surface.latitude() - lat).abs() < 1e-11);
            assert!((p.surface.longitude() - lon).abs() < 1e-11);
            assert!((p.ellipsoidal_height_m - h).abs() < 1e-7);
        }
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(from_ecef([v, 1., 2.]).is_err());
        }
        assert!(from_ecef([f64::MAX; 3]).is_err());
    }
    #[test]
    fn height_and_polar_roundtrips_include_orbits_and_dateline() {
        for lat in [-90., -89.999999, -80., -45., 0., 45., 80., 89.999999, 90.] {
            for lon in [-180., -120., 0., 120., 180.] {
                for h in [-1000., 0., 100., 400000., 35786000.] {
                    let p = GeodeticPosition {
                        surface: GeographicPosition::new(lat, lon).unwrap(),
                        ellipsoidal_height_m: h,
                    };
                    let xyz = p.to_ecef().unwrap();
                    let q = from_ecef(xyz).unwrap();
                    let back = q.to_ecef().unwrap();
                    assert!((q.surface.latitude() - lat).abs() < 1e-11);
                    assert!((q.ellipsoidal_height_m - h).abs() < 2e-8);
                    assert!(
                        xyz.iter().zip(back).all(|(a, b)| (a - b).abs() < 3e-8),
                        "{p:?} -> {q:?}"
                    );
                }
            }
        }
    }
}
