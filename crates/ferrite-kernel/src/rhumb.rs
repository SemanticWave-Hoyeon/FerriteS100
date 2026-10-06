//! WGS84 loxodromic segments (S-101 curve interpolation), separate from
//! ellipsoidal shortest-path geodesics used for navigation.
use crate::geodesy::{direct, inverse, GeographicPosition, Mercator, WGS84_A, WGS84_F};
use anyhow::{ensure, Result};
#[derive(Debug, Clone, Copy)]
pub struct RhumbSegment {
    start: GeographicPosition,
    end: GeographicPosition,
    x0: f64,
    y0: f64,
    dx: f64,
    dy: f64,
    pole_meridian: bool,
}
impl RhumbSegment {
    /// Shortest longitude lift; exactly antipodal longitudes conventionally
    /// travel west. Geographic endpoints are never modified.
    pub fn new(start: GeographicPosition, end: GeographicPosition) -> Result<Self> {
        let longitude_delta = (end.longitude() - start.longitude() + 180.).rem_euclid(360.) - 180.;
        let pole = start.latitude().abs() == 90. || end.latitude().abs() == 90.;
        ensure!(
            !pole || longitude_delta == 0.,
            "Non-meridional rhumb segment at a pole is undefined"
        );
        if pole {
            return Ok(Self {
                start,
                end,
                x0: 0.,
                y0: 0.,
                dx: 0.,
                dy: 0.,
                pole_meridian: true,
            });
        }
        let a = Mercator::World.project(start)?;
        let b = Mercator::World.project(end)?;
        Ok(Self {
            start,
            end,
            x0: a[0],
            y0: a[1],
            dx: WGS84_A * longitude_delta.to_radians(),
            dy: b[1] - a[1],
            pole_meridian: false,
        })
    }
    pub fn point(self, fraction: f64) -> Result<GeographicPosition> {
        ensure!(
            fraction.is_finite() && (0. ..=1.).contains(&fraction),
            "Invalid rhumb interpolation fraction"
        );
        if fraction == 0. {
            return Ok(self.start);
        }
        if fraction == 1. {
            return Ok(self.end);
        }
        if self.pole_meridian {
            let route = inverse(self.start, self.end)?;
            return direct(
                self.start,
                if self.end.latitude() >= self.start.latitude() {
                    0.
                } else {
                    180.
                },
                route.distance_m * fraction,
            );
        }
        let longitude = ((self.x0 + self.dx * fraction) / WGS84_A).to_degrees();
        let longitude = (longitude + 180.).rem_euclid(360.) - 180.;
        Mercator::World.unproject([
            WGS84_A * longitude.to_radians(),
            self.y0 + self.dy * fraction,
        ])
    }
    pub fn bearing_deg(self) -> f64 {
        if self.pole_meridian {
            return if self.end.latitude() >= self.start.latitude() {
                0.
            } else {
                180.
            };
        }
        self.dx.atan2(self.dy).to_degrees().rem_euclid(360.)
    }
    /// Upper bound on |dECEF/dt| for the authored parameter. Ellipsoidal
    /// Mercator's conformal scale is at most one; the polar meridian branch
    /// uses geodesic arc length instead.
    pub fn parameter_length_bound_m(self) -> Result<f64> {
        if self.pole_meridian {
            self.distance_m()
        } else {
            Ok(self.dx.hypot(self.dy))
        }
    }
    pub fn distance_m(self) -> Result<f64> {
        let meridian = inverse(
            GeographicPosition::new(self.start.latitude(), 0.)?,
            GeographicPosition::new(self.end.latitude(), 0.)?,
        )?
        .distance_m;
        if self.pole_meridian {
            return Ok(meridian);
        }
        let dpsi = self.dy / WGS84_A;
        let phi0 = self.start.latitude().to_radians();
        let phi1 = self.end.latitude().to_radians();
        let mid = (phi0 + phi1) / 2.;
        let half = (phi1 - phi0) / 2.;
        let e2 = WGS84_F * (2. - WGS84_F);
        // Dividing two almost equal endpoint differences loses precision on
        // near-parallel routes. Integrate their positive derivatives instead.
        // The interval is restricted relative to distance from the pole; the
        // four-point Gauss rule then resolves both smooth integrands to f64.
        let q = if (phi1 - phi0).abs() <= 0.01 * mid.cos() {
            let mut meridian_derivative = 0.;
            let mut isometric_derivative = 0.;
            for (node, weight) in [
                (0.3399810435848563, 0.6521451548625461),
                (0.8611363115940526, 0.3478548451374538),
            ] {
                for sign in [-1., 1.] {
                    let phi = mid + sign * half * node;
                    let den = 1. - e2 * phi.sin().powi(2);
                    meridian_derivative += weight * WGS84_A * (1. - e2) / den.powf(1.5);
                    isometric_derivative += weight * (1. - e2) / (phi.cos() * den);
                }
            }
            meridian_derivative / isometric_derivative
        } else {
            meridian / dpsi.abs()
        };
        let distance = meridian.hypot(q * self.dx / WGS84_A);
        ensure!(distance.is_finite(), "Rhumb distance overflow");
        Ok(distance)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn p(lat: f64, lon: f64) -> GeographicPosition {
        GeographicPosition::new(lat, lon).unwrap()
    }
    #[test]
    fn rhumb_dateline_parallel_and_geodesic_are_distinct() {
        let r = RhumbSegment::new(p(70., 179.), p(70., -179.)).unwrap();
        let m = r.point(0.5).unwrap();
        assert!((m.latitude() - 70.).abs() < 1e-10);
        assert!((m.longitude().abs() - 180.).abs() < 1e-10);
        assert!((r.bearing_deg() - 90.).abs() < 1e-10);
        let geo =
            crate::geodesy::sample_shortest_path(p(70., 179.), p(70., -179.), 10000., 100).unwrap();
        assert!(geo[geo.len() / 2].latitude() > 70.);
        assert!(r.distance_m().unwrap() > inverse(p(70., 179.), p(70., -179.)).unwrap().distance_m);
    }
    #[test]
    fn near_parallel_distance_matches_independent_geographiclib_reference() {
        let r = RhumbSegment::new(p(70., 179.), p(70.00000001, -179.)).unwrap();
        assert!((r.distance_m().unwrap() - 76373.0825323803).abs() < 1e-7);
        assert!((r.bearing_deg() - 89.999999163052692).abs() < 1e-9);
    }
    #[test]
    fn rhumb_subsegments_keep_constant_bearing_and_total_distance() {
        let r = RhumbSegment::new(p(-35., 160.), p(75., -120.)).unwrap();
        let mut length = 0.;
        for i in 0..64 {
            let sub = RhumbSegment::new(
                r.point(i as f64 / 64.).unwrap(),
                r.point((i + 1) as f64 / 64.).unwrap(),
            )
            .unwrap();
            assert!((sub.bearing_deg() - r.bearing_deg()).abs() < 1e-10);
            length += sub.distance_m().unwrap();
        }
        assert!((length - r.distance_m().unwrap()).abs() < 1e-6);
        assert!(r.point(f64::NAN).is_err());
        assert!(r.point(-1.).is_err());
        assert!(RhumbSegment::new(p(90., 0.), p(80., 10.)).is_err());
        assert!(
            (RhumbSegment::new(p(80., 0.), p(90., 0.))
                .unwrap()
                .point(0.5)
                .unwrap()
                .latitude()
                - 85.)
                .abs()
                < 0.001
        );
    }
}
