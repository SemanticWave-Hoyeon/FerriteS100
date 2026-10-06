//! Conservative f64 bounds for a height-zero WGS84 surface rectangle.
//! Longitude is a continuous interval, including lifts across the date line.
//! Interval products enclose the curved surface and every chord between it.
use crate::geodesy::{WGS84_A, WGS84_F};
use anyhow::{ensure, Result};
#[derive(Clone, Copy, Debug)]
pub struct GeographicSurfaceBounds {
    pub min_m: [f64; 3],
    pub max_m: [f64; 3],
}
fn product(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    let p = [a[0] * b[0], a[0] * b[1], a[1] * b[0], a[1] * b[1]];
    [
        p.iter().copied().fold(f64::INFINITY, f64::min),
        p.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    ]
}
fn trig(interval: [f64; 2], cosine: bool) -> [f64; 2] {
    if interval[1] - interval[0] >= 360. {
        return [-1., 1.];
    }
    let start = interval[0].rem_euclid(360.);
    let end = start + (interval[1] - interval[0]);
    let value = |d: f64| {
        if cosine {
            d.to_radians().cos()
        } else {
            d.to_radians().sin()
        }
    };
    let mut low = value(start).min(value(end));
    let mut high = value(start).max(value(end));
    let phase = if cosine { 0. } else { 90. };
    let first = ((start - phase) / 180.).ceil() as i32;
    let last = ((end - phase) / 180.).floor() as i32;
    for k in first..=last {
        let v = if k.rem_euclid(2) == 0 { 1. } else { -1. };
        low = low.min(v);
        high = high.max(v);
    }
    [low - 1e-13, high + 1e-13]
}
impl GeographicSurfaceBounds {
    pub fn new(latitude_deg: [f64; 2], longitude_deg: [f64; 2]) -> Result<Self> {
        ensure!(
            latitude_deg
                .iter()
                .chain(&longitude_deg)
                .all(|v| v.is_finite()),
            "Non-finite geographic bound"
        );
        ensure!(
            latitude_deg[0] >= -90. && latitude_deg[1] <= 90. && latitude_deg[0] <= latitude_deg[1],
            "Invalid latitude bound"
        );
        ensure!(
            longitude_deg[0] <= longitude_deg[1] && longitude_deg.iter().all(|v| v.abs() <= 1e12),
            "Invalid continuous longitude bound"
        );
        // Enclose normalization and Mercator inversion roundoff, also for large lifts.
        let lon_pad =
            64. * f64::EPSILON * longitude_deg[0].abs().max(longitude_deg[1].abs()).max(360.);
        let lat_pad = 64. * f64::EPSILON * 90.;
        let lat = [
            (latitude_deg[0] - lat_pad).max(-90.),
            (latitude_deg[1] + lat_pad).min(90.),
        ];
        let lon = [longitude_deg[0] - lon_pad, longitude_deg[1] + lon_pad];
        let sin = trig(lat, false);
        let cos = trig(lat, true);
        let sq_min = if sin[0] <= 0. && sin[1] >= 0. {
            0.
        } else {
            sin[0].powi(2).min(sin[1].powi(2))
        };
        let sq_max = sin[0].powi(2).max(sin[1].powi(2));
        let e2 = WGS84_F * (2. - WGS84_F);
        let n = [
            WGS84_A / (1. - e2 * sq_min).sqrt(),
            WGS84_A / (1. - e2 * sq_max).sqrt(),
        ];
        let radial = product(n, cos);
        let x = product(radial, trig(lon, true));
        let y = product(radial, trig(lon, false));
        let z = product([n[0] * (1. - e2), n[1] * (1. - e2)], sin);
        // Ten centimetres is a conservative numerical enclosure, not geometric LOD.
        Ok(Self {
            min_m: [x[0] - 0.1, y[0] - 0.1, z[0] - 0.1],
            max_m: [x[1] + 0.1, y[1] + 0.1, z[1] + 0.1],
        })
    }
    pub fn sphere(self) -> ([f64; 3], f64) {
        let center = std::array::from_fn(|i| (self.min_m[i] + self.max_m[i]) / 2.);
        let half: [f64; 3] = std::array::from_fn(|i| (self.max_m[i] - self.min_m[i]) / 2.);
        (center, half[0].hypot(half[1]).hypot(half[2]) + 0.1)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::geodesy::GeographicPosition;
    #[test]
    fn curved_surface_and_chords_are_enclosed_across_poles_and_lifts() {
        for lat in [
            [-90., -89.],
            [-80., -60.],
            [-10., 10.],
            [70., 89.],
            [89., 90.],
            [-90., 90.],
            [0., 0.],
        ] {
            for lon in [
                [-180., 180.],
                [179., 181.],
                [-181., -179.],
                [1e9, 1e9 + 2.],
                [-740., -100.],
                [45., 45.],
                [80., 100.],
            ] {
                let b = GeographicSurfaceBounds::new(lat, lon).unwrap();
                let (center, radius) = b.sphere();
                for a in 0..=24 {
                    for c in 0..=24 {
                        let latitude = lat[0] + (lat[1] - lat[0]) * a as f64 / 24.;
                        let longitude = lon[0] + (lon[1] - lon[0]) * c as f64 / 24.;
                        let p = GeographicPosition::new(
                            latitude,
                            (longitude + 180.).rem_euclid(360.) - 180.,
                        )
                        .unwrap()
                        .to_ecef(0.)
                        .unwrap();
                        assert!(
                            (0..3).all(|i| p[i] >= b.min_m[i] && p[i] <= b.max_m[i]),
                            "{lat:?} {lon:?} {p:?} {b:?}"
                        );
                        assert!(
                            (p[0] - center[0])
                                .hypot(p[1] - center[1])
                                .hypot(p[2] - center[2])
                                <= radius
                        );
                        let chord = std::array::from_fn::<_, 3, _>(|i| (p[i] + center[i]) / 2.);
                        assert!((0..3).all(|i| chord[i] >= b.min_m[i] && chord[i] <= b.max_m[i]));
                    }
                }
            }
        }
    }
    #[test]
    fn invalid_bounds_are_rejected() {
        for (lat, lon) in [
            ([91., 92.], [0., 1.]),
            ([10., -10.], [0., 1.]),
            ([0., 1.], [2., 1.]),
            ([0., 1.], [0., f64::NAN]),
        ] {
            assert!(GeographicSurfaceBounds::new(lat, lon).is_err());
        }
    }
}
