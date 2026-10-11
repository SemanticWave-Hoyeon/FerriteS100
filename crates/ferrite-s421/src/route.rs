//! Route data structures

use serde::{Deserialize, Serialize};

use crate::{
    navigation::{evaluate_leg, LegMetrics},
    s421::LegGeometry,
};

/// A route consisting of waypoints
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Route {
    /// Unique route ID
    pub id: u32,
    /// Route name
    pub name: Option<String>,
    /// List of waypoints
    pub waypoints: Vec<Waypoint>,
    /// Next waypoint ID
    next_id: u32,
}

impl Route {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            name: None,
            waypoints: Vec::new(),
            next_id: 1,
        }
    }

    pub fn with_name(id: u32, name: &str) -> Self {
        Self {
            id,
            name: Some(name.to_string()),
            waypoints: Vec::new(),
            next_id: 1,
        }
    }

    /// Get next waypoint ID and increment
    pub fn next_id(&mut self) -> Option<u32> {
        let id = self.next_id;
        if id == 0 {
            return None;
        }
        let next = id.checked_add(1)?;
        self.next_id = next;
        Some(id)
    }

    /// Set the next ID counter (used after import)
    pub fn set_next_id(&mut self, id: u32) {
        self.next_id = id;
    }

    /// Add a waypoint to the route
    pub fn add_waypoint(&mut self, waypoint: Waypoint) {
        self.waypoints.push(waypoint);
    }

    /// Remove the last waypoint
    pub fn remove_last_waypoint(&mut self) -> Option<Waypoint> {
        self.waypoints.pop()
    }

    /// Remove a waypoint by ID
    pub fn remove_waypoint(&mut self, id: u32) -> bool {
        if let Some(pos) = self.waypoints.iter().position(|wp| wp.id == id) {
            self.waypoints.remove(pos);
            true
        } else {
            false
        }
    }

    /// Clear all waypoints
    pub fn clear(&mut self) {
        self.waypoints.clear();
        self.next_id = 1;
    }

    /// Metrics from each explicitly declared incoming leg. Unknown is not a geodesic default.
    pub fn leg_metrics(&self) -> Vec<Option<LegMetrics>> {
        self.waypoints
            .windows(2)
            .map(|pair| {
                let geometry = pair[1].incoming_geometry?;
                evaluate_leg(
                    [pair[0].lon, pair[0].lat],
                    [pair[1].lon, pair[1].lat],
                    geometry,
                )
                .ok()
            })
            .collect()
    }

    /// WGS84 nautical miles, unavailable if any leg is undeclared or cannot be evaluated.
    pub fn total_distance(&self) -> Option<f64> {
        self.leg_metrics().iter().try_fold(0.0, |sum, metric| {
            let sum = sum + metric.as_ref()?.distance_nm;
            sum.is_finite().then_some(sum)
        })
    }

    pub fn leg_distances(&self) -> Vec<Option<f64>> {
        self.leg_metrics()
            .into_iter()
            .map(|metric| metric.map(|m| m.distance_nm))
            .collect()
    }

    /// Get number of waypoints
    pub fn len(&self) -> usize {
        self.waypoints.len()
    }

    /// Check if route is empty
    pub fn is_empty(&self) -> bool {
        self.waypoints.is_empty()
    }
}

/// A waypoint in a route
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waypoint {
    /// Unique ID
    pub id: u32,
    /// Longitude (degrees)
    pub lon: f64,
    /// Latitude (degrees)
    pub lat: f64,
    /// Optional name
    pub name: Option<String>,
    /// Turn radius (nautical miles)
    pub turn_radius: Option<f64>,
    /// Original declared geometry of the leg arriving at this waypoint.
    #[serde(default)]
    pub incoming_geometry: Option<LegGeometry>,
}

impl Waypoint {
    pub fn new(id: u32, lon: f64, lat: f64) -> Self {
        Self {
            id,
            lon,
            lat,
            name: None,
            turn_radius: None,
            incoming_geometry: None,
        }
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    pub fn with_turn_radius(mut self, radius: f64) -> Self {
        self.turn_radius = Some(radius);
        self
    }

    /// Format position as DMS string
    pub fn format_dms(&self) -> String {
        let lat_dms = format_dms(self.lat, true);
        let lon_dms = format_dms(self.lon, false);
        format!("{} {}", lat_dms, lon_dms)
    }
}

/// Format decimal degrees as DMS string
fn format_dms(decimal_degrees: f64, is_lat: bool) -> String {
    let abs_deg = decimal_degrees.abs();
    let degrees = abs_deg.floor() as i32;
    let minutes_full = (abs_deg - degrees as f64) * 60.0;
    let minutes = minutes_full.floor() as i32;
    let seconds = (minutes_full - minutes as f64) * 60.0;

    let dir = if is_lat {
        if decimal_degrees >= 0.0 {
            "N"
        } else {
            "S"
        }
    } else {
        if decimal_degrees >= 0.0 {
            "E"
        } else {
            "W"
        }
    };

    if is_lat {
        format!("{:02}°{:02}'{:05.2}\"{}", degrees, minutes, seconds, dir)
    } else {
        format!("{:03}°{:02}'{:05.2}\"{}", degrees, minutes, seconds, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_operations() {
        let mut route = Route::new(1);
        assert!(route.is_empty());

        let first = route.next_id().unwrap();
        route.add_waypoint(Waypoint::new(first, 129.0, 35.0));
        let second = route.next_id().unwrap();
        route.add_waypoint(Waypoint::new(second, 129.1, 35.1));
        assert_eq!(route.len(), 2);

        let dist = route.total_distance();
        assert_eq!(dist, None); // Coordinates alone do not declare the route curve.

        route.clear();
        assert!(route.is_empty());
    }

    #[test]
    fn test_format_dms() {
        let wp = Waypoint::new(1, 129.083333, 35.2);
        let dms = wp.format_dms();
        assert!(dms.contains("N"));
        assert!(dms.contains("E"));
    }
}
