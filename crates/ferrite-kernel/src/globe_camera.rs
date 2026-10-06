//! WGS84 perspective camera math independent of products, windows, and GPU APIs.
//! ECEF and distances use metres. Screen coordinates are physical pixels,
//! origin upper left. Clip depth follows WebGPU's 0..1 convention.
use crate::{
    geocentric::{from_ecef, GeodeticPosition},
    geodesy::{intersect_wgs84_ray, GeographicPosition, WGS84_A, WGS84_B},
};
use anyhow::{ensure, Result};
type V = [f64; 3];
fn dot(a: V, b: V) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
fn sub(a: V, b: V) -> V {
    std::array::from_fn(|i| a[i] - b[i])
}
fn cross(a: V, b: V) -> V {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn unit(v: V) -> Result<V> {
    let n = v[0].hypot(v[1]).hypot(v[2]);
    ensure!(n.is_finite() && n > 0., "Invalid camera basis");
    Ok(v.map(|x| x / n))
}
fn finite(v: V) -> bool {
    v.iter().all(|x| x.is_finite())
}
#[derive(Debug, Clone, Copy)]
pub struct ViewRay {
    pub origin_m: V,
    pub direction: V,
}
#[derive(Debug, Clone, Copy)]
pub struct ProjectedPosition {
    pub screen_px: [f64; 2],
    pub forward_depth_m: f64,
    pub clip_depth: f64,
}
#[derive(Debug, Clone, Copy)]
pub struct GlobePick {
    pub geodetic: GeodeticPosition,
    pub ecef_m: V,
    pub distance_m: f64,
}
/// Product-neutral snapshot of the f64 perspective calculation. Backends may
/// encode these values for their device; CPU projection remains authoritative.
#[derive(Debug, Clone, Copy)]
pub struct GlobeProjectionFrame {
    pub eye_m: [f64; 3],
    pub right: [f64; 3],
    pub up: [f64; 3],
    pub forward: [f64; 3],
    pub divisors: [f64; 2],
    pub depth: [f64; 2],
}
/// Local principal ground scales of perspective projection. Millimetre size is
/// a display calibration supplied by the caller, not inferred from the window.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceScale {
    pub metres_per_pixel_min: f64,
    pub metres_per_pixel_max: f64,
    pub denominator_min: f64,
    pub denominator_max: f64,
}
#[derive(Debug, Clone)]
pub struct GlobeCamera {
    eye_m: V,
    forward: V,
    right: V,
    up: V,
    viewport: [f64; 2],
    tan_half_fov: f64,
    near_m: f64,
    far_m: f64,
    // Immutable camera coefficients. Keep the original arithmetic order and
    // divide by cached denominators instead of substituting reciprocals.
    clip_x_divisor: f64,
    depth_scale: f64,
    depth_offset: f64,
    side_extents: [f64; 2],
    side_norms: [f64; 2],
}
impl GlobeCamera {
    pub fn look_at(
        eye_m: V,
        target_m: V,
        up: V,
        viewport: [f64; 2],
        vertical_fov_deg: f64,
        near_m: f64,
        far_m: f64,
    ) -> Result<Self> {
        ensure!(
            finite(eye_m) && finite(target_m) && finite(up),
            "Non-finite globe camera"
        );
        ensure!(
            viewport.iter().all(|x| x.is_finite() && *x > 0.),
            "Invalid camera viewport"
        );
        ensure!(
            vertical_fov_deg.is_finite() && (1e-3..179.).contains(&vertical_fov_deg),
            "Invalid camera field of view"
        );
        ensure!(
            near_m.is_finite() && far_m.is_finite() && near_m > 0. && far_m > near_m,
            "Invalid camera clipping range"
        );
        let outside = (eye_m[0] / WGS84_A).powi(2)
            + (eye_m[1] / WGS84_A).powi(2)
            + (eye_m[2] / WGS84_B).powi(2);
        ensure!(
            outside.is_finite() && outside > 1.,
            "Globe camera must be outside the ellipsoid"
        );
        let forward = unit(sub(target_m, eye_m))?;
        let up_hint = unit(up)?;
        let raw_right = cross(forward, up_hint);
        ensure!(
            dot(raw_right, raw_right) > 1e-24,
            "Collinear camera direction/up"
        );
        let right = unit(raw_right)?;
        let up = unit(cross(right, forward))?;
        let tan_half_fov = (vertical_fov_deg.to_radians() / 2.).tan();
        let side_extents = [tan_half_fov * viewport[0] / viewport[1], tan_half_fov];
        Ok(Self {
            eye_m,
            forward,
            right,
            up,
            viewport,
            tan_half_fov,
            near_m,
            far_m,
            clip_x_divisor: tan_half_fov * (viewport[0] / viewport[1]),
            depth_scale: far_m / (far_m - near_m),
            depth_offset: far_m * near_m / (far_m - near_m),
            side_extents,
            side_norms: side_extents.map(|v| v.hypot(1.)),
        })
    }
    /// Distance to focus, not geocentric radius. Heading is clockwise from north;
    /// tilt zero looks straight down along the geodetic normal. At a pole the
    /// focus longitude defines the east/north convention.
    pub fn orbit(
        focus: GeographicPosition,
        distance_m: f64,
        heading_deg: f64,
        tilt_deg: f64,
        viewport: [f64; 2],
        vertical_fov_deg: f64,
        near_m: f64,
        far_m: f64,
    ) -> Result<Self> {
        ensure!(
            distance_m.is_finite()
                && distance_m > 0.
                && heading_deg.is_finite()
                && tilt_deg.is_finite()
                && (0. ..90.).contains(&tilt_deg),
            "Invalid globe orbit"
        );
        let (s, c) = focus.latitude().to_radians().sin_cos();
        let (sl, cl) = focus.longitude().to_radians().sin_cos();
        let east = [-sl, cl, 0.];
        let north = [-s * cl, -s * sl, c];
        let normal = [c * cl, c * sl, s];
        let (sh, ch) = heading_deg.to_radians().sin_cos();
        let heading: V = std::array::from_fn(|i| north[i] * ch + east[i] * sh);
        let (st, ct) = tilt_deg.to_radians().sin_cos();
        let target = focus.to_ecef(0.)?;
        let eye =
            std::array::from_fn(|i| target[i] + distance_m * (normal[i] * ct - heading[i] * st));
        Self::look_at(
            eye,
            target,
            heading,
            viewport,
            vertical_fov_deg,
            near_m,
            far_m,
        )
    }
    /// Differentiate homogeneous projection in the local geodetic east/north
    /// tangent plane. Singular values retain tilt anisotropy without assuming a
    /// planar geographic bounding box or averaging away the coarse direction.
    pub fn surface_scale(
        &self,
        point: GeographicPosition,
        pixels_per_mm: f64,
    ) -> Result<Option<SurfaceScale>> {
        ensure!(
            pixels_per_mm.is_finite() && pixels_per_mm > 0.,
            "Invalid display calibration"
        );
        let e = point.to_ecef(0.)?;
        if self.project_visible(e)?.is_none() {
            return Ok(None);
        }
        let p = sub(e, self.eye_m);
        let z = dot(p, self.forward);
        let x = dot(p, self.right);
        let y = dot(p, self.up);
        let (s, c) = point.latitude().to_radians().sin_cos();
        let (sl, cl) = point.longitude().to_radians().sin_cos();
        let east = [-sl, cl, 0.];
        let north = [-s * cl, -s * sl, c];
        let focal = self.viewport[1] / (2. * self.tan_half_fov);
        let derivative = |d: V| {
            [
                focal * (dot(d, self.right) * z - x * dot(d, self.forward)) / (z * z),
                -focal * (dot(d, self.up) * z - y * dot(d, self.forward)) / (z * z),
            ]
        };
        let a = derivative(east);
        let b = derivative(north);
        let aa = dot([a[0], a[1], 0.], [a[0], a[1], 0.]);
        let bb = dot([b[0], b[1], 0.], [b[0], b[1], 0.]);
        let ab = a[0] * b[0] + a[1] * b[1];
        let largest = ((aa + bb + (aa - bb).hypot(2. * ab)) / 2.).sqrt();
        let smallest = (a[0] * b[1] - a[1] * b[0]).abs() / largest;
        ensure!(
            smallest.is_finite() && smallest > 0. && largest.is_finite(),
            "Singular surface scale at horizon"
        );
        let min = 1. / largest;
        let max = 1. / smallest;
        Ok(Some(SurfaceScale {
            metres_per_pixel_min: min,
            metres_per_pixel_max: max,
            denominator_min: min * pixels_per_mm * 1000.,
            denominator_max: max * pixels_per_mm * 1000.,
        }))
    }
    /// Screen derivative of a true-north bearing in the geodetic tangent plane.
    /// Returns pixels per metre; clockwise screen orientation can be resolved
    /// without finite differencing or assuming that perspective preserves angles.
    pub fn project_bearing(&self, point: GeographicPosition, bearing_deg: f64) -> Result<[f64; 2]> {
        ensure!(bearing_deg.is_finite(), "Invalid geographic bearing");
        let e = point.to_ecef(0.)?;
        ensure!(
            self.project_visible(e)?.is_some(),
            "Bearing anchor is not visible"
        );
        let p = sub(e, self.eye_m);
        let z = dot(p, self.forward);
        let x = dot(p, self.right);
        let y = dot(p, self.up);
        let (s, c) = point.latitude().to_radians().sin_cos();
        let (sl, cl) = point.longitude().to_radians().sin_cos();
        let (sb, cb) = bearing_deg.to_radians().sin_cos();
        let d = [-sl * sb - s * cl * cb, cl * sb - s * sl * cb, c * cb];
        let focal = self.viewport[1] / (2. * self.tan_half_fov);
        let v = [
            focal * (dot(d, self.right) * z - x * dot(d, self.forward)) / (z * z),
            -focal * (dot(d, self.up) * z - y * dot(d, self.forward)) / (z * z),
        ];
        ensure!(
            v.iter().all(|x| x.is_finite()) && v[0].hypot(v[1]) > 0.,
            "Singular projected bearing"
        );
        Ok(v)
    }
    pub fn eye_m(&self) -> V {
        self.eye_m
    }
    pub fn viewport(&self) -> [f64; 2] {
        self.viewport
    }
    /// Signed distances in metres to the six WebGPU clip half-spaces.
    /// Camera-relative coordinates avoid cancellation from global plane offsets.
    /// The side planes can expand by physical pixels for fixed-width strokes.
    pub fn frustum_distances(&self, point_m: V, margin_px: f64) -> Result<[f64; 6]> {
        ensure!(
            finite(point_m) && margin_px.is_finite() && margin_px >= 0.,
            "Invalid frustum distance input"
        );
        let p = sub(point_m, self.eye_m);
        let x = dot(p, self.right);
        let y = dot(p, self.up);
        let z = dot(p, self.forward);
        let (hx, hy, nx, ny) = if margin_px == 0. {
            (
                self.side_extents[0],
                self.side_extents[1],
                self.side_norms[0],
                self.side_norms[1],
            )
        } else {
            let hx = self.side_extents[0] * (1. + 2. * margin_px / self.viewport[0]);
            let hy = self.side_extents[1] * (1. + 2. * margin_px / self.viewport[1]);
            (hx, hy, hx.hypot(1.), hy.hypot(1.))
        };
        let distances = [
            (x + hx * z) / nx,
            (-x + hx * z) / nx,
            (y + hy * z) / ny,
            (-y + hy * z) / ny,
            z - self.near_m,
            self.far_m - z,
        ];
        ensure!(
            distances.iter().all(|v| v.is_finite()),
            "Frustum distance overflow"
        );
        Ok(distances)
    }
    /// A conservative displacement sphere misses the view when one separating
    /// plane is farther than its radius. Borderline floating-point cases stay.
    pub fn sphere_outside_frustum(
        &self,
        centre_m: V,
        radius_m: f64,
        margin_px: f64,
    ) -> Result<bool> {
        ensure!(
            radius_m.is_finite() && radius_m >= 0.,
            "Invalid frustum sphere radius"
        );
        let p = sub(centre_m, self.eye_m);
        let tolerance =
            64. * f64::EPSILON * p[0].hypot(p[1]).hypot(p[2]).max(self.far_m).max(radius_m);
        Ok(self
            .frustum_distances(centre_m, margin_px)?
            .iter()
            .any(|d| *d < -radius_m - tolerance))
    }
    pub fn depth_range_m(&self) -> [f64; 2] {
        [self.near_m, self.far_m]
    }
    pub fn projection_frame(&self) -> GlobeProjectionFrame {
        GlobeProjectionFrame {
            eye_m: self.eye_m,
            right: self.right,
            up: self.up,
            forward: self.forward,
            divisors: [self.clip_x_divisor, self.tan_half_fov],
            depth: [self.depth_scale, self.depth_offset],
        }
    }
    /// Floating-point reversed depth: near maps to one and far to zero.
    /// Derive coefficients directly from near/far, avoiding subtraction from
    /// a depth already rounded close to one. The ordinary clip API is unchanged.
    pub fn reverse_depth_projection_frame(&self) -> GlobeProjectionFrame {
        let mut frame = self.projection_frame();
        frame.depth = [
            -self.near_m / (self.far_m - self.near_m),
            -self.depth_offset,
        ];
        frame
    }
    pub fn clip_ecef_reverse_depth(&self, point_m: V) -> Result<[f64; 4]> {
        let mut clip = self.clip_ecef(point_m)?;
        clip[2] = self.depth_offset - self.near_m / (self.far_m - self.near_m) * clip[3];
        ensure!(clip[2].is_finite(), "Globe reversed depth overflow");
        Ok(clip)
    }
    /// Homogeneous clip position retains points outside the frustum. Subtract
    /// the f64 ECEF eye before narrowing to GPU floats to avoid metre-scale
    /// loss of precision in high zoom views.
    pub fn clip_ecef(&self, point_m: V) -> Result<[f64; 4]> {
        ensure!(finite(point_m), "Non-finite globe vertex");
        let relative = sub(point_m, self.eye_m);
        let z = dot(relative, self.forward);
        let clip = [
            dot(relative, self.right) / self.clip_x_divisor,
            dot(relative, self.up) / self.tan_half_fov,
            self.depth_scale * z - self.depth_offset,
            z,
        ];
        ensure!(clip.iter().all(|x| x.is_finite()), "Globe clip overflow");
        Ok(clip)
    }
    /// None means behind the camera, beyond near/far, or occluded by the WGS84
    /// ellipsoid. Off-screen positions are retained for primitive clipping.
    pub fn project_visible(&self, point_m: V) -> Result<Option<ProjectedPosition>> {
        let clip = self.clip_ecef(point_m)?;
        let depth = clip[3];
        if depth < self.near_m || depth > self.far_m {
            return Ok(None);
        }
        let relative = sub(point_m, self.eye_m);
        let length = relative[0].hypot(relative[1]).hypot(relative[2]);
        if let Some(hit) = intersect_wgs84_ray(self.eye_m, relative)? {
            let tolerance = 1e-5_f64.max(length * 32. * f64::EPSILON);
            if hit.distance_m + tolerance < length {
                return Ok(None);
            }
        }
        Ok(Some(ProjectedPosition {
            screen_px: [
                (clip[0] / depth + 1.) * self.viewport[0] / 2.,
                (1. - clip[1] / depth) * self.viewport[1] / 2.,
            ],
            forward_depth_m: depth,
            clip_depth: clip[2] / depth,
        }))
    }
    /// Camera-facing physical-pixel offset at the same forward depth as an
    /// ECEF anchor. Used by screen-fixed strokes/billboards; this is display
    /// geometry, never an ellipsoidal-height or chart-datum conversion.
    /// Embed an authored device pixel in a camera-facing display plane inside
    /// the clip volume. This is temporary display geometry, never a geographic
    /// location, ellipsoid height, or chart datum. It intentionally has no Earth
    /// horizon test: Portrayal CRS belongs to the output device.
    pub fn device_plane_point(&self, screen_px: [f64; 2]) -> Result<V> {
        ensure!(
            screen_px.iter().all(|x| x.is_finite()),
            "Invalid globe device pixel"
        );
        let depth = self.near_m + (self.far_m - self.near_m).min(self.near_m) * 0.5;
        ensure!(
            depth.is_finite() && depth > self.near_m && depth < self.far_m,
            "No representable device display plane"
        );
        let metres_per_pixel = 2. * depth * self.tan_half_fov / self.viewport[1];
        let x = (screen_px[0] - self.viewport[0] * 0.5) * metres_per_pixel;
        let y = (self.viewport[1] * 0.5 - screen_px[1]) * metres_per_pixel;
        let point = std::array::from_fn(|i| {
            self.eye_m[i] + self.forward[i] * depth + self.right[i] * x + self.up[i] * y
        });
        ensure!(finite(point), "Globe device plane overflow");
        Ok(point)
    }
    pub fn offset_pixels(&self, point_m: V, offset: [f64; 2]) -> Result<V> {
        ensure!(
            offset.iter().all(|x| x.is_finite()),
            "Invalid globe screen offset"
        );
        let depth = self.clip_ecef(point_m)?[3];
        ensure!(depth > 0., "Globe billboard behind camera");
        let metres_per_pixel = 2. * depth * self.tan_half_fov / self.viewport[1];
        let point = std::array::from_fn(|i| {
            point_m[i] + metres_per_pixel * (self.right[i] * offset[0] - self.up[i] * offset[1])
        });
        ensure!(finite(point), "Globe screen offset overflow");
        Ok(point)
    }
    pub fn ray(&self, screen_px: [f64; 2]) -> Result<ViewRay> {
        ensure!(
            screen_px.iter().all(|x| x.is_finite()),
            "Invalid globe pick pixel"
        );
        let sx = (screen_px[0] * 2. / self.viewport[0] - 1.) * self.tan_half_fov * self.viewport[0]
            / self.viewport[1];
        let sy = (1. - screen_px[1] * 2. / self.viewport[1]) * self.tan_half_fov;
        let direction = unit(std::array::from_fn(|i| {
            self.forward[i] + self.right[i] * sx + self.up[i] * sy
        }))?;
        Ok(ViewRay {
            origin_m: self.eye_m,
            direction,
        })
    }
    pub fn pick(&self, screen_px: [f64; 2]) -> Result<Option<GlobePick>> {
        let ray = self.ray(screen_px)?;
        let Some(hit) = intersect_wgs84_ray(ray.origin_m, ray.direction)? else {
            return Ok(None);
        };
        let depth = self.clip_ecef(hit.ecef_m)?[3];
        if depth < self.near_m || depth > self.far_m {
            return Ok(None);
        }
        Ok(Some(GlobePick {
            geodetic: from_ecef(hit.ecef_m)?,
            ecef_m: hit.ecef_m,
            distance_m: hit.distance_m,
        }))
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn cached_camera_coefficients_match_original_arithmetic_exactly() {
        for lat in [-89.9, -30., 0., 50., 89.9] {
            for (range, heading, tilt, viewport, fov, near, far) in [
                (500., 0., 0., [2780., 1934.], 45., 1., 1e8),
                (1e7, 87., 75., [800., 1000.], 120., 0.01, 1e12),
                (10000., 179., 45., [1000., 800.], 10., 100., 100000.),
            ] {
                let camera = GlobeCamera::orbit(
                    GeographicPosition::new(lat, 179.9).unwrap(),
                    range,
                    heading,
                    tilt,
                    viewport,
                    fov,
                    near,
                    far,
                )
                .unwrap();
                for e in [
                    camera.eye_m,
                    [0.; 3],
                    [1e9, -1e9, 2e8],
                    GeographicPosition::new(lat, -179.9)
                        .unwrap()
                        .to_ecef(0.)
                        .unwrap(),
                ] {
                    let p = sub(e, camera.eye_m);
                    let x = dot(p, camera.right);
                    let y = dot(p, camera.up);
                    let z = dot(p, camera.forward);
                    let aspect = camera.viewport[0] / camera.viewport[1];
                    let old_clip = [
                        x / (camera.tan_half_fov * aspect),
                        y / camera.tan_half_fov,
                        camera.far_m / (camera.far_m - camera.near_m) * z
                            - camera.far_m * camera.near_m / (camera.far_m - camera.near_m),
                        z,
                    ];
                    assert_eq!(
                        camera.clip_ecef(e).unwrap().map(f64::to_bits),
                        old_clip.map(f64::to_bits)
                    );
                    for margin in [0., -0., 0.001, 5., 10000.] {
                        let hx = camera.tan_half_fov * camera.viewport[0] / camera.viewport[1]
                            * (1. + 2. * margin / camera.viewport[0]);
                        let hy = camera.tan_half_fov * (1. + 2. * margin / camera.viewport[1]);
                        let old_distances = [
                            (x + hx * z) / hx.hypot(1.),
                            (-x + hx * z) / hx.hypot(1.),
                            (y + hy * z) / hy.hypot(1.),
                            (-y + hy * z) / hy.hypot(1.),
                            z - camera.near_m,
                            camera.far_m - z,
                        ];
                        assert_eq!(
                            camera
                                .frustum_distances(e, margin)
                                .unwrap()
                                .map(f64::to_bits),
                            old_distances.map(f64::to_bits)
                        );
                    }
                }
            }
        }
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            500.,
            0.,
            0.,
            [800., 600.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        assert!(c.clip_ecef([f64::NAN, 0., 0.]).is_err());
        assert!(c.frustum_distances([0.; 3], -1.).is_err());
        assert!(c.frustum_distances([f64::INFINITY, 0., 0.], 0.).is_err());
    }

    use super::*;
    fn p(lat: f64, lon: f64) -> GeographicPosition {
        GeographicPosition::new(lat, lon).unwrap()
    }
    #[test]
    fn physical_billboard_offsets_preserve_camera_depth() {
        for tilt in [0., 35., 80.] {
            let p = p(48.65, -2.05);
            let c = GlobeCamera::orbit(p, 10000., 90., tilt, [1000., 800.], 45., 1., 1e9).unwrap();
            let anchor = p.to_ecef(0.).unwrap();
            let a = c.clip_ecef(anchor).unwrap();
            let offset = c.offset_pixels(anchor, [17., -9.]).unwrap();
            let b = c.clip_ecef(offset).unwrap();
            assert!((a[3] - b[3]).abs() < 1e-8);
            let dx = (b[0] / b[3] - a[0] / a[3]) * 500.;
            let dy = -(b[1] / b[3] - a[1] / a[3]) * 400.;
            assert!((dx - 17.).abs() < 1e-8 && (dy + 9.).abs() < 1e-8);
        }
    }
    #[test]
    fn perspective_pick_matches_projection_at_tilt_heading_poles_and_dateline() {
        for focus in [
            p(0., 0.),
            p(48.65, -2.05),
            p(80., 179.9),
            p(90., 30.),
            p(-90., -120.),
        ] {
            for distance in [100., 10000., WGS84_A, 4. * WGS84_A] {
                for tilt in [0., 35., 80.] {
                    for heading in [0., 90., 270.] {
                        let c = GlobeCamera::orbit(
                            focus,
                            distance,
                            heading,
                            tilt,
                            [1600., 900.],
                            45.,
                            0.1,
                            1e9,
                        )
                        .unwrap();
                        let q = c
                            .project_visible(focus.to_ecef(0.).unwrap())
                            .unwrap()
                            .unwrap();
                        assert!(
                            (q.screen_px[0] - 800.).abs() < 1e-5
                                && (q.screen_px[1] - 450.).abs() < 1e-5,
                            "{q:?}"
                        );
                        let hit = c.pick(q.screen_px).unwrap().unwrap();
                        let error = crate::geodesy::inverse(focus, hit.geodetic.surface)
                            .unwrap()
                            .distance_m;
                        assert!(error < 2e-5, "distance={distance} tilt={tilt} {error}");
                        assert!(hit.geodetic.ellipsoidal_height_m.abs() < 2e-5);
                    }
                }
            }
        }
    }
    #[test]
    fn far_side_occluded_and_space_missed_without_spherical_substitution() {
        let c = GlobeCamera::orbit(p(0., 0.), 2. * WGS84_A, 0., 0., [1000., 800.], 45., 1., 1e9)
            .unwrap();
        assert!(c
            .project_visible(p(0., 180.).to_ecef(0.).unwrap())
            .unwrap()
            .is_none());
        assert!(c
            .project_visible(p(0., 0.).to_ecef(0.).unwrap())
            .unwrap()
            .is_some());
        assert!(c.pick([0., 0.]).unwrap().is_none());
        // Equatorial camera at 3a: exact ellipsoid tangent at longitude acos(1/3).
        let horizon = (1. / 3_f64).acos().to_degrees();
        assert!(c
            .project_visible(p(0., horizon - 0.01).to_ecef(0.).unwrap())
            .unwrap()
            .is_some());
        assert!(c
            .project_visible(p(0., horizon + 0.01).to_ecef(0.).unwrap())
            .unwrap()
            .is_none());
    }
    #[test]
    fn invalid_camera_and_pick_fail_explicitly() {
        for eye in [[0.; 3], [WGS84_A, 0., 0.], [f64::NAN, 0., 0.]] {
            assert!(
                GlobeCamera::look_at(eye, [0.; 3], [0., 0., 1.], [1000., 800.], 45., 1., 1e9)
                    .is_err()
            );
        }
        let c = GlobeCamera::orbit(p(0., 0.), 1000., 0., 0., [1000., 800.], 45., 1., 1e9).unwrap();
        assert!(c.pick([f64::NAN, 1.]).is_err());
        assert!(
            GlobeCamera::orbit(p(0., 0.), 1000., 0., 90., [1000., 800.], 45., 1., 1e9).is_err()
        );
        assert!(GlobeCamera::look_at(
            [2. * WGS84_A, 0., 0.],
            [0.; 3],
            [-1., 0., 0.],
            [1000., 800.],
            45.,
            1.,
            1e9
        )
        .is_err());
    }
}

#[cfg(test)]
mod frustum_tests {
    use super::*;
    #[test]
    fn world_half_spaces_match_homogeneous_clip_and_keep_crossing_spheres() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(48.65, -2.05).unwrap(),
            500.,
            0.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        for lat in [48.64, 48.65, 48.66] {
            for lon in [-2.06, -2.05, -2.04] {
                let p = GeographicPosition::new(lat, lon)
                    .unwrap()
                    .to_ecef(0.)
                    .unwrap();
                let h = c.clip_ecef(p).unwrap();
                let d = c.frustum_distances(p, 0.).unwrap();
                assert_eq!(
                    d.iter().all(|x| *x >= 0.),
                    h[0] >= -h[3]
                        && h[0] <= h[3]
                        && h[1] >= -h[3]
                        && h[1] <= h[3]
                        && h[2] >= 0.
                        && h[2] <= h[3]
                );
                assert_eq!(
                    c.sphere_outside_frustum(p, 0., 0.).unwrap(),
                    d.iter().any(|x| *x < -1e-5)
                );
            }
        }
        let eye = c.eye_m();
        assert!(c.sphere_outside_frustum(eye, 0., 0.).unwrap());
        assert!(!c.sphere_outside_frustum(eye, 2., 0.).unwrap());
        assert!(c.frustum_distances(eye, -1.).is_err());
        assert!(c.sphere_outside_frustum(eye, f64::NAN, 0.).is_err());
    }
}

#[cfg(test)]
mod surface_scale_tests {
    use super::*;
    #[test]
    fn focus_metric_matches_independent_camera_ray_ground_distances() {
        for lat in [0., 70., 89.] {
            for tilt in [0., 45., 70.] {
                let f = GeographicPosition::new(lat, 179.99).unwrap();
                let c =
                    GlobeCamera::orbit(f, 30000., 0., tilt, [1200., 800.], 45., 1., 1e8).unwrap();
                let m = c.surface_scale(f, 96. / 25.4).unwrap().unwrap();
                let expected = 30000. * 2. * 22.5_f64.to_radians().tan() / 800.;
                assert!((m.metres_per_pixel_min / expected - 1.).abs() < 1e-9);
                assert!(
                    (m.metres_per_pixel_max * tilt.to_radians().cos() / expected - 1.).abs() < 1e-9
                );
                for (axis, scale) in [(0, m.metres_per_pixel_min), (1, m.metres_per_pixel_max)] {
                    let mut a = [600., 400.];
                    let mut b = a;
                    a[axis] -= 0.001;
                    b[axis] += 0.001;
                    let a = c.pick(a).unwrap().unwrap().geodetic.surface;
                    let b = c.pick(b).unwrap().unwrap().geodetic.surface;
                    let ground = crate::geodesy::inverse(a, b).unwrap().distance_m / 0.002;
                    assert!((ground / scale - 1.).abs() < 1e-6);
                }
            }
        }
    }
    #[test]
    fn metric_density_scaling_and_occlusion_are_explicit() {
        let f = GeographicPosition::new(0., 0.).unwrap();
        let a = GlobeCamera::orbit(f, 5000., 0., 45., [900., 600.], 45., 1., 1e8).unwrap();
        let b = GlobeCamera::orbit(f, 5000., 0., 45., [1800., 1200.], 45., 1., 1e8).unwrap();
        let x = a.surface_scale(f, 96. / 25.4).unwrap().unwrap();
        let y = b.surface_scale(f, 192. / 25.4).unwrap().unwrap();
        assert!((x.denominator_max - y.denominator_max).abs() < 1e-8);
        assert!(a
            .surface_scale(GeographicPosition::new(0., 180.).unwrap(), 1.)
            .unwrap()
            .is_none());
        assert!(a.surface_scale(f, 0.).is_err());
    }
}

#[cfg(test)]
mod reverse_depth_tests {
    use super::*;
    #[test]
    fn reversed_depth_preserves_clip_planes_and_resolves_close_surfaces() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(50., 0.).unwrap(),
            100000.,
            0.,
            0.,
            [1000., 800.],
            45.,
            10.,
            1e10,
        )
        .unwrap();
        let f = c.reverse_depth_projection_frame();
        let point = |z: f64| std::array::from_fn(|i| f.eye_m[i] + f.forward[i] * z);
        for z in [10., 100., 100000., 100000.01, 1e9, 1e10] {
            let old = c.clip_ecef(point(z)).unwrap();
            let new = c.clip_ecef_reverse_depth(point(z)).unwrap();
            assert_eq!(
                [old[0].to_bits(), old[1].to_bits(), old[3].to_bits()],
                [new[0].to_bits(), new[1].to_bits(), new[3].to_bits()]
            );
            assert!((old[2] / old[3] + new[2] / new[3] - 1.).abs() < 1e-12);
        }
        let a = c.clip_ecef(point(100000.)).unwrap();
        let b = c.clip_ecef(point(100000.01)).unwrap();
        assert_eq!((a[2] / a[3]) as f32, (b[2] / b[3]) as f32);
        let a = c.clip_ecef_reverse_depth(point(100000.)).unwrap();
        let b = c.clip_ecef_reverse_depth(point(100000.01)).unwrap();
        assert!((a[2] / a[3]) as f32 > (b[2] / b[3]) as f32);
    }
}

#[cfg(test)]
mod device_plane_tests {
    use super::*;
    #[test]
    fn device_pixels_survive_camera_distance_heading_tilt_and_geographic_focus() {
        for focus in [(0., 0.), (179.9, 70.), (-179.9, -70.)] {
            for range in [30., 30000., 2e7] {
                for heading in [0., 90., 180., 270.] {
                    for tilt in [0., 45., 70.] {
                        let c = GlobeCamera::orbit(
                            GeographicPosition::new(focus.1, focus.0).unwrap(),
                            range,
                            heading,
                            tilt,
                            [800., 600.],
                            45.,
                            1.,
                            1e9,
                        )
                        .unwrap();
                        for pixel in [[0., 0.], [400., 300.], [800., 600.], [-100., 700.]] {
                            let p = c.device_plane_point(pixel).unwrap();
                            let clip = c.clip_ecef(p).unwrap();
                            assert!(clip[3] > 1. && clip[3] < 1e9);
                            let screen = [
                                (clip[0] / clip[3] + 1.) * 400.,
                                (1. - clip[1] / clip[3]) * 300.,
                            ];
                            assert!(
                                (screen[0] - pixel[0]).abs() < 2e-5
                                    && (screen[1] - pixel[1]).abs() < 2e-5,
                                "Device pixel changed with camera: {screen:?}/{pixel:?}"
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn device_plane_rejects_invalid_and_overflowing_pixels() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            30000.,
            0.,
            0.,
            [800., 600.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        assert!(c.device_plane_point([f64::NAN, 0.]).is_err());
        // Finite authored input can still overflow intermediate arithmetic.
        let wide = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            30000.,
            0.,
            0.,
            [1., 1.],
            178.,
            1.,
            1e8,
        )
        .unwrap();
        assert!(wide.device_plane_point([f64::MAX, f64::MAX]).is_err());
    }
}
