//! FerriteS100 Plugin API
//!
//! ABI-stable plugin interface using abi_stable crate.
//! Plugins are compiled as DLLs and loaded at runtime.

#![allow(non_local_definitions)] // Required for abi_stable sabi_trait macro

use abi_stable::{
    sabi_trait,
    std_types::{RBox, ROption, RStr, RString, RVec},
    StableAbi,
};

pub mod host_api;
pub mod types;

pub use host_api::*;
pub use types::*;

/// Plugin API version - increment when breaking changes are made.
///
/// History:
/// - 1: Initial release (route-plugin baseline).
/// - 2: Added `HostApi::chart_*` methods for in-process chart-data queries
///   (used by the s101-explorer plugin). Existing v1 plugins must be
///   rebuilt — adding fn-pointer fields changes `HostApi` layout.
/// - 3: Added `HostApi::mcp_server_info` for the in-process HTTP MCP
///   server + cloudflared tunnel. Plugins read this to render the
///   public registration URL + bearer token in their UI.
pub const PLUGIN_API_VERSION: u32 = 3;

/// ABI-stable coordinate position
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug, Default)]
pub struct Position {
    pub lon: f64,
    pub lat: f64,
}

impl Position {
    pub fn new(lon: f64, lat: f64) -> Self {
        Self { lon, lat }
    }
}

/// Geographic bounds
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug, Default)]
pub struct GeoBounds {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

/// ABI-stable drawing instruction for rendering
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub enum DrawingInstruction {
    /// Draw a symbol at a position
    Point {
        symbol_ref: RString,
        position: Position,
        rotation: f32,
        priority: i32,
    },
    /// Draw a line between points
    Line {
        points: RVec<Position>,
        color: u32, // RGBA
        width: f32,
        priority: i32,
    },
    /// Draw text at a position
    Text {
        text: RString,
        position: Position,
        font_size: f32,
        color: u32, // RGBA
        priority: i32,
    },
    /// Draw a circle/arc
    Circle {
        center: Position,
        radius_nm: f64,
        color: u32,
        width: f32,
        priority: i32,
    },
}

/// Mouse button type
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Mouse event data
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug)]
pub struct MouseEvent {
    pub position: Position,
    pub button: MouseButton,
    pub is_double_click: bool,
}

/// Plugin trait - all plugins must implement this
#[sabi_trait]
pub trait Plugin: Send + Sync {
    /// Unique plugin ID (UUID format recommended)
    fn id(&self) -> RStr<'_>;

    /// Human-readable plugin name
    fn name(&self) -> RStr<'_>;

    /// Plugin version string
    fn version(&self) -> RStr<'_>;

    /// Optional toolbar button label (None = no toolbar button)
    fn toolbar_label(&self) -> ROption<RStr<'_>>;

    /// Optional toolbar tooltip
    fn toolbar_tooltip(&self) -> ROption<RStr<'_>>;

    /// Check if plugin is currently active
    fn is_active(&self) -> bool;

    /// Activate or deactivate the plugin
    fn set_active(&mut self, active: bool);

    /// Handle mouse click on chart
    /// Returns true if the event was consumed
    fn on_mouse_click(&mut self, event: MouseEvent) -> bool;

    /// Handle mouse move on chart (for preview/hover effects)
    fn on_mouse_move(&mut self, position: Position);

    /// Get drawing instructions for rendering
    fn get_drawing_instructions(&self) -> RVec<DrawingInstruction>;

    /// Get UI panel data (serialized as JSON)
    /// Host will render this in the side panel
    fn get_ui_data(&self) -> RVec<u8>;

    /// Handle UI event from host (serialized as JSON)
    fn handle_ui_event(&mut self, event_json: RStr<'_>);

    /// Get settings schema (for Settings dialog)
    fn get_settings_schema(&self) -> RVec<SettingsItem>;

    /// Get current settings as JSON
    fn get_settings(&self) -> RVec<u8>;

    /// Apply settings from JSON
    fn apply_settings(&mut self, settings_json: RStr<'_>);

    /// Called when plugin is loaded, receives HostApi
    fn initialize(&mut self, host_api: HostApi);

    /// Called when plugin is about to be unloaded
    fn shutdown(&mut self);

    /// Export data (e.g., S-421 XML)
    fn export_data(&self) -> ROption<RVec<u8>>;

    /// Import data (e.g., S-421 XML)
    fn import_data(&mut self, data: RStr<'_>) -> bool;

    /// Clear all data
    fn clear(&mut self);
}

/// Settings item types for plugin configuration UI
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub enum SettingsItem {
    /// Boolean toggle
    Bool {
        key: RString,
        label: RString,
        description: RString,
        default_value: bool,
    },
    /// Single choice from options
    Choice {
        key: RString,
        label: RString,
        description: RString,
        options: RVec<RString>,
        default_index: u32,
    },
    /// Numeric value
    Number {
        key: RString,
        label: RString,
        description: RString,
        min: f64,
        max: f64,
        step: f64,
        default_value: f64,
    },
    /// Color picker
    Color {
        key: RString,
        label: RString,
        description: RString,
        default_rgba: u32,
    },
    /// Text input
    Text {
        key: RString,
        label: RString,
        description: RString,
        default_value: RString,
    },
}

/// Plugin module structure for DLL export
#[repr(C)]
#[derive(StableAbi)]
pub struct PluginModule {
    /// Create a new plugin instance
    pub create_plugin: extern "C" fn() -> Plugin_TO<'static, RBox<()>>,

    /// Plugin API version this plugin was built against
    pub api_version: u32,

    /// Minimum host version required
    pub min_host_version: RString,

    /// Plugin metadata
    pub metadata: PluginMetadata,
}

/// Plugin metadata
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub struct PluginMetadata {
    pub id: RString,
    pub name: RString,
    pub version: RString,
    pub author: RString,
    pub description: RString,
}

/// Type alias for the plugin module getter function
pub type GetPluginModuleFn = extern "C" fn() -> &'static PluginModule;

/// Name of the exported function that returns the plugin module
pub const PLUGIN_MODULE_SYMBOL: &str = "get_plugin_module";
