//! Product-neutral visibility of height-zero WGS84 source curves.
//! The ellipsoid's horizon is a plane, not the renderer's triangulated Earth.
use crate::{
    geodesy::{WGS84_A, WGS84_B},
    rhumb::RhumbSegment,
};
use anyhow::{ensure, Result};
#[derive(Debug, Clone, Copy)]
pub struct SurfaceHorizon {
    normal: [f64; 3],
    offset: f64,
    tolerance: f64,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceInterval {
    pub start: f64,
    pub end: f64,
}
impl SurfaceHorizon {
    pub fn from_eye(eye: [f64; 3]) -> Result<Self> {
        ensure!(eye.iter().all(|v| v.is_finite()), "Non-finite horizon eye");
        let axes = [WGS84_A, WGS84_A, WGS84_B];
        let outside = (0..3).map(|i| (eye[i] / axes[i]).powi(2)).sum::<f64>();
        ensure!(
            outside > 1. && outside.is_finite(),
            "Horizon eye must be outside WGS84"
        );
        let q = std::array::from_fn::<_, 3, _>(|i| eye[i] / axes[i] / axes[i]);
        let norm = q[0].hypot(q[1]).hypot(q[2]);
        ensure!(norm > 0. && norm.is_finite(), "Horizon plane overflow");
        let offset = 1. / norm;
        Ok(Self {
            normal: q.map(|v| v / norm),
            offset,
            tolerance: 64. * f64::EPSILON * (WGS84_A + offset),
        })
    }
    /// Positive means on the visible side. Valid only for source positions on
    /// the height-zero ellipsoid; never apply to extruded screen geometry.
    pub fn signed_distance_m(self, point: [f64; 3]) -> Result<f64> {
        ensure!(
            point.iter().all(|v| v.is_finite()),
            "Non-finite horizon point"
        );
        Ok(self
            .normal
            .iter()
            .zip(point)
            .map(|(n, p)| n * p)
            .sum::<f64>()
            - self.offset)
    }
    pub fn numerical_tolerance_m(self) -> f64 {
        self.tolerance
    }
    /// Source fractions stay ordered. Work and output are explicitly bounded.
    /// A whole visible interval may have two hidden endpoints before partition.
    pub fn rhumb_intervals(
        self,
        route: RhumbSegment,
        budget: usize,
    ) -> Result<(Vec<SourceInterval>, usize)> {
        ensure!(
            (1..=262144).contains(&budget),
            "Invalid horizon work budget"
        );
        let value = |t: f64| self.signed_distance_m(route.point(t)?.to_ecef(0.)?);
        let a = route.point(0.)?;
        let b = route.point(1.)?;
        let fa = value(0.)?;
        let fb = value(1.)?;
        let length = route.parameter_length_bound_m()?;
        // A parallel viewed from a polar eye has constant plane distance.
        // Enclose the tiny horizontal term from trigonometric pole roundoff.
        let variation = 2. * WGS84_A * self.normal[0].hypot(self.normal[1]);
        if length == 0. || a.latitude() == b.latitude() && variation <= self.tolerance {
            return Ok((
                if fa >= -self.tolerance {
                    vec![SourceInterval { start: 0., end: 1. }]
                } else {
                    Vec::new()
                },
                1,
            ));
        }
        let mut pending = vec![(0., 1., fa, fb, 0u8)];
        let mut result: Vec<SourceInterval> = Vec::new();
        let mut work = 0;
        while let Some((t0, t1, fa, fb, level)) = pending.pop() {
            work += 1;
            ensure!(work <= budget, "Horizon partition work budget exceeded");
            // |d²ECEF/ds²| <= 4/b for the Mercator parameter (the polar
            // geodesic branch is tighter). A unit-normal plane inherits this
            // bound. Secant remainder <= span²/(2*b).
            let span = length * (t1 - t0);
            let bound = span * span / (2. * WGS84_B);
            let monotone = (fb - fa).abs() > 4. * bound + self.tolerance;
            let mut kept = None;
            if fa.min(fb) >= bound + self.tolerance || monotone && fa >= 0. && fb >= 0. {
                kept = Some(SourceInterval { start: t0, end: t1 });
            } else if fa.max(fb) < -bound - self.tolerance || monotone && fa < 0. && fb < 0. {
                continue;
            } else if monotone && (fa >= 0.) != (fb >= 0.) {
                let mut lo = t0;
                let mut hi = t1;
                let left_visible = fa >= 0.;
                for _ in 0..80 {
                    let mid = (lo + hi) / 2.;
                    if mid == lo || mid == hi {
                        break;
                    }
                    if (value(mid)? >= 0.) == left_visible {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let cut = if left_visible { lo } else { hi };
                kept = Some(if left_visible {
                    SourceInterval {
                        start: t0,
                        end: cut,
                    }
                } else {
                    SourceInterval {
                        start: cut,
                        end: t1,
                    }
                });
            } else if bound <= self.tolerance
                && fa.abs() <= self.tolerance
                && fb.abs() <= self.tolerance
            {
                // Only a numerical shell, not a geometric visibility shortcut.
                kept = Some(SourceInterval { start: t0, end: t1 });
            }
            if let Some(interval) = kept {
                if interval.start < interval.end {
                    if let Some(previous) = result.last_mut().filter(|p| p.end == interval.start) {
                        previous.end = interval.end;
                    } else {
                        result.push(interval);
                    }
                }
            } else {
                let mid = (t0 + t1) / 2.;
                ensure!(
                    level < 56 && mid != t0 && mid != t1,
                    "Horizon partition numeric resolution exceeded"
                );
                let fm = value(mid)?;
                pending.push((mid, t1, fm, fb, level + 1));
                pending.push((t0, mid, fa, fm, level + 1));
            }
        }
        Ok((result, work))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        geodesy::{intersect_wgs84_ray, GeographicPosition},
        globe_camera::GlobeCamera,
    };
    fn p(lat: f64, lon: f64) -> GeographicPosition {
        GeographicPosition::new(lat, lon).unwrap()
    }
    #[test]
    fn plane_matches_independent_ellipsoid_ray_visibility() {
        for lat in [-89., -45., 0., 45., 89.] {
            for range in [150., 30000., 20_000_000.] {
                for tilt in [0., 70.] {
                    let c = GlobeCamera::orbit(
                        p(lat, 179.9),
                        range,
                        37.,
                        tilt,
                        [640., 480.],
                        45.,
                        1.,
                        1e8,
                    )
                    .unwrap();
                    let eye = c.eye_m();
                    let h = SurfaceHorizon::from_eye(eye).unwrap();
                    for plat in [-89., -60., -20., 0., 20., 60., 89.] {
                        for lon in (-180..180).step_by(10) {
                            let v = p(plat, lon as f64).to_ecef(0.).unwrap();
                            let d = std::array::from_fn(|i| v[i] - eye[i]);
                            let length = d[0].hypot(d[1]).hypot(d[2]);
                            let hit = intersect_wgs84_ray(eye, d).unwrap().unwrap();
                            let visible = hit.distance_m + 1e-5 >= length;
                            let signed = h.signed_distance_m(v).unwrap();
                            if signed.abs() > 1e-4 {
                                assert_eq!(
                                    signed > 0.,
                                    visible,
                                    "{lat}/{range}/{tilt}/{plat}/{lon}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn two_hidden_ends_keep_visible_middle_and_exact_equatorial_cuts() {
        for range in [150., 30000., 20_000_000.] {
            let h = SurfaceHorizon::from_eye([WGS84_A + range, 0., 0.]).unwrap();
            let route = RhumbSegment::new(p(0., -80.), p(0., 80.)).unwrap();
            let (v, work) = h.rhumb_intervals(route, 4096).unwrap();
            assert_eq!(v.len(), 1);
            assert!(work < 4096);
            assert!(v[0].start < 0.5 && v[0].end > 0.5);
            let angle = (WGS84_A / (WGS84_A + range)).acos().to_degrees();
            assert!((route.point(v[0].start).unwrap().longitude() + angle).abs() < 1e-7);
            assert!((route.point(v[0].end).unwrap().longitude() - angle).abs() < 1e-7);
            let (hidden, _) = h
                .rhumb_intervals(RhumbSegment::new(p(0., 160.), p(0., 170.)).unwrap(), 4096)
                .unwrap();
            assert!(hidden.is_empty());
        }
    }
    #[test]
    fn dateline_polar_parallel_and_budget_guards() {
        let eye = [-3. * WGS84_A, 0., 0.];
        let h = SurfaceHorizon::from_eye(eye).unwrap();
        let r = RhumbSegment::new(p(40., 179.), p(40., -179.)).unwrap();
        assert_eq!(
            h.rhumb_intervals(r, 4096).unwrap().0,
            vec![SourceInterval { start: 0., end: 1. }]
        );
        let h = SurfaceHorizon::from_eye([0., 0., 3. * WGS84_B]).unwrap();
        let theta = (1_f64 / 3.).asin();
        let lat = ((WGS84_A / WGS84_B) * theta.tan()).atan().to_degrees();
        let r = RhumbSegment::new(p(lat, -80.), p(lat, 80.)).unwrap();
        assert_eq!(h.rhumb_intervals(r, 4096).unwrap().0.len(), 1);
        assert!(h.rhumb_intervals(r, 0).is_err());
        assert!(SurfaceHorizon::from_eye([0.; 3]).is_err());
        assert!(SurfaceHorizon::from_eye([f64::NAN; 3]).is_err());
        let h = SurfaceHorizon::from_eye([WGS84_A + 150., 0., 0.]).unwrap();
        assert!(h
            .rhumb_intervals(RhumbSegment::new(p(0., -80.), p(0., 80.)).unwrap(), 1)
            .is_err());
    }
}
