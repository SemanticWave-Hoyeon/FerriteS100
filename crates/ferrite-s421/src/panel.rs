//! Data model of the S-421 route panel: what a route provider publishes
//! (the viewer's route editor, [`NATIVE_PROVIDER`], or the external route
//! planner plugin, [`PLUGIN_PROVIDER`]) as its plugin UI JSON
//! ([`crate::ui::UiData`] plus the editor's fields), read into plain rows
//! and options. UI crates draw it and send the panel's requests back as
//! [`crate::ui::UiEvent`] JSON to the provider.
//!
//! Reading is tolerant as the panel always was: a missing or mistyped field
//! takes its default and never discards the rest.
use serde_json::Value;

/// The panel the product publishes.
pub const PANEL: ferrite_product_api::PanelDescriptor = crate::layer::ROUTE_PANEL;

/// The viewer's native S-421 route editor (`editor::PROVIDER_ID`).
pub const NATIVE_PROVIDER: &str = "native:s421-route";
/// The external route planner plugin.
pub const PLUGIN_PROVIDER: &str = "com.ferrite.route-planner";
/// The providers that may drive the route panel, by precedence. Exact
/// identities only: a plugin whose id merely contains "route" cannot claim
/// the panel or receive its commands.
pub const PROVIDERS: [&str; 2] = [NATIVE_PROVIDER, PLUGIN_PROVIDER];

/// An action button.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteAction {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}

impl RouteAction {
    /// The event the button sends; None for unknown actions (not drawn
    /// as clickable requests).
    pub fn event(&self) -> Option<&'static str> {
        Some(match self.id.as_str() {
            "new" => r#"{"type":"New"}"#,
            "finish" => r#"{"type":"Finish"}"#,
            "clear" => r#"{"type":"Clear"}"#,
            "export" => r#"{"type":"Export"}"#,
            "import" => r#"{"type":"Import"}"#,
            "copy" => r#"{"type":"CopyAsAuthored"}"#,
            _ => return None,
        })
    }
}

/// A route of the route list.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    pub id: u32,
    pub name: String,
    pub waypoint_count: u64,
    pub total_distance: String,
    pub active: bool,
    /// Whether the route can be renamed (unset: the provider's default).
    pub editable: Option<bool>,
}

/// A waypoint of the active route.
#[derive(Debug, Clone, PartialEq)]
pub struct WaypointRow {
    pub id: u32,
    pub name: String,
    pub position: String,
    /// The leg to this waypoint.
    pub leg_distance: Option<String>,
    pub turn_radius_nm: Option<f64>,
    pub initial_bearing_deg: Option<f64>,
    pub final_bearing_deg: Option<f64>,
    /// `Loxodrome` / `Orthodrome`.
    pub leg_geometry: Option<String>,
}

/// A catalogue status line.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogueLine {
    pub loaded: bool,
    pub message: String,
}

/// What a route provider publishes for the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutePanelData {
    pub editing: bool,
    pub title: String,
    pub notice: Option<String>,
    pub display_available: bool,
    pub rendering_enabled: bool,
    /// Action buttons (None: not published).
    pub actions: Option<Vec<RouteAction>>,
    /// The route list (None: not published; empty: no routes).
    pub routes: Option<Vec<RouteRow>>,
    /// Whether waypoints can be deleted and renamed (unset: the provider's
    /// default).
    pub content_editable: Option<bool>,
    /// Whether turn radii can be edited (`turn_radius_editable`, else
    /// `content_editable`).
    pub turn_radius_editable: bool,
    /// Id of the first active route of the list.
    pub active_route_id: Option<u32>,
    /// Index of the active route; the waypoint section is shown with one.
    pub active_route_index: Option<u64>,
    /// The provider published an active route index (any value).
    pub has_active_route_index: bool,
    pub waypoint_count: Option<u64>,
    pub total_distance: Option<String>,
    pub waypoints: Option<Vec<WaypointRow>>,
    pub leg_geometry_editable: bool,
    pub fc_status: Option<CatalogueLine>,
    pub pc_status: Option<CatalogueLine>,
}

impl RoutePanelData {
    /// The panel data of a provider's UI JSON; None when it is not JSON.
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str::<Value>(json)
            .ok()
            .map(|value| Self::from_json(&value))
    }

    /// The panel data of a provider's UI JSON value.
    pub fn from_json(ui: &Value) -> Self {
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        let flag = |v: &Value, key: &str| v.get(key).and_then(Value::as_bool);
        let routes = ui.get("routes").and_then(Value::as_array);
        let catalogue = |key: &str| {
            ui.get(key).map(|status| CatalogueLine {
                loaded: flag(status, "loaded").unwrap_or(false),
                message: text(status, "message").unwrap_or_else(|| "Unknown".into()),
            })
        };
        Self {
            editing: flag(ui, "editing").unwrap_or(false),
            title: text(ui, "title").unwrap_or_else(|| "Route Plan".into()),
            notice: text(ui, "notice"),
            display_available: flag(ui, "display_available").unwrap_or(true),
            rendering_enabled: flag(ui, "rendering_enabled").unwrap_or(true),
            actions: ui.get("actions").and_then(Value::as_array).map(|actions| {
                actions
                    .iter()
                    .map(|action| RouteAction {
                        id: text(action, "id").unwrap_or_default(),
                        label: text(action, "label").unwrap_or_else(|| "?".into()),
                        enabled: flag(action, "enabled").unwrap_or(false),
                    })
                    .collect()
            }),
            routes: routes.map(|routes| {
                routes
                    .iter()
                    .map(|route| RouteRow {
                        id: route.get("id").and_then(Value::as_u64).unwrap_or(0) as u32,
                        name: text(route, "name").unwrap_or_else(|| "Route".into()),
                        waypoint_count: route
                            .get("waypoint_count")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        total_distance: text(route, "total_distance")
                            .unwrap_or_else(|| "0 NM".into()),
                        active: flag(route, "active").unwrap_or(false),
                        editable: flag(route, "editable"),
                    })
                    .collect()
            }),
            content_editable: flag(ui, "content_editable"),
            turn_radius_editable: ui
                .get("turn_radius_editable")
                .or_else(|| ui.get("content_editable"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            active_route_id: routes
                .and_then(|routes| routes.iter().find(|r| flag(r, "active") == Some(true)))
                .and_then(|r| r.get("id"))
                .and_then(Value::as_u64)
                .and_then(|id| u32::try_from(id).ok()),
            active_route_index: ui.get("active_route_index").and_then(Value::as_u64),
            has_active_route_index: ui.get("active_route_index").is_some(),
            waypoint_count: ui.get("waypoint_count").and_then(Value::as_u64),
            total_distance: text(ui, "total_distance"),
            waypoints: ui
                .get("waypoints")
                .and_then(Value::as_array)
                .map(|waypoints| {
                    waypoints
                        .iter()
                        .map(|wp| WaypointRow {
                            id: wp.get("id").and_then(Value::as_u64).unwrap_or(0) as u32,
                            name: text(wp, "name").unwrap_or_else(|| "WP".into()),
                            position: text(wp, "position").unwrap_or_default(),
                            leg_distance: text(wp, "leg_distance"),
                            turn_radius_nm: wp.get("turn_radius_nm").and_then(Value::as_f64),
                            initial_bearing_deg: wp
                                .get("initial_bearing_deg")
                                .and_then(Value::as_f64),
                            final_bearing_deg: wp.get("final_bearing_deg").and_then(Value::as_f64),
                            leg_geometry: text(wp, "leg_geometry"),
                        })
                        .collect()
                }),
            leg_geometry_editable: flag(ui, "leg_geometry_editable").unwrap_or(false),
            fc_status: catalogue("fc_status"),
            pc_status: catalogue("pc_status"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_data_reads_fields_tolerantly_with_the_panel_defaults() {
        let data = RoutePanelData::parse(
            r#"{"routes":[{"id":4294967301,"active":true},{"id":"x"}],
                "turn_radius_editable":"yes","content_editable":true,
                "active_route_index":null,"fc_status":null,
                "actions":[{"id":"copy"},{"id":"zap","label":"Z","enabled":true}],
                "waypoints":[{"id":2,"leg_distance":"1 NM","turn_radius_nm":0.5}]}"#,
        )
        .unwrap();
        assert_eq!(data.title, "Route Plan");
        assert!(data.display_available && data.rendering_enabled && !data.editing);
        let routes = data.routes.as_ref().unwrap();
        // `as u32` for rows, a checked conversion for the active route id.
        assert_eq!(routes[0].id, 5);
        assert_eq!(routes[1].id, 0);
        assert_eq!(routes[1].name, "Route");
        assert_eq!(routes[1].total_distance, "0 NM");
        assert_eq!(data.active_route_id, None);
        // A mistyped turn_radius_editable does not fall back.
        assert!(!data.turn_radius_editable);
        assert_eq!(data.content_editable, Some(true));
        assert!(data.has_active_route_index && data.active_route_index.is_none());
        assert_eq!(
            data.fc_status,
            Some(CatalogueLine {
                loaded: false,
                message: "Unknown".into()
            })
        );
        assert_eq!(data.pc_status, None);
        let actions = data.actions.as_ref().unwrap();
        assert_eq!(actions[0].event(), Some(r#"{"type":"CopyAsAuthored"}"#));
        assert_eq!(actions[0].label, "?");
        assert_eq!(actions[1].event(), None);
        let wp = &data.waypoints.as_ref().unwrap()[0];
        assert_eq!(
            (wp.id, wp.name.as_str(), wp.position.as_str()),
            (2, "WP", "")
        );
        assert_eq!(wp.turn_radius_nm, Some(0.5));
        assert!(RoutePanelData::parse("not json").is_none());
        assert_eq!(
            RoutePanelData::parse("[1]").unwrap().title,
            "Route Plan",
            "a non-object value reads as defaults"
        );
    }
}
