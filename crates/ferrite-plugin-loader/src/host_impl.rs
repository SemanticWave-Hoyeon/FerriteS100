//! Host API implementation
//!
//! Provides the sandboxed API that plugins use to interact with the host.

#![allow(clippy::type_complexity)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use abi_stable::std_types::{ROption, RStr, RString};
use tracing::{debug, error, info, trace, warn};

use ferrite_plugin_api::{FileFilter, GeoBounds, HostApi, LogLevel};

/// Host context that holds shared state
pub struct HostContext {
    /// Config directory for plugin settings
    pub config_dir: PathBuf,
    /// Current chart bounds (if loaded)
    pub chart_bounds: Option<GeoBounds>,
    /// Current zoom level
    pub zoom_level: f64,
    /// Display scale (pixels per degree)
    pub display_scale: f64,
    /// Callback for UI refresh requests
    pub on_ui_refresh: Option<Box<dyn Fn() + Send + Sync>>,
    /// Callback for chart redraw requests
    pub on_chart_redraw: Option<Box<dyn Fn() + Send + Sync>>,
    /// Callback for file save requests
    pub on_file_save: Option<Box<dyn Fn(&str, &str, &str) -> bool + Send + Sync>>,
    /// Callback for file open requests
    pub on_file_open: Option<Box<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    /// Callback for toast notifications
    pub on_toast: Option<Box<dyn Fn(&str, bool) + Send + Sync>>,
}

impl Default for HostContext {
    fn default() -> Self {
        Self {
            config_dir: PathBuf::from("./config/plugins"),
            chart_bounds: None,
            zoom_level: 1.0,
            display_scale: 1.0,
            on_ui_refresh: None,
            on_chart_redraw: None,
            on_file_save: None,
            on_file_open: None,
            on_toast: None,
        }
    }
}

/// Thread-safe host context wrapper
pub type SharedHostContext = Arc<Mutex<HostContext>>;

/// Create a HostApi instance from a shared context
pub fn create_host_api(context: SharedHostContext) -> HostApi {
    let ctx_ptr = Arc::into_raw(context) as *const ();

    HostApi {
        context: ctx_ptr,
        log: host_log,
        get_chart_bounds: host_get_chart_bounds,
        get_zoom_level: host_get_zoom_level,
        get_display_scale: host_get_display_scale,
        request_file_save: host_request_file_save,
        request_file_open: host_request_file_open,
        save_config: host_save_config,
        load_config: host_load_config,
        request_ui_refresh: host_request_ui_refresh,
        request_chart_redraw: host_request_chart_redraw,
        show_toast: host_show_toast,
    }
}

/// Recover SharedHostContext from raw pointer
/// SAFETY: Must only be called with a valid context pointer
unsafe fn get_context(ctx: *const ()) -> SharedHostContext {
    let arc = Arc::from_raw(ctx as *const Mutex<HostContext>);
    let cloned = arc.clone();
    std::mem::forget(arc); // Don't drop the original Arc
    cloned
}

// Host API function implementations

extern "C" fn host_log(_ctx: *const (), level: LogLevel, message: RStr<'_>) {
    let msg = message.as_str();
    match level {
        LogLevel::Trace => trace!(target: "plugin", "{}", msg),
        LogLevel::Debug => debug!(target: "plugin", "{}", msg),
        LogLevel::Info => info!(target: "plugin", "{}", msg),
        LogLevel::Warn => warn!(target: "plugin", "{}", msg),
        LogLevel::Error => error!(target: "plugin", "{}", msg),
    }
}

extern "C" fn host_get_chart_bounds(ctx: *const ()) -> ROption<GeoBounds> {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();
    match guard.chart_bounds {
        Some(bounds) => ROption::RSome(bounds),
        None => ROption::RNone,
    }
}

extern "C" fn host_get_zoom_level(ctx: *const ()) -> f64 {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();
    guard.zoom_level
}

extern "C" fn host_get_display_scale(ctx: *const ()) -> f64 {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();
    guard.display_scale
}

extern "C" fn host_request_file_save(
    ctx: *const (),
    filter: FileFilter,
    default_name: RStr<'_>,
    data: RStr<'_>,
) -> bool {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    if let Some(ref callback) = guard.on_file_save {
        let filter_str = format!(
            "{}|{}",
            filter.name.as_str(),
            filter
                .extensions
                .iter()
                .map(|e| format!("*.{}", e.as_str()))
                .collect::<Vec<_>>()
                .join(";")
        );
        callback(&filter_str, default_name.as_str(), data.as_str())
    } else {
        warn!("File save requested but no callback registered");
        false
    }
}

extern "C" fn host_request_file_open(ctx: *const (), filter: FileFilter) -> ROption<RString> {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    if let Some(ref callback) = guard.on_file_open {
        let filter_str = format!(
            "{}|{}",
            filter.name.as_str(),
            filter
                .extensions
                .iter()
                .map(|e| format!("*.{}", e.as_str()))
                .collect::<Vec<_>>()
                .join(";")
        );
        match callback(&filter_str) {
            Some(content) => ROption::RSome(RString::from(content)),
            None => ROption::RNone,
        }
    } else {
        warn!("File open requested but no callback registered");
        ROption::RNone
    }
}

extern "C" fn host_save_config(ctx: *const (), plugin_id: RStr<'_>, config_json: RStr<'_>) -> bool {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    let config_path = guard
        .config_dir
        .join(format!("{}.json", plugin_id.as_str()));

    // Ensure directory exists
    if let Some(parent) = config_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!("Failed to create config directory: {}", e);
            return false;
        }
    }

    match std::fs::write(&config_path, config_json.as_str()) {
        Ok(_) => {
            debug!("Saved plugin config: {}", config_path.display());
            true
        }
        Err(e) => {
            error!("Failed to save plugin config: {}", e);
            false
        }
    }
}

extern "C" fn host_load_config(ctx: *const (), plugin_id: RStr<'_>) -> ROption<RString> {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    let config_path = guard
        .config_dir
        .join(format!("{}.json", plugin_id.as_str()));

    match std::fs::read_to_string(&config_path) {
        Ok(content) => {
            debug!("Loaded plugin config: {}", config_path.display());
            ROption::RSome(RString::from(content))
        }
        Err(_) => ROption::RNone,
    }
}

extern "C" fn host_request_ui_refresh(ctx: *const ()) {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    if let Some(ref callback) = guard.on_ui_refresh {
        callback();
    }
}

extern "C" fn host_request_chart_redraw(ctx: *const ()) {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    if let Some(ref callback) = guard.on_chart_redraw {
        callback();
    }
}

extern "C" fn host_show_toast(ctx: *const (), message: RStr<'_>, is_error: bool) {
    let context = unsafe { get_context(ctx) };
    let guard = context.lock().unwrap();

    if let Some(ref callback) = guard.on_toast {
        callback(message.as_str(), is_error);
    } else if is_error {
        error!("Toast (no handler): {}", message.as_str());
    } else {
        info!("Toast (no handler): {}", message.as_str());
    }
}
