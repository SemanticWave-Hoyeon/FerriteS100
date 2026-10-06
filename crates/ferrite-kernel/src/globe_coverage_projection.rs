//! Project already tessellated WGS84 coverage surfaces into local device pixels.
//! Source triangulation owns boundary interpolation and holes. This module owns
//! ellipsoidal horizon and homogeneous frustum clipping; it never selects data.
use crate::{
    coverage_selection::Region,
    geodesy::{WGS84_A, WGS84_B},
    globe_camera::GlobeCamera,
};
use anyhow::{ensure, Result};
use geo::CoordsIter;

#[derive(Debug, Clone, Copy)]
pub struct CoverageProjectionLimits {
    pub max_triangles: usize,
    /// Bounds logical retained region coordinates, not allocator/overlay scratch.
    pub max_region_coordinates: usize,
}
impl Default for CoverageProjectionLimits {
    fn default() -> Self {
        Self {
            max_triangles: 524288,
            max_region_coordinates: 1048576,
        }
    }
}
#[derive(Clone, Copy)]
struct Point {
    clip: [f64; 4],
    horizon: f64,
}
impl Point {
    fn distance(self, plane: usize) -> f64 {
        let [x, y, z, w] = self.clip;
        match plane {
            0 => self.horizon,
            1 => w + x,
            2 => w - x,
            3 => w + y,
            4 => w - y,
            5 => z,
            6 => w - z,
            _ => unreachable!(),
        }
    }
    fn between(a: Self, b: Self, t: f64) -> Self {
        Self {
            clip: std::array::from_fn(|i| a.clip[i] + t * (b.clip[i] - a.clip[i])),
            horizon: a.horizon + t * (b.horizon - a.horizon),
        }
    }
}
fn trim(input: &[Point], plane: usize) -> Result<Vec<Point>> {
    let mut output = Vec::with_capacity(input.len() + 1);
    let Some(mut a) = input.last().copied() else {
        return Ok(output);
    };
    let mut da = a.distance(plane);
    for &b in input {
        let db = b.distance(plane);
        if (da >= 0.) != (db >= 0.) {
            let t = da / (da - db);
            ensure!(
                t.is_finite() && (0. ..=1.).contains(&t),
                "Invalid coverage clip intersection"
            );
            output.push(Point::between(a, b, t));
        }
        if db >= 0. {
            output.push(b)
        }
        a = b;
        da = db;
    }
    Ok(output)
}
fn bounded(region: &Region, limit: usize) -> Result<usize> {
    let mut count = 0usize;
    for polygon in region.polygons() {
        count = count
            .checked_add(polygon.coords_count())
            .ok_or_else(|| anyhow::anyhow!("Coverage coordinate count overflow"))?;
        ensure!(
            count <= limit,
            "Projected coverage coordinate budget exceeded"
        );
    }
    Ok(count)
}
/// Input vertices must lie on WGS84 at zero ellipsoidal height. The input is a
/// conforming triangle approximation, not unprojected longitude/latitude rings.
/// Visible parts are clipped at the ellipsoid's tangent plane, then all six
/// frustum planes before division by depth. Back-side surfaces return empty.
/// The caller retains the tessellator's approximation error; this does not
/// replace its geographic edge interpolation or guarantee exact limb pixels.
/// One batch union preserves shared edges in one floating-point precision
/// domain, including holes/disconnected pieces. Limits bound input work and
/// aggregate logical input/output coordinates, not temporary
/// memory allocated inside the geometry library's boolean operations.
pub fn project_coverage_triangles(
    camera: &GlobeCamera,
    triangles: impl IntoIterator<Item = [[f64; 3]; 3]>,
    limits: CoverageProjectionLimits,
) -> Result<Region> {
    ensure!(
        limits.max_triangles > 0 && limits.max_region_coordinates >= 4,
        "Invalid coverage projection budget"
    );
    let eye = camera.eye_m();
    let axes = [WGS84_A, WGS84_A, WGS84_B];
    let eye_radius = (0..3).map(|i| (eye[i] / axes[i]).powi(2)).sum::<f64>();
    ensure!(
        eye_radius.is_finite() && eye_radius > 1.,
        "Coverage camera must be outside WGS84"
    );
    let viewport = camera.viewport();
    let mut polygons = Vec::new();
    let mut input_coordinates = 0usize;
    let mut count = 0usize;
    for triangle in triangles {
        count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Coverage triangle count overflow"))?;
        ensure!(
            count <= limits.max_triangles,
            "Coverage triangle budget exceeded"
        );
        let mut points = Vec::with_capacity(3);
        for vertex in triangle {
            ensure!(
                vertex.iter().all(|v| v.is_finite()),
                "Non-finite coverage vertex"
            );
            let radius = (0..3).map(|i| (vertex[i] / axes[i]).powi(2)).sum::<f64>();
            ensure!(
                (radius - 1.).abs() <= 1e-10,
                "Coverage vertex is not on WGS84 surface"
            );
            let horizon = (0..3)
                .map(|i| (eye[i] / axes[i]) * (vertex[i] / axes[i]))
                .sum::<f64>()
                - 1.;
            ensure!(horizon.is_finite(), "Coverage horizon overflow");
            points.push(Point {
                clip: camera.clip_ecef(vertex)?,
                horizon,
            });
        }
        for plane in 0..7 {
            points = trim(&points, plane)?;
            if points.len() < 3 {
                break;
            }
        }
        if points.len() < 3 {
            continue;
        }
        let mut ring = Vec::with_capacity(points.len() + 1);
        for point in points {
            let [x, y, _, w] = point.clip;
            ensure!(w > 0., "Invalid clipped coverage depth");
            let p = [
                (x / w + 1.) * viewport[0] / 2.,
                (1. - y / w) * viewport[1] / 2.,
            ];
            ensure!(p.iter().all(|x| x.is_finite()), "Coverage screen overflow");
            if ring.last() != Some(&p) {
                ring.push(p)
            }
        }
        if ring.first() == ring.last() {
            ring.pop();
        }
        if ring.len() < 3 {
            continue;
        }
        // Translation avoids catastrophic cancellation for narrow clipped cells.
        let origin = ring[0];
        let area = ring
            .windows(2)
            .map(|p| {
                (p[0][0] - origin[0]) * (p[1][1] - origin[1])
                    - (p[1][0] - origin[0]) * (p[0][1] - origin[1])
            })
            .sum::<f64>();
        ensure!(area.is_finite(), "Projected coverage area overflow");
        if area == 0. {
            continue;
        }
        ring.push(ring[0]);
        input_coordinates = input_coordinates
            .checked_add(ring.len())
            .ok_or_else(|| anyhow::anyhow!("Coverage input coordinate count overflow"))?;
        // Reserve equal space for an output boundary before invoking overlay.
        ensure!(
            input_coordinates <= limits.max_region_coordinates / 2,
            "Coverage input/output coordinate budget exceeded"
        );
        polygons.push(geo::Polygon::new(
            geo::LineString::from(ring.into_iter().map(|p| (p[0], p[1])).collect::<Vec<_>>()),
            vec![],
        ));
    }
    let result = Region::from_polygon_batch(polygons)?;
    bounded(&result, limits.max_region_coordinates - input_coordinates)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geodesy::GeographicPosition;
    use geo::{Contains, Point};
    fn p(lat: f64, lon: f64) -> [f64; 3] {
        GeographicPosition::new(lat, (lon + 180.).rem_euclid(360.) - 180.)
            .unwrap()
            .to_ecef(0.)
            .unwrap()
    }
    fn camera(lat: f64, lon: f64, range: f64, heading: f64, tilt: f64) -> GlobeCamera {
        GlobeCamera::orbit(
            GeographicPosition::new(lat, (lon + 180.).rem_euclid(360.) - 180.).unwrap(),
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
    fn has(r: &Region, p: [f64; 2]) -> bool {
        r.polygons()
            .iter()
            .any(|g| g.contains(&Point::new(p[0], p[1])))
    }
    #[test]
    fn dateline_polar_and_tilted_front_surfaces_project() {
        for (lat, lon, heading, tilt) in [
            (48., 179.99, 0., 0.),
            (48., 179.99, 90., 70.),
            (85., 0., 30., 45.),
        ] {
            let c = camera(lat, lon, 30000., heading, tilt);
            let tris = [
                [
                    p(lat - 0.01, lon - 0.01),
                    p(lat - 0.01, lon + 0.01),
                    p(lat + 0.01, lon + 0.01),
                ],
                [
                    p(lat - 0.01, lon - 0.01),
                    p(lat + 0.01, lon + 0.01),
                    p(lat + 0.01, lon - 0.01),
                ],
            ];
            let r =
                project_coverage_triangles(&c, tris, CoverageProjectionLimits::default()).unwrap();
            assert!(!r.is_empty());
            let center = c.project_visible(p(lat, lon)).unwrap().unwrap().screen_px;
            assert!(has(&r, center));
        }
    }
    #[test]
    fn rear_is_empty_and_crossing_horizon_is_clipped() {
        let c = camera(0., 0., WGS84_A * 2., 0., 0.);
        assert!(project_coverage_triangles(
            &c,
            [[p(-5., 175.), p(5., 175.), p(0., 180.)]],
            CoverageProjectionLimits::default()
        )
        .unwrap()
        .is_empty());
        let tri = [p(-8., 50.), p(8., 50.), p(0., 75.)];
        let r = project_coverage_triangles(&c, [tri], CoverageProjectionLimits::default()).unwrap();
        assert!(!r.is_empty());
        // A back-facing source vertex would otherwise be projected into this image.
        let q = c.clip_ecef(tri[2]).unwrap();
        let v = c.viewport();
        let back = [
            (q[0] / q[3] + 1.) * v[0] / 2.,
            (1. - q[1] / q[3]) * v[1] / 2.,
        ];
        assert!(!has(&r, back));
    }
    #[test]
    fn shared_triangle_union_preserves_hole_and_disconnected_island() {
        let c = camera(0., 0., 50000., 0., 0.);
        let mut tris = Vec::new();
        let rects = [
            [-0.1, -0.1, 0.1, -0.03],
            [-0.1, 0.03, 0.1, 0.1],
            [-0.1, -0.03, -0.03, 0.03],
            [0.03, -0.03, 0.1, 0.03],
            [0.15, 0.15, 0.18, 0.18],
        ];
        for [x0, y0, x1, y1] in rects {
            let [a, b, d, e] = [p(y0, x0), p(y0, x1), p(y1, x1), p(y1, x0)];
            tris.extend([[a, b, d], [a, d, e]]);
        }
        let r = project_coverage_triangles(&c, tris, CoverageProjectionLimits::default()).unwrap();
        assert!(!has(&r, [320., 240.]));
        assert!(r.polygons().iter().any(|p| !p.interiors().is_empty()));
        let island = c.project_visible(p(0.165, 0.165)).unwrap().unwrap();
        assert!(has(&r, island.screen_px));
    }
    #[test]
    fn malformed_surface_and_budgets_are_rejected() {
        let c = camera(0., 0., 50000., 0., 0.);
        let t = [p(-0.1, -0.1), p(-0.1, 0.1), p(0.1, 0.1)];
        let mut bad = t;
        bad[0][0] = f64::NAN;
        assert!(
            project_coverage_triangles(&c, [bad], CoverageProjectionLimits::default()).is_err()
        );
        bad = t;
        bad[0][0] += 100.;
        assert!(
            project_coverage_triangles(&c, [bad], CoverageProjectionLimits::default()).is_err()
        );
        assert!(project_coverage_triangles(
            &c,
            [t, t],
            CoverageProjectionLimits {
                max_triangles: 1,
                max_region_coordinates: 100
            }
        )
        .is_err());
        assert!(project_coverage_triangles(
            &c,
            [t],
            CoverageProjectionLimits {
                max_triangles: 1,
                max_region_coordinates: 3
            }
        )
        .is_err());
    }
    #[test]
    fn pixel_membership_matches_independent_ray_triangle_oracle() {
        let cross = |a: [f64; 3], b: [f64; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let dot = |a: [f64; 3], b: [f64; 3]| (0..3).map(|i| a[i] * b[i]).sum::<f64>();
        let sub = |a: [f64; 3], b: [f64; 3]| std::array::from_fn(|i| a[i] - b[i]);
        let mut positive = 0;
        for (lat, lon, heading, tilt) in [
            (48., 179.99, 0., 0.),
            (48., 179.99, 90., 70.),
            (85., 0., 30., 45.),
        ] {
            let c = camera(lat, lon, 50000., heading, tilt);
            let tris = [
                [
                    p(lat - 0.05, lon - 0.5),
                    p(lat - 0.05, lon + 0.5),
                    p(lat + 0.05, lon + 0.5),
                ],
                [
                    p(lat - 0.05, lon - 0.5),
                    p(lat + 0.05, lon + 0.5),
                    p(lat + 0.05, lon - 0.5),
                ],
            ];
            let r =
                project_coverage_triangles(&c, tris, CoverageProjectionLimits::default()).unwrap();
            for y in (8..480).step_by(16) {
                for x in (8..640).step_by(16) {
                    let pixel = [x as f64 + 0.5, y as f64 + 0.5];
                    let ray = c.ray(pixel).unwrap();
                    let expected = tris.iter().any(|t| {
                        let e1 = sub(t[1], t[0]);
                        let e2 = sub(t[2], t[0]);
                        let q = cross(ray.direction, e2);
                        let det = dot(e1, q);
                        if det.abs() < 1e-12 {
                            return false;
                        }
                        let relative = sub(ray.origin_m, t[0]);
                        let u = dot(relative, q) / det;
                        if !(0. ..=1.).contains(&u) {
                            return false;
                        }
                        let z = cross(relative, e1);
                        let v = dot(ray.direction, z) / det;
                        if v < 0. || u + v > 1. {
                            return false;
                        }
                        let distance = dot(e2, z) / det;
                        if distance <= 0. {
                            return false;
                        }
                        let hit: [f64; 3] =
                            std::array::from_fn(|i| ray.origin_m[i] + distance * ray.direction[i]);
                        let clip = c.clip_ecef(hit).unwrap();
                        let axes = [WGS84_A, WGS84_A, WGS84_B];
                        let horizon = (0..3)
                            .map(|i| (c.eye_m()[i] / axes[i]) * (hit[i] / axes[i]))
                            .sum::<f64>()
                            - 1.;
                        horizon >= 0. && clip[2] >= 0. && clip[2] <= clip[3]
                    });
                    assert_eq!(
                        has(&r, pixel),
                        expected,
                        "pixel={pixel:?}, heading={heading}, tilt={tilt}"
                    );
                    positive += usize::from(expected);
                }
            }
        }
        assert!(positive > 20);
    }
    #[test]
    fn aggregate_coordinate_budget_and_inside_camera_reject() {
        let c = camera(0., 0., 50000., 0., 0.);
        let a = [p(-0.1, -0.1), p(-0.1, 0.), p(0., 0.)];
        let b = [p(0.01, 0.01), p(0.01, 0.1), p(0.1, 0.1)];
        assert!(project_coverage_triangles(
            &c,
            [a, b],
            CoverageProjectionLimits {
                max_triangles: 2,
                max_region_coordinates: 4
            }
        )
        .is_err());
        assert!(GlobeCamera::look_at(
            [0., 0., 0.],
            [WGS84_A, 0., 0.],
            [0., 0., 1.],
            [640., 480.],
            45.,
            1.,
            1e9
        )
        .is_err());
    }
}
