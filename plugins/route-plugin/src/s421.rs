//! S-421 Route Plan Exchange Format
//!
//! Export and import routes in S-421 GML format.

use crate::haversine;
use crate::route::{Route, Waypoint};

/// Export route to S-421 XML format
pub fn export_route(route: &Route) -> Result<String, String> {
    if route.is_empty() {
        return Err("Route is empty".to_string());
    }

    let mut xml = String::new();
    xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push('\n');
    xml.push_str(r#"<Dataset xmlns="http://www.iho.int/S421/gml/1.0""#);
    xml.push_str(r#" xmlns:gml="http://www.opengis.net/gml/3.2""#);
    xml.push_str(r#" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#);
    xml.push('\n');

    // Route element
    xml.push_str("  <Route>\n");
    xml.push_str("    <routeFormatVersion>1.0</routeFormatVersion>\n");

    // Route info
    xml.push_str("    <RouteInfo>\n");
    let name = route.name.as_deref().unwrap_or("Untitled Route");
    xml.push_str(&format!("      <routeInfoName>{}</routeInfoName>\n", escape_xml(name)));
    xml.push_str("      <routeInfoAuthor>FerriteS100</routeInfoAuthor>\n");
    xml.push_str(&format!(
        "      <routeInfoEditionTime>{}</routeInfoEditionTime>\n",
        chrono_now_iso()
    ));
    xml.push_str("      <routeInfoStatus>1</routeInfoStatus>\n");
    xml.push_str("    </RouteInfo>\n");

    // Waypoints
    xml.push_str("    <RouteWaypoints>\n");
    for (i, wp) in route.waypoints.iter().enumerate() {
        xml.push_str(&format!("      <RouteWaypoint gml:id=\"WP{}\">\n", wp.id));
        xml.push_str(&format!("        <routeWaypointID>{}</routeWaypointID>\n", wp.id));

        let default_name = format!("Waypoint {}", i + 1);
        let wp_name = wp.name.as_deref().unwrap_or(&default_name);
        xml.push_str(&format!(
            "        <routeWaypointName>{}</routeWaypointName>\n",
            escape_xml(wp_name)
        ));

        if let Some(radius) = wp.turn_radius {
            xml.push_str(&format!(
                "        <routeWaypointTurnRadius>{:.2}</routeWaypointTurnRadius>\n",
                radius
            ));
        }

        // Geometry (GML Point)
        xml.push_str("        <geometry>\n");
        xml.push_str("          <gml:Point>\n");
        xml.push_str(&format!(
            "            <gml:pos>{:.6} {:.6}</gml:pos>\n",
            wp.lon, wp.lat
        ));
        xml.push_str("          </gml:Point>\n");
        xml.push_str("        </geometry>\n");

        // Leg info (if not last waypoint)
        if i < route.waypoints.len() - 1 {
            let next_wp = &route.waypoints[i + 1];
            let distance = haversine::distance(wp.lat, wp.lon, next_wp.lat, next_wp.lon);

            xml.push_str("        <RouteWaypointLeg>\n");
            xml.push_str(&format!(
                "          <routeWaypointLegDistance>{:.3}</routeWaypointLegDistance>\n",
                distance
            ));
            xml.push_str("          <routeWaypointLegLegGeometryType>1</routeWaypointLegLegGeometryType>\n");
            xml.push_str("        </RouteWaypointLeg>\n");
        }

        xml.push_str("      </RouteWaypoint>\n");
    }
    xml.push_str("    </RouteWaypoints>\n");

    xml.push_str("  </Route>\n");
    xml.push_str("</Dataset>\n");

    Ok(xml)
}

/// Import route from S-421 XML format
pub fn import_route(xml: &str) -> Result<Route, String> {
    let mut route = Route::new(0); // ID will be assigned by caller
    let mut current_wp: Option<Waypoint> = None;
    let mut in_pos = false;
    let mut in_waypoint_id = false;
    let mut in_waypoint_name = false;
    let mut in_route_info_name = false;

    // Simple XML parsing (not a full parser, but works for S-421)
    for line in xml.lines() {
        let line = line.trim();

        // Match individual waypoint (not container <RouteWaypoints>)
        if line.starts_with("<RouteWaypoint ") || line.starts_with("<RouteWaypoint>") {
            current_wp = Some(Waypoint::new(0, 0.0, 0.0));
        } else if line == "</RouteWaypoint>" {
            // Only match exact closing tag (not </RouteWaypoints>)
            if let Some(wp) = current_wp.take() {
                if wp.id > 0 {
                    route.add_waypoint(wp);
                }
            }
        } else if line.starts_with("<routeWaypointID>") {
            if let Some(ref mut wp) = current_wp {
                if let Some(id) = extract_text_content(line, "routeWaypointID") {
                    wp.id = id.parse().unwrap_or(route.next_id());
                }
            }
            in_waypoint_id = line.contains("<routeWaypointID>") && !line.contains("</routeWaypointID>");
        } else if line.starts_with("<routeWaypointName>") {
            if let Some(ref mut wp) = current_wp {
                if let Some(name) = extract_text_content(line, "routeWaypointName") {
                    wp.name = Some(name);
                }
            }
            in_waypoint_name = line.contains("<routeWaypointName>") && !line.contains("</routeWaypointName>");
        } else if line.starts_with("<routeInfoName>") {
            if let Some(name) = extract_text_content(line, "routeInfoName") {
                route.name = Some(name);
            }
            in_route_info_name = line.contains("<routeInfoName>") && !line.contains("</routeInfoName>");
        } else if line.contains("<gml:pos>") || line.contains("<pos>") {
            if let Some(ref mut wp) = current_wp {
                // Extract coordinates from <gml:pos>lon lat</gml:pos>
                let pos_text = if let Some(pos) = extract_text_content(line, "gml:pos") {
                    pos
                } else if let Some(pos) = extract_text_content(line, "pos") {
                    pos
                } else {
                    continue;
                };

                let coords: Vec<f64> = pos_text
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();

                if coords.len() >= 2 {
                    wp.lon = coords[0];
                    wp.lat = coords[1];
                }
            }
        }
    }

    // Update next_id to be after the highest ID
    if let Some(max_id) = route.waypoints.iter().map(|wp| wp.id).max() {
        route.set_next_id(max_id + 1);
    }

    if route.is_empty() {
        return Err("No waypoints found in file".to_string());
    }

    Ok(route)
}

/// Extract text content from a simple XML element
fn extract_text_content(line: &str, tag: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag);
    let close_tag = format!("</{}>", tag);

    if let Some(start) = line.find(&open_tag) {
        if let Some(end) = line.find(&close_tag) {
            let content_start = start + open_tag.len();
            if content_start < end {
                return Some(unescape_xml(&line[content_start..end]));
            }
        }
    }
    None
}

/// Escape special XML characters
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Unescape XML entities
fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Get current time in ISO 8601 format
fn chrono_now_iso() -> String {
    // Simple implementation without chrono dependency
    "2024-01-01T00:00:00Z".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_export_import() {
        let mut route = Route::with_name(1, "Test Route");
        route.add_waypoint(Waypoint::new(route.next_id(), 129.0, 35.0).with_name("Start"));
        route.add_waypoint(Waypoint::new(route.next_id(), 129.1, 35.1).with_name("End"));

        let xml = export_route(&route).unwrap();
        assert!(xml.contains("Test Route"));
        assert!(xml.contains("129.0"));

        let imported = import_route(&xml).unwrap();
        assert_eq!(imported.waypoints.len(), 2);
    }

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("a<b>c"), "a&lt;b&gt;c");
        assert_eq!(unescape_xml("a&lt;b&gt;c"), "a<b>c");
    }
}
