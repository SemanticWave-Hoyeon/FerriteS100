//! One WGS84 numerical model for declared route-leg metrics.
//! Missing S-421 leg declarations are not inferred from waypoint positions.
use crate::s421::LegGeometry;
use ferrite_kernel::{
    geodesy::{direct, inverse, GeographicPosition},
    rhumb::RhumbSegment,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegMetrics {
    pub distance_nm: f64,
    pub initial_bearing_deg: Option<f64>,
    pub final_bearing_deg: Option<f64>,
    /// Longitude, latitude; lies on the declared curve, not a coordinate average.
    pub midpoint: [f64; 2],
}

pub fn evaluate_leg(
    start: [f64; 2],
    end: [f64; 2],
    geometry: LegGeometry,
) -> Result<LegMetrics, String> {
    let a = GeographicPosition::new(start[1], start[0]).map_err(|e| e.to_string())?;
    let b = GeographicPosition::new(end[1], end[0]).map_err(|e| e.to_string())?;
    // The host sampling policy also refuses an ambiguous longitude sheet.
    if ((end[0] - start[0] + 180.).rem_euclid(360.) - 180.).abs() == 180. {
        return Err("Ambiguous 180-degree route leg unsupported".into());
    }
    let (distance, initial, final_bearing, midpoint) = match geometry {
        LegGeometry::Orthodrome => {
            let g = inverse(a, b).map_err(|e| e.to_string())?;
            let midpoint =
                direct(a, g.initial_azimuth_deg, g.distance_m / 2.).map_err(|e| e.to_string())?;
            (
                g.distance_m,
                g.initial_azimuth_deg,
                g.final_azimuth_deg,
                midpoint,
            )
        }
        LegGeometry::Loxodrome => {
            let r = RhumbSegment::new(a, b).map_err(|e| e.to_string())?;
            (
                r.distance_m().map_err(|e| e.to_string())?,
                r.bearing_deg(),
                r.bearing_deg(),
                r.point(0.5).map_err(|e| e.to_string())?,
            )
        }
    };
    Ok(LegMetrics {
        distance_nm: distance / 1852.,
        initial_bearing_deg: (distance > 0.).then_some(initial),
        final_bearing_deg: (distance > 0.).then_some(final_bearing),
        midpoint: [midpoint.longitude(), midpoint.latitude()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_long_high_latitude_legs_keep_different_geometry_and_distance() {
        let g = evaluate_leg([-30., 70.], [30., 70.], LegGeometry::Orthodrome).unwrap();
        let r = evaluate_leg([-30., 70.], [30., 70.], LegGeometry::Loxodrome).unwrap();
        assert!((g.distance_nm - 1187.2212872068706).abs() < 1e-8);
        assert!((r.distance_nm - 1237.1449657237144).abs() < 1e-8);
        assert!((g.midpoint[1] - 72.50641025675012).abs() < 1e-10);
        assert!((r.midpoint[1] - 70.).abs() < 1e-10);
        assert!((r.initial_bearing_deg.unwrap() - 90.).abs() < 1e-10);
        assert!(g.initial_bearing_deg.unwrap() < 90.);
        assert!(g.final_bearing_deg.unwrap() > 90.);
    }
    #[test]
    fn coincident_and_dateline_metrics_do_not_invent_a_course_or_midpoint_at_greenwich() {
        for geometry in [LegGeometry::Loxodrome, LegGeometry::Orthodrome] {
            let zero = evaluate_leg([0., 50.], [0., 50.], geometry).unwrap();
            assert_eq!(zero.distance_nm, 0.);
            assert_eq!(zero.initial_bearing_deg, None);
            assert_eq!(zero.final_bearing_deg, None);
            let crossing = evaluate_leg([179., 60.], [-179., 60.], geometry).unwrap();
            assert!(crossing.midpoint[0].abs() > 179.);
            assert!(crossing.distance_nm > 0.);
            assert!(evaluate_leg([0., 0.], [180., 0.], geometry).is_err());
            assert!(evaluate_leg([f64::NAN, 0.], [0., 0.], geometry).is_err());
        }
    }
}
