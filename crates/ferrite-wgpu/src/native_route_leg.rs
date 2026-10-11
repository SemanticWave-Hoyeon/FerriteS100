//! Bounded WGS84 leg sampling for host illustration, not full S421 XSLT portrayal.
use ferrite_kernel::geodesy::{direct, inverse, GeographicPosition};
use ferrite_kernel::rhumb::RhumbSegment;
/// Numerical receiver policy: at most 1000m geodesic distance / rhumb parameter
/// length per step. This is NOT a proven screen-error tolerance or a standard limit.
pub(crate) const MAX_STEP_M: f64 = 1000.;
pub const MAX_LEG_VERTICES: usize = 65_536;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRouteLegGeometry {
    Loxodrome,
    Orthodrome,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeRouteLegDeclaration {
    pub route_id: u32,
    pub from: u32,
    pub to: u32,
    pub geometry: NativeRouteLegGeometry,
    /// Declared provenance (0 locally authored / 1 published / 2 candidate), original ID and lexical value.
    pub source_profile: u8,
    pub source_gml_id: String,
    pub original_geometry_text: String,
}
#[cfg(test)]
thread_local! { static SOLVER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn solver_call_count() -> usize {
    SOLVER_CALLS.with(|count| count.get())
}
/// Return a continuous drawing longitude sheet; canonical original endpoints
/// remain outside this transient copy. Endpoint latitude and lifted longitude
/// are exact, and interiors use the existing WGS84 solvers.
pub(crate) fn sample_leg(
    start: [f64; 2],
    end: [f64; 2],
    geometry: NativeRouteLegGeometry,
    max_vertices: usize,
) -> Result<Vec<[f64; 2]>, String> {
    #[cfg(test)]
    SOLVER_CALLS.with(|count| count.set(count.get() + 1));
    let a = GeographicPosition::new(start[1], start[0]).map_err(|e| e.to_string())?;
    let b = GeographicPosition::new(end[1], end[0]).map_err(|e| e.to_string())?;
    let delta = (end[0] - start[0] + 180.).rem_euclid(360.) - 180.;
    // No undocumented longitude direction / nonunique antipodal route choice.
    if delta.abs() == 180. {
        return Err("Ambiguous 180-degree leg longitude unsupported".into());
    }
    let geodesic = inverse(a, b).map_err(|e| e.to_string())?;
    let rhumb = if geometry == NativeRouteLegGeometry::Loxodrome {
        Some(RhumbSegment::new(a, b).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let length = match rhumb {
        Some(segment) => segment
            .parameter_length_bound_m()
            .map_err(|e| e.to_string())?,
        None => geodesic.distance_m,
    };
    let steps = (length / MAX_STEP_M).ceil().max(1.);
    if !steps.is_finite()
        || !(2..=MAX_LEG_VERTICES).contains(&max_vertices)
        || steps > (max_vertices - 1) as f64
    {
        return Err("S421 semantic leg vertex budget exceeded".into());
    }
    let steps = steps as usize;
    let mut points = Vec::with_capacity(steps + 1);
    points.push(start);
    let mut longitude = start[0];
    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let point = match rhumb {
            Some(segment) => segment.point(t).map_err(|e| e.to_string())?,
            None => direct(a, geodesic.initial_azimuth_deg, geodesic.distance_m * t)
                .map_err(|e| e.to_string())?,
        };
        longitude += (point.longitude() - longitude + 180.).rem_euclid(360.) - 180.;
        points.push([longitude, point.latitude()]);
    }
    // Choose the sheet using the sampled path, then anchor its endpoint to
    // the original longitude. Accumulated solver rounding must not alter the
    // original endpoint or leave a tiny gap at an adjacent leg boundary.
    let sheet = ((longitude - end[0]) / 360.).round();
    let endpoint_longitude = if sheet == 0. {
        end[0]
    } else {
        end[0] + sheet * 360.
    };
    points.push([endpoint_longitude, end[1]]);
    Ok(points)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_latitude_geodesic_bows_north_rhumb_keeps_parallel() {
        let g = sample_leg(
            [-30., 70.],
            [30., 70.],
            NativeRouteLegGeometry::Orthodrome,
            MAX_LEG_VERTICES,
        )
        .unwrap();
        let r = sample_leg(
            [-30., 70.],
            [30., 70.],
            NativeRouteLegGeometry::Loxodrome,
            MAX_LEG_VERTICES,
        )
        .unwrap();
        assert!(g[g.len() / 2][1] > 72.);
        assert!(r.iter().all(|p| (p[1] - 70.).abs() < 1e-10));
        assert_eq!(g[0], [-30., 70.]);
        assert_eq!(*g.last().unwrap(), [30., 70.]);
    }
    #[test]
    fn dateline_sheet_and_original_endpoint_latitudes_are_retained() {
        for geometry in [
            NativeRouteLegGeometry::Loxodrome,
            NativeRouteLegGeometry::Orthodrome,
        ] {
            let points = sample_leg([179., 60.], [-179., 60.], geometry, MAX_LEG_VERTICES).unwrap();
            assert_eq!(points[0], [179., 60.]);
            assert_eq!(*points.last().unwrap(), [181., 60.]);
            assert!(points.windows(2).all(|p| (p[1][0] - p[0][0]).abs() < 1.));
        }
    }
    #[test]
    fn budget_ambiguity_and_polar_rhumb_decline_without_straight_fallback() {
        assert!(sample_leg([0., 0.], [1., 1.], NativeRouteLegGeometry::Orthodrome, 2).is_err());
        assert!(sample_leg(
            [0., 0.],
            [180., 0.],
            NativeRouteLegGeometry::Orthodrome,
            MAX_LEG_VERTICES
        )
        .is_err());
        assert!(sample_leg(
            [0., 90.],
            [1., 80.],
            NativeRouteLegGeometry::Loxodrome,
            MAX_LEG_VERTICES
        )
        .is_err());
        assert!(sample_leg([0., 0.], [0., 0.], NativeRouteLegGeometry::Loxodrome, 2).is_ok());
    }
}
