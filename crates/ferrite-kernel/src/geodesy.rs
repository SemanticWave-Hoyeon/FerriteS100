//! WGS84 geodesy, independent of chart products, rendering, and navigation UI.
//! Angles are degrees, distances are metres. Geographic positions carry named
//! latitude/longitude fields (EPSG:4326 authority axis order is latitude first).
//! Chart-datum depth is NOT ellipsoidal height; no vertical datum conversion is
//! implied by these operations. Geodesics describe the ellipsoid surface only.
use anyhow::{ensure, Result};
use geographiclib_rs::{DirectGeodesic, Geodesic, InverseGeodesic};
use std::sync::OnceLock;

pub const WGS84_A: f64 = 6_378_137.;
pub const WGS84_INV_F: f64 = 298.257_223_563;
pub const WGS84_F: f64 = 1. / WGS84_INV_F;
pub const WGS84_B: f64 = WGS84_A * (1. - WGS84_F);
const E2: f64 = WGS84_F * (2. - WGS84_F);
fn solver() -> &'static Geodesic {
    static G: OnceLock<Geodesic> = OnceLock::new();
    G.get_or_init(Geodesic::wgs84)
}

/// Checked WGS84 geographic position. Longitude is canonical [-180,180].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeographicPosition {
    latitude: f64,
    longitude: f64,
}
/// A drawing copy of a canonical geographic position. The lifted longitude
/// may lie outside [-180,180]; it is not a replacement CRS/source coordinate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeographicDrawingCopy {
    position: GeographicPosition,
    longitude: f64,
}
impl GeographicDrawingCopy {
    pub fn position(self) -> GeographicPosition { self.position }
    pub fn latitude(self) -> f64 { self.position.latitude() }
    pub fn longitude(self) -> f64 { self.longitude }
}
impl GeographicPosition {
    pub fn new(latitude: f64, longitude: f64) -> Result<Self> {
        ensure!(
            latitude.is_finite() && (-90. ..=90.).contains(&latitude),
            "Invalid WGS84 latitude"
        );
        ensure!(
            longitude.is_finite() && (-180. ..=180.).contains(&longitude),
            "Invalid WGS84 longitude"
        );
        Ok(Self {
            latitude,
            longitude,
        })
    }
    pub fn latitude(self) -> f64 {
        self.latitude
    }
    pub fn longitude(self) -> f64 {
        self.longitude
    }
    /// Canonical longitude lifted to the nearest copy around a known meridian.
    /// Use for dateline-safe drawing; never change the stored CRS position.
    pub fn longitude_near(self, meridian: f64) -> Result<f64> {
        ensure!(
            meridian.is_finite() && meridian.abs() <= 1e9,
            "Invalid unwrapped meridian"
        );
        Ok(meridian + (self.longitude - meridian + 180.).rem_euclid(360.) - 180.)
    }
    /// EPSG:4979 to EPSG:4978; height is explicitly above the WGS84 ellipsoid.
    pub fn to_ecef(self, ellipsoidal_height_m: f64) -> Result<[f64; 3]> {
        ensure!(
            ellipsoidal_height_m.is_finite(),
            "Non-finite ellipsoidal height"
        );
        let (s, c) = self.latitude.to_radians().sin_cos();
        let (sl, cl) = self.longitude.to_radians().sin_cos();
        let n = WGS84_A / (1. - E2 * s * s).sqrt();
        let xyz = [
            (n + ellipsoidal_height_m) * c * cl,
            (n + ellipsoidal_height_m) * c * sl,
            (n * (1. - E2) + ellipsoidal_height_m) * s,
        ];
        ensure!(xyz.iter().all(|v| v.is_finite()), "ECEF overflow");
        Ok(xyz)
    }
}

/// Forward azimuths clockwise from true north. At coincident points or an
/// ambiguous antipodal shortest path the solver returns one conventional
/// solution, not a uniquely defined navigational bearing.
#[derive(Debug, Clone, Copy)]
pub struct GeodesicInverse {
    pub distance_m: f64,
    pub initial_azimuth_deg: f64,
    pub final_azimuth_deg: f64,
    pub arc_deg: f64,
}
pub fn inverse(start: GeographicPosition, end: GeographicPosition) -> Result<GeodesicInverse> {
    let (distance_m, initial_azimuth_deg, final_azimuth_deg, arc_deg): (f64, f64, f64, f64) =
        solver().inverse(start.latitude, start.longitude, end.latitude, end.longitude);
    ensure!(
        [distance_m, initial_azimuth_deg, final_azimuth_deg, arc_deg]
            .iter()
            .all(|v| v.is_finite()),
        "Geodesic inverse failed"
    );
    Ok(GeodesicInverse {
        distance_m,
        initial_azimuth_deg,
        final_azimuth_deg,
        arc_deg,
    })
}
/// Signed distance follows the specified geodesic; negative distances travel
/// backwards. This direct path is not necessarily the shortest path if long.
pub fn direct(
    start: GeographicPosition,
    azimuth_deg: f64,
    distance_m: f64,
) -> Result<GeographicPosition> {
    ensure!(
        azimuth_deg.is_finite() && distance_m.is_finite(),
        "Invalid geodesic direct input"
    );
    let (lat, lon): (f64, f64) = solver().direct(
        start.latitude,
        start.longitude,
        (azimuth_deg + 180.).rem_euclid(360.) - 180.,
        distance_m,
    );
    GeographicPosition::new(lat, lon)
}

/// An arc reached at fixed geodesic travel distance from a WGS84 centre.
/// This is a surface curve,
/// not a Cartesian circle or a straight interpolation in longitude/latitude.
/// Radius is metres; azimuth and signed sweep are degrees clockwise from north.
/// Long travel distances beyond a cut locus need not equal the shortest inverse
/// distance to the centre; this API does not resolve that circle ambiguity.
#[derive(Debug, Clone, Copy)]
pub struct GeodesicRadiusArc {
    center: GeographicPosition,
    radius_m: f64,
    start_azimuth_deg: f64,
    sweep_deg: f64,
}
impl GeodesicRadiusArc {
    pub fn new(center: GeographicPosition, radius_m: f64, start_azimuth_deg: f64, sweep_deg: f64) -> Result<Self> {
        ensure!(radius_m.is_finite() && radius_m >= 0., "Invalid geographic arc radius");
        ensure!(start_azimuth_deg.is_finite() && sweep_deg.is_finite(), "Invalid geographic arc angle");
        Ok(Self { center, radius_m, start_azimuth_deg, sweep_deg })
    }
    /// Evaluate a fraction of the signed arc, including exact endpoints.
    pub fn position_at(self, fraction: f64) -> Result<GeographicPosition> {
        ensure!(fraction.is_finite() && (0.0..=1.0).contains(&fraction), "Invalid arc fraction");
        if self.radius_m == 0. { return Ok(self.center); }
        let angle = self.start_azimuth_deg + self.sweep_deg * fraction;
        ensure!(angle.is_finite(), "Geographic arc angle overflow");
        direct(self.center, angle, self.radius_m)
    }
    /// Samples with an explicit azimuth-step bound and vertex budget. This is
    /// an angular sampling contract, not a screen-pixel error guarantee.
    /// A renderer must choose/refine sampling for its projection and camera.
    pub fn sample(self, max_azimuth_step_deg: f64, max_vertices: usize) -> Result<Vec<GeographicPosition>> {
        ensure!(max_azimuth_step_deg.is_finite() && max_azimuth_step_deg > 0. && max_azimuth_step_deg <= 180., "Invalid arc sampling step");
        ensure!((2..=1_000_001).contains(&max_vertices), "Invalid arc vertex budget");
        let segments = (self.sweep_deg.abs() / max_azimuth_step_deg).ceil().max(1.);
        ensure!(segments.is_finite() && segments <= (max_vertices - 1) as f64, "Geographic arc vertex budget exceeded");
        let segments = segments as usize;
        let first = self.position_at(0.)?;
        let mut points = Vec::with_capacity(segments + 1);
        points.push(first);
        for i in 1..segments { points.push(self.position_at(i as f64 / segments as f64)?); }
        // Closed turns have identical endpoint bits. Avoid a false tiny gap
        // from separate trigonometric evaluations of equivalent bearings.
        points.push(if self.sweep_deg.rem_euclid(360.) == 0. { first } else { self.position_at(1.)? });
        Ok(points)
    }
    /// Lift successive longitudes to nearby drawing copies without modifying
    /// source positions. A closed surface arc around a pole can end one turn
    /// away in a flat chart; forcing its last longitude to the first would
    /// introduce a spurious map-wide segment. Sampling remains an angular
    /// contract: callers must refine for their projection/camera, especially
    /// at polar singularities or a 180-degree nearest-copy tie.
    pub fn sample_drawing_copies(self, max_azimuth_step_deg: f64, max_vertices: usize, reference_meridian: f64) -> Result<Vec<GeographicDrawingCopy>> {
        ensure!(reference_meridian.is_finite() && reference_meridian.abs() <= 1e9, "Invalid drawing reference meridian");
        let positions = self.sample(max_azimuth_step_deg, max_vertices)?;
        let mut previous = reference_meridian;
        let mut copies = Vec::with_capacity(positions.len());
        for position in positions {
            ensure!(previous.abs() <= 1e9, "Drawing path longitude overflow");
            // Use an integer turn offset to keep the original longitude bits
            // when no wrap is needed, and exact closure within the same copy.
            // A tie selects the -180-degree displacement, like longitude_near.
            let turns = ((previous - position.longitude() - 180.) / 360.).ceil();
            let longitude = position.longitude() + 360. * turns;
            copies.push(GeographicDrawingCopy { position, longitude });
            previous = longitude;
        }
        Ok(copies)
    }
}

/// Samples one shortest ellipsoidal geodesic. Refuses to exceed the caller's
/// vertex budget instead of silently reducing accuracy or allocating unbounded
/// memory. Heights and vertical datums are outside this surface calculation.
pub fn sample_shortest_path(
    start: GeographicPosition,
    end: GeographicPosition,
    max_segment_m: f64,
    max_vertices: usize,
) -> Result<Vec<GeographicPosition>> {
    ensure!(
        max_segment_m.is_finite() && max_segment_m > 0.,
        "Invalid geodesic segment length"
    );
    ensure!(
        (2..=1_000_001).contains(&max_vertices),
        "Invalid geodesic vertex budget"
    );
    let path = inverse(start, end)?;
    let segments = (path.distance_m / max_segment_m).ceil().max(1.);
    ensure!(
        segments <= (max_vertices - 1) as f64,
        "Geodesic vertex budget exceeded"
    );
    let segments = segments as usize;
    let mut points = Vec::with_capacity(segments + 1);
    points.push(start);
    for i in 1..segments {
        points.push(direct(
            start,
            path.initial_azimuth_deg,
            path.distance_m * i as f64 / segments as f64,
        )?);
    }
    points.push(end);
    Ok(points)
}

/// Distinguishes ellipsoidal World Mercator (EPSG:3395) from spherical Web
/// Mercator (EPSG:3857). Both use metres, easting first, central meridian zero.
/// Neither projection supports the poles; reject singularities explicitly.
#[derive(Debug, Clone, Copy)]
pub enum Mercator {
    World,
    Web,
}
impl Mercator {
    pub fn epsg(self) -> u32 {
        match self {
            Self::World => 3395,
            Self::Web => 3857,
        }
    }
    pub fn project(self, p: GeographicPosition) -> Result<[f64; 2]> {
        ensure!(p.latitude.abs() < 90., "Mercator is undefined at the poles");
        let phi = p.latitude.to_radians();
        let mut y = (phi.tan().asinh()) * WGS84_A;
        if matches!(self, Self::World) {
            let e = E2.sqrt();
            y -= WGS84_A * e * (e * phi.sin()).atanh();
        }
        ensure!(
            y.is_finite() && (y / WGS84_A).abs() < 30.,
            "Mercator northing exceeds numerical domain"
        );
        Ok([WGS84_A * p.longitude.to_radians(), y])
    }
    pub fn unproject(self, xy: [f64; 2]) -> Result<GeographicPosition> {
        ensure!(
            xy.iter().all(|v| v.is_finite()),
            "Non-finite Mercator coordinate"
        );
        // Test the metre-domain bound before division: the exact canonical
        // edge can round to 180.00000000000003 degrees on conversion.
        let lon = (xy[0] / WGS84_A).to_degrees().clamp(-180., 180.);
        ensure!(
            xy[0].abs() <= WGS84_A * std::f64::consts::PI,
            "Mercator easting outside canonical longitude extent"
        );
        let psi = xy[1] / WGS84_A;
        // At extreme northings f64 loses the separation from a singular pole.
        ensure!(
            psi.abs() < 30.,
            "Mercator northing exceeds numerical domain"
        );
        let mut phi = psi.sinh().atan();
        if matches!(self, Self::World) {
            let e = E2.sqrt();
            let mut converged = false;
            for _ in 0..20 {
                let next = (psi + e * (e * phi.sin()).atanh()).sinh().atan();
                if (next - phi).abs() < 1e-14 {
                    phi = next;
                    converged = true;
                    break;
                }
                phi = next;
            }
            ensure!(converged, "Mercator inverse did not converge");
        }
        GeographicPosition::new(phi.to_degrees(), lon)
    }
}

/// Nearest forward hit of a ray with the zero-height WGS84 ellipsoid.
/// ECEF origin is metres; direction need not be normalized. The returned ray
/// distance is metres, not the arbitrary input direction parameter. Used by
/// globe picking/occlusion without coupling the kernel to a graphics backend.
#[derive(Debug, Clone, Copy)]
pub struct EllipsoidRayHit {
    pub ecef_m: [f64; 3],
    pub distance_m: f64,
}
pub fn intersect_wgs84_ray(
    origin_m: [f64; 3],
    direction: [f64; 3],
) -> Result<Option<EllipsoidRayHit>> {
    ensure!(
        origin_m
            .iter()
            .chain(direction.iter())
            .all(|x| x.is_finite()),
        "Invalid ECEF ray"
    );
    let norm = direction[0].hypot(direction[1]).hypot(direction[2]);
    ensure!(norm.is_finite() && norm > 0., "Invalid ECEF ray direction");
    let axes = [WGS84_A, WGS84_A, WGS84_B];
    let mut a = 0.;
    let mut b = 0.;
    let mut c = -1.;
    for i in 0..3 {
        let o = origin_m[i] / axes[i];
        let d = (direction[i] / norm) / axes[i];
        a += d * d;
        b += 2. * o * d;
        c += o * o;
    }
    let discriminant = b * b - 4. * a * c;
    ensure!(
        [a, b, c, discriminant].iter().all(|v| v.is_finite()),
        "ECEF ray overflow"
    );
    if discriminant < 0. {
        return Ok(None);
    }
    // Stable quadratic formula, avoids cancellation for near-surface hits.
    let q = -0.5 * (b + discriminant.sqrt().copysign(b));
    let (t0, t1) = if q == 0. {
        (-b / (2. * a), -b / (2. * a))
    } else {
        (q / a, c / q)
    };
    let distance_m = [t0, t1]
        .into_iter()
        .filter(|t| t.is_finite() && *t >= 0.)
        .min_by(f64::total_cmp);
    Ok(distance_m.map(|distance_m| EllipsoidRayHit {
        ecef_m: std::array::from_fn(|i| origin_m[i] + (direction[i] / norm) * distance_m),
        distance_m,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(lat: f64, lon: f64) -> GeographicPosition {
        GeographicPosition::new(lat, lon).unwrap()
    }
    fn near(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "{a} != {b}");
    }
    #[test]
    fn official_geographiclib_reference_examples() {
        // GeographicLib Python documentation, Wellington -> Salamanca.
        // https://geographiclib.sourceforge.io/html/python/examples.html
        let r = inverse(p(-41.32, 174.81), p(40.96, -5.50)).unwrap();
        near(r.distance_m, 19_959_679.267_353_82, 1e-6);
        near(r.initial_azimuth_deg, 161.067_669_986_160_15, 1e-10);
        near(r.final_azimuth_deg, 18.825_195_123_248_392, 1e-10);
        // GeoRust's published JFK direct example (same WGS84 algorithm).
        let q = direct(p(40.64, -73.78), 45., 10_000_000.).unwrap();
        near(q.latitude(), 32.621_100_463_725_796, 1e-10);
        near(q.longitude(), 49.052_487_092_959_836, 1e-10);
    }
    #[test]
    fn antipodes_and_dateline_use_ellipsoid_shortest_path() {
        let r = inverse(p(0., 0.), p(0., 180.)).unwrap();
        near(r.distance_m, 20_003_931.458_625_447, 1e-6);
        let r = inverse(p(0., 179.9), p(0., -179.9)).unwrap();
        near(r.distance_m, WGS84_A * 0.2_f64.to_radians(), 1e-6);
        let q = direct(p(0., 179.9), r.initial_azimuth_deg, r.distance_m).unwrap();
        near(q.longitude(), -179.9, 1e-10);
        near(q.longitude_near(179.9).unwrap(), 180.1, 1e-10);
        near(
            inverse(p(90., 0.), p(90., 120.)).unwrap().distance_m,
            0.,
            1e-9,
        );
    }
    #[test]
    fn invalid_input_and_resource_budget_fail_closed() {
        assert!(GeographicPosition::new(91., 0.).is_err());
        assert!(GeographicPosition::new(0., 181.).is_err());
        assert!(GeographicPosition::new(f64::NAN, 0.).is_err());
        assert!(direct(p(0., 0.), f64::INFINITY, 10.).is_err());
        assert!(sample_shortest_path(p(0., 0.), p(0., 180.), 1., 1000).is_err());
        assert!(sample_shortest_path(p(0., 0.), p(0., 1.), 0., 1000).is_err());
        assert!(Mercator::World.project(p(90., 0.)).is_err());
        assert!(Mercator::Web.unproject([0., f64::INFINITY]).is_err());
        assert!(Mercator::Web.unproject([0., WGS84_A * 31.]).is_err());
    }
    #[test]
    fn geodesic_sampling_crosses_dateline_with_bounded_steps() {
        let points = sample_shortest_path(p(70., 179.), p(70., -179.), 10_000., 100).unwrap();
        assert!(points.len() > 2);
        assert_eq!(points.first(), Some(&p(70., 179.)));
        assert_eq!(points.last(), Some(&p(70., -179.)));
        for s in points.windows(2) {
            assert!(inverse(s[0], s[1]).unwrap().distance_m <= 10_000. + 1e-6);
        }
        assert!(points[points.len() / 2].latitude() > 70.);
    }
    #[test]
    fn ecef_axes_and_ellipsoidal_height() {
        let q = p(0., 0.).to_ecef(100.).unwrap();
        near(q[0], WGS84_A + 100., 1e-9);
        near(q[1], 0., 1e-9);
        near(q[2], 0., 1e-9);
        let q = p(0., 90.).to_ecef(0.).unwrap();
        near(q[0], 0., 1e-9);
        near(q[1], WGS84_A, 1e-9);
        let q = p(-90., 130.).to_ecef(0.).unwrap();
        near(q[0], 0., 1e-9);
        near(q[1], 0., 1e-9);
        near(q[2], -WGS84_B, 1e-9);
        assert!(p(0., 0.).to_ecef(f64::NAN).is_err());
    }
    #[test]
    fn mercator_distinguishes_ellipsoid_from_web_sphere() {
        // EPSG:3857 analytic spherical reference at latitude 45 degrees.
        let w = Mercator::Web.project(p(45., 10.)).unwrap();
        near(w[0], 1_113_194.907_932_735_7, 1e-7);
        near(w[1], 5_621_521.486_192_066, 1e-7);
        let e = Mercator::World.project(p(45., 10.)).unwrap();
        near(e[1], 5_591_295.918_553_391_5, 1e-7);
        assert!((w[1] - e[1]).abs() > 30_000.);
        for m in [Mercator::Web, Mercator::World] {
            for lat in [-89., -80., -45., 0., 45., 80., 89.] {
                let start = p(lat, -175.);
                let q = m.unproject(m.project(start).unwrap()).unwrap();
                near(q.latitude(), lat, 1e-10);
                near(q.longitude(), -175., 1e-10);
            }
        }
    }
    #[test]
    fn globe_ray_picking_hits_near_side_and_rejects_misses() {
        let hit = intersect_wgs84_ray([WGS84_A + 1000., 0., 0.], [-7., 0., 0.])
            .unwrap()
            .unwrap();
        near(hit.distance_m, 1000., 1e-6);
        near(hit.ecef_m[0], WGS84_A, 1e-6);
        let hit = intersect_wgs84_ray([0., 0., WGS84_B + 1000.], [0., 0., -1.])
            .unwrap()
            .unwrap();
        near(hit.distance_m, 1000., 1e-6);
        near(hit.ecef_m[2], WGS84_B, 1e-6);
        assert!(intersect_wgs84_ray([WGS84_A + 1000., 0., 0.], [1., 0., 0.])
            .unwrap()
            .is_none());
        assert!(intersect_wgs84_ray([WGS84_A + 1000., 0., 0.], [0., 1., 0.])
            .unwrap()
            .is_none());
        assert!(intersect_wgs84_ray([0., 0., 0.], [0., 0., 0.]).is_err());
        assert!(intersect_wgs84_ray([f64::NAN, 0., 0.], [1., 0., 0.]).is_err());
        // Interior rays choose the forward exit, never the negative root.
        near(
            intersect_wgs84_ray([0., 0., 0.], [1., 0., 0.])
                .unwrap()
                .unwrap()
                .distance_m,
            WGS84_A,
            1e-6,
        );
    }
}

#[cfg(test)]
mod geographic_arc_tests {
    use super::*;
    #[test]
    fn drawing_copies_preserve_source_and_polar_winding_without_a_long_bridge() {
        for (lat,lon) in [(50.,179.9),(50.,-179.9),(89.9,179.9),(-89.9,-179.9)] {
            let center=GeographicPosition::new(lat,lon).unwrap();
            for sweep in [360.,-360.,720.,-720.] {
                let arc=GeodesicRadiusArc::new(center,50_000.,35.,sweep).unwrap();
                let source=arc.sample(5.,300).unwrap();
                let copies=arc.sample_drawing_copies(5.,300,lon).unwrap();
                assert_eq!(copies.len(),source.len());
                for (p,q) in source.iter().zip(&copies) {
                    assert_eq!(*p,q.position());
                    assert_eq!(p.latitude().to_bits(),q.latitude().to_bits());
                    assert!((q.longitude()-p.longitude()).rem_euclid(360.).min((p.longitude()-q.longitude()).rem_euclid(360.))<1e-10);
                    let original=p.to_ecef(0.).unwrap();
                    let radians=q.longitude().to_radians();
                    let planar=original[0].hypot(original[1]);
                    assert!((planar*radians.cos()-original[0]).abs()<1e-6);
                    assert!((planar*radians.sin()-original[1]).abs()<1e-6);
                }
                assert!(copies.windows(2).all(|p| (p[1].longitude()-p[0].longitude()).abs()<180.));
                assert_eq!(copies.first().unwrap().position(),copies.last().unwrap().position());
                let winding=copies.last().unwrap().longitude()-copies.first().unwrap().longitude();
                if lat.abs()<80. {
                    assert_eq!(copies.first(),copies.last());
                } else {
                    assert!((winding.abs()-sweep.abs()).abs()<1e-10);
                }
            }
        }
        let center=GeographicPosition::new(50.,179.9).unwrap();
        let zero=GeodesicRadiusArc::new(center,0.,0.,360.).unwrap();
        assert!(zero.sample_drawing_copies(5.,100,f64::NAN).is_err());
        assert!(zero.sample_drawing_copies(5.,100,1e10).is_err());
        assert!(zero.sample_drawing_copies(1.,360,179.9).is_err());
        let points=zero.sample_drawing_copies(90.,5,179.9).unwrap();
        assert!(points.iter().all(|p| p.position()==center && p.longitude().to_bits()==center.longitude().to_bits()));
    }
    #[test]
    fn geographic_radius_arcs_keep_metric_radius_and_signed_bearings() {
        for (lat,lon) in [(0.,0.), (50.,179.9), (89.5,-179.9),(-89.5,179.9)] {
            let center=GeographicPosition::new(lat,lon).unwrap();
            for sweep in [270.,-270.,720.,-360.] {
                let arc=GeodesicRadiusArc::new(center,50_000.,35.,sweep).unwrap();
                let points=arc.sample(5.,200).unwrap();
                for (i,p) in points.iter().enumerate() {
                    let measure=inverse(center,*p).unwrap();
                    assert!((measure.distance_m-50_000.).abs()<1e-6);
                    let expected=35.+sweep*i as f64/(points.len()-1) as f64;
                    let error=(measure.initial_azimuth_deg-expected+180.).rem_euclid(360.)-180.;
                    assert!(error.abs()<1e-8, "{lat} {lon} {sweep} {i} {error}");
                }
                if sweep.rem_euclid(360.)==0. { assert_eq!(points.first(),points.last()); }
            }
        }
    }
    #[test]
    fn geographic_arc_budget_and_degenerate_inputs_are_explicit() {
        let center=GeographicPosition::new(50.,0.).unwrap();
        for radius in [-1.,f64::NAN,f64::INFINITY] { assert!(GeodesicRadiusArc::new(center,radius,0.,360.).is_err()); }
        assert!(GeodesicRadiusArc::new(center,1.,f64::INFINITY,360.).is_err());
        let arc=GeodesicRadiusArc::new(center,10.,0.,360.).unwrap();
        for step in [0.,-1.,181.,f64::NAN] { assert!(arc.sample(step,100).is_err()); }
        assert!(arc.sample(1.,360).is_err());
        assert_eq!(arc.sample(1.,361).unwrap().len(),361);
        assert!(arc.position_at(-0.1).is_err());
        assert!(arc.position_at(f64::NAN).is_err());
        let point_arc=GeodesicRadiusArc::new(center,0.,13.,-360.).unwrap().sample(90.,5).unwrap();
        assert!(point_arc.iter().all(|p| *p==center));
        assert_eq!(GeodesicRadiusArc::new(center,10.,0.,0.).unwrap().sample(90.,2).unwrap().len(),2);
        assert!(GeodesicRadiusArc::new(center,10.,0.,f64::MAX).unwrap().sample(1.,1000).is_err());
    }
}
