//! Host API - Sandboxed interface for plugins to interact with host
//!
//! This API is the ONLY way plugins can interact with the host application.
//! Direct file system, network, or system access is not available to plugins.

use abi_stable::{
    std_types::{ROption, RStr, RString, RVec},
    StableAbi,
};

use crate::GeoBounds;

/// Log level for plugin logging
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

/// File filter for file dialogs
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub struct FileFilter {
    /// Filter name (e.g., "S-421 Route Files")
    pub name: RString,
    /// File extensions (e.g., ["gml", "xml"])
    pub extensions: RVec<RString>,
}

/// Host API provided to plugins
///
/// This is the sandboxed interface that plugins use to interact with the host.
/// Plugins cannot access the file system, network, or system directly.
#[repr(C)]
#[derive(StableAbi, Clone)]
pub struct HostApi {
    /// Internal pointer to host context (opaque to plugin)
    pub context: *const (),

    /// Log a message through the host's logging system
    pub log: extern "C" fn(ctx: *const (), level: LogLevel, message: RStr<'_>),

    /// Get current chart bounds (if a chart is loaded)
    pub get_chart_bounds: extern "C" fn(ctx: *const ()) -> ROption<GeoBounds>,

    /// Get current zoom level
    pub get_zoom_level: extern "C" fn(ctx: *const ()) -> f64,

    /// Get current display scale (pixels per degree)
    pub get_display_scale: extern "C" fn(ctx: *const ()) -> f64,

    /// Request to save a file (host shows save dialog)
    /// Returns true if save was successful
    pub request_file_save: extern "C" fn(
        ctx: *const (),
        filter: FileFilter,
        default_name: RStr<'_>,
        data: RStr<'_>,
    ) -> bool,

    /// Request to open a file (host shows open dialog)
    /// Returns file contents if successful
    pub request_file_open: extern "C" fn(ctx: *const (), filter: FileFilter) -> ROption<RString>,

    /// Save plugin configuration (host manages storage)
    pub save_config:
        extern "C" fn(ctx: *const (), plugin_id: RStr<'_>, config_json: RStr<'_>) -> bool,

    /// Load plugin configuration
    pub load_config: extern "C" fn(ctx: *const (), plugin_id: RStr<'_>) -> ROption<RString>,

    /// Request UI refresh (re-render plugin panel)
    pub request_ui_refresh: extern "C" fn(ctx: *const ()),

    /// Request chart redraw (re-render plugin drawings)
    pub request_chart_redraw: extern "C" fn(ctx: *const ()),

    /// Show a toast notification
    pub show_toast: extern "C" fn(ctx: *const (), message: RStr<'_>, is_error: bool),

    // === Chart-data queries (PLUGIN_API_VERSION ≥ 2) =====================
    //
    // These let in-process plugins read the loaded S-101 cell + Feature
    // Catalogue without re-parsing the chart file. Each returns JSON in an
    // owned `RString` — small per-call allocation, no full-data copy. The
    // host owns the underlying indices; plugins are pure read clients.
    //
    // `chart_loaded` is the gate: every other method returns an empty/error
    // payload until the host reports a chart is ready.
    /// True when an S-101 cell is loaded and indices are built.
    pub chart_loaded: extern "C" fn(ctx: *const ()) -> bool,

    /// Mirror of `dataset_metadata` from the MCP server: chart identification,
    /// extent, feature-type histogram, catalogue summary. JSON.
    pub chart_dataset_metadata: extern "C" fn(ctx: *const ()) -> RString,

    /// Substring search across catalogue entries. Returns JSON list capped
    /// at `limit`. Empty term returns `[]`.
    pub chart_catalogue_search:
        extern "C" fn(ctx: *const (), term: RStr<'_>, limit: u32) -> RString,

    /// Catalogue definition for a feature type code.
    pub chart_catalogue_describe_feature: extern "C" fn(ctx: *const (), code: RStr<'_>) -> RString,

    /// Catalogue definition for an attribute (simple or complex).
    pub chart_catalogue_describe_attribute:
        extern "C" fn(ctx: *const (), code: RStr<'_>) -> RString,

    /// Geometry summary + attributes for a feature id.
    pub chart_feature_get: extern "C" fn(ctx: *const (), id: i64) -> RString,

    /// Features whose bbox intersects (w, s, e, n).
    pub chart_feature_query_bbox:
        extern "C" fn(ctx: *const (), w: f64, s: f64, e: f64, n: f64, limit: u32) -> RString,

    /// Features within `radius_m` of (lat, lon), nearest first.
    pub chart_feature_nearby:
        extern "C" fn(ctx: *const (), lat: f64, lon: f64, radius_m: f64, limit: u32) -> RString,
}

// SAFETY: HostApi contains only function pointers and a context pointer
// that are valid for the lifetime of the plugin
unsafe impl Send for HostApi {}
unsafe impl Sync for HostApi {}

impl HostApi {
    /// Log a trace message
    pub fn trace(&self, message: &str) {
        (self.log)(self.context, LogLevel::Trace, RStr::from(message));
    }

    /// Log a debug message
    pub fn debug(&self, message: &str) {
        (self.log)(self.context, LogLevel::Debug, RStr::from(message));
    }

    /// Log an info message
    pub fn info(&self, message: &str) {
        (self.log)(self.context, LogLevel::Info, RStr::from(message));
    }

    /// Log a warning message
    pub fn warn(&self, message: &str) {
        (self.log)(self.context, LogLevel::Warn, RStr::from(message));
    }

    /// Log an error message
    pub fn error(&self, message: &str) {
        (self.log)(self.context, LogLevel::Error, RStr::from(message));
    }

    /// Get chart bounds
    pub fn chart_bounds(&self) -> Option<GeoBounds> {
        (self.get_chart_bounds)(self.context).into_option()
    }

    /// Get zoom level
    pub fn zoom_level(&self) -> f64 {
        (self.get_zoom_level)(self.context)
    }

    /// Get display scale
    pub fn display_scale(&self) -> f64 {
        (self.get_display_scale)(self.context)
    }

    /// Save file with dialog
    pub fn save_file(&self, filter: FileFilter, default_name: &str, data: &str) -> bool {
        (self.request_file_save)(
            self.context,
            filter,
            RStr::from(default_name),
            RStr::from(data),
        )
    }

    /// Open file with dialog
    pub fn open_file(&self, filter: FileFilter) -> Option<String> {
        (self.request_file_open)(self.context, filter)
            .into_option()
            .map(|s| s.into_string())
    }

    /// Save plugin config
    pub fn save_plugin_config(&self, plugin_id: &str, config_json: &str) -> bool {
        (self.save_config)(self.context, RStr::from(plugin_id), RStr::from(config_json))
    }

    /// Load plugin config
    pub fn load_plugin_config(&self, plugin_id: &str) -> Option<String> {
        (self.load_config)(self.context, RStr::from(plugin_id))
            .into_option()
            .map(|s| s.into_string())
    }

    /// Request UI panel refresh
    pub fn refresh_ui(&self) {
        (self.request_ui_refresh)(self.context);
    }

    /// Request chart redraw
    pub fn redraw_chart(&self) {
        (self.request_chart_redraw)(self.context);
    }

    /// Show toast notification
    pub fn toast(&self, message: &str) {
        (self.show_toast)(self.context, RStr::from(message), false);
    }

    /// Show error toast notification
    pub fn toast_error(&self, message: &str) {
        (self.show_toast)(self.context, RStr::from(message), true);
    }

    // ── Chart-data queries ────────────────────────────────────────────

    /// True when an S-101 cell + Feature Catalogue are loaded and indexed.
    pub fn chart_loaded(&self) -> bool {
        (self.chart_loaded)(self.context)
    }

    /// JSON metadata for the loaded cell (extent, feature-type histogram,
    /// catalogue summary). Empty object when no chart is loaded.
    pub fn chart_dataset_metadata(&self) -> String {
        (self.chart_dataset_metadata)(self.context).into_string()
    }

    /// Substring search across the Feature Catalogue. Returns JSON.
    pub fn chart_catalogue_search(&self, term: &str, limit: u32) -> String {
        (self.chart_catalogue_search)(self.context, RStr::from(term), limit).into_string()
    }

    pub fn chart_catalogue_describe_feature(&self, code: &str) -> String {
        (self.chart_catalogue_describe_feature)(self.context, RStr::from(code)).into_string()
    }

    pub fn chart_catalogue_describe_attribute(&self, code: &str) -> String {
        (self.chart_catalogue_describe_attribute)(self.context, RStr::from(code)).into_string()
    }

    pub fn chart_feature_get(&self, id: i64) -> String {
        (self.chart_feature_get)(self.context, id).into_string()
    }

    pub fn chart_feature_query_bbox(&self, w: f64, s: f64, e: f64, n: f64, limit: u32) -> String {
        (self.chart_feature_query_bbox)(self.context, w, s, e, n, limit).into_string()
    }

    pub fn chart_feature_nearby(&self, lat: f64, lon: f64, radius_m: f64, limit: u32) -> String {
        (self.chart_feature_nearby)(self.context, lat, lon, radius_m, limit).into_string()
    }
}

/// Create a dummy HostApi for testing
#[cfg(test)]
pub fn dummy_host_api() -> HostApi {
    extern "C" fn dummy_log(_: *const (), _: LogLevel, _: RStr<'_>) {}
    extern "C" fn dummy_bounds(_: *const ()) -> ROption<GeoBounds> {
        ROption::RNone
    }
    extern "C" fn dummy_zoom(_: *const ()) -> f64 {
        1.0
    }
    extern "C" fn dummy_scale(_: *const ()) -> f64 {
        1.0
    }
    extern "C" fn dummy_save(_: *const (), _: FileFilter, _: RStr<'_>, _: RStr<'_>) -> bool {
        false
    }
    extern "C" fn dummy_open(_: *const (), _: FileFilter) -> ROption<RString> {
        ROption::RNone
    }
    extern "C" fn dummy_save_config(_: *const (), _: RStr<'_>, _: RStr<'_>) -> bool {
        false
    }
    extern "C" fn dummy_load_config(_: *const (), _: RStr<'_>) -> ROption<RString> {
        ROption::RNone
    }
    extern "C" fn dummy_refresh(_: *const ()) {}
    extern "C" fn dummy_redraw(_: *const ()) {}
    extern "C" fn dummy_toast(_: *const (), _: RStr<'_>, _: bool) {}
    extern "C" fn dummy_chart_loaded(_: *const ()) -> bool {
        false
    }
    extern "C" fn dummy_empty_string(_: *const ()) -> RString {
        RString::new()
    }
    extern "C" fn dummy_search(_: *const (), _: RStr<'_>, _: u32) -> RString {
        RString::new()
    }
    extern "C" fn dummy_describe(_: *const (), _: RStr<'_>) -> RString {
        RString::new()
    }
    extern "C" fn dummy_feature_get(_: *const (), _: i64) -> RString {
        RString::new()
    }
    extern "C" fn dummy_bbox(_: *const (), _: f64, _: f64, _: f64, _: f64, _: u32) -> RString {
        RString::new()
    }
    extern "C" fn dummy_nearby(_: *const (), _: f64, _: f64, _: f64, _: u32) -> RString {
        RString::new()
    }

    HostApi {
        context: std::ptr::null(),
        log: dummy_log,
        get_chart_bounds: dummy_bounds,
        get_zoom_level: dummy_zoom,
        get_display_scale: dummy_scale,
        request_file_save: dummy_save,
        request_file_open: dummy_open,
        save_config: dummy_save_config,
        load_config: dummy_load_config,
        request_ui_refresh: dummy_refresh,
        request_chart_redraw: dummy_redraw,
        show_toast: dummy_toast,
        chart_loaded: dummy_chart_loaded,
        chart_dataset_metadata: dummy_empty_string,
        chart_catalogue_search: dummy_search,
        chart_catalogue_describe_feature: dummy_describe,
        chart_catalogue_describe_attribute: dummy_describe,
        chart_feature_get: dummy_feature_get,
        chart_feature_query_bbox: dummy_bbox,
        chart_feature_nearby: dummy_nearby,
    }
}
