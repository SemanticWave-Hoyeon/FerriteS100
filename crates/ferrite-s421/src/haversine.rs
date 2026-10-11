//! Haversine distance calculation
//!
//! Calculates great-circle distance between two points on Earth.

/// Earth radius in nautical miles
const EARTH_RADIUS_NM: f64 = 3440.065;

/// Calculate distance between two points using Haversine formula
///
/// # Arguments
/// * `lat1`, `lon1` - First point (degrees)
/// * `lat2`, `lon2` - Second point (degrees)
///
/// # Returns
/// Distance in nautical miles
pub fn distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();
    let delta_lat = (lat2 - lat1).to_radians();
    let delta_lon = (lon2 - lon1).to_radians();

    let a = (delta_lat / 2.0).sin().powi(2)
        + lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);

    let c = 2.0 * a.sqrt().asin();

    EARTH_RADIUS_NM * c
}

/// Calculate initial bearing from point 1 to point 2
///
/// # Returns
/// Bearing in degrees (0-360)
pub fn bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();
    let delta_lon = (lon2 - lon1).to_radians();

    let x = delta_lon.sin() * lat2_rad.cos();
    let y = lat1_rad.cos() * lat2_rad.sin() - lat1_rad.sin() * lat2_rad.cos() * delta_lon.cos();

    let bearing_rad = x.atan2(y);
    (bearing_rad.to_degrees() + 360.0) % 360.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance() {
        // Tokyo to San Francisco (approximately 4500 NM)
        let dist = distance(35.6762, 139.6503, 37.7749, -122.4194);
        assert!((dist - 4500.0).abs() < 100.0); // Within 100 NM tolerance
    }

    #[test]
    fn test_same_point() {
        let dist = distance(35.0, 129.0, 35.0, 129.0);
        assert!(dist < 0.001);
    }

    #[test]
    fn test_bearing() {
        // Due north
        let brg = bearing(35.0, 129.0, 36.0, 129.0);
        assert!((brg - 0.0).abs() < 1.0);

        // Due east (approximately)
        let brg = bearing(35.0, 129.0, 35.0, 130.0);
        assert!((brg - 90.0).abs() < 5.0);
    }
}
