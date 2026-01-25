//! Route data structures

use serde::{Deserialize, Serialize};

use crate::haversine;

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
    pub fn next_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
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

    /// Calculate total route distance in nautical miles
    pub fn total_distance(&self) -> f64 {
        if self.waypoints.len() < 2 {
            return 0.0;
        }

        self.waypoints
            .windows(2)
            .map(|pair| haversine::distance(pair[0].lat, pair[0].lon, pair[1].lat, pair[1].lon))
            .sum()
    }

    /// Get leg distances
    pub fn leg_distances(&self) -> Vec<f64> {
        if self.waypoints.len() < 2 {
            return Vec::new();
        }

        self.waypoints
            .windows(2)
            .map(|pair| haversine::distance(pair[0].lat, pair[0].lon, pair[1].lat, pair[1].lon))
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
}

impl Waypoint {
    pub fn new(id: u32, lon: f64, lat: f64) -> Self {
        Self {
            id,
            lon,
            lat,
            name: None,
            turn_radius: None,
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
        if decimal_degrees >= 0.0 { "N" } else { "S" }
    } else {
        if decimal_degrees >= 0.0 { "E" } else { "W" }
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

        route.add_waypoint(Waypoint::new(route.next_id(), 129.0, 35.0));
        route.add_waypoint(Waypoint::new(route.next_id(), 129.1, 35.1));
        assert_eq!(route.len(), 2);

        let dist = route.total_distance();
        assert!(dist > 0.0);

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
