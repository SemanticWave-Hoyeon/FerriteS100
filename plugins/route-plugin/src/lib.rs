//! S-421 Route Planning Plugin for FerriteS100
//!
//! Features:
//! - Click to add waypoints
//! - Haversine distance calculation
//! - S-421 export/import
//! - Route visualization
//! - Loads S-421 FC/PC catalogues independently

mod catalogue;
mod haversine;
mod route;
mod s421;
mod ui;

use std::path::PathBuf;
use std::sync::OnceLock;

use catalogue::{CatalogueManager, CatalogueStatus};

use abi_stable::{
    sabi_trait::TD_Opaque,
    std_types::{RBox, ROption, RStr, RString, RVec},
};
use serde::{Deserialize, Serialize};

use ferrite_plugin_api::{
    DrawingInstruction, HostApi, MouseButton, MouseEvent, Plugin, PluginMetadata,
    PluginModule, Plugin_TO, Position, SettingsItem, PLUGIN_API_VERSION,
};

use route::{Route, Waypoint};

/// Plugin ID (UUID)
const PLUGIN_ID: &str = "com.ferrite.route-planner";
const PLUGIN_NAME: &str = "Route Planner";
const PLUGIN_VERSION: &str = "0.1.0";

/// Route planner plugin state
pub struct RoutePlugin {
    /// All routes
    routes: Vec<Route>,
    /// Index of active route (for editing/export)
    active_route_index: Option<usize>,
    /// Next route ID counter
    next_route_id: u32,
    /// Is panel visible (toggled by toolbar button)
    panel_visible: bool,
    /// Is rendering enabled (toggled in panel UI)
    rendering_enabled: bool,
    /// Is in editing mode (adding waypoints)
    editing: bool,
    /// Host API for interacting with host application
    host_api: Option<HostApi>,
    /// Plugin settings
    settings: RouteSettings,
    /// S-421 Catalogue manager
    catalogue: CatalogueManager,
}

/// Plugin settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSettings {
    /// Show distance labels on legs
    pub show_distance_labels: bool,
    /// Show waypoint numbers
    pub show_waypoint_numbers: bool,
    /// Show bearing on legs
    pub show_bearings: bool,
    /// Distance unit
    pub distance_unit: DistanceUnit,
    /// Line color (RGBA)
    pub line_color: u32,
    /// Line width
    pub line_width: f32,
    /// Waypoint symbol
    pub waypoint_symbol: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DistanceUnit {
    NauticalMiles,
    Kilometers,
    StatuteMiles,
}

impl Default for RouteSettings {
    fn default() -> Self {
        Self {
            show_distance_labels: true,
            show_waypoint_numbers: true,
            show_bearings: false,
            distance_unit: DistanceUnit::NauticalMiles,
            line_color: 0xFF6600FF, // Orange, full alpha
            line_width: 2.0,
            waypoint_symbol: "RTEWPT01".to_string(),
        }
    }
}

impl RoutePlugin {
    pub fn new() -> Self {
        let mut catalogue = CatalogueManager::new(PathBuf::from("./Catalogues"));
        // Try to load catalogues on creation
        let _ = catalogue.load_all();

        Self {
            routes: Vec::new(),
            active_route_index: None,
            next_route_id: 1,
            panel_visible: false,
            rendering_enabled: true, // Rendering on by default
            editing: false,
            host_api: None,
            settings: RouteSettings::default(),
            catalogue,
        }
    }

    /// Get next route ID and increment
    fn next_route_id(&mut self) -> u32 {
        let id = self.next_route_id;
        self.next_route_id += 1;
        id
    }

    /// Get active route (immutable)
    fn active_route(&self) -> Option<&Route> {
        self.active_route_index.and_then(|i| self.routes.get(i))
    }

    /// Get active route (mutable)
    fn active_route_mut(&mut self) -> Option<&mut Route> {
        self.active_route_index.and_then(|i| self.routes.get_mut(i))
    }

    /// Create new route and set as active
    fn create_new_route(&mut self) -> usize {
        let id = self.next_route_id();
        // Find the next available route number (not ID) for display name
        let route_number = self.find_next_route_number();
        let route = Route::with_name(id, &format!("Route {}", route_number));
        self.routes.push(route);
        let index = self.routes.len() - 1;
        self.active_route_index = Some(index);
        index
    }

    /// Find the next available route number for naming
    fn find_next_route_number(&self) -> u32 {
        // Extract existing route numbers from names like "Route 1", "Route 2", etc.
        let mut used_numbers: Vec<u32> = self
            .routes
            .iter()
            .filter_map(|r| {
                r.name.as_ref().and_then(|name| {
                    if name.starts_with("Route ") {
                        name[6..].parse::<u32>().ok()
                    } else {
                        None
                    }
                })
            })
            .collect();
        used_numbers.sort();

        // Find the first gap or use next number
        let mut next = 1;
        for num in used_numbers {
            if num == next {
                next += 1;
            } else if num > next {
                break;
            }
        }
        next
    }

    /// Get FC status for UI
    pub fn fc_status(&self) -> &CatalogueStatus {
        &self.catalogue.fc_status
    }

    /// Get PC status for UI
    pub fn pc_status(&self) -> &CatalogueStatus {
        &self.catalogue.pc_status
    }

    /// Reload catalogues
    pub fn reload_catalogues(&mut self) {
        let (fc_result, pc_result) = self.catalogue.load_all();
        if let Err(e) = fc_result {
            self.log(&format!("FC load error: {}", e));
        }
        if let Err(e) = pc_result {
            self.log(&format!("PC load error: {}", e));
        }
    }

    fn log(&self, msg: &str) {
        if let Some(ref api) = self.host_api {
            api.info(msg);
        }
    }

    fn request_redraw(&self) {
        if let Some(ref api) = self.host_api {
            api.redraw_chart();
            api.refresh_ui();
        }
    }
}

impl Plugin for RoutePlugin {
    fn id(&self) -> RStr<'_> {
        RStr::from(PLUGIN_ID)
    }

    fn name(&self) -> RStr<'_> {
        RStr::from(PLUGIN_NAME)
    }

    fn version(&self) -> RStr<'_> {
        RStr::from(PLUGIN_VERSION)
    }

    fn toolbar_label(&self) -> ROption<RStr<'_>> {
        ROption::RSome(RStr::from("Route"))
    }

    fn toolbar_tooltip(&self) -> ROption<RStr<'_>> {
        ROption::RSome(RStr::from("S-421 Route Planning - Click to add waypoints"))
    }

    fn is_active(&self) -> bool {
        // Return panel visibility state (for toolbar toggle)
        // Drawing instructions are collected from all plugins, not just active ones
        self.panel_visible
    }

    fn set_active(&mut self, active: bool) {
        // Toolbar button toggles panel visibility, not rendering
        self.panel_visible = active;
        self.log(&format!("Route plugin panel visible: {}", active));
        self.request_redraw();
    }

    fn on_mouse_click(&mut self, event: MouseEvent) -> bool {
        // Only consume clicks when panel is visible and in editing mode
        if !self.panel_visible || !self.editing {
            return false;
        }

        self.log(&format!(
            "Mouse click received at ({:.6}, {:.6})",
            event.position.lon, event.position.lat
        ));

        match event.button {
            MouseButton::Left => {
                // Add waypoint to active route
                let result = if let Some(route) = self.active_route_mut() {
                    let wp_id = route.next_id();
                    let wp = Waypoint::new(wp_id, event.position.lon, event.position.lat);
                    route.add_waypoint(wp);
                    Some((wp_id, route.waypoints.len()))
                } else {
                    None
                };

                if let Some((wp_id, total)) = result {
                    self.log(&format!(
                        "Added waypoint {} at ({:.6}, {:.6}), total waypoints: {}",
                        wp_id, event.position.lon, event.position.lat, total
                    ));
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
            MouseButton::Right => {
                // Remove last waypoint or exit editing mode
                let removed = if let Some(route) = self.active_route_mut() {
                    route.remove_last_waypoint().is_some()
                } else {
                    false
                };

                if removed {
                    self.log("Removed last waypoint");
                    self.request_redraw();
                } else if self.active_route().is_some() {
                    // No waypoints left, exit editing mode
                    self.editing = false;
                    self.log("Exited editing mode");
                    self.request_redraw();
                }
                true
            }
            _ => false,
        }
    }

    fn on_mouse_move(&mut self, _position: Position) {
        // Could show preview line to cursor position
    }

    fn get_drawing_instructions(&self) -> RVec<DrawingInstruction> {
        let mut instructions = RVec::new();

        // Don't draw if rendering is disabled
        if !self.rendering_enabled {
            return instructions;
        }

        // PLRTE color from S-421 PC: #D63F24 (red)
        const PLRTE_COLOR: u32 = 0xD63F24FF; // RGBA
        // Inactive route color (gray)
        const INACTIVE_COLOR: u32 = 0x888888FF; // RGBA

        // Draw all routes
        for (route_idx, route) in self.routes.iter().enumerate() {
            if route.waypoints.is_empty() {
                continue;
            }

            let is_active = self.active_route_index == Some(route_idx);
            let line_color = if is_active { self.settings.line_color } else { INACTIVE_COLOR };
            let marker_color = if is_active { PLRTE_COLOR } else { INACTIVE_COLOR };

            // Draw legs (lines between waypoints)
            if route.waypoints.len() >= 2 {
                let points: RVec<Position> = route
                    .waypoints
                    .iter()
                    .map(|wp| Position::new(wp.lon, wp.lat))
                    .collect();

                instructions.push(DrawingInstruction::Line {
                    points,
                    color: line_color,
                    width: self.settings.line_width,
                    priority: if is_active { 100 } else { 99 },
                });
            }

            // Draw waypoint markers
            for (i, wp) in route.waypoints.iter().enumerate() {
                instructions.push(DrawingInstruction::Circle {
                    center: Position::new(wp.lon, wp.lat),
                    radius_nm: 0.02,
                    color: marker_color,
                    width: 2.5,
                    priority: if is_active { 101 } else { 99 },
                });

                // Draw waypoint number (only for active route)
                if is_active && self.settings.show_waypoint_numbers {
                    instructions.push(DrawingInstruction::Text {
                        text: RString::from(format!("{}", i + 1)),
                        position: Position::new(wp.lon, wp.lat),
                        font_size: 12.0,
                        color: 0x000000FF,
                        priority: 102,
                    });
                }
            }

            // Draw distance labels (only for active route)
            if is_active && self.settings.show_distance_labels && route.waypoints.len() >= 2 {
                for i in 0..route.waypoints.len() - 1 {
                    let wp1 = &route.waypoints[i];
                    let wp2 = &route.waypoints[i + 1];
                    let distance = haversine::distance(wp1.lat, wp1.lon, wp2.lat, wp2.lon);

                    // Position label at midpoint
                    let mid_lon = (wp1.lon + wp2.lon) / 2.0;
                    let mid_lat = (wp1.lat + wp2.lat) / 2.0;

                    // Always show both NM and km
                    let km = distance * 1.852;
                    let label = format!("{:.1} NM ({:.1} km)", distance, km);

                    instructions.push(DrawingInstruction::Text {
                        text: RString::from(label),
                        position: Position::new(mid_lon, mid_lat),
                        font_size: 10.0,
                        color: 0x333333FF,
                        priority: 100,
                    });
                }
            }
        }

        instructions
    }

    fn get_ui_data(&self) -> RVec<u8> {
        let ui_data = ui::build_ui_data(
            &self.routes,
            self.active_route_index,
            self.editing,
            self.rendering_enabled,
            &self.settings,
            &self.catalogue.fc_status,
            &self.catalogue.pc_status,
        );
        match serde_json::to_vec(&ui_data) {
            Ok(bytes) => RVec::from(bytes),
            Err(_) => RVec::new(),
        }
    }

    fn handle_ui_event(&mut self, event_json: RStr<'_>) {
        if let Ok(event) = serde_json::from_str::<ui::UiEvent>(event_json.as_str()) {
            match event {
                ui::UiEvent::New => {
                    // Create new route and enter editing mode
                    let index = self.create_new_route();
                    self.editing = true;
                    self.log(&format!("Started new route {} - click on chart to add waypoints", index + 1));
                    self.request_redraw();
                }
                ui::UiEvent::Finish => {
                    // Exit editing mode
                    self.editing = false;
                    self.log("Finished editing route");
                    self.request_redraw();
                }
                ui::UiEvent::Clear => {
                    // Clear all routes
                    self.routes.clear();
                    self.active_route_index = None;
                    self.editing = false;
                    self.log("All routes cleared");
                    self.request_redraw();
                }
                ui::UiEvent::Export => {
                    self.export_route();
                }
                ui::UiEvent::Import => {
                    self.import_route();
                }
                ui::UiEvent::SelectWaypoint { id } => {
                    self.log(&format!("Selected waypoint {}", id));
                }
                ui::UiEvent::DeleteWaypoint { id } => {
                    if let Some(route) = self.active_route_mut() {
                        if route.remove_waypoint(id) {
                            self.log(&format!("Deleted waypoint {}", id));
                            self.request_redraw();
                        }
                    }
                }
                ui::UiEvent::SelectRoute { index } => {
                    if index < self.routes.len() {
                        self.active_route_index = Some(index);
                        self.log(&format!("Selected route {}", index + 1));
                        self.request_redraw();
                    }
                }
                ui::UiEvent::DeleteRoute { id } => {
                    if let Some(pos) = self.routes.iter().position(|r| r.id == id) {
                        self.routes.remove(pos);
                        // Adjust active index
                        if let Some(active_idx) = self.active_route_index {
                            if active_idx == pos {
                                // Active route was deleted
                                self.active_route_index = if self.routes.is_empty() {
                                    None
                                } else {
                                    Some(active_idx.min(self.routes.len() - 1))
                                };
                            } else if active_idx > pos {
                                self.active_route_index = Some(active_idx - 1);
                            }
                        }
                        self.log(&format!("Deleted route {}", id));
                        self.request_redraw();
                    }
                }
                ui::UiEvent::RenameRoute { id, name } => {
                    if let Some(route) = self.routes.iter_mut().find(|r| r.id == id) {
                        let trimmed = name.trim();
                        if !trimmed.is_empty() {
                            route.name = Some(trimmed.to_string());
                            self.log(&format!("Renamed route {} to '{}'", id, trimmed));
                            self.request_redraw();
                        }
                    }
                }
                ui::UiEvent::RenameWaypoint { id, name } => {
                    if let Some(route) = self.active_route_mut() {
                        if let Some(wp) = route.waypoints.iter_mut().find(|wp| wp.id == id) {
                            let trimmed = name.trim();
                            if !trimmed.is_empty() {
                                wp.name = Some(trimmed.to_string());
                                self.log(&format!("Renamed waypoint {} to '{}'", id, trimmed));
                                self.request_redraw();
                            }
                        }
                    }
                }
                ui::UiEvent::ToggleRendering => {
                    self.rendering_enabled = !self.rendering_enabled;
                    self.log(&format!("Route rendering: {}", if self.rendering_enabled { "ON" } else { "OFF" }));
                    self.request_redraw();
                }
            }
        }
    }

    fn get_settings_schema(&self) -> RVec<SettingsItem> {
        let mut items = RVec::new();

        items.push(SettingsItem::Bool {
            key: RString::from("show_distance_labels"),
            label: RString::from("Show Distance Labels"),
            description: RString::from("Display distance on each leg"),
            default_value: true,
        });

        items.push(SettingsItem::Bool {
            key: RString::from("show_waypoint_numbers"),
            label: RString::from("Show Waypoint Numbers"),
            description: RString::from("Display number on each waypoint"),
            default_value: true,
        });

        items.push(SettingsItem::Bool {
            key: RString::from("show_bearings"),
            label: RString::from("Show Bearings"),
            description: RString::from("Display bearing on each leg"),
            default_value: false,
        });

        items.push(SettingsItem::Choice {
            key: RString::from("distance_unit"),
            label: RString::from("Distance Unit"),
            description: RString::from("Unit for distance display"),
            options: RVec::from(vec![
                RString::from("Nautical Miles"),
                RString::from("Kilometers"),
                RString::from("Statute Miles"),
            ]),
            default_index: 0,
        });

        items.push(SettingsItem::Color {
            key: RString::from("line_color"),
            label: RString::from("Line Color"),
            description: RString::from("Color for route lines"),
            default_rgba: 0xFF6600FF,
        });

        items.push(SettingsItem::Number {
            key: RString::from("line_width"),
            label: RString::from("Line Width"),
            description: RString::from("Width of route lines in pixels"),
            min: 1.0,
            max: 10.0,
            step: 0.5,
            default_value: 2.0,
        });

        items
    }

    fn get_settings(&self) -> RVec<u8> {
        match serde_json::to_vec(&self.settings) {
            Ok(bytes) => RVec::from(bytes),
            Err(_) => RVec::new(),
        }
    }

    fn apply_settings(&mut self, settings_json: RStr<'_>) {
        if let Ok(settings) = serde_json::from_str::<RouteSettings>(settings_json.as_str()) {
            self.settings = settings;
            self.log("Settings applied");
            self.request_redraw();
        }
    }

    fn initialize(&mut self, host_api: HostApi) {
        self.host_api = Some(host_api);
        self.log("Route plugin initialized");

        // Load saved settings
        if let Some(ref api) = self.host_api {
            if let Some(config) = api.load_plugin_config(PLUGIN_ID) {
                if let Ok(settings) = serde_json::from_str::<RouteSettings>(&config) {
                    self.settings = settings;
                    self.log("Loaded saved settings");
                }
            }
        }
    }

    fn shutdown(&mut self) {
        // Save settings
        if let Some(ref api) = self.host_api {
            if let Ok(json) = serde_json::to_string(&self.settings) {
                api.save_plugin_config(PLUGIN_ID, &json);
                self.log("Settings saved");
            }
        }
        self.log("Route plugin shutdown");
    }

    fn export_data(&self) -> ROption<RVec<u8>> {
        if let Some(route) = self.active_route() {
            match s421::export_route(route) {
                Ok(xml) => ROption::RSome(RVec::from(xml.into_bytes())),
                Err(_) => ROption::RNone,
            }
        } else {
            ROption::RNone
        }
    }

    fn import_data(&mut self, data: RStr<'_>) -> bool {
        match s421::import_route(data.as_str()) {
            Ok(mut route) => {
                route.id = self.next_route_id();
                if route.name.is_none() {
                    route.name = Some(format!("Route {}", route.id));
                }
                self.log(&format!(
                    "Imported route with {} waypoints",
                    route.waypoints.len()
                ));
                self.routes.push(route);
                self.active_route_index = Some(self.routes.len() - 1);
                self.request_redraw();
                true
            }
            Err(e) => {
                self.log(&format!("Import failed: {}", e));
                false
            }
        }
    }

    fn clear(&mut self) {
        self.routes.clear();
        self.active_route_index = None;
        self.request_redraw();
    }
}

impl RoutePlugin {
    fn export_route(&self) {
        let route = match self.active_route() {
            Some(r) => r,
            None => {
                if let Some(ref api) = self.host_api {
                    api.toast_error("No active route to export");
                }
                return;
            }
        };

        if route.waypoints.is_empty() {
            if let Some(ref api) = self.host_api {
                api.toast_error("No waypoints to export");
            }
            return;
        }

        if let Some(ref api) = self.host_api {
            match s421::export_route(route) {
                Ok(xml) => {
                    let default_name = route.name.as_deref().unwrap_or("route");
                    let file_name = format!("{}.gml", default_name.replace(' ', "_"));
                    let filter = ferrite_plugin_api::FileFilter {
                        name: RString::from("S-421 Route Files"),
                        extensions: RVec::from(vec![RString::from("gml")]),
                    };
                    if api.save_file(filter, &file_name, &xml) {
                        api.toast("Route exported successfully");
                    }
                }
                Err(e) => {
                    api.toast_error(&format!("Export failed: {}", e));
                }
            }
        }
    }

    fn import_route(&mut self) {
        // Get file content first (requires immutable borrow of host_api)
        let content = if let Some(ref api) = self.host_api {
            let filter = ferrite_plugin_api::FileFilter {
                name: RString::from("S-421 Route Files"),
                extensions: RVec::from(vec![RString::from("gml"), RString::from("xml")]),
            };
            api.open_file(filter)
        } else {
            None
        };

        // Now import data (requires mutable borrow of self)
        if let Some(content) = content {
            match s421::import_route(&content) {
                Ok(mut route) => {
                    // Assign proper ID
                    route.id = self.next_route_id();
                    if route.name.is_none() {
                        route.name = Some(format!("Route {}", route.id));
                    }
                    let count = route.waypoints.len();
                    self.routes.push(route);
                    self.active_route_index = Some(self.routes.len() - 1);

                    // Toast notification
                    if let Some(ref api) = self.host_api {
                        api.toast(&format!("Imported route with {} waypoints", count));
                    }
                    self.request_redraw();
                }
                Err(e) => {
                    if let Some(ref api) = self.host_api {
                        api.toast_error(&format!("Import failed: {}", e));
                    }
                }
            }
        }
    }
}

// Plugin module export

static PLUGIN_MODULE: OnceLock<PluginModule> = OnceLock::new();

fn init_plugin_module() -> PluginModule {
    PluginModule {
        create_plugin,
        api_version: PLUGIN_API_VERSION,
        min_host_version: RString::from("0.2.0"),
        metadata: PluginMetadata {
            id: RString::from(PLUGIN_ID),
            name: RString::from(PLUGIN_NAME),
            version: RString::from(PLUGIN_VERSION),
            author: RString::from("FerriteS100 Team"),
            description: RString::from("S-421 Route Planning - Add waypoints, measure distances, export routes"),
        },
    }
}

extern "C" fn create_plugin() -> Plugin_TO<'static, RBox<()>> {
    Plugin_TO::from_value(RoutePlugin::new(), TD_Opaque)
}

#[no_mangle]
pub extern "C" fn get_plugin_module() -> &'static PluginModule {
    PLUGIN_MODULE.get_or_init(init_plugin_module)
}
