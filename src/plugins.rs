//! Plugin system integration for FerriteS100
//!
//! This module handles plugin loading, management, and UI integration.

#![allow(dead_code)] // Some methods/fields reserved for future use

use std::path::PathBuf;

use tracing::{debug, info, warn};

use abi_stable::std_types::RStr;
use ferrite_plugin_api::{DrawingInstruction, GeoBounds, MouseButton, MouseEvent, Position};
use ferrite_plugin_loader::{PluginManager, SharedHostContext};

/// Plugin system state
pub struct PluginSystem {
    /// Plugin manager
    manager: PluginManager,
    /// Shared host context
    host_context: SharedHostContext,
}

impl PluginSystem {
    /// Create a new plugin system
    pub fn new(plugins_dir: PathBuf, host_version: &str) -> Self {
        // Development mode for now (no signature verification)
        let manager = PluginManager::new(plugins_dir, host_version, true);
        let host_context = manager.host_context_mut();

        Self {
            manager,
            host_context,
        }
    }

    /// Load all discovered plugins
    pub fn load_all(&mut self) {
        let discovered = self.manager.discover_plugins();
        info!("Discovered {} plugin directories", discovered.len());
        for dir in &discovered {
            info!("  - {}", dir.display());
        }

        let results = self.manager.load_all_plugins();
        info!("Plugin load results: {} total", results.len());
        for result in results {
            match result {
                Ok(id) => info!("Loaded plugin: {}", id),
                Err(e) => warn!("Failed to load plugin: {}", e),
            }
        }

        // Log loaded plugins count
        let loaded = self.manager.loaded_plugins();
        info!("Total loaded plugins: {}", loaded.len());
    }

    /// Update host context with current chart state
    pub fn update_context(&self, bounds: Option<GeoBounds>, zoom: f64, scale: f64) {
        self.manager.update_context(|ctx| {
            ctx.chart_bounds = bounds;
            ctx.zoom_level = zoom;
            ctx.display_scale = scale;
        });
    }

    /// Set file save callback
    pub fn set_file_save_callback<F>(&self, callback: F)
    where
        F: Fn(&str, &str, &str) -> bool + Send + Sync + 'static,
    {
        self.manager.update_context(|ctx| {
            ctx.on_file_save = Some(Box::new(callback));
        });
    }

    /// Set file open callback
    pub fn set_file_open_callback<F>(&self, callback: F)
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        self.manager.update_context(|ctx| {
            ctx.on_file_open = Some(Box::new(callback));
        });
    }

    /// Set toast notification callback
    pub fn set_toast_callback<F>(&self, callback: F)
    where
        F: Fn(&str, bool) + Send + Sync + 'static,
    {
        self.manager.update_context(|ctx| {
            ctx.on_toast = Some(Box::new(callback));
        });
    }

    /// Handle mouse click from main app
    /// Returns true if any plugin consumed the event
    pub fn handle_click(
        &mut self,
        lon: f64,
        lat: f64,
        button: MouseButton,
        is_double: bool,
    ) -> bool {
        let event = MouseEvent {
            position: Position::new(lon, lat),
            button,
            is_double_click: is_double,
        };

        // Send to all active plugins
        for plugin in self.manager.active_plugins_mut() {
            if plugin.on_mouse_click(event) {
                return true;
            }
        }
        false
    }

    /// Handle mouse move from main app
    pub fn handle_mouse_move(&mut self, lon: f64, lat: f64) {
        let pos = Position::new(lon, lat);
        for plugin in self.manager.active_plugins_mut() {
            plugin.on_mouse_move(pos);
        }
    }

    /// Get all drawing instructions from all loaded plugins
    /// (Each plugin decides internally whether to return instructions based on its rendering_enabled state)
    pub fn get_drawing_instructions(&self) -> Vec<DrawingInstruction> {
        let mut instructions = Vec::new();
        for plugin in self.manager.all_plugins() {
            instructions.extend(plugin.get_drawing_instructions().into_iter());
        }
        instructions
    }

    /// Get drawing instructions converted to ferrite-render format
    pub fn get_render_instructions(&self) -> Vec<ferrite_render::DrawingInstruction> {
        use ferrite_render::{
            Color, DisplayPriority, LineInstruction, LineStyle, PointInstruction, TextInstruction,
            ViewingGroup, WorldPoint,
        };

        let mut instructions = Vec::new();

        for plugin_instr in self.get_drawing_instructions() {
            match plugin_instr {
                DrawingInstruction::Point {
                    symbol_ref,
                    position,
                    rotation,
                    priority,
                } => {
                    let mut point = PointInstruction::new(
                        symbol_ref.to_string(),
                        WorldPoint::new(position.lon, position.lat),
                    );
                    point.rotation = rotation;
                    point.priority = DisplayPriority(priority);
                    point.viewing_group = ViewingGroup(21010); // Always displayed
                    instructions.push(ferrite_render::DrawingInstruction::Point(point));
                }
                DrawingInstruction::Line {
                    points,
                    color,
                    width,
                    priority,
                } => {
                    if points.len() >= 2 {
                        let coords: Vec<WorldPoint> = points
                            .iter()
                            .map(|p| WorldPoint::new(p.lon, p.lat))
                            .collect();
                        // Convert RGBA u32 to Color
                        let r = ((color >> 24) & 0xFF) as u8;
                        let g = ((color >> 16) & 0xFF) as u8;
                        let b = ((color >> 8) & 0xFF) as u8;
                        let a = (color & 0xFF) as u8;
                        let line_color = Color::from_u8(r, g, b, a);
                        let line_style = LineStyle::solid(line_color, width);
                        let line = LineInstruction::new(coords)
                            .with_style(line_style)
                            .with_priority(priority);
                        instructions.push(ferrite_render::DrawingInstruction::Line(line));
                    }
                }
                DrawingInstruction::Text {
                    text,
                    position,
                    font_size,
                    color,
                    priority,
                } => {
                    let r = ((color >> 24) & 0xFF) as u8;
                    let g = ((color >> 16) & 0xFF) as u8;
                    let b = ((color >> 8) & 0xFF) as u8;
                    let a = (color & 0xFF) as u8;
                    let text_color = Color::from_u8(r, g, b, a);
                    let text_instr = TextInstruction::new(
                        text.to_string(),
                        WorldPoint::new(position.lon, position.lat),
                    )
                    .with_font_size(font_size)
                    .with_color(text_color)
                    .with_priority(priority);
                    instructions.push(ferrite_render::DrawingInstruction::Text(text_instr));
                }
                DrawingInstruction::Circle {
                    center,
                    radius_nm,
                    color,
                    width,
                    priority,
                } => {
                    // Convert circle to polyline (approximation with N segments)
                    // radius_nm is in nautical miles, need to convert to degrees
                    // 1 NM ≈ 1/60 degree latitude
                    let radius_deg = radius_nm / 60.0;
                    let segments = 32; // Number of line segments

                    let mut points = Vec::with_capacity(segments + 1);
                    for i in 0..=segments {
                        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (segments as f64);
                        // Adjust for longitude scaling based on latitude
                        let lat_factor = center.lat.to_radians().cos();
                        let lon = center.lon + radius_deg * angle.cos() / lat_factor.max(0.1);
                        let lat = center.lat + radius_deg * angle.sin();
                        points.push(WorldPoint::new(lon, lat));
                    }

                    // Convert color
                    let r = ((color >> 24) & 0xFF) as u8;
                    let g = ((color >> 16) & 0xFF) as u8;
                    let b = ((color >> 8) & 0xFF) as u8;
                    let a = (color & 0xFF) as u8;
                    let line_color = Color::from_u8(r, g, b, a);
                    let line_style = LineStyle::solid(line_color, width);

                    let line = LineInstruction::new(points)
                        .with_style(line_style)
                        .with_priority(priority);
                    instructions.push(ferrite_render::DrawingInstruction::Line(line));
                }
            }
        }

        instructions
    }

    /// Get loaded plugin info for UI
    pub fn get_loaded_plugins(&self) -> Vec<PluginInfo> {
        self.manager
            .loaded_plugins()
            .iter()
            .map(|manifest| PluginInfo {
                id: manifest.id.clone(),
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                description: manifest.description.clone(),
                enabled: manifest.enabled,
            })
            .collect()
    }

    /// Toggle plugin active state
    pub fn toggle_plugin(&mut self, plugin_id: &str) {
        if let Some(plugin) = self.manager.get_plugin_mut(plugin_id) {
            let is_active = plugin.is_active();
            plugin.set_active(!is_active);
            debug!("Plugin {} active: {}", plugin_id, !is_active);
        }
    }

    /// Check if a plugin is active
    pub fn is_plugin_active(&self, plugin_id: &str) -> bool {
        self.manager
            .get_plugin(plugin_id)
            .map(|p| p.is_active())
            .unwrap_or(false)
    }

    /// Get toolbar buttons for active plugins
    pub fn get_toolbar_buttons(&self) -> Vec<ToolbarButton> {
        let mut buttons = Vec::new();
        for manifest in self.manager.loaded_plugins() {
            if let Some(plugin) = self.manager.get_plugin(&manifest.id) {
                if let Some(label) = plugin.toolbar_label().into_option() {
                    let tooltip = plugin
                        .toolbar_tooltip()
                        .into_option()
                        .map(|s| s.as_str().to_string());
                    buttons.push(ToolbarButton {
                        plugin_id: manifest.id.clone(),
                        label: label.as_str().to_string(),
                        tooltip,
                        active: plugin.is_active(),
                    });
                }
            }
        }
        buttons
    }

    /// Shutdown all plugins
    pub fn shutdown(&mut self) {
        self.manager.shutdown_all();
    }

    /// Deactivate all plugins (close panels)
    pub fn deactivate_all_plugins(&mut self) {
        // Collect plugin IDs first to avoid borrow issues
        let plugin_ids: Vec<String> = self
            .manager
            .loaded_plugins()
            .iter()
            .map(|m| m.id.clone())
            .collect();

        for plugin_id in plugin_ids {
            if let Some(plugin) = self.manager.get_plugin_mut(&plugin_id) {
                if plugin.is_active() {
                    plugin.set_active(false);
                    debug!("Plugin {} deactivated", plugin_id);
                }
            }
        }
    }

    /// Get UI data from active plugins (JSON)
    pub fn get_active_plugin_ui_data(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for manifest in self.manager.loaded_plugins() {
            if let Some(plugin) = self.manager.get_plugin(&manifest.id) {
                if plugin.is_active() {
                    let data = plugin.get_ui_data();
                    if !data.is_empty() {
                        if let Ok(json) = String::from_utf8(data.to_vec()) {
                            result.push((manifest.id.clone(), json));
                        }
                    }
                }
            }
        }
        result
    }

    /// Send UI event to a specific plugin
    pub fn send_ui_event(&mut self, plugin_id: &str, event_json: &str) {
        if let Some(plugin) = self.manager.get_plugin_mut(plugin_id) {
            plugin.handle_ui_event(RStr::from(event_json));
        }
    }

    /// Clear all plugin data (called when charts are cleared)
    pub fn clear_all_data(&mut self) {
        // Call clear() on all loaded plugins (no hardcoding)
        for plugin in self.manager.active_plugins_mut() {
            plugin.clear();
        }
    }
}

/// Plugin info for UI display
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
}

/// Toolbar button info
#[derive(Debug, Clone)]
pub struct ToolbarButton {
    pub plugin_id: String,
    pub label: String,
    pub tooltip: Option<String>,
    pub active: bool,
}
