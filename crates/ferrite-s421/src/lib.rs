//! S-421 Route Planning Plugin for FerriteS100
//!
//! Features:
//! - Click to add waypoints
//! - Declared WGS84 geodesic/rhumb leg metrics
//! - S-421 export/import
//! - Route visualization
//! - Loads S-421 FC/PC catalogues independently

pub mod catalogue;
pub mod editing_style_contract;
pub mod haversine;
pub mod navigation;
pub mod route;
pub mod s421;
pub mod ui;

use std::path::PathBuf;

use catalogue::{CatalogueManager, CatalogueStatus};

use abi_stable::std_types::{ROption, RStr, RString, RVec};
use serde::{Deserialize, Serialize};

use ferrite_plugin_api::{
    DrawingInstruction, HostApi, MouseButton, MouseEvent, Plugin, Position, SettingsItem,
};

use route::{Route, Waypoint};

/// Plugin ID (UUID)
pub const PLUGIN_ID: &str = "com.ferrite.route-planner";
pub const PLUGIN_NAME: &str = "Route Planner";
pub const PLUGIN_VERSION: &str = "0.1.0";

/// Route planner plugin state
pub struct RoutePlugin {
    /// All routes
    routes: Vec<Route>,
    imported_sources: std::collections::HashMap<u32, s421::ImportedDataset>,
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

/// Default PLRTE color from S-421 PC (Day palette): RGB(214, 63, 36) = #D63F24
const DEFAULT_PLRTE_COLOR: u32 = 0xD63F24FF;

impl Default for RouteSettings {
    fn default() -> Self {
        Self {
            show_distance_labels: true,
            show_waypoint_numbers: true,
            show_bearings: false,
            distance_unit: DistanceUnit::NauticalMiles,
            // PLRTE color from S-421 PC ColorProfile (Day palette)
            line_color: DEFAULT_PLRTE_COLOR,
            line_width: 2.0,
            waypoint_symbol: "RTEWPT01".to_string(),
        }
    }
}

impl RoutePlugin {
    pub fn new() -> Self {
        Self::with_catalogues(PathBuf::from("./Catalogues"))
    }

    pub fn with_catalogues(path: PathBuf) -> Self {
        let mut catalogue = CatalogueManager::new(path);
        // Try to load catalogues on creation
        let _ = catalogue.load_all();

        Self {
            routes: Vec::new(),
            imported_sources: std::collections::HashMap::new(),
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

    /// Private host transaction workspace. Retains original imported lexical bytes,
    /// but never replays HostApi callbacks while a candidate is being prepared.
    pub fn fork_native_transaction(&self) -> Self {
        Self {
            routes: self.routes.clone(),
            imported_sources: self.imported_sources.clone(),
            active_route_index: self.active_route_index,
            next_route_id: self.next_route_id,
            panel_visible: self.panel_visible,
            rendering_enabled: self.rendering_enabled,
            editing: self.editing,
            host_api: None,
            settings: self.settings.clone(),
            catalogue: CatalogueManager {
                base_path: self.catalogue.base_path.clone(),
                fc: self.catalogue.fc.clone(),
                pc: self.catalogue.pc.clone(),
                fc_status: self.catalogue.fc_status.clone(),
                pc_status: self.catalogue.pc_status.clone(),
            },
        }
    }
    /// Checked local authoring API. Never silently edits an imported source.
    pub fn begin_native_local_route(&mut self) -> Result<u32, String> {
        if self.editing {
            return Err("A local route is already being edited".into());
        }
        let index = self.create_new_route()?;
        self.panel_visible = true;
        self.rendering_enabled = true;
        self.editing = true;
        Ok(self.routes[index].id)
    }
    pub fn append_native_local_waypoint(&mut self, lon: f64, lat: f64) -> Result<u32, String> {
        if !self.editing || !self.panel_visible || !self.rendering_enabled {
            return Err("Local route editor is inactive".into());
        }
        let route = self.active_route().ok_or("No active local route")?;
        if self.imported_sources.contains_key(&route.id) {
            return Err("Imported source requires a separate authored copy".into());
        }
        let total = self
            .routes
            .iter()
            .try_fold(0usize, |n, r| n.checked_add(r.len()))
            .ok_or("Native waypoint total overflow")?;
        if total >= 16_384 {
            return Err("Native aggregate waypoint limit exceeded".into());
        }
        if route
            .waypoints
            .last()
            .is_some_and(|p| (p.lon - lon).abs() > 180.0)
        {
            return Err("Undeclared cross-antimeridian editing leg unsupported".into());
        }
        self.add_waypoint_checked(lon, lat).map(|(id, _)| id)
    }
    pub fn finish_native_local_route(&mut self) -> Result<(), String> {
        let route = self.active_route().ok_or("No active local route")?;
        if !self.editing || self.imported_sources.contains_key(&route.id) {
            return Err("No local edit in progress".into());
        }
        if route.len() < 2 {
            return Err("Route requires at least two waypoints".into());
        }
        self.editing = false;
        Ok(())
    }

    /// Set an explicit local waypoint turn radius in nautical miles. The same
    /// published decimal parser used by import/export validates the lexical form
    /// before f64 conversion; no epsilon acceptance, rounding, or guessed default.
    pub fn set_native_turn_radius(&mut self, id: u32, radius_nm: &str) -> Result<(), String> {
        let route = self.active_route().ok_or("No active route")?;
        if self.imported_sources.contains_key(&route.id) {
            return Err("Imported waypoint is read-only; create a separate authored route".into());
        }
        let index = route
            .waypoints
            .iter()
            .position(|w| w.id == id)
            .ok_or("Unknown waypoint")?;
        if radius_nm.len() > 64 {
            return Err("Turn radius input exceeds supported decimal length".into());
        }
        let radius = s421::published_radius(radius_nm)?;
        self.active_route_mut().unwrap().waypoints[index].turn_radius = Some(radius);
        Ok(())
    }

    /// Change an authored incoming leg without relabeling a retained producer source.
    pub fn set_native_leg_geometry(
        &mut self,
        id: u32,
        geometry: s421::LegGeometry,
    ) -> Result<(), String> {
        let route = self.active_route().ok_or("No active route")?;
        if self.imported_sources.contains_key(&route.id) {
            return Err(
                "Imported leg geometry is read-only; create a separate authored route".into(),
            );
        }
        let index = route
            .waypoints
            .iter()
            .position(|w| w.id == id)
            .ok_or("Unknown waypoint")?;
        if index == 0 {
            return Err("First waypoint has no incoming leg".into());
        }
        let from = &route.waypoints[index - 1];
        let to = &route.waypoints[index];
        navigation::evaluate_leg([from.lon, from.lat], [to.lon, to.lat], geometry)?;
        self.active_route_mut().unwrap().waypoints[index].incoming_geometry = Some(geometry);
        Ok(())
    }

    /// Native controller access; no DLL loader needed. Editing and panel visibility are distinct.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
    pub fn panel_visible(&self) -> bool {
        self.panel_visible
    }
    pub fn rendering_enabled(&self) -> bool {
        self.rendering_enabled
    }
    pub fn editing(&self) -> bool {
        self.editing
    }
    pub fn set_editing(&mut self, editing: bool) {
        self.editing = editing;
    }
    pub fn source_for_route(&self, id: u32) -> Option<&s421::ImportedDataset> {
        self.imported_sources.get(&id)
    }
    /// Parse and preflight entirely before changing the active route or counters.
    pub fn import_xml_atomic(&mut self, xml: &str) -> Result<u32, String> {
        let source = s421::import_dataset(xml)?;
        if source.routes.len() != 1 {
            return Err("Native route UI requires one Route member".into());
        }
        let retained = self
            .imported_sources
            .values()
            .try_fold(xml.len(), |n, s| n.checked_add(s.original_xml.len()))
            .ok_or("Retained route source byte overflow")?;
        if self.routes.len() >= 128 || retained > 32 * 1024 * 1024 {
            return Err("Native route receiver retention policy exceeded".into());
        }
        let id = self.next_route_id;
        let next = id.checked_add(1).ok_or("Route counter exhausted")?;
        let mut route = source.routes[0].route.clone();
        route.id = id;
        self.routes.push(route);
        self.imported_sources.insert(id, source);
        self.next_route_id = next;
        self.active_route_index = Some(self.routes.len() - 1);
        self.request_redraw();
        Ok(id)
    }
    pub fn export_active_published(
        &self,
        metadata: s421::PublishedExport<'_>,
    ) -> Result<String, String> {
        let route = self.active_route().ok_or("No active route")?;
        if self
            .source_for_route(route.id)
            .is_some_and(|source| source.profile == s421::Profile::Candidate2)
        {
            return Err(
                "CDV2 export unsupported; original XML retained without relabeling as published1"
                    .into(),
            );
        }
        if self.source_for_route(route.id).is_some() {
            return Err("Imported published route cannot be regenerated losslessly; save retained original XML instead".into());
        }
        s421::export_published(route, metadata)
    }

    /// Save an imported dataset byte-for-byte, without changing product edition,
    /// identity, extensions, coordinate lexical forms, or declared curve controls.
    /// A changed host projection must never masquerade as the retained original.
    pub fn export_active_original(&self) -> Result<String, String> {
        let route = self.active_route().ok_or("No active route")?;
        let source = self
            .source_for_route(route.id)
            .ok_or("Active route has no retained original")?;
        let imported = source
            .routes
            .first()
            .ok_or("Retained dataset has no route")?;
        if source.routes.len() != 1 {
            return Err("Original export requires the one-route import subset".into());
        }
        let mut expected = imported.route.clone();
        expected.id = route.id;
        let before = serde_json::to_vec(&expected).map_err(|e| e.to_string())?;
        let after = serde_json::to_vec(route).map_err(|e| e.to_string())?;
        if before != after {
            return Err("Imported route was modified; refusing to save stale original or discard source fields".into());
        }
        Ok(source.original_xml.to_string())
    }

    fn export_active_local(&self) -> Result<String, String> {
        let route = self.active_route().ok_or("No active route")?;
        if self.source_for_route(route.id).is_some() {
            return self.export_active_original();
        }
        self.export_active_published(s421::PublishedExport {
            route_id: &format!("FERRITE.ROUTE.{}", route.id),
            edition: 1,
            status: 1,
        })
    }
    /// Get active route (immutable)
    pub fn active_route(&self) -> Option<&Route> {
        self.active_route_index.and_then(|i| self.routes.get(i))
    }

    /// Get active route (mutable)
    fn active_route_mut(&mut self) -> Option<&mut Route> {
        self.active_route_index.and_then(|i| self.routes.get_mut(i))
    }

    /// Create new route and set as active
    fn create_new_route(&mut self) -> Result<usize, String> {
        if self.routes.len() >= 128 {
            return Err("Native route receiver route limit exceeded".into());
        }
        let id = self.next_route_id;
        let next = id.checked_add(1).ok_or("Route counter exhausted")?;
        // Find the next available route number (not ID) for display name
        let route_number = self.find_next_route_number();
        let route = Route::with_name(id, &format!("Route {}", route_number));
        self.routes.push(route);
        let index = self.routes.len() - 1;
        self.active_route_index = Some(index);
        self.next_route_id = next;
        Ok(index)
    }

    fn add_waypoint_checked(&mut self, lon: f64, lat: f64) -> Result<(u32, usize), String> {
        if !lon.is_finite()
            || !lat.is_finite()
            || !(-180.0..=180.0).contains(&lon)
            || !(-90.0..=90.0).contains(&lat)
        {
            return Err("Invalid clicked waypoint position".into());
        }
        let route = self.active_route_mut().ok_or("No active route")?;
        if route.waypoints.len() >= 16_384 {
            return Err("Native route receiver waypoint limit exceeded".into());
        }
        let id = route.next_id().ok_or("Waypoint counter exhausted")?;
        route.add_waypoint(Waypoint::new(id, lon, lat));
        Ok((id, route.waypoints.len()))
    }

    /// Find the next available route number for naming
    fn find_next_route_number(&self) -> u32 {
        // Extract existing route numbers from names like "Route 1", "Route 2", etc.
        let mut used_numbers: Vec<u32> = self
            .routes
            .iter()
            .filter_map(|r| {
                r.name.as_ref().and_then(|name| {
                    if let Some(number) = name.strip_prefix("Route ") {
                        number.parse::<u32>().ok()
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
        // Only consume clicks when panel is visible, rendering is enabled, and in editing mode
        if !self.panel_visible || !self.rendering_enabled || !self.editing {
            return false;
        }

        self.log(&format!(
            "Mouse click received at ({:.6}, {:.6})",
            event.position.lon, event.position.lat
        ));

        match event.button {
            MouseButton::Left => {
                // Add waypoint to active route
                let result = self.add_waypoint_checked(event.position.lon, event.position.lat);
                if let Err(error) = &result {
                    self.log(error);
                }
                let result = result.ok();

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

        // Get PLRTE color from PC (fallback to default if not loaded)
        let plrte_color = self.catalogue.get_color_rgba("PLRTE").unwrap_or(0xD63F24FF);
        // Get APLRT (alternate route) color for inactive routes
        let inactive_color = self.catalogue.get_color_rgba("CHGRF").unwrap_or(0x888888FF);

        // Draw all routes
        for (route_idx, route) in self.routes.iter().enumerate() {
            if route.waypoints.is_empty() {
                continue;
            }

            let is_active = self.active_route_index == Some(route_idx);
            // Use PLRTE for active route line, or settings color if customized
            let line_color = if is_active {
                // Use PC PLRTE color (from settings which defaults to PC value)
                self.settings.line_color
            } else {
                inactive_color
            };
            let marker_color = if is_active {
                plrte_color
            } else {
                inactive_color
            };

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
                    let Some(geometry) = wp2.incoming_geometry else {
                        continue;
                    };
                    let Ok(metric) =
                        navigation::evaluate_leg([wp1.lon, wp1.lat], [wp2.lon, wp2.lat], geometry)
                    else {
                        continue;
                    };
                    let distance = metric.distance_nm;

                    // Position label at midpoint
                    let [mid_lon, mid_lat] = metric.midpoint;

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
            // Retained producer datasets are read-only. Route deletion remains
            // an explicit unload; changes to their content require an authored copy.
            let changes_imported = match &event {
                ui::UiEvent::DeleteWaypoint { .. } | ui::UiEvent::RenameWaypoint { .. } => self
                    .active_route()
                    .is_some_and(|r| self.imported_sources.contains_key(&r.id)),
                ui::UiEvent::RenameRoute { id, .. } => self.imported_sources.contains_key(id),
                _ => false,
            };
            if changes_imported {
                self.log("Imported source is read-only; create a separate authored route");
                return;
            }
            match event {
                ui::UiEvent::New => {
                    // Create new route and enter editing mode
                    let index = match self.create_new_route() {
                        Ok(index) => index,
                        Err(error) => {
                            self.log(&error);
                            return;
                        }
                    };
                    self.editing = true;
                    self.log(&format!(
                        "Started new route {} - click on chart to add waypoints",
                        index + 1
                    ));
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
                    self.imported_sources.clear();
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
                        let removed = self.routes.remove(pos);
                        self.imported_sources.remove(&removed.id);
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
                ui::UiEvent::SetTurnRadius { id, radius_nm } => {
                    match self.set_native_turn_radius(id, &radius_nm) {
                        Ok(()) => self.request_redraw(),
                        Err(error) => self.log(&error),
                    }
                }
                ui::UiEvent::SetLegGeometry { id, geometry } => {
                    match self.set_native_leg_geometry(id, geometry) {
                        Ok(()) => self.request_redraw(),
                        Err(error) => self.log(&error),
                    }
                }
                ui::UiEvent::ToggleRendering => {
                    self.rendering_enabled = !self.rendering_enabled;
                    // Turn off editing mode when rendering is disabled
                    if !self.rendering_enabled {
                        self.editing = false;
                    }
                    self.log(&format!(
                        "Route rendering: {}",
                        if self.rendering_enabled { "ON" } else { "OFF" }
                    ));
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

        // Log catalogue status
        if let Some(ref pc) = self.catalogue.pc {
            self.log(&format!(
                "S-421 PC loaded: {} colors, {} line styles, {} symbols",
                pc.colors.len(),
                pc.line_styles.len(),
                pc.symbols.len()
            ));
        }

        // Load saved settings
        if let Some(ref api) = self.host_api {
            if let Some(config) = api.load_plugin_config(PLUGIN_ID) {
                if let Ok(settings) = serde_json::from_str::<RouteSettings>(&config) {
                    self.settings = settings;
                    self.log("Loaded saved settings");
                }
            }
        }

        // Update default line color from PC if not customized (uses old wrong color)
        if self.settings.line_color == 0xFF6600FF {
            if let Some(plrte) = self.catalogue.get_color_rgba("PLRTE") {
                self.settings.line_color = plrte;
                self.log("Updated line color from PC PLRTE");
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
        if self.active_route().is_some() {
            match self.export_active_local() {
                Ok(xml) => ROption::RSome(RVec::from(xml.into_bytes())),
                Err(_) => ROption::RNone,
            }
        } else {
            ROption::RNone
        }
    }

    fn import_data(&mut self, data: RStr<'_>) -> bool {
        match self.import_xml_atomic(data.as_str()) {
            Ok(_) => true,
            Err(e) => {
                self.log(&format!("Import failed: {e}"));
                false
            }
        }
    }

    fn clear(&mut self) {
        self.routes.clear();
        self.imported_sources.clear();
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
            match self.export_active_local() {
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
            match self.import_xml_atomic(&content) {
                Ok(id) => {
                    let count = self
                        .routes
                        .iter()
                        .find(|r| r.id == id)
                        .map_or(0, Route::len);
                    if let Some(api) = &self.host_api {
                        api.toast(&format!("Imported route with {} waypoints", count));
                    }
                }
                Err(e) => {
                    if let Some(api) = &self.host_api {
                        api.toast_error(&format!("Import failed: {}", e));
                    }
                }
            }
        }
    }
}

/// Native code may use this controller directly; external ABI wrapper remains separate.
pub type RouteController = RoutePlugin;
pub type RoutePlanner = RoutePlugin;
impl Default for RoutePlugin {
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(test)]
mod native_import_transaction_tests {
    use super::*;
    #[test]
    fn explicit_radius_uses_published_decimal_bounds_and_preserves_failed_state() {
        let mut c = RouteController::with_catalogues(PathBuf::from("/nonexistent/s421-test"));
        assert!(c.set_native_turn_radius(1, "0.25").is_err());
        c.begin_native_local_route().unwrap();
        let id = c.append_native_local_waypoint(25., 59.).unwrap();
        for (text, expected) in [
            ("0", 0.0_f64),
            ("+.25", 0.25),
            ("5.0000", 5.),
            ("-0.00", -0.),
        ] {
            c.set_native_turn_radius(id, text).unwrap();
            assert_eq!(
                c.active_route().unwrap().waypoints[0]
                    .turn_radius
                    .unwrap()
                    .to_bits(),
                expected.to_bits()
            );
        }
        c.set_native_turn_radius(id, "0.25").unwrap();
        let before = serde_json::to_vec(c.active_route().unwrap()).unwrap();
        for invalid in [
            "5.0000000000000001",
            "0.001",
            "5.01",
            "-0.01",
            "1e0",
            "NaN",
            "inf",
            "-inf",
            ".",
            "",
            " 0.25",
            "0.25 ",
        ] {
            assert!(c.set_native_turn_radius(id, invalid).is_err(), "{invalid}");
            assert_eq!(
                serde_json::to_vec(c.active_route().unwrap()).unwrap(),
                before
            );
        }
        assert!(c.set_native_turn_radius(id, &"0".repeat(65)).is_err());
        assert!(c.set_native_turn_radius(u32::MAX, "0.25").is_err());
        assert_eq!(
            serde_json::to_vec(c.active_route().unwrap()).unwrap(),
            before
        );
        let event = serde_json::to_string(&ui::UiEvent::SetTurnRadius {
            id,
            radius_nm: "0.5".into(),
        })
        .unwrap();
        c.handle_ui_event(RStr::from(event.as_str()));
        assert_eq!(
            c.active_route().unwrap().waypoints[0].turn_radius,
            Some(0.5)
        );
        let event = serde_json::to_string(&ui::UiEvent::SetTurnRadius {
            id,
            radius_nm: "0.501".into(),
        })
        .unwrap();
        c.handle_ui_event(RStr::from(event.as_str()));
        assert_eq!(
            c.active_route().unwrap().waypoints[0].turn_radius,
            Some(0.5)
        );
    }
    #[test]
    fn imported_content_mutation_events_and_radius_are_rejected_without_state_changes() {
        for xml in [
            include_str!("../tests/fixtures/v1-GMIN.gml"),
            include_str!("../tests/fixtures/v2-GBASIC.gml"),
        ] {
            let mut c = RouteController::with_catalogues(PathBuf::from("/nonexistent/s421-test"));
            let route_id = c.import_xml_atomic(xml).unwrap();
            let id = c.active_route().unwrap().waypoints[0].id;
            let before = serde_json::to_vec(c.active_route().unwrap()).unwrap();
            assert!(c.set_native_turn_radius(id, "0.25").is_err());
            for event in [
                ui::UiEvent::DeleteWaypoint { id },
                ui::UiEvent::RenameWaypoint {
                    id,
                    name: "Changed waypoint".into(),
                },
                ui::UiEvent::RenameRoute {
                    id: route_id,
                    name: "Changed route".into(),
                },
                ui::UiEvent::SetTurnRadius {
                    id,
                    radius_nm: "0.25".into(),
                },
            ] {
                let json = serde_json::to_string(&event).unwrap();
                c.handle_ui_event(RStr::from(json.as_str()));
                assert_eq!(
                    serde_json::to_vec(c.active_route().unwrap()).unwrap(),
                    before
                );
                assert_eq!(c.export_active_original().unwrap(), xml);
            }
            // A retained route cannot be renamed by ID while another local route is active.
            c.begin_native_local_route().unwrap();
            let json = serde_json::to_string(&ui::UiEvent::RenameRoute {
                id: route_id,
                name: "Changed route".into(),
            })
            .unwrap();
            c.handle_ui_event(RStr::from(json.as_str()));
            let imported = c.routes.iter().find(|r| r.id == route_id).unwrap();
            assert_eq!(serde_json::to_vec(imported).unwrap(), before);
        }
    }

    #[test]
    fn imported_save_preserves_exact_source_and_rejects_changed_host_model() {
        for xml in [
            include_str!("../tests/fixtures/v1-GMIN.gml"),
            include_str!("../tests/fixtures/v2-GBASIC.gml"),
        ] {
            let mut c = RouteController::with_catalogues(PathBuf::from("/nonexistent/s421-test"));
            c.import_xml_atomic(xml).unwrap();
            assert_eq!(
                c.export_active_original().unwrap().as_bytes(),
                xml.as_bytes()
            );
            assert_eq!(c.export_active_local().unwrap().as_bytes(), xml.as_bytes());
            let reopened = s421::import_dataset(&c.export_active_local().unwrap()).unwrap();
            assert_eq!(
                reopened.profile,
                c.source_for_route(c.active_route().unwrap().id)
                    .unwrap()
                    .profile
            );
            assert!(c
                .export_active_published(s421::PublishedExport {
                    route_id: "REWRITTEN.ROUTE",
                    edition: 1,
                    status: 1,
                })
                .is_err());
            c.active_route_mut().unwrap().waypoints[0].lon += 0.000001;
            assert!(c.export_active_original().unwrap_err().contains("modified"));
            assert!(c.export_active_local().is_err());
        }
    }

    #[test]
    fn authored_geometry_choice_updates_metrics_and_failed_choices_preserve_state() {
        use s421::LegGeometry;
        let mut c = RouteController::with_catalogues(PathBuf::from("/nonexistent/s421-test"));
        c.begin_native_local_route().unwrap();
        let first = c.append_native_local_waypoint(-30., 70.).unwrap();
        let second = c.append_native_local_waypoint(30., 70.).unwrap();
        assert_eq!(c.active_route().unwrap().total_distance(), None);
        assert!(c
            .set_native_leg_geometry(first, LegGeometry::Orthodrome)
            .is_err());
        assert!(c
            .set_native_leg_geometry(u32::MAX, LegGeometry::Orthodrome)
            .is_err());
        assert_eq!(c.active_route().unwrap().total_distance(), None);
        c.set_native_leg_geometry(second, LegGeometry::Orthodrome)
            .unwrap();
        assert!(
            (c.active_route().unwrap().total_distance().unwrap() - 1187.2212872068706).abs() < 1e-8
        );
        c.set_native_leg_geometry(second, LegGeometry::Loxodrome)
            .unwrap();
        assert!(
            (c.active_route().unwrap().total_distance().unwrap() - 1237.1449657237144).abs() < 1e-8
        );
        // A solver rejection must not overwrite the existing declaration.
        c.active_route_mut().unwrap().waypoints[1].lon = 150.;
        assert!(c
            .set_native_leg_geometry(second, LegGeometry::Orthodrome)
            .is_err());
        assert_eq!(
            c.active_route().unwrap().waypoints[1].incoming_geometry,
            Some(LegGeometry::Loxodrome)
        );
        assert_eq!(c.active_route().unwrap().total_distance(), None);
    }
    #[test]
    fn malformed_import_preserves_routes_counter_active_and_editing() {
        let mut controller =
            RouteController::with_catalogues(PathBuf::from("/nonexistent/s421-test"));
        controller.set_editing(true);
        let before = controller.next_route_id;
        assert!(controller.import_xml_atomic("<bad/>").is_err());
        assert!(controller.routes().is_empty());
        assert_eq!(controller.next_route_id, before);
        assert!(controller.editing());
        assert!(!controller.panel_visible());
    }

    #[test]
    fn create_click_caps_and_exhausted_ids_do_not_mutate() {
        let mut c = RoutePlugin::with_catalogues(PathBuf::from("/nonexistent/ferrite-s421-unit"));
        c.next_route_id = u32::MAX;
        assert!(c.create_new_route().is_err());
        assert!(c.routes.is_empty());
        assert_eq!(c.next_route_id, u32::MAX);
        c.next_route_id = 1;
        c.create_new_route().unwrap();
        assert!(c.add_waypoint_checked(f64::NAN, 0.0).is_err());
        assert!(c.active_route().unwrap().waypoints.is_empty());
        c.active_route_mut().unwrap().set_next_id(u32::MAX);
        assert!(c.add_waypoint_checked(25.0, 59.0).is_err());
        assert!(c.active_route().unwrap().waypoints.is_empty());
        c.active_route_mut().unwrap().set_next_id(1);
        for _ in 0..16_384 {
            c.add_waypoint_checked(25.0, 59.0).unwrap();
        }
        let before = c.active_route().unwrap().waypoints.len();
        assert!(c.add_waypoint_checked(25.0, 59.0).is_err());
        assert_eq!(c.active_route().unwrap().waypoints.len(), before);
        for id in 2..=128 {
            c.routes.push(Route::new(id));
        }
        let before = c.next_route_id;
        assert!(c.create_new_route().is_err());
        assert_eq!(c.next_route_id, before);
        assert_eq!(c.routes.len(), 128);
    }
}
