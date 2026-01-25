//! UI data structures for plugin panel

use serde::{Deserialize, Serialize};

use crate::catalogue::CatalogueStatus;
use crate::route::Route;
use crate::{DistanceUnit, RouteSettings};

/// UI data sent to host for rendering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiData {
    /// Panel title
    pub title: String,
    /// Is rendering enabled
    pub rendering_enabled: bool,
    /// Is in editing mode
    pub editing: bool,
    /// All routes
    pub routes: Vec<RouteUi>,
    /// Active route index
    pub active_route_index: Option<usize>,
    /// Waypoint list for active route
    pub waypoints: Vec<WaypointUi>,
    /// Total distance for active route
    pub total_distance: String,
    /// Total waypoints count for active route
    pub waypoint_count: usize,
    /// Available actions
    pub actions: Vec<ActionButton>,
    /// FC status
    pub fc_status: CatalogueStatusUi,
    /// PC status
    pub pc_status: CatalogueStatusUi,
}

/// Route info for UI display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteUi {
    /// Route ID
    pub id: u32,
    /// Route name
    pub name: String,
    /// Number of waypoints
    pub waypoint_count: usize,
    /// Total distance
    pub total_distance: String,
    /// Is this route active
    pub active: bool,
}

/// Catalogue status for UI display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogueStatusUi {
    pub loaded: bool,
    pub message: String,
}

impl From<&CatalogueStatus> for CatalogueStatusUi {
    fn from(status: &CatalogueStatus) -> Self {
        match status {
            CatalogueStatus::NotLoaded => Self {
                loaded: false,
                message: "Not loaded".to_string(),
            },
            CatalogueStatus::Loading => Self {
                loaded: false,
                message: "Loading...".to_string(),
            },
            CatalogueStatus::Loaded { version, feature_count } => Self {
                loaded: true,
                message: format!("v{} ({} items)", version, feature_count),
            },
            CatalogueStatus::Error(e) => Self {
                loaded: false,
                message: format!("Error: {}", e),
            },
        }
    }
}

/// Waypoint data for UI display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaypointUi {
    /// Waypoint ID
    pub id: u32,
    /// Display name
    pub name: String,
    /// Position as DMS string
    pub position: String,
    /// Distance to next waypoint (if applicable)
    pub leg_distance: Option<String>,
}

/// Action button
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionButton {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}

/// UI events from host
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UiEvent {
    /// Start new route (enter editing mode)
    New,
    /// Finish editing (exit editing mode)
    Finish,
    /// Clear all routes
    Clear,
    /// Export active route to file
    Export,
    /// Import route from file
    Import,
    /// Select a waypoint
    SelectWaypoint { id: u32 },
    /// Delete a waypoint
    DeleteWaypoint { id: u32 },
    /// Select a route by index
    SelectRoute { index: usize },
    /// Delete a route by ID
    DeleteRoute { id: u32 },
    /// Rename a route
    RenameRoute { id: u32, name: String },
    /// Rename a waypoint
    RenameWaypoint { id: u32, name: String },
    /// Toggle rendering on/off
    ToggleRendering,
}

/// Build UI data from route state
pub fn build_ui_data(
    routes: &[Route],
    active_route_index: Option<usize>,
    editing: bool,
    rendering_enabled: bool,
    settings: &RouteSettings,
    fc_status: &CatalogueStatus,
    pc_status: &CatalogueStatus,
) -> UiData {
    // Build route list
    let routes_ui: Vec<RouteUi> = routes
        .iter()
        .enumerate()
        .map(|(idx, route)| RouteUi {
            id: route.id,
            name: route.name.clone().unwrap_or_else(|| format!("Route {}", route.id)),
            waypoint_count: route.waypoints.len(),
            total_distance: format_distance(route.total_distance(), settings.distance_unit),
            active: active_route_index == Some(idx),
        })
        .collect();

    // Get active route data
    let active_route = active_route_index.and_then(|i| routes.get(i));

    let (waypoints, total_distance, waypoint_count) = if let Some(route) = active_route {
        let distances = route.leg_distances();

        let waypoints: Vec<WaypointUi> = route
            .waypoints
            .iter()
            .enumerate()
            .map(|(i, wp)| {
                let name = wp
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("WP {}", i + 1));

                // Show distance FROM previous waypoint (not TO next)
                // First waypoint has no incoming distance
                let leg_distance = if i > 0 {
                    Some(format_distance(distances[i - 1], settings.distance_unit))
                } else {
                    None
                };

                WaypointUi {
                    id: wp.id,
                    name,
                    position: wp.format_dms(),
                    leg_distance,
                }
            })
            .collect();

        let total_distance = format_distance(route.total_distance(), settings.distance_unit);
        let waypoint_count = route.len();

        (waypoints, total_distance, waypoint_count)
    } else {
        (Vec::new(), "0.0 NM".to_string(), 0)
    };

    // Build actions based on editing state
    let mut actions = Vec::new();

    if editing {
        // In editing mode: show Finish button
        actions.push(ActionButton {
            id: "finish".to_string(),
            label: "Finish".to_string(),
            enabled: true,
        });
    } else {
        // Not editing: show New button
        actions.push(ActionButton {
            id: "new".to_string(),
            label: "New".to_string(),
            enabled: true,
        });
    }

    // Always show these
    actions.push(ActionButton {
        id: "clear".to_string(),
        label: "Clear All".to_string(),
        enabled: !routes.is_empty(),
    });
    actions.push(ActionButton {
        id: "export".to_string(),
        label: "Export".to_string(),
        enabled: active_route.is_some_and(|r| r.waypoints.len() >= 2),
    });
    actions.push(ActionButton {
        id: "import".to_string(),
        label: "Import".to_string(),
        enabled: !editing,
    });

    UiData {
        title: if editing { "Route Plan (Editing)".to_string() } else { "Route Plan".to_string() },
        rendering_enabled,
        editing,
        routes: routes_ui,
        active_route_index,
        waypoints,
        total_distance,
        waypoint_count,
        actions,
        fc_status: CatalogueStatusUi::from(fc_status),
        pc_status: CatalogueStatusUi::from(pc_status),
    }
}

/// Format distance with both NM and km
fn format_distance(nm: f64, _unit: DistanceUnit) -> String {
    // Always show both NM and km
    let km = nm * 1.852;
    format!("{:.1} NM ({:.1} km)", nm, km)
}
