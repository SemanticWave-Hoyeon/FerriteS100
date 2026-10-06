//! Forward-depth components of authored WGS84 rhumb curves. Clip before divide.
use crate::globe_lines::ProjectedLineSample;
use ferrite_kernel::{
    geodesy::{GeographicPosition, Mercator, WGS84_A, WGS84_B},
    globe_camera::GlobeCamera,
    rhumb::RhumbSegment,
};
use ferrite_render::WorldPoint;
#[derive(Debug, Clone)]
pub struct ProjectedLineComponent {
    pub samples: Vec<ProjectedLineSample>,
    /// Closed paths keep phase zero at their authored origin, even when the
    /// containing forward component starts at a clipping boundary before it.
    pub initial_phase_px: f64,
}
#[derive(Clone)]
struct Raw {
    ecef: [f64; 3],
    clip: [f64; 4],
}
// Camera-independent routes and their most frequently sampled source positions.
// Retained only within an immutable RenderContext geometry revision. Projection,
// clipping roots, subdivision and dash phase are still evaluated per camera.
const DYADIC_DENOMINATOR: usize = 16;
const DYADIC_SLOTS: usize = DYADIC_DENOMINATOR - 2;
type DyadicSamples = [std::sync::OnceLock<[f64; 3]>; DYADIC_SLOTS];

pub(crate) struct PreparedCurve {
    segments: Vec<PreparedSegment>,
    surface_sphere: Option<([f64; 3], f64)>,
    dyadic: Vec<DyadicSamples>,
    metrics: [std::sync::atomic::AtomicUsize; 3],
    scratch_metrics: [std::sync::atomic::AtomicUsize; 4],
}
struct PreparedSegment {
    route: RhumbSegment,
    length: f64,
    ecef: [[f64; 3]; 3],
}
impl PreparedCurve {
    pub(crate) fn estimated_bytes(points: usize) -> usize {
        std::mem::size_of::<Self>()
            + points.saturating_sub(1) * std::mem::size_of::<PreparedSegment>()
    }
    pub(crate) fn new(points: &[WorldPoint]) -> Result<Self, String> {
        if points.len() > 262144 {
            return Err("Prepared curve source budget exceeded".into());
        }
        let mut segments = Vec::with_capacity(points.len().saturating_sub(1));
        for pair in points.windows(2) {
            let a = position(pair[0])?;
            let b = position(pair[1])?;
            let route = RhumbSegment::new(a, b).map_err(|e| e.to_string())?;
            let length = parameter_length(a, b, route)?;
            let mut ecef = [[0.; 3]; 3];
            for (i, t) in [0., 0.5, 1.].into_iter().enumerate() {
                ecef[i] = route
                    .point(t)
                    .and_then(|p| p.to_ecef(0.))
                    .map_err(|e| e.to_string())?;
            }
            segments.push(PreparedSegment {
                route,
                length,
                ecef,
            });
        }
        Ok(Self {
            segments,
            surface_sphere: geographic_curve_sphere(points)?,
            dyadic: Vec::new(),
            metrics: std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)),
            scratch_metrics: std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)),
        })
    }
    pub(crate) fn surface_sphere(&self) -> Option<([f64; 3], f64)> {
        self.surface_sphere
    }
    /// Optional samples use only budget left after immutable route preparation.
    /// Already reserved storage is never expanded during projection or worker work.
    pub(crate) fn reserve_dyadic(&mut self, available_bytes: usize) -> usize {
        if !self.dyadic.is_empty() {
            return 0;
        }
        let n = self
            .segments
            .len()
            .min(available_bytes / std::mem::size_of::<DyadicSamples>());
        if n == 0 {
            return 0;
        }
        let mut cells = Vec::with_capacity(n);
        cells.resize_with(n, || std::array::from_fn(|_| std::sync::OnceLock::new()));
        assert!(cells.capacity() * std::mem::size_of::<DyadicSamples>() <= available_bytes);
        self.dyadic = cells;
        self.dyadic_bytes()
    }
    pub(crate) fn scratch_stats(&self) -> [usize; 4] {
        std::array::from_fn(|i| self.scratch_metrics[i].load(std::sync::atomic::Ordering::Relaxed))
    }
    pub(crate) fn dyadic_stats(&self) -> [usize; 3] {
        std::array::from_fn(|i| self.metrics[i].load(std::sync::atomic::Ordering::Relaxed))
    }
    pub(crate) fn dyadic_bytes(&self) -> usize {
        self.dyadic.capacity() * std::mem::size_of::<DyadicSamples>()
    }
    /// Drop storage, rather than clear retaining capacity, when the source cache
    /// needs room for a new immutable route or its geometry revision changes.
    pub(crate) fn evict_dyadic(&mut self) -> usize {
        let bytes = self.dyadic_bytes();
        self.dyadic = Vec::new();
        bytes
    }
    #[cfg(test)]
    fn populated_dyadic_samples(&self) -> usize {
        self.dyadic
            .iter()
            .flat_map(|x| x.iter())
            .filter(|x| x.get().is_some())
            .count()
    }
    // Caller owns these source points for this entire immutable preparation.
    // Reuse only the route constants; retain source interpolation, longitude lift
    // and bearing expressions/order from render::sample_curve_position.
    pub(crate) fn sample_source_world(
        &self,
        points: &[WorldPoint],
        sample: ferrite_render::CurveSample,
    ) -> Result<WorldPoint, String> {
        let a = *points
            .get(sample.segment)
            .ok_or("Invalid placement source segment")?;
        points
            .get(sample.segment + 1)
            .ok_or("Invalid placement source endpoint")?;
        let route = self
            .segments
            .get(sample.segment)
            .ok_or("Prepared curve source length mismatch")?
            .route;
        let p = route.point(sample.fraction).map_err(|e| e.to_string())?;
        Ok(WorldPoint::new(
            p.longitude_near(a.x).map_err(|e| e.to_string())?,
            p.latitude(),
        ))
    }
    pub(crate) fn sample_source_position(
        &self,
        points: &[WorldPoint],
        sample: ferrite_render::CurveSample,
    ) -> Result<(WorldPoint, f64), String> {
        let world = self.sample_source_world(points, sample)?;
        Ok((world, self.segments[sample.segment].route.bearing_deg()))
    }
    pub(crate) fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.segments.capacity() * std::mem::size_of::<PreparedSegment>()
            + self.dyadic_bytes()
    }
}
pub(crate) fn reclaim_dyadic_for_route(
    cache: &mut std::collections::HashMap<usize, PreparedCurve>,
    retained: &mut usize,
    potential: &[bool],
    required: usize,
    budget: usize,
) -> (usize, usize) {
    let mut bytes = 0;
    let mut curves = 0;
    while required > budget.saturating_sub(*retained) {
        let victim = cache
            .iter()
            .filter(|(_, c)| c.dyadic_bytes() > 0)
            .min_by_key(|(id, _)| (potential.get(**id).copied().unwrap_or(false), **id))
            .map(|(id, _)| *id);
        let Some(victim) = victim else {
            break;
        };
        let released = cache.get_mut(&victim).unwrap().evict_dyadic();
        *retained -= released;
        bytes += released;
        curves += 1;
    }
    (bytes, curves)
}
// Preserve the exact authored longitude lift and polar fallback used by strokes.
// A prepared source is immutable for its RenderContext geometry revision.
pub(crate) fn geographic_curve_sphere(
    points: &[WorldPoint],
) -> Result<Option<([f64; 3], f64)>, String> {
    let mut lat = [f64::INFINITY, f64::NEG_INFINITY];
    let mut lon = [f64::INFINITY, f64::NEG_INFINITY];
    let mut previous = None;
    let mut has_pole = false;
    for point in points {
        let p = position(*point)?;
        has_pole |= p.latitude().abs() == 90.;
        let lifted = if let Some(x) = previous {
            p.longitude_near(x).map_err(|e| e.to_string())?
        } else {
            p.longitude()
        };
        previous = Some(lifted);
        lat = [lat[0].min(p.latitude()), lat[1].max(p.latitude())];
        lon = [lon[0].min(lifted), lon[1].max(lifted)];
    }
    Ok(if has_pole {
        None
    } else {
        ferrite_kernel::surface_bounds::GeographicSurfaceBounds::new(lat, lon)
            .ok()
            .map(|b| b.sphere())
    })
}
#[derive(Clone, Copy)]
struct Route<'a> {
    route: RhumbSegment,
    prepared: Option<&'a PreparedSegment>,
    dyadic: Option<&'a DyadicSamples>,
    metrics: Option<&'a [std::sync::atomic::AtomicUsize; 3]>,
}
impl From<RhumbSegment> for Route<'_> {
    fn from(route: RhumbSegment) -> Self {
        Self {
            route,
            prepared: None,
            dyadic: None,
            metrics: None,
        }
    }
}
/// Reuse only exact dyadic f64 values. No rounding or nearest-sample geometry.
fn dyadic_slot(t: f64) -> Option<usize> {
    let scaled = t * DYADIC_DENOMINATOR as f64;
    if !(1. ..DYADIC_DENOMINATOR as f64).contains(&scaled) || scaled.fract() != 0. {
        return None;
    }
    let n = scaled as usize;
    if n == DYADIC_DENOMINATOR / 2 || t != n as f64 / DYADIC_DENOMINATOR as f64 {
        return None;
    }
    Some(if n < DYADIC_DENOMINATOR / 2 {
        n - 1
    } else {
        n - 2
    })
}
fn at<'a>(route: impl Into<Route<'a>>, t: f64, camera: &GlobeCamera) -> Result<Raw, String> {
    let route = route.into();
    let fixed = if t == 0. {
        Some(0)
    } else if t == 0.5 {
        Some(1)
    } else if t == 1. {
        Some(2)
    } else {
        None
    };
    let derive = || {
        route
            .route
            .point(t)
            .and_then(|p| p.to_ecef(0.))
            .map_err(|e| e.to_string())
    };
    let ecef = if let (Some(p), Some(i)) = (route.prepared, fixed) {
        p.ecef[i]
    } else if let Some(cell) = route
        .dyadic
        .and_then(|cells| dyadic_slot(t).map(|i| &cells[i]))
    {
        if let Some(value) = cell.get() {
            if let Some(m) = route.metrics {
                m[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            *value
        } else {
            // A failed derivation is never stored. A concurrent first sample
            // can compute twice but both use the exact authored f64 parameter.
            if let Some(m) = route.metrics {
                m[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let value = derive()?;
            let _ = cell.set(value);
            value
        }
    } else {
        if let Some(m) = route.metrics {
            m[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        derive()?
    };
    Ok(Raw {
        ecef,
        clip: camera.clip_ecef(ecef).map_err(|e| e.to_string())?,
    })
}
fn projected(
    p: &Raw,
    segment: usize,
    fraction: f64,
    camera: &GlobeCamera,
) -> Result<ProjectedLineSample, String> {
    if p.clip[3] <= 0. {
        return Err("Clipped curve retained nonpositive depth".into());
    }
    let v = camera.viewport();
    let screen = [
        (p.clip[0] / p.clip[3] + 1.) * v[0] / 2.,
        (1. - p.clip[1] / p.clip[3]) * v[1] / 2.,
    ];
    if screen.iter().any(|v| !v.is_finite()) {
        return Err("Clipped curve projection overflow".into());
    }
    Ok(ProjectedLineSample {
        ecef_m: p.ecef,
        screen_px: screen,
        forward_depth_m: p.clip[3],
        source_segment: segment,
        source_fraction: fraction,
    })
}
fn position(p: WorldPoint) -> Result<GeographicPosition, String> {
    if !p.x.is_finite() || p.x.abs() > 1e9 {
        return Err("Invalid globe curve longitude".into());
    }
    GeographicPosition::new(p.y, (p.x + 180.).rem_euclid(360.) - 180.).map_err(|e| e.to_string())
}
fn parameter_length(
    a: GeographicPosition,
    b: GeographicPosition,
    route: RhumbSegment,
) -> Result<f64, String> {
    if a.latitude().abs() == 90. || b.latitude().abs() == 90. {
        return route.distance_m().map_err(|e| e.to_string());
    }
    let ay = Mercator::World.project(a).map_err(|e| e.to_string())?[1];
    let by = Mercator::World.project(b).map_err(|e| e.to_string())?[1];
    let dx =
        WGS84_A * ((b.longitude() - a.longitude() + 180.).rem_euclid(360.) - 180.).to_radians();
    Ok(dx.hypot(by - ay))
}
fn plane(p: &Raw, which: usize, depth: [f64; 2]) -> f64 {
    if which == 0 {
        p.clip[3] - depth[0]
    } else {
        depth[1] - p.clip[3]
    }
}
fn root(
    route: Route<'_>,
    camera: &GlobeCamera,
    mut a: f64,
    mut b: f64,
    which: usize,
    depth: [f64; 2],
) -> Result<f64, String> {
    let fa = plane(&at(route, a, camera)?, which, depth);
    let fb = plane(&at(route, b, camera)?, which, depth);
    if fa == 0. {
        return Ok(a);
    }
    if fb == 0. {
        return Ok(b);
    }
    if (fa >= 0.) == (fb >= 0.) {
        return Err("Curve clipping root was not bracketed".into());
    }
    let left_inside = fa >= 0.;
    for _ in 0..80 {
        let mid = (a + b) / 2.;
        if mid == a || mid == b {
            break;
        }
        let inside = plane(&at(route, mid, camera)?, which, depth) >= 0.;
        if inside == left_inside {
            a = mid;
        } else {
            b = mid;
        }
    }
    // Retain the inside endpoint at f64 parameter resolution; no synthetic ECEF
    // interpolation is substituted for the authored geographic curve point.
    Ok(if left_inside { a } else { b })
}
struct Piece<'a> {
    route: Route<'a>,
    segment: usize,
    start: f64,
    end: f64,
}
fn depth_pieces<'a>(
    route: Route<'a>,
    segment: usize,
    length: f64,
    camera: &GlobeCamera,
    budget: usize,
) -> Result<Vec<Piece<'a>>, String> {
    let depth = camera.depth_range_m();
    if length == 0. {
        let p = at(route, 0., camera)?;
        return Ok(if p.clip[3] >= depth[0] && p.clip[3] <= depth[1] {
            vec![Piece {
                route,
                segment,
                start: 0.,
                end: 1.,
            }]
        } else {
            Vec::new()
        });
    }
    let mut pending = vec![(0., 1., at(route, 0., camera)?, at(route, 1., camera)?, 0u8)];
    let mut pieces = Vec::new();
    while let Some((t0, t1, a, b, level)) = pending.pop() {
        let span = length * (t1 - t0);
        // In ellipsoidal Mercator metres |dECEF/ds| <= 1. Differentiating its
        // conformal scale and geodetic frame bounds |d²ECEF/ds²| by 4/b.
        // Hence chord/plane displacement <= span²/(2*b). The meridional polar
        // geodesic branch has the smaller ellipsoid normal-curvature bound.
        let bound = span * span / (2. * WGS84_B);
        let tolerance =
            64. * f64::EPSILON * (WGS84_A + camera.eye_m().iter().map(|v| v.abs()).sum::<f64>());
        let mut start = t0;
        let mut end = t1;
        let mut uncertain = false;
        let mut outside = false;
        for which in 0..2 {
            let fa = plane(&a, which, depth);
            let fb = plane(&b, which, depth);
            // A secant slope larger than the derivative remainder proves monotonicity.
            let monotone = (fb - fa).abs() > 4. * bound + tolerance;
            if fa.min(fb) >= bound + tolerance || (monotone && fa >= 0. && fb >= 0.) {
                continue;
            }
            if fa.max(fb) < -bound - tolerance || (monotone && fa < 0. && fb < 0.) {
                outside = true;
                break;
            }
            if monotone && (fa >= 0.) != (fb >= 0.) {
                let cut = root(route, camera, t0, t1, which, depth)?;
                if fa >= 0. {
                    end = end.min(cut);
                } else {
                    start = start.max(cut);
                }
            } else {
                uncertain = true;
            }
        }
        if outside || start >= end {
            continue;
        }
        if uncertain {
            let mid = (t0 + t1) / 2.;
            if level >= 48 || mid == t0 || mid == t1 {
                return Err("Curve depth partition exceeded numeric resolution".into());
            }
            let m = at(route, mid, camera)?;
            pending.push((mid, t1, m.clone(), b, level + 1));
            pending.push((t0, mid, a, m, level + 1));
        } else {
            pieces.push(Piece {
                route,
                segment,
                start,
                end,
            });
        }
        if pending.len() + pieces.len() > budget {
            return Err("Curve depth partition budget exceeded".into());
        }
    }
    Ok(pieces)
}
fn sample_piece(
    piece: &Piece<'_>,
    camera: &GlobeCamera,
    screen_error: f64,
    chord_error: f64,
    phase_accuracy: bool,
    total_segments: usize,
    budget: usize,
) -> Result<(Vec<ProjectedLineSample>, usize), String> {
    let a = projected(
        &at(piece.route, piece.start, camera)?,
        piece.segment,
        piece.start,
        camera,
    )?;
    let b = projected(
        &at(piece.route, piece.end, camera)?,
        piece.segment,
        piece.end,
        camera,
    )?;
    let mut out = vec![a.clone()];
    let mut refinements = 0;
    let mut pending = vec![(piece.start, piece.end, a, b, 0u8)];
    while let Some((t0, t1, a, b, level)) = pending.pop() {
        let mid = (t0 + t1) / 2.;
        let raw = at(piece.route, mid, camera)?;
        let m = projected(&raw, piece.segment, mid, camera)?;
        let linear: [f64; 3] = std::array::from_fn(|i| (a.ecef_m[i] + b.ecef_m[i]) / 2.);
        let chord = (m.ecef_m[0] - linear[0])
            .hypot(m.ecef_m[1] - linear[1])
            .hypot(m.ecef_m[2] - linear[2]);
        let clip = camera.clip_ecef(linear).map_err(|e| e.to_string())?;
        let v = camera.viewport();
        let screen = [
            (clip[0] / clip[3] + 1.) * v[0] / 2.,
            (1. - clip[1] / clip[3]) * v[1] / 2.,
        ];
        let offscreen = if camera
            .sphere_outside_frustum(
                linear,
                (0..3)
                    .map(|i| (b.ecef_m[i] - a.ecef_m[i]).powi(2))
                    .sum::<f64>()
                    .sqrt()
                    / 2.
                    + chord,
                0.,
            )
            .map_err(|e| e.to_string())?
        {
            true
        } else {
            false
        };
        let pixel = if offscreen {
            0.
        } else {
            (m.screen_px[0] - screen[0]).hypot(m.screen_px[1] - screen[1])
        };
        // Surface strokes must remain within the overlay depth tolerance, too.
        // A screen-straight chord can otherwise cut beneath the ellipsoid.
        let depth_tolerance = if offscreen {
            chord_error
        } else {
            (a.forward_depth_m.min(b.forward_depth_m).powi(2) / camera.depth_range_m()[0]
                * f32::EPSILON as f64
                * 0.125)
                .min(chord_error)
                .max(1e-7)
        };
        let distance = |x: [f64; 2], y: [f64; 2]| (x[0] - y[0]).hypot(x[1] - y[1]);
        let split_length = distance(a.screen_px, m.screen_px) + distance(m.screen_px, b.screen_px);
        let straight = distance(a.screen_px, b.screen_px);
        let phase_error = (split_length - straight).max(0.);
        let phase_tolerance = (screen_error * (t1 - t0) / (total_segments as f64))
            .max(64. * f64::EPSILON * split_length);
        if chord > depth_tolerance
            || pixel > screen_error
            || (phase_accuracy && phase_error > phase_tolerance)
        {
            if level >= 48 || mid == t0 || mid == t1 || out.len() + pending.len() + 2 >= budget {
                return Err("Clipped curve subdivision budget/resolution exceeded".into());
            }
            refinements += 1;
            pending.push((mid, t1, m.clone(), b, level + 1));
            pending.push((t0, mid, a, m, level + 1));
        } else {
            out.push(b);
        }
    }
    Ok((out, refinements))
}

type DepthWork = (f64, f64, Raw, Raw, u8);
type SampleWork = (f64, f64, ProjectedLineSample, ProjectedLineSample, u8);
const CURVE_SCRATCH_RETAIN_LIMIT: usize = 256 * 1024;
#[derive(Default)]
struct CurveScratch<'a> {
    depth: Vec<DepthWork>,
    pieces: Vec<Piece<'a>>,
    pending: Vec<SampleWork>,
    samples: Vec<ProjectedLineSample>,
    peak_retained: usize,
    discards: usize,
}
impl CurveScratch<'_> {
    fn bytes(&self) -> usize {
        self.depth.capacity() * std::mem::size_of::<DepthWork>()
            + self.pieces.capacity() * std::mem::size_of::<Piece<'_>>()
            + self.pending.capacity() * std::mem::size_of::<SampleWork>()
            + self.samples.capacity() * std::mem::size_of::<ProjectedLineSample>()
    }
    fn bound_retention(&mut self) {
        // Existing sampling budgets still govern live results. Oversized work
        // capacity is freed before it can be retained into another segment.
        if self.bytes() > CURVE_SCRATCH_RETAIN_LIMIT {
            self.samples = Vec::new();
            self.discards += 1;
        }
        if self.bytes() > CURVE_SCRATCH_RETAIN_LIMIT {
            self.pieces = Vec::new();
            self.discards += 1;
        }
        if self.bytes() > CURVE_SCRATCH_RETAIN_LIMIT {
            self.pending = Vec::new();
            self.depth = Vec::new();
            self.discards += 1;
        }
        assert!(self.bytes() <= CURVE_SCRATCH_RETAIN_LIMIT);
        self.peak_retained = self.peak_retained.max(self.bytes());
    }
}
fn depth_pieces_reusing<'a>(
    route: Route<'a>,
    segment: usize,
    length: f64,
    camera: &GlobeCamera,
    budget: usize,
    pending: &mut Vec<DepthWork>,
    pieces: &mut Vec<Piece<'a>>,
) -> Result<(), String> {
    pending.clear();
    pieces.clear();
    let depth = camera.depth_range_m();
    if length == 0. {
        let p = at(route, 0., camera)?;
        if p.clip[3] >= depth[0] && p.clip[3] <= depth[1] {
            pieces.push(Piece {
                route,
                segment,
                start: 0.,
                end: 1.,
            });
        }
        return Ok(());
    }
    pending.push((0., 1., at(route, 0., camera)?, at(route, 1., camera)?, 0u8));
    while let Some((t0, t1, a, b, level)) = pending.pop() {
        let span = length * (t1 - t0);
        // In ellipsoidal Mercator metres |dECEF/ds| <= 1. Differentiating its
        // conformal scale and geodetic frame bounds |d²ECEF/ds²| by 4/b.
        // Hence chord/plane displacement <= span²/(2*b). The meridional polar
        // geodesic branch has the smaller ellipsoid normal-curvature bound.
        let bound = span * span / (2. * WGS84_B);
        let tolerance =
            64. * f64::EPSILON * (WGS84_A + camera.eye_m().iter().map(|v| v.abs()).sum::<f64>());
        let mut start = t0;
        let mut end = t1;
        let mut uncertain = false;
        let mut outside = false;
        for which in 0..2 {
            let fa = plane(&a, which, depth);
            let fb = plane(&b, which, depth);
            // A secant slope larger than the derivative remainder proves monotonicity.
            let monotone = (fb - fa).abs() > 4. * bound + tolerance;
            if fa.min(fb) >= bound + tolerance || (monotone && fa >= 0. && fb >= 0.) {
                continue;
            }
            if fa.max(fb) < -bound - tolerance || (monotone && fa < 0. && fb < 0.) {
                outside = true;
                break;
            }
            if monotone && (fa >= 0.) != (fb >= 0.) {
                let cut = root(route, camera, t0, t1, which, depth)?;
                if fa >= 0. {
                    end = end.min(cut);
                } else {
                    start = start.max(cut);
                }
            } else {
                uncertain = true;
            }
        }
        if outside || start >= end {
            continue;
        }
        if uncertain {
            let mid = (t0 + t1) / 2.;
            if level >= 48 || mid == t0 || mid == t1 {
                return Err("Curve depth partition exceeded numeric resolution".into());
            }
            let m = at(route, mid, camera)?;
            pending.push((mid, t1, m.clone(), b, level + 1));
            pending.push((t0, mid, a, m, level + 1));
        } else {
            pieces.push(Piece {
                route,
                segment,
                start,
                end,
            });
        }
        if pending.len() + pieces.len() > budget {
            return Err("Curve depth partition budget exceeded".into());
        }
    }
    Ok(())
}
fn sample_piece_reusing(
    piece: &Piece<'_>,
    camera: &GlobeCamera,
    screen_error: f64,
    chord_error: f64,
    phase_accuracy: bool,
    total_segments: usize,
    budget: usize,
    pending: &mut Vec<SampleWork>,
    out: &mut Vec<ProjectedLineSample>,
) -> Result<usize, String> {
    pending.clear();
    out.clear();
    let a = projected(
        &at(piece.route, piece.start, camera)?,
        piece.segment,
        piece.start,
        camera,
    )?;
    let b = projected(
        &at(piece.route, piece.end, camera)?,
        piece.segment,
        piece.end,
        camera,
    )?;
    out.push(a.clone());
    let mut refinements = 0;
    pending.push((piece.start, piece.end, a, b, 0u8));
    while let Some((t0, t1, a, b, level)) = pending.pop() {
        let mid = (t0 + t1) / 2.;
        let raw = at(piece.route, mid, camera)?;
        let m = projected(&raw, piece.segment, mid, camera)?;
        let linear: [f64; 3] = std::array::from_fn(|i| (a.ecef_m[i] + b.ecef_m[i]) / 2.);
        let chord = (m.ecef_m[0] - linear[0])
            .hypot(m.ecef_m[1] - linear[1])
            .hypot(m.ecef_m[2] - linear[2]);
        let clip = camera.clip_ecef(linear).map_err(|e| e.to_string())?;
        let v = camera.viewport();
        let screen = [
            (clip[0] / clip[3] + 1.) * v[0] / 2.,
            (1. - clip[1] / clip[3]) * v[1] / 2.,
        ];
        let offscreen = if camera
            .sphere_outside_frustum(
                linear,
                (0..3)
                    .map(|i| (b.ecef_m[i] - a.ecef_m[i]).powi(2))
                    .sum::<f64>()
                    .sqrt()
                    / 2.
                    + chord,
                0.,
            )
            .map_err(|e| e.to_string())?
        {
            true
        } else {
            false
        };
        let pixel = if offscreen {
            0.
        } else {
            (m.screen_px[0] - screen[0]).hypot(m.screen_px[1] - screen[1])
        };
        // Surface strokes must remain within the overlay depth tolerance, too.
        // A screen-straight chord can otherwise cut beneath the ellipsoid.
        let depth_tolerance = if offscreen {
            chord_error
        } else {
            (a.forward_depth_m.min(b.forward_depth_m).powi(2) / camera.depth_range_m()[0]
                * f32::EPSILON as f64
                * 0.125)
                .min(chord_error)
                .max(1e-7)
        };
        let distance = |x: [f64; 2], y: [f64; 2]| (x[0] - y[0]).hypot(x[1] - y[1]);
        let split_length = distance(a.screen_px, m.screen_px) + distance(m.screen_px, b.screen_px);
        let straight = distance(a.screen_px, b.screen_px);
        let phase_error = (split_length - straight).max(0.);
        let phase_tolerance = (screen_error * (t1 - t0) / (total_segments as f64))
            .max(64. * f64::EPSILON * split_length);
        if chord > depth_tolerance
            || pixel > screen_error
            || (phase_accuracy && phase_error > phase_tolerance)
        {
            if level >= 48 || mid == t0 || mid == t1 || out.len() + pending.len() + 2 >= budget {
                return Err("Clipped curve subdivision budget/resolution exceeded".into());
            }
            refinements += 1;
            pending.push((mid, t1, m.clone(), b, level + 1));
            pending.push((t0, mid, a, m, level + 1));
        } else {
            out.push(b);
        }
    }
    Ok(refinements)
}
/// Clip only near/far depth here. Keep finite off-screen arc lengths for dash
/// phase; viewport clipping happens after phase calculation, before tessellation.
/// A removed depth interval creates a disconnected projected component. Its dash
/// origin is the first retained source point, except the authored closed origin.
pub fn project_rhumb_components(
    points: &[WorldPoint],
    camera: &GlobeCamera,
    screen_error: f64,
    chord_error: f64,
    phase_accuracy: bool,
    budget: usize,
) -> Result<(Vec<ProjectedLineComponent>, usize), String> {
    project_rhumb_components_prepared(
        points,
        camera,
        screen_error,
        chord_error,
        phase_accuracy,
        budget,
        None,
    )
}
pub(crate) fn project_rhumb_components_prepared(
    points: &[WorldPoint],
    camera: &GlobeCamera,
    screen_error: f64,
    chord_error: f64,
    phase_accuracy: bool,
    budget: usize,
    prepared: Option<&PreparedCurve>,
) -> Result<(Vec<ProjectedLineComponent>, usize), String> {
    project_rhumb_components_prepared_reusing(
        points,
        camera,
        screen_error,
        chord_error,
        phase_accuracy,
        budget,
        prepared,
        false,
    )
}
pub(crate) fn project_rhumb_components_prepared_reusing(
    points: &[WorldPoint],
    camera: &GlobeCamera,
    screen_error: f64,
    chord_error: f64,
    phase_accuracy: bool,
    budget: usize,
    prepared: Option<&PreparedCurve>,
    reuse_scratch: bool,
) -> Result<(Vec<ProjectedLineComponent>, usize), String> {
    if prepared.is_some_and(|p| p.segments.len() != points.len().saturating_sub(1)) {
        return Err("Prepared curve source length mismatch".into());
    }
    if points.len() > budget
        || !(2..=262144).contains(&budget)
        || !screen_error.is_finite()
        || screen_error <= 0.
        || !chord_error.is_finite()
        || chord_error <= 0.
    {
        return Err("Invalid clipped curve sampling limits".into());
    }
    let mut components: Vec<ProjectedLineComponent> = Vec::new();
    let mut last: Option<(usize, f64)> = None;
    let mut count = 0;
    let mut refinements = 0;
    let profiling = crate::profiler::is_profiling_enabled();
    let mut scratch = CurveScratch::default();
    let mut scratch_reused_segments = 0usize;
    for (segment, pair) in points.windows(2).enumerate() {
        let (route, length) = if let Some(curve) = prepared {
            let p = &curve.segments[segment];
            (
                Route {
                    route: p.route,
                    prepared: Some(p),
                    dyadic: curve.dyadic.get(segment),
                    metrics: profiling.then_some(&curve.metrics),
                },
                p.length,
            )
        } else {
            let a = position(pair[0])?;
            let b = position(pair[1])?;
            let route = RhumbSegment::new(a, b).map_err(|e| e.to_string())?;
            (Route::from(route), parameter_length(a, b, route)?)
        };
        let mut pieces = if reuse_scratch {
            scratch_reused_segments += usize::from(scratch.depth.capacity() > 0);
            depth_pieces_reusing(
                route,
                segment,
                length,
                camera,
                budget,
                &mut scratch.depth,
                &mut scratch.pieces,
            )?;
            std::mem::take(&mut scratch.pieces)
        } else {
            depth_pieces(route, segment, length, camera, budget)?
        };
        for piece in pieces.drain(..) {
            let connected = last.is_some_and(|(s, t)| {
                (s == segment && t == piece.start)
                    || (s + 1 == segment && t == 1. && piece.start == 0.)
            });
            let (mut samples, n) = if reuse_scratch {
                let n = sample_piece_reusing(
                    &piece,
                    camera,
                    screen_error,
                    chord_error,
                    phase_accuracy,
                    points.len().saturating_sub(1).max(1),
                    budget.saturating_sub(count),
                    &mut scratch.pending,
                    &mut scratch.samples,
                )?;
                (std::mem::take(&mut scratch.samples), n)
            } else {
                sample_piece(
                    &piece,
                    camera,
                    screen_error,
                    chord_error,
                    phase_accuracy,
                    points.len().saturating_sub(1).max(1),
                    budget.saturating_sub(count),
                )?
            };
            refinements += n;
            count += samples.len();
            if count > budget {
                return Err("Clipped curve aggregate sample budget exceeded".into());
            }
            if connected {
                components
                    .last_mut()
                    .unwrap()
                    .samples
                    .extend(samples.drain(1..));
                if reuse_scratch {
                    samples.clear();
                    scratch.samples = samples;
                }
            } else {
                components.push(ProjectedLineComponent {
                    samples,
                    initial_phase_px: 0.,
                });
            }
            last = Some((segment, piece.end));
        }
        if reuse_scratch {
            scratch.pieces = pieces;
            scratch.bound_retention();
        }
    }
    if reuse_scratch && profiling {
        if let Some(curve) = prepared {
            use std::sync::atomic::Ordering::Relaxed;
            curve.scratch_metrics[0].fetch_add(1, Relaxed);
            curve.scratch_metrics[1].fetch_add(scratch_reused_segments, Relaxed);
            curve.scratch_metrics[2].fetch_max(scratch.peak_retained, Relaxed);
            curve.scratch_metrics[3].fetch_add(scratch.discards, Relaxed);
        }
    }
    // Preserve the authored join and phase anchor across the storage seam of a
    // closed line. Near/far gaps elsewhere remain distinct components.
    if components.len() > 1 && points.first() == points.last() {
        let first = &components[0].samples[0];
        let end = components.last().unwrap().samples.last().unwrap();
        if first.source_segment == 0
            && first.source_fraction == 0.
            && end.source_segment + 2 == points.len()
            && end.source_fraction == 1.
        {
            let mut last_component = components.pop().unwrap();
            let prefix = last_component
                .samples
                .windows(2)
                .map(|p| {
                    (p[1].screen_px[0] - p[0].screen_px[0])
                        .hypot(p[1].screen_px[1] - p[0].screen_px[1])
                })
                .sum::<f64>();
            last_component
                .samples
                .extend(components[0].samples.drain(1..));
            last_component.initial_phase_px = -prefix;
            components[0] = last_component;
        }
    }
    Ok((components, refinements))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_sampling_preserves_all_f64_bits_and_failures() {
        let fixtures = [
            vec![
                WorldPoint::new(0., 0.),
                WorldPoint::new(0.01, 0.001),
                WorldPoint::new(0.02, 0.),
                WorldPoint::new(0., 0.),
            ],
            vec![
                WorldPoint::new(179.9, 70.),
                WorldPoint::new(-179.9, 70.01),
                WorldPoint::new(-179.8, 70.),
            ],
            vec![
                WorldPoint::new(0., 89.9),
                WorldPoint::new(0., 90.),
                WorldPoint::new(0., 89.8),
            ],
            (0..257)
                .map(|i| WorldPoint::new(i as f64 * 0.00001, (i % 2) as f64 * 0.000001))
                .collect(),
        ];
        let mut successful = 0;
        for points in fixtures {
            let prepared = PreparedCurve::new(&points).unwrap();
            for range in [100., 10000., 1000000.] {
                for tilt in [0., 70.] {
                    for phase in [false, true] {
                        let camera = GlobeCamera::orbit(
                            position(points[0]).unwrap(),
                            range,
                            30.,
                            tilt,
                            [1200., 800.],
                            45.,
                            1.,
                            5e7,
                        )
                        .unwrap();
                        for source in [None, Some(&prepared)] {
                            let a = project_rhumb_components_prepared_reusing(
                                &points, &camera, 0.025, 5., phase, 262144, source, false,
                            );
                            let b = project_rhumb_components_prepared_reusing(
                                &points, &camera, 0.025, 5., phase, 262144, source, true,
                            );
                            match (a, b) {
                                (Ok((a, ar)), Ok((b, br))) => {
                                    successful += 1;
                                    assert_eq!(ar, br);
                                    assert_eq!(a.len(), b.len());
                                    for (a, b) in a.iter().zip(&b) {
                                        assert_eq!(
                                            a.initial_phase_px.to_bits(),
                                            b.initial_phase_px.to_bits()
                                        );
                                        assert_eq!(a.samples.len(), b.samples.len());
                                        for (a, b) in a.samples.iter().zip(&b.samples) {
                                            assert_eq!(
                                                a.ecef_m.map(f64::to_bits),
                                                b.ecef_m.map(f64::to_bits)
                                            );
                                            assert_eq!(
                                                a.screen_px.map(f64::to_bits),
                                                b.screen_px.map(f64::to_bits)
                                            );
                                            assert_eq!(
                                                a.forward_depth_m.to_bits(),
                                                b.forward_depth_m.to_bits()
                                            );
                                            assert_eq!(
                                                a.source_fraction.to_bits(),
                                                b.source_fraction.to_bits()
                                            );
                                            assert_eq!(a.source_segment, b.source_segment);
                                        }
                                    }
                                }
                                (Err(a), Err(b)) => assert_eq!(a, b),
                                _ => panic!("scratch changed success/failure"),
                            }
                        }
                    }
                }
            }
        }
        assert!(successful >= 48);
        let camera = GlobeCamera::orbit(
            position(WorldPoint::new(0., 0.)).unwrap(),
            10000.,
            0.,
            0.,
            [1200., 800.],
            45.,
            1.,
            5e7,
        )
        .unwrap();
        for points in [
            vec![WorldPoint::new(f64::NAN, 0.), WorldPoint::new(0., 0.)],
            vec![WorldPoint::new(0., 91.), WorldPoint::new(0., 0.)],
        ] {
            let a = project_rhumb_components_prepared_reusing(
                &points, &camera, 0.025, 5., false, 262144, None, false,
            )
            .unwrap_err();
            let b = project_rhumb_components_prepared_reusing(
                &points, &camera, 0.025, 5., false, 262144, None, true,
            )
            .unwrap_err();
            assert_eq!(a, b);
        }
    }
    #[test]
    fn scratch_frees_oversized_capacity_between_segments() {
        let mut scratch = CurveScratch::default();
        scratch.depth.reserve_exact(64);
        scratch.pending.reserve_exact(64);
        scratch.samples.reserve_exact(
            CURVE_SCRATCH_RETAIN_LIMIT / std::mem::size_of::<ProjectedLineSample>() + 1,
        );
        assert!(scratch.bytes() > CURVE_SCRATCH_RETAIN_LIMIT);
        scratch.bound_retention();
        assert!(scratch.bytes() <= CURVE_SCRATCH_RETAIN_LIMIT);
        assert_eq!(scratch.samples.capacity(), 0);
        assert!(scratch.discards > 0);
        scratch
            .pieces
            .reserve_exact(CURVE_SCRATCH_RETAIN_LIMIT / std::mem::size_of::<Piece<'_>>() + 1);
        scratch.bound_retention();
        assert!(scratch.bytes() <= CURVE_SCRATCH_RETAIN_LIMIT);
        assert_eq!(scratch.pieces.capacity(), 0);
        assert!(scratch.peak_retained <= CURVE_SCRATCH_RETAIN_LIMIT);
    }

    #[test]
    fn source_budget_reclaims_inactive_optional_samples_before_routes() {
        let points = [WorldPoint::new(0., 0.), WorldPoint::new(0.01, 0.01)];
        let mut cache = std::collections::HashMap::new();
        for id in [1usize, 2] {
            let mut c = PreparedCurve::new(&points).unwrap();
            c.reserve_dyadic(std::mem::size_of::<DyadicSamples>());
            cache.insert(id, c);
        }
        let mut bytes = cache.values().map(|c| c.bytes() + 64).sum::<usize>();
        let original = bytes;
        let optional = std::mem::size_of::<DyadicSamples>();
        let reclaimed = reclaim_dyadic_for_route(
            &mut cache,
            &mut bytes,
            &[false, false, true],
            128,
            original + 32,
        );
        assert_eq!(reclaimed, (optional, 1));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[&1].dyadic_bytes(), 0);
        assert_eq!(cache[&2].dyadic_bytes(), optional);
        assert_eq!(bytes, original - optional);
        let reclaimed = reclaim_dyadic_for_route(
            &mut cache,
            &mut bytes,
            &[false, false, true],
            usize::MAX,
            original + 32,
        );
        assert_eq!(reclaimed, (optional, 1));
        assert_eq!(cache.len(), 2);
        assert_eq!(bytes, cache.values().map(|c| c.bytes() + 64).sum::<usize>());
    }

    #[test]
    fn dyadic_first_sampling_is_thread_safe_and_bit_identical() {
        let points = [WorldPoint::new(179.9, 70.), WorldPoint::new(-179.9, 70.01)];
        let mut source = PreparedCurve::new(&points).unwrap();
        source.reserve_dyadic(std::mem::size_of::<DyadicSamples>());
        let camera = GlobeCamera::orbit(
            position(points[0]).unwrap(),
            10000.,
            30.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let route = Route {
            route: source.segments[0].route,
            prepared: Some(&source.segments[0]),
            dyadic: source.dyadic.get(0),
            metrics: None,
        };
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..8 {
                let camera = &camera;
                workers.push(scope.spawn(move || {
                    for _ in 0..4 {
                        for n in 1..16 {
                            let t = n as f64 / 16.;
                            let reference = at(route.route, t, camera).unwrap();
                            let cached = at(route, t, camera).unwrap();
                            assert_eq!(reference.ecef, cached.ecef);
                            assert_eq!(reference.clip, cached.clip);
                        }
                    }
                }));
            }
            for w in workers {
                w.join().unwrap();
            }
        });
        assert_eq!(source.populated_dyadic_samples(), 14);
    }

    #[test]
    fn dyadic_storage_is_bounded_lazy_exact_and_released() {
        let points = [WorldPoint::new(179.9, 70.), WorldPoint::new(-179.9, 70.01)];
        let mut source = PreparedCurve::new(&points).unwrap();
        let base = source.bytes();
        let slot_bytes = std::mem::size_of::<DyadicSamples>();
        assert_eq!(source.reserve_dyadic(slot_bytes - 1), 0);
        assert_eq!(source.reserve_dyadic(slot_bytes), slot_bytes);
        assert_eq!(source.bytes(), base + slot_bytes);
        assert_eq!(source.populated_dyadic_samples(), 0);
        let camera = GlobeCamera::orbit(
            position(points[0]).unwrap(),
            10000.,
            30.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let route = Route {
            route: source.segments[0].route,
            prepared: Some(&source.segments[0]),
            dyadic: source.dyadic.get(0),
            metrics: None,
        };
        for n in 0..=16 {
            let t = n as f64 / 16.;
            let cold = at(route.route, t, &camera).unwrap();
            let cached = at(route, t, &camera).unwrap();
            assert_eq!(cold.ecef, cached.ecef);
            assert_eq!(cold.clip, cached.clip);
        }
        assert_eq!(source.populated_dyadic_samples(), 14);
        assert_eq!(source.bytes(), base + slot_bytes);
        // Camera-dependent clipping roots are not snapped to a cache sample.
        assert!(dyadic_slot(0.1).is_none());
        assert!(dyadic_slot(0.125 + f64::EPSILON).is_none());
        assert_eq!(source.evict_dyadic(), slot_bytes);
        assert_eq!(source.bytes(), base);
        assert_eq!(source.populated_dyadic_samples(), 0);
    }

    #[test]
    fn prepared_sources_preserve_exact_camera_clipping_subdivision_and_phase() {
        let curves = [
            vec![WorldPoint::new(-0.8, -0.01), WorldPoint::new(0.8, -0.01)],
            vec![
                WorldPoint::new(0., 0.),
                WorldPoint::new(0., -0.02),
                WorldPoint::new(0.005, -0.02),
                WorldPoint::new(0.005, 0.),
                WorldPoint::new(0., 0.),
            ],
            vec![WorldPoint::new(179.9, 70.), WorldPoint::new(-179.9, 70.01)],
            vec![WorldPoint::new(30., 89.9), WorldPoint::new(30., 90.)],
        ];
        for points in curves {
            let prepared = PreparedCurve::new(&points).unwrap();
            assert_eq!(
                prepared.bytes(),
                PreparedCurve::estimated_bytes(points.len())
            );
            let mut prepared = prepared;
            prepared.reserve_dyadic(2 * 1024 * 1024);
            let focus = position(points[0]).unwrap();
            for (range, tilt) in [(500., 45.), (10000., 0.), (100000., 75.)] {
                let c = GlobeCamera::orbit(focus, range, 30., tilt, [1000., 800.], 45., 1., 1e8)
                    .unwrap();
                for phase in [false, true] {
                    let cold =
                        project_rhumb_components(&points, &c, 0.25, 5., phase, 262144).unwrap();
                    let warm = project_rhumb_components_prepared(
                        &points,
                        &c,
                        0.25,
                        5.,
                        phase,
                        262144,
                        Some(&prepared),
                    )
                    .unwrap();
                    assert_eq!(cold.1, warm.1);
                    assert_eq!(cold.0.len(), warm.0.len());
                    for (a, b) in cold.0.iter().zip(&warm.0) {
                        assert_eq!(a.initial_phase_px, b.initial_phase_px);
                        assert_eq!(a.samples.len(), b.samples.len());
                        for (a, b) in a.samples.iter().zip(&b.samples) {
                            assert_eq!(a.ecef_m, b.ecef_m);
                            assert_eq!(a.screen_px, b.screen_px);
                            assert_eq!(a.forward_depth_m, b.forward_depth_m);
                            assert_eq!(a.source_segment, b.source_segment);
                            assert_eq!(a.source_fraction, b.source_fraction);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn forward_components_preserve_source_roots_and_never_bridge_removed_depth() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            500.,
            0.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let points = [WorldPoint::new(-0.8, -0.01), WorldPoint::new(0.8, -0.01)];
        let (runs, _) = project_rhumb_components(&points, &c, 0.25, 5., false, 262144).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(
            runs[0].samples.last().unwrap().source_fraction
                < runs[1].samples.first().unwrap().source_fraction
        );
        let route =
            RhumbSegment::new(position(points[0]).unwrap(), position(points[1]).unwrap()).unwrap();
        for i in 0..2001 {
            let t = i as f64 / 2000.;
            let raw = at(route, t, &c).unwrap();
            let inside = raw.clip[3] >= 1. && raw.clip[3] <= 1e8;
            let retained = runs.iter().any(|r| {
                t >= r.samples.first().unwrap().source_fraction
                    && t <= r.samples.last().unwrap().source_fraction
            });
            assert_eq!(inside, retained);
        }
        for run in runs {
            for s in run.samples {
                assert!(s.forward_depth_m >= 1.);
                let p = at(route, s.source_fraction, &c).unwrap();
                assert_eq!(p.ecef, s.ecef_m);
            }
        }
        assert!(project_rhumb_components(
            &[WorldPoint::new(0., -0.01), WorldPoint::new(0., -0.02)],
            &c,
            0.25,
            5.,
            true,
            262144
        )
        .unwrap()
        .0
        .is_empty());
    }
    #[test]
    fn closed_depth_components_keep_authored_join_and_phase_origin() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            500.,
            0.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let points = [
            WorldPoint::new(0., 0.),
            WorldPoint::new(0., -0.02),
            WorldPoint::new(0.005, -0.02),
            WorldPoint::new(0.005, 0.),
            WorldPoint::new(0., 0.),
        ];
        let (runs, _) = project_rhumb_components(&points, &c, 0.25, 5., false, 262144).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].initial_phase_px < 0.);
        assert!(runs[0]
            .samples
            .iter()
            .any(|s| s.source_segment == 3 && s.source_fraction == 1.));
    }
}
