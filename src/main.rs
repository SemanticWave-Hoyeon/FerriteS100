// Hide console window in release mode on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! FerriteS100 - Rust implementation of S-100/S-101 ENC viewer
//!
//! This application loads and parses S-101 Electronic Navigational Charts
//! using dynamically loaded Feature Catalogue (FC) and Portrayal Catalogue (PC).

/// Use mimalloc for better allocation performance (fewer small-alloc stalls)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Application version (from Cargo.toml)
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod plugins;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Wrapper around `s101_mcp::mcp::tools::dispatch` that converts the
/// per-tool result into the JSON string shape the plugin HostApi forwards.
/// `tool` and `args` mirror what an LLM-with-MCP would send via
/// `tools/call`. Errors are surfaced as `{"error": "..."}` so the plugin
/// side never has to guard against missing data.
fn dispatch_chart_query(
    indices: &s101_mcp::Indices,
    tool: &str,
    args: serde_json::Value,
) -> String {
    match s101_mcp::mcp::tools::dispatch(indices, tool, args) {
        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|e| format!(r#"{{"error":"{}"}}"#, e)),
        Err(e) => format!(r#"{{"error":"{}"}}"#, e),
    }
}

fn s101_mcp_chart_dataset_metadata(idx: &s101_mcp::Indices) -> String {
    dispatch_chart_query(idx, "dataset_metadata", serde_json::json!({}))
}
fn s101_mcp_catalogue_search(idx: &s101_mcp::Indices, term: &str, limit: u32) -> String {
    dispatch_chart_query(
        idx,
        "catalogue_search",
        serde_json::json!({"term": term, "limit": limit as usize}),
    )
}
fn s101_mcp_catalogue_describe_feature(idx: &s101_mcp::Indices, code: &str) -> String {
    dispatch_chart_query(
        idx,
        "catalogue_describe_feature",
        serde_json::json!({"code": code}),
    )
}
fn s101_mcp_catalogue_describe_attribute(idx: &s101_mcp::Indices, code: &str) -> String {
    dispatch_chart_query(
        idx,
        "catalogue_describe_attribute",
        serde_json::json!({"code": code}),
    )
}
fn s101_mcp_feature_get(idx: &s101_mcp::Indices, id: i64) -> String {
    dispatch_chart_query(idx, "feature_get", serde_json::json!({"id": id}))
}
fn s101_mcp_feature_query_bbox(
    idx: &s101_mcp::Indices,
    w: f64,
    s: f64,
    e: f64,
    n: f64,
    limit: u32,
) -> String {
    dispatch_chart_query(
        idx,
        "feature_query_bbox",
        serde_json::json!({"w": w, "s": s, "e": e, "n": n, "limit": limit as usize}),
    )
}
fn s101_mcp_feature_nearby(
    idx: &s101_mcp::Indices,
    lat: f64,
    lon: f64,
    radius_m: f64,
    limit: u32,
) -> String {
    dispatch_chart_query(
        idx,
        "feature_nearby",
        serde_json::json!({"lat": lat, "lon": lon, "radius_m": radius_m, "limit": limit as usize}),
    )
}

/// Show a native Windows error dialog (release mode only, no-op on other platforms)
#[cfg(all(windows, not(debug_assertions)))]
fn show_error_dialog(title: &str, message: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let wide_title: Vec<u16> = std::ffi::OsStr::new(title)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let wide_msg: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut() as _,
            wide_msg.as_ptr(),
            wide_title.as_ptr(),
            MB_ICONERROR | MB_OK,
        );
    }
}
use tracing::{debug, error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use walkdir::WalkDir;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Icon, Window, WindowId},
};

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{
    ContextParameters as LuaContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue,
};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    AreaInstruction, Color, GeoBounds, HAlign, LineInstruction, LineStyle, PointInstruction,
    RenderContext, TextInstruction as RenderTextInstruction, VAlign, Viewport, WorldPoint,
};
use ferrite_s100_core::{S101Cell, SpatialPrimitiveType};
use ferrite_wgpu::{
    CatalogueStatus, DisplayMode, SelectedFeature, SettingsState, SymbolCache, WgpuRenderer,
};

/// Embedded Natural Earth 110m coastline GeoJSON (~140KB)
/// Source: https://www.naturalearthdata.com/ (Public Domain)
const WORLD_MAP_GEOJSON: &str = include_str!("../assets/ne_110m_coastline.geojson");

/// Parse Natural Earth GeoJSON coastlines into line segments.
/// Returns Vec of line strings, each being a list of [longitude, latitude] pairs.
fn parse_world_map_coastlines() -> Vec<Vec<[f64; 2]>> {
    let parsed: serde_json::Value = match serde_json::from_str(WORLD_MAP_GEOJSON) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse world map GeoJSON: {}", e);
            return Vec::new();
        }
    };

    let mut coastlines = Vec::new();

    if let Some(features) = parsed.get("features").and_then(|f| f.as_array()) {
        for feature in features {
            let geometry = match feature.get("geometry") {
                Some(g) => g,
                None => continue,
            };
            let geo_type = geometry.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let coords = match geometry.get("coordinates") {
                Some(c) => c,
                None => continue,
            };

            match geo_type {
                "LineString" => {
                    if let Some(line) = parse_coord_array(coords) {
                        if line.len() >= 2 {
                            coastlines.push(line);
                        }
                    }
                }
                "MultiLineString" => {
                    if let Some(lines) = coords.as_array() {
                        for line_coords in lines {
                            if let Some(line) = parse_coord_array(line_coords) {
                                if line.len() >= 2 {
                                    coastlines.push(line);
                                }
                            }
                        }
                    }
                }
                "Polygon" => {
                    // Extract exterior ring as a line
                    if let Some(rings) = coords.as_array() {
                        if let Some(exterior) = rings.first() {
                            if let Some(line) = parse_coord_array(exterior) {
                                if line.len() >= 2 {
                                    coastlines.push(line);
                                }
                            }
                        }
                    }
                }
                "MultiPolygon" => {
                    if let Some(polygons) = coords.as_array() {
                        for polygon in polygons {
                            if let Some(rings) = polygon.as_array() {
                                if let Some(exterior) = rings.first() {
                                    if let Some(line) = parse_coord_array(exterior) {
                                        if line.len() >= 2 {
                                            coastlines.push(line);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    coastlines
}

/// Parse a GeoJSON coordinate array [[lon, lat], ...] into Vec<[f64; 2]>
fn parse_coord_array(value: &serde_json::Value) -> Option<Vec<[f64; 2]>> {
    let arr = value.as_array()?;
    let mut points = Vec::with_capacity(arr.len());
    for coord in arr {
        let pair = coord.as_array()?;
        if pair.len() >= 2 {
            let lon = pair[0].as_f64()?;
            let lat = pair[1].as_f64()?;
            points.push([lon, lat]);
        }
    }
    Some(points)
}

/// Get the application base directory.
/// Searches for a directory containing `Catalogues/` in this order:
/// 1. Executable's directory (Windows dist, Linux)
/// 2. macOS .app bundle Resources: `../Resources/` relative to executable
/// 3. Ancestors of the executable's directory (handles `target/release/` exe
///    launched from Explorer where CWD also lacks `Catalogues/`)
/// 4. Current working directory (typical for `cargo run`)
/// 5. Ancestors of the current working directory
fn get_app_base_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    if let Some(ref exe_dir) = exe_dir {
        // Check next to executable (Windows/Linux dist)
        if exe_dir.join("Catalogues").exists() {
            return exe_dir.clone();
        }
        // Check macOS .app bundle: Contents/MacOS/../Resources/ = Contents/Resources/
        let resources_dir = exe_dir.join("../Resources");
        if resources_dir.join("Catalogues").exists() {
            if let Ok(canonical) = resources_dir.canonicalize() {
                return canonical;
            }
            return resources_dir;
        }
        // Walk up from exe_dir looking for Catalogues/. Covers the case of
        // running the dev/release exe directly from `target/release/` via
        // File Explorer, where CWD = exe_dir and neither contains Catalogues.
        for ancestor in exe_dir.ancestors().skip(1) {
            if ancestor.join("Catalogues").exists() {
                return ancestor.to_path_buf();
            }
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("Catalogues").exists() {
        return cwd;
    }
    // Walk up from CWD as a last resort.
    for ancestor in cwd.ancestors().skip(1) {
        if ancestor.join("Catalogues").exists() {
            return ancestor.to_path_buf();
        }
    }
    cwd
}

/// Application configuration
struct AppConfig {
    /// Path to Feature Catalogue XML
    fc_path: PathBuf,
    /// Path to Portrayal Catalogue directory
    pc_path: PathBuf,
    /// Path to log directory (used only in debug builds)
    log_path: PathBuf,
    /// Debug mode enabled (--debug flag)
    debug_mode: bool,
    /// Auto-load chart file(s) on startup
    auto_chart: Vec<PathBuf>,
    /// Auto-save screenshot after loading (then exit)
    auto_screenshot: Option<PathBuf>,
    /// Debug interior rings: log detailed ring info
    debug_rings: bool,
    /// Override zoom level for auto-screenshot (1.0 = fit to window)
    auto_zoom: Option<f64>,
    /// Override center position for auto-screenshot (lat,lon in degrees)
    auto_center: Option<(f64, f64)>,
    /// Allow unsigned plugins to load even in release builds.
    /// Set via `--dev-plugins` or `FERRITE_DEV_PLUGINS=1`.
    /// Use only for local testing; production distributions should
    /// ship signed plugins instead.
    dev_plugins: bool,
}

impl AppConfig {
    fn from_args() -> Self {
        let base = get_app_base_dir();
        let args: Vec<String> = std::env::args().collect();
        let debug_mode = args.iter().any(|arg| arg == "--debug" || arg == "--DEBUG");
        let debug_rings = args.iter().any(|arg| arg == "--debug-rings");
        let dev_plugins = args.iter().any(|arg| arg == "--dev-plugins")
            || std::env::var("FERRITE_DEV_PLUGINS").is_ok_and(|v| v == "1" || v == "true");

        // Parse --chart <path> (can appear multiple times or use glob)
        let mut auto_chart = Vec::new();
        let mut i = 1;
        while i < args.len() {
            if args[i] == "--chart" {
                if let Some(path_str) = args.get(i + 1) {
                    let path = PathBuf::from(path_str);
                    if path.is_dir() {
                        // Load all .000 files from directory
                        if let Ok(entries) = std::fs::read_dir(&path) {
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.extension().is_some_and(|e| e == "000") {
                                    auto_chart.push(p);
                                }
                            }
                        }
                    } else {
                        auto_chart.push(path);
                    }
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }

        // Parse --screenshot <path>
        let auto_screenshot = args
            .windows(2)
            .find(|w| w[0] == "--screenshot")
            .map(|w| PathBuf::from(&w[1]));

        // Parse --zoom <level>
        let auto_zoom = args
            .windows(2)
            .find(|w| w[0] == "--zoom")
            .and_then(|w| w[1].parse::<f64>().ok());

        // Parse --center <lat,lon> (e.g., --center 50.7908,-1.1135)
        let auto_center = args.windows(2).find(|w| w[0] == "--center").and_then(|w| {
            let parts: Vec<&str> = w[1].split(',').collect();
            if parts.len() == 2 {
                if let (Ok(lat), Ok(lon)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    return Some((lat, lon));
                }
            }
            None
        });

        // Force debug mode if debug-rings or screenshot is set
        let debug_mode = debug_mode || debug_rings || auto_screenshot.is_some();

        AppConfig {
            fc_path: base.join("Catalogues/FC/S-101"),
            pc_path: base.join("Catalogues/PC/S-101"),
            log_path: base.join("logs"),
            debug_mode,
            auto_chart,
            auto_screenshot,
            debug_rings,
            auto_zoom,
            auto_center,
            dev_plugins,
        }
    }
}

/// Result of background chart loading
struct ChartLoadResult {
    path: PathBuf,
    cell: S101Cell,
}

/// Background loading state
struct BackgroundLoadingState {
    /// Number of files being loaded
    total_files: usize,
    /// Number of files loaded so far
    loaded_count: usize,
    /// Receiver for loaded cells
    receiver: Receiver<Option<ChartLoadResult>>,
}

/// Rendered symbol info for hit testing
/// Fields ordered by size (largest first) for optimal memory layout
#[derive(Clone, Debug)]
struct RenderedSymbol {
    world_x: f64,
    world_y: f64,
    feature_id: i64,
    screen_x: f32,
    screen_y: f32,
    /// Drawing priority (higher = drawn on top, should be selected first)
    priority: i32,
    symbol_ref: String,
    /// Cell index this symbol belongs to (for correct feature lookup in multi-cell scenarios)
    cell_index: Option<u32>,
}

/// Chart viewer application for winit
struct ChartApp {
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    render_context: RenderContext,
    bounds: GeoBounds,
    /// Symbol cache for SVG symbol rendering
    symbol_cache: SymbolCache,
    /// Current color profile name (Day, Dusk, Night)
    current_profile_name: String,
    /// Current mouse position
    mouse_pos: (f64, f64),
    /// Rendered symbols for hit testing
    rendered_symbols: Vec<RenderedSymbol>,
    /// Pending async hit-test build result
    pending_hit_test: Option<Receiver<Vec<RenderedSymbol>>>,
    /// Is mouse being dragged for panning
    is_dragging: bool,
    /// Last drag position
    drag_start: (f64, f64),
    /// Current zoom level (1.0 = fit to window)
    zoom_level: f64,
    /// Pan offset in world coordinates
    pan_offset: (f64, f64),
    /// Pan velocity for inertia (world coordinates per second)
    pan_velocity: (f64, f64),
    /// Last frame time for velocity calculation
    last_frame_time: std::time::Instant,
    /// Recent mouse positions for velocity calculation (screen coords, time)
    recent_positions: Vec<((f64, f64), std::time::Instant)>,
    /// Feature Catalogue reference for attribute lookup
    fc: Arc<FeatureCatalogue>,
    /// Portrayal Catalogue reference
    pc: Arc<PortrayalCatalogue>,
    /// Feature Catalogue status (for UI display)
    fc_status: CatalogueStatus,
    /// Portrayal Catalogue status (for UI display)
    pc_status: CatalogueStatus,
    /// All loaded S101 cells
    cells: Vec<S101Cell>,
    /// Whether chart data is loaded
    chart_loaded: bool,
    /// In-process S-101 indices (catalogue / feature / geometry) for the
    /// loaded cell. Populated in `finalize_loading` from cell[0] + the FC,
    /// shared with plugins via the `HostApi::chart_*` methods. Built only
    /// when at least one cell is loaded; reset in `clear_charts`.
    indices: Option<Arc<s101_mcp::Indices>>,
    /// Paths of already loaded chart files (to prevent duplicates)
    loaded_paths: std::collections::HashSet<PathBuf>,
    /// Background loading state (Some if loading in progress)
    loading_state: Option<BackgroundLoadingState>,
    /// Plugin system
    plugin_system: plugins::PluginSystem,
    /// Base instruction count (chart instructions only, before plugin instructions)
    base_instruction_count: usize,
    /// Debug mode enabled
    debug_mode: bool,
    /// Charts to auto-load on startup
    pending_auto_chart: Vec<PathBuf>,
    /// Auto-screenshot path (take screenshot after load, then exit)
    auto_screenshot: Option<PathBuf>,
    /// Debug interior rings
    debug_rings: bool,
    /// Override zoom level for auto-screenshot
    auto_zoom: Option<f64>,
    /// Override center position for auto-screenshot (lat, lon)
    auto_center: Option<(f64, f64)>,
    /// Frame count since load completed (for auto-screenshot timing)
    frames_since_loaded: Option<u32>,
    /// Frame times for FPS calculation
    frame_times: std::collections::VecDeque<std::time::Instant>,
    /// Previous CPU time measurement (kernel_time, user_time, wall_time) in 100-nanosecond intervals
    #[cfg(windows)]
    prev_cpu_times: Option<(u64, u64, std::time::Instant)>,
    /// Last debug stats update time (for throttling to 0.5s intervals)
    last_debug_update: std::time::Instant,
    /// Zoom debounce: time of last scroll event (for deferred geometry rebuild)
    zoom_last_scroll: std::time::Instant,
    /// Zoom debounce: the zoom level at which geometry was last rebuilt
    zoom_rebuilt_level: f64,
    /// Zoom debounce phase: 0=none, 2=needs phase1, 1=phase1 done waiting for phase2
    zoom_rebuild_phase: u8,
    /// Zoom debounce: cursor position during zoom (for pivot)
    zoom_cursor_screen: (f32, f32),
    /// Pan rebuild pending: deferred rebuild after inertia/drag stops
    /// 0 = none, 1 = phase 1 done (waiting for phase 2), 2 = needs phase 1
    pan_rebuild_phase: u8,
    /// Time when pan rebuild was requested
    pan_rebuild_time: std::time::Instant,
    /// Animated zoom: target zoom level (we interpolate zoom_level toward this)
    zoom_target: f64,
    /// Whether zoom animation is active
    zoom_animating: bool,
    /// Anchor world point: the world position under cursor at zoom start.
    /// Used for drift-free zoom by directly computing pan_offset each frame
    /// instead of accumulating floating-point deltas.
    zoom_anchor_world: (f64, f64),
}

impl ChartApp {
    #[allow(clippy::too_many_arguments)]
    fn new(
        symbol_cache: SymbolCache,
        initial_profile: String,
        fc: Arc<FeatureCatalogue>,
        pc: Arc<PortrayalCatalogue>,
        fc_status: CatalogueStatus,
        pc_status: CatalogueStatus,
        debug_mode: bool,
        pending_auto_chart: Vec<PathBuf>,
        auto_screenshot: Option<PathBuf>,
        debug_rings: bool,
        auto_zoom: Option<f64>,
        auto_center: Option<(f64, f64)>,
        dev_plugins: bool,
    ) -> Self {
        ChartApp {
            window: None,
            renderer: None,
            render_context: RenderContext::new(Viewport::new(1920.0, 1080.0)),
            bounds: GeoBounds::new(-180.0, -90.0, 180.0, 90.0),
            symbol_cache,
            current_profile_name: initial_profile,
            mouse_pos: (0.0, 0.0),
            rendered_symbols: Vec::new(),
            pending_hit_test: None,
            is_dragging: false,
            drag_start: (0.0, 0.0),
            zoom_level: 1.0,
            pan_offset: (0.0, 0.0),
            pan_velocity: (0.0, 0.0),
            last_frame_time: std::time::Instant::now(),
            recent_positions: Vec::new(),
            fc,
            pc,
            fc_status,
            pc_status,
            cells: Vec::new(),
            chart_loaded: false,
            indices: None,
            loaded_paths: std::collections::HashSet::new(),
            loading_state: None,
            plugin_system: {
                let base = get_app_base_dir();
                // Resolution order — first hit wins:
                //   1. <exe_dir>/plugins         distribution next to the exe
                //                                (e.g. `target/release/plugins/`)
                //   2. <exe_dir>/plugins_out     same idea but the dev name
                //   3. <base>/plugins_out        path-walked project root, dev
                //   4. <base>/plugins            path-walked project root, dist
                //
                // Putting exe-adjacent first lets `target/release/` be a
                // self-contained dist directory that ships its own plugins
                // even when the project root upstream still has its own
                // `plugins_out/`.
                let exe_dir = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()));
                let plugins_path = exe_dir
                    .as_ref()
                    .and_then(|d| {
                        let p1 = d.join("plugins");
                        if p1.exists() {
                            return Some(p1);
                        }
                        let p2 = d.join("plugins_out");
                        if p2.exists() {
                            return Some(p2);
                        }
                        None
                    })
                    .unwrap_or_else(|| {
                        if base.join("plugins_out").exists() {
                            base.join("plugins_out")
                        } else {
                            base.join("plugins")
                        }
                    });
                info!("Plugin directory: {}", plugins_path.display());

                let mut ps = plugins::PluginSystem::new(plugins_path, VERSION, dev_plugins);
                ps.load_all();

                // Set up file dialog callbacks for plugins
                ps.set_file_save_callback(|filter_str, default_name, data| {
                    // Parse filter string "name|*.ext1;*.ext2"
                    let parts: Vec<&str> = filter_str.splitn(2, '|').collect();
                    let filter_name = *parts.first().unwrap_or(&"Files");
                    let extensions: Vec<&str> = parts
                        .get(1)
                        .unwrap_or(&"*.*")
                        .split(';')
                        .filter_map(|e| e.strip_prefix("*."))
                        .collect();

                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Save File")
                        .set_file_name(default_name)
                        .add_filter(filter_name, &extensions)
                        .save_file()
                    {
                        match std::fs::write(&path, data) {
                            Ok(_) => {
                                info!("Plugin saved file: {}", path.display());
                                true
                            }
                            Err(e) => {
                                error!("Failed to save file: {}", e);
                                false
                            }
                        }
                    } else {
                        false
                    }
                });

                ps.set_file_open_callback(|filter_str| {
                    // Parse filter string "name|*.ext1;*.ext2"
                    let parts: Vec<&str> = filter_str.splitn(2, '|').collect();
                    let filter_name = *parts.first().unwrap_or(&"Files");
                    let extensions: Vec<&str> = parts
                        .get(1)
                        .unwrap_or(&"*.*")
                        .split(';')
                        .filter_map(|e| e.strip_prefix("*."))
                        .collect();

                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Open File")
                        .add_filter(filter_name, &extensions)
                        .pick_file()
                    {
                        match std::fs::read_to_string(&path) {
                            Ok(content) => {
                                info!("Plugin opened file: {}", path.display());
                                Some(content)
                            }
                            Err(e) => {
                                error!("Failed to read file: {}", e);
                                None
                            }
                        }
                    } else {
                        None
                    }
                });

                ps
            },
            base_instruction_count: 0,
            debug_mode,
            pending_auto_chart,
            auto_screenshot,
            debug_rings,
            auto_zoom,
            auto_center,
            frames_since_loaded: None,
            frame_times: std::collections::VecDeque::with_capacity(60),
            #[cfg(windows)]
            prev_cpu_times: None,
            last_debug_update: std::time::Instant::now(),
            zoom_last_scroll: std::time::Instant::now(),
            zoom_rebuilt_level: 1.0,
            zoom_rebuild_phase: 0,
            zoom_cursor_screen: (0.0, 0.0),
            pan_rebuild_phase: 0,
            pan_rebuild_time: std::time::Instant::now(),
            zoom_target: 1.0,
            zoom_animating: false,
            zoom_anchor_world: (0.0, 0.0),
        }
    }

    /// Get current color profile
    #[allow(dead_code)]
    fn get_current_profile(&self) -> Option<&ferrite_portrayal_catalog::ColorProfile> {
        self.pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name)
    }

    /// Switch to a different color profile (Day, Dusk, Night)
    /// Clears symbol cache to force re-rendering with new colors
    fn set_color_profile(&mut self, profile_name: &str) {
        if self.pc.color_profiles.profiles.contains_key(profile_name) {
            if self.current_profile_name != profile_name {
                self.current_profile_name = profile_name.to_string();
                // Clear symbol cache to force re-rendering with new colors
                self.symbol_cache.clear();
                // Clear GPU-cached symbol textures in renderer
                if let Some(renderer) = &mut self.renderer {
                    renderer.clear_symbol_textures();
                }
                tracing::info!("Switched to color profile: {}", profile_name);

                // Fast path: remap color tokens to new RGB values without re-running Lua
                if self.chart_loaded {
                    let pc = &self.pc;
                    let pname = self.current_profile_name.clone();
                    self.render_context
                        .remap_colors(&|token: &str| lookup_pc_color(pc, token, &pname));
                }
            }
        } else {
            tracing::warn!("Color profile '{}' not found", profile_name);
        }
    }

    /// Regenerate portrayal instructions with current color profile
    /// Called when color profile changes to update Area/Line colors
    fn regenerate_portrayal(&mut self) {
        if self.cells.is_empty() {
            return;
        }
        let regen_start = std::time::Instant::now();

        let (width, height) = if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            (size.width as f32, size.height as f32)
        } else {
            (1920.0, 1080.0)
        };

        // Clear existing instructions
        self.render_context = RenderContext::new(Viewport::new(width, height));
        self.render_context.set_bounds(self.bounds);

        // Get current settings from renderer
        let current_settings = self.renderer.as_ref().map(|r| r.settings().clone());

        // Try Lua portrayal with current color profile and settings
        let lua_result = try_lua_portrayal(
            &self.cells,
            &self.fc,
            &self.pc,
            &mut self.render_context,
            &self.current_profile_name,
            current_settings.as_ref(),
        );

        if let Err(e) = lua_result {
            tracing::warn!(
                "Lua portrayal failed during profile change: {}. Using default instructions.",
                e
            );
            for cell in &self.cells {
                generate_default_instructions(
                    cell,
                    &mut self.render_context,
                    &self.pc,
                    &self.current_profile_name,
                );
            }
        }

        let regen_elapsed = regen_start.elapsed();
        tracing::info!(
            "Regenerated portrayal with {} profile ({:.2}ms)",
            self.current_profile_name,
            regen_elapsed.as_secs_f64() * 1000.0
        );
        if let Some(renderer) = &mut self.renderer {
            renderer
                .cpu_profiler
                .record("regenerate_portrayal", regen_elapsed);
        }
    }

    /// Get available color profile names
    #[allow(dead_code)]
    fn get_available_profiles(&self) -> Vec<&str> {
        self.pc
            .color_profiles
            .profiles
            .keys()
            .map(|s| s.as_str())
            .collect()
    }

    /// Get visible viewing groups for the current display mode
    /// Returns None if All mode (show everything), otherwise returns the set of visible viewing group IDs
    fn get_visible_viewing_groups(&self) -> Option<std::collections::HashSet<u32>> {
        let display_mode = self
            .renderer
            .as_ref()
            .map(|r| r.settings().display_mode)
            .unwrap_or(DisplayMode::Standard);

        // Map UI DisplayMode to PC display mode ID
        let mode_id = match display_mode {
            DisplayMode::Base => "DisplayBase",
            DisplayMode::Standard => "StandardDisplay",
            DisplayMode::All => return None, // Show all viewing groups
        };

        // Get the display mode from PC
        let Some(mode) = self.pc.display_modes.get(mode_id) else {
            tracing::debug!("Display mode '{}' not found in PC, showing all", mode_id);
            return None; // Mode not found, show all
        };

        // Collect all viewing groups from the visible layers
        let mut visible_vgs = std::collections::HashSet::new();
        for layer_id in &mode.viewing_group_layers {
            let vgs = self
                .pc
                .viewing_group_layers
                .get_viewing_groups_for_layer(layer_id);
            visible_vgs.extend(vgs);
        }

        // Always include plugin viewing group (21010) so overlays are never filtered out
        visible_vgs.insert(21010);

        // If no viewing groups found, return None to show all (safety fallback)
        if visible_vgs.is_empty() {
            tracing::warn!(
                "No viewing groups found for display mode '{}', showing all",
                mode_id
            );
            return None;
        }

        tracing::debug!(
            "Display mode '{}': {} visible viewing groups",
            mode_id,
            visible_vgs.len(),
        );

        Some(visible_vgs)
    }

    /// Start loading chart files in background (non-blocking)
    fn load_charts(&mut self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        // Don't start new loading if already loading
        if self.loading_state.is_some() {
            warn!("Loading already in progress, ignoring new load request");
            return Ok(());
        }

        // Reset bounds from world extent to empty so expand() calculates from chart data
        if !self.chart_loaded {
            self.bounds = GeoBounds::default();
        }

        // Filter out already loaded files
        let new_paths: Vec<PathBuf> = paths
            .iter()
            .filter(|p| {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                !self.loaded_paths.contains(&canonical)
            })
            .cloned()
            .collect();

        if new_paths.is_empty() {
            #[cfg(debug_assertions)]
            info!("All selected files are already loaded");
            return Ok(());
        }

        let total_files = new_paths.len();
        #[cfg(debug_assertions)]
        info!(
            "Starting background load of {} chart file(s) ({} skipped as duplicates)",
            total_files,
            paths.len() - total_files
        );

        // Create channel for receiving loaded cells
        let (tx, rx) = mpsc::channel();

        // Clone FC for background thread
        let fc = Arc::clone(&self.fc);

        // Spawn background thread for loading
        std::thread::spawn(move || {
            let fc_feature_codes = fc.feature_type_codes();

            // Load each file in the background thread
            for path in new_paths {
                #[cfg(debug_assertions)]
                info!("Background loading: {}", path.display());

                let result = match S101Cell::load(&path) {
                    Ok(mut cell) => {
                        // Normalize feature codes
                        cell.normalize_feature_codes(&fc_feature_codes);

                        #[cfg(debug_assertions)]
                        {
                            let stats = cell.statistics();
                            info!(
                                "Loaded: {} features, {} points, {} curves, {} surfaces",
                                stats.features, stats.points, stats.curves, stats.surfaces
                            );
                        }

                        Some(ChartLoadResult { path, cell })
                    }
                    Err(e) => {
                        error!("Failed to load chart {}: {}", path.display(), e);
                        None
                    }
                };

                // Send result (even None to track progress)
                if tx.send(result).is_err() {
                    // Receiver dropped, stop loading
                    break;
                }
            }
        });

        // Set loading state
        self.loading_state = Some(BackgroundLoadingState {
            total_files,
            loaded_count: 0,
            receiver: rx,
        });

        // Update UI to show loading
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loading_progress = Some((total_files, 0));
        }

        Ok(())
    }

    /// Poll for background loading completion (non-blocking)
    /// Returns true if loading is complete
    fn poll_loading(&mut self) -> bool {
        let loading_state = match &mut self.loading_state {
            Some(state) => state,
            None => return true, // No loading in progress
        };

        let mut completed = false;
        let mut new_cells = Vec::new();

        // Non-blocking receive of all available results
        loop {
            match loading_state.receiver.try_recv() {
                Ok(result) => {
                    loading_state.loaded_count += 1;

                    if let Some(load_result) = result {
                        new_cells.push(load_result);
                    }

                    // Check if all files are loaded
                    if loading_state.loaded_count >= loading_state.total_files {
                        completed = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // No more results available right now
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Sender dropped (thread finished or crashed)
                    completed = true;
                    break;
                }
            }
        }

        // Process newly loaded cells
        for load_result in new_cells {
            // Expand bounds
            for point in load_result.cell.points.values() {
                let wp = WorldPoint::new(point.position.x, point.position.y);
                self.bounds.expand(wp);
            }
            for curve in load_result.cell.curves.values() {
                for pos in curve.all_positions() {
                    let wp = WorldPoint::new(pos.x, pos.y);
                    self.bounds.expand(wp);
                }
            }

            // Track loaded path
            let canonical = load_result
                .path
                .canonicalize()
                .unwrap_or_else(|_| load_result.path.clone());
            self.loaded_paths.insert(canonical);

            // Add cell
            self.cells.push(load_result.cell);
        }

        // Update UI progress
        if let Some(renderer) = &mut self.renderer {
            if let Some(state) = &self.loading_state {
                renderer.ui_state.loading_progress = Some((state.total_files, state.loaded_count));
            }
        }

        // Finalize if complete
        if completed {
            self.finalize_loading();
        }

        completed
    }

    /// Finalize loading after all cells are loaded
    fn finalize_loading(&mut self) {
        // Clear loading state
        self.loading_state = None;

        if !self.cells.is_empty() {
            // Expand bounds by 10%
            self.bounds.expand_by_percent(0.1);
            self.chart_loaded = true;

            // Generate drawing instructions for all cells
            if let Err(e) = self.regenerate_instructions() {
                error!("Failed to generate instructions: {}", e);
            }

            // Pre-compute area triangulations to avoid cold-path stall on first render
            if let Some(renderer) = &mut self.renderer {
                renderer.precompute_triangulations(self.render_context.raw_instructions());
            }

            // Build the s101-mcp indices for the first loaded cell so
            // in-process plugins (s101-explorer) can query catalogue +
            // feature + geometry data via HostApi without re-parsing the
            // file. Single-chart only — plan2 doesn't yet cover merging
            // indices across cells.
            self.rebuild_chart_indices();
        }

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loading_progress = None;

            if self.chart_loaded {
                let chart_info = if self.cells.len() == 1 {
                    self.cells
                        .first()
                        .and_then(|c| {
                            c.file_path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                        })
                        .unwrap_or_else(|| "Chart".to_string())
                } else {
                    format!("{} charts loaded", self.cells.len())
                };
                renderer.ui_state.loaded_chart = Some(chart_info);

                let total_count: usize = self.cells.iter().map(|c| c.statistics().features).sum();
                renderer.ui_state.feature_count = total_count;
                renderer.ui_state.chart_count = self.cells.len();

                // Set compilation scale
                let min_scale = self
                    .cells
                    .iter()
                    .map(|c| c.compilation_scale)
                    .min()
                    .unwrap_or(22000);
                renderer.set_compilation_scale(min_scale);

                // Compute per-cell bounding boxes for world map masking
                let mut chart_boxes = Vec::with_capacity(self.cells.len());
                for cell in &self.cells {
                    let mut cmin_x = f64::MAX;
                    let mut cmin_y = f64::MAX;
                    let mut cmax_x = f64::MIN;
                    let mut cmax_y = f64::MIN;
                    for point in cell.points.values() {
                        let x = point.position.x;
                        let y = point.position.y;
                        if x < cmin_x {
                            cmin_x = x;
                        }
                        if y < cmin_y {
                            cmin_y = y;
                        }
                        if x > cmax_x {
                            cmax_x = x;
                        }
                        if y > cmax_y {
                            cmax_y = y;
                        }
                    }
                    for curve in cell.curves.values() {
                        for pos in curve.all_positions() {
                            if pos.x < cmin_x {
                                cmin_x = pos.x;
                            }
                            if pos.y < cmin_y {
                                cmin_y = pos.y;
                            }
                            if pos.x > cmax_x {
                                cmax_x = pos.x;
                            }
                            if pos.y > cmax_y {
                                cmax_y = pos.y;
                            }
                        }
                    }
                    if cmin_x < cmax_x && cmin_y < cmax_y {
                        chart_boxes.push((cmin_x, cmin_y, cmax_x, cmax_y));
                    }
                }
                renderer.set_world_map_chart_boxes(chart_boxes);
            }
        }

        info!(
            "Loading complete: {} charts, {} features",
            self.cells.len(),
            self.cells
                .iter()
                .map(|c| c.statistics().features)
                .sum::<usize>()
        );

        // Debug interior rings if --debug-rings
        if self.debug_rings {
            self.log_interior_ring_debug();
        }

        // Start auto-screenshot countdown (wait a few frames for rendering)
        if self.auto_screenshot.is_some() && self.chart_loaded {
            // Apply center override if specified (lat,lon → pan_offset in world coords)
            if let Some((lat, lon)) = self.auto_center {
                let chart_center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0;
                let chart_center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0;
                self.pan_offset.0 = lon - chart_center_x;
                self.pan_offset.1 = lat - chart_center_y;
            }
            // Apply zoom override if specified
            if let Some(zoom) = self.auto_zoom {
                self.zoom_level = zoom;
                self.zoom_target = zoom;
            }
            // Re-render with zoom applied
            self.update_view();
            self.frames_since_loaded = Some(0);
        }
    }

    /// Clear all loaded charts
    /// Log detailed interior ring debug info for all loaded cells
    fn log_interior_ring_debug(&self) {
        info!("=== INTERIOR RING DEBUG ===");
        for (ci, cell) in self.cells.iter().enumerate() {
            let mut surfaces_with_holes = 0;
            let mut total_interior_rings = 0;
            let mut unclosed_rings = 0;

            for surface in cell.surfaces.values() {
                if surface.interior_rings.is_empty() {
                    continue;
                }
                surfaces_with_holes += 1;
                total_interior_rings += surface.interior_rings.len();

                for (ri, ring_curves) in surface.interior_rings.iter().enumerate() {
                    // Check closure by collecting raw points
                    let mut pts = Vec::new();
                    for oc in ring_curves {
                        let key = oc.curve_id.key();
                        if let Some(curve) = cell.curves.get(&key) {
                            let positions = curve.all_positions();
                            if oc.orientation {
                                for p in &positions {
                                    pts.push((p.x, p.y));
                                }
                            } else {
                                for p in positions.iter().rev() {
                                    pts.push((p.x, p.y));
                                }
                            }
                        } else if let Some(composite) = cell.composite_curves.get(&key) {
                            for sub in &composite.curves {
                                let sk = sub.curve_id.key();
                                if let Some(c) = cell.curves.get(&sk) {
                                    let positions = c.all_positions();
                                    let forward = oc.orientation == sub.orientation;
                                    if forward {
                                        for p in &positions {
                                            pts.push((p.x, p.y));
                                        }
                                    } else {
                                        for p in positions.iter().rev() {
                                            pts.push((p.x, p.y));
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let is_closed = if pts.len() >= 2 {
                        let first = pts.first().unwrap();
                        let last = pts.last().unwrap();
                        (first.0 - last.0).abs() < 1e-7 && (first.1 - last.1).abs() < 1e-7
                    } else {
                        false
                    };

                    if !is_closed {
                        unclosed_rings += 1;
                    }

                    // Find which features reference this surface
                    let surface_key = surface.id.key();
                    let referencing_features: Vec<_> = cell
                        .features
                        .values()
                        .filter(|f| {
                            f.spatial_associations
                                .iter()
                                .any(|sa| sa.spatial_id.key() == surface_key)
                        })
                        .filter_map(|f| f.feature_code.as_deref())
                        .collect();

                    // Compute ring bounding box
                    let (mut rx0, mut ry0, mut rx1, mut ry1) =
                        (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                    for &(x, y) in &pts {
                        if x < rx0 {
                            rx0 = x;
                        }
                        if y < ry0 {
                            ry0 = y;
                        }
                        if x > rx1 {
                            rx1 = x;
                        }
                        if y > ry1 {
                            ry1 = y;
                        }
                    }

                    info!(
                        "  Cell[{}] Surface {} ring[{}]: {} curves, {} pts, closed={}, bbox=[{:.6},{:.6}]-[{:.6},{:.6}], features={:?}",
                        ci,
                        surface_key,
                        ri,
                        ring_curves.len(),
                        pts.len(),
                        is_closed,
                        rx0, ry0, rx1, ry1,
                        referencing_features,
                    );
                }
            }

            info!(
                "Cell[{}]: {} surfaces with holes, {} total interior rings, {} unclosed",
                ci, surfaces_with_holes, total_interior_rings, unclosed_rings
            );
        }
        info!("=== END INTERIOR RING DEBUG ===");
    }

    /// Rebuild the s101-mcp `Indices` from `cells[0]` + the loaded FC, then
    /// register the JSON-emitting query callbacks plugins use through
    /// `HostApi::chart_*`. Called from `finalize_loading` after a chart set
    /// is ready. Skips silently when no cell is loaded.
    fn rebuild_chart_indices(&mut self) {
        let Some(first) = self.cells.first() else {
            return;
        };
        // We need owned `S101Cell` + `FeatureCatalogue` for `Indices::build`.
        // The host keeps Vec<S101Cell> for rendering, so re-parse only when
        // necessary. For now we clone via re-load of the source file —
        // cheap enough (sub-100ms) and sidesteps the question of whether
        // S101Cell should be Clone.
        use ferrite_s100_core::S101Cell;
        let path = first.file_path.clone();
        let mut cell = match S101Cell::load(&path) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to re-load cell for index build: {}", e);
                return;
            }
        };
        cell.normalize_feature_codes(&self.fc.feature_type_codes());
        // FeatureCatalogue owns its data so we deep-clone via serde rather
        // than re-parsing the XML.
        let fc_clone: ferrite_feature_catalog::FeatureCatalogue = (*self.fc).clone();
        let indices = Arc::new(s101_mcp::Indices::build(cell, fc_clone));
        info!(
            "Chart indices ready for plugins: {} features, {} catalogue feature types",
            indices.feature.len(),
            indices.catalogue.feature_count()
        );
        self.indices = Some(indices.clone());

        // Register JSON-emitting query closures with the plugin host. Each
        // closure captures an Arc<Indices> so the host (and through it, any
        // plugin) can call queries on a stable snapshot regardless of what
        // ChartApp does next.
        let q_dataset = {
            let i = indices.clone();
            Box::new(move || s101_mcp_chart_dataset_metadata(&i))
                as Box<dyn Fn() -> String + Send + Sync>
        };
        let q_search = {
            let i = indices.clone();
            Box::new(move |term: &str, limit: u32| s101_mcp_catalogue_search(&i, term, limit))
                as Box<dyn Fn(&str, u32) -> String + Send + Sync>
        };
        let q_desc_feat = {
            let i = indices.clone();
            Box::new(move |code: &str| s101_mcp_catalogue_describe_feature(&i, code))
                as Box<dyn Fn(&str) -> String + Send + Sync>
        };
        let q_desc_attr = {
            let i = indices.clone();
            Box::new(move |code: &str| s101_mcp_catalogue_describe_attribute(&i, code))
                as Box<dyn Fn(&str) -> String + Send + Sync>
        };
        let q_get = {
            let i = indices.clone();
            Box::new(move |id: i64| s101_mcp_feature_get(&i, id))
                as Box<dyn Fn(i64) -> String + Send + Sync>
        };
        let q_bbox = {
            let i = indices.clone();
            Box::new(move |w: f64, s: f64, e: f64, n: f64, limit: u32| {
                s101_mcp_feature_query_bbox(&i, w, s, e, n, limit)
            }) as Box<dyn Fn(f64, f64, f64, f64, u32) -> String + Send + Sync>
        };
        let q_nearby = {
            let i = indices.clone();
            Box::new(move |lat: f64, lon: f64, r: f64, limit: u32| {
                s101_mcp_feature_nearby(&i, lat, lon, r, limit)
            }) as Box<dyn Fn(f64, f64, f64, u32) -> String + Send + Sync>
        };

        self.plugin_system
            .set_chart_query_callbacks(ferrite_plugin_loader::ChartQueryCallbacks {
                dataset_metadata: q_dataset,
                catalogue_search: q_search,
                catalogue_describe_feature: q_desc_feat,
                catalogue_describe_attribute: q_desc_attr,
                feature_get: q_get,
                feature_query_bbox: q_bbox,
                feature_nearby: q_nearby,
            });
        self.plugin_system.set_chart_loaded(true);
    }

    fn clear_charts(&mut self) {
        #[cfg(debug_assertions)]
        info!("Clearing all charts");

        self.cells.clear();
        self.bounds = GeoBounds::new(-180.0, -90.0, 180.0, 90.0);
        self.chart_loaded = false;
        self.indices = None;
        self.plugin_system.set_chart_loaded(false);
        self.zoom_level = 1.0;
        self.zoom_target = 1.0;
        self.zoom_animating = false;
        self.pan_offset = (0.0, 0.0);
        self.rendered_symbols.clear();
        self.loaded_paths.clear();

        // Clear render context and base instruction count
        if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            self.render_context =
                RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
        }
        self.base_instruction_count = 0;

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loaded_chart = None;
            renderer.ui_state.feature_count = 0;
            renderer.ui_state.chart_count = 0;
            renderer.ui_state.selected_feature = None;

            // Clear renderer frame
            renderer.begin_frame();
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);
        }

        info!("All charts cleared");
    }

    /// Regenerate drawing instructions from loaded cells
    /// Compute instruction cache file path for the current chart set.
    ///
    /// The cache key is built from the canonical (absolute, symlink-resolved)
    /// path of each chart so the same chart loaded via different working
    /// directories — e.g. `cargo run` vs running the exe from `target/release/`
    /// — yields the same cache hash. Without canonicalization, every distinct
    /// CWD spawned a parallel cache file for identical chart content.
    fn instruction_cache_path(&self) -> Option<PathBuf> {
        if self.cells.is_empty() {
            return None;
        }
        let first_path = &self.cells[0].file_path;
        let cache_dir = first_path.parent()?;

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for cell in &self.cells {
            let canonical =
                std::fs::canonicalize(&cell.file_path).unwrap_or_else(|_| cell.file_path.clone());
            // Lowercase on Windows: NTFS is case-insensitive but path strings
            // can vary in case, which would otherwise produce different hashes.
            #[cfg(windows)]
            let key = canonical.to_string_lossy().to_lowercase();
            #[cfg(not(windows))]
            let key = canonical.to_string_lossy().into_owned();
            key.hash(&mut hasher);
        }
        self.current_profile_name.hash(&mut hasher);
        let hash = hasher.finish();

        Some(cache_dir.join(format!(".ferrite_cache_{:016x}.bin", hash)))
    }

    /// Check if instruction cache is valid (newer than all chart files)
    fn is_cache_valid(&self, cache_path: &Path) -> bool {
        let cache_meta = match fs::metadata(cache_path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let cache_mtime = match cache_meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };
        // Cache must be newer than all chart files
        for cell in &self.cells {
            if let Ok(meta) = fs::metadata(&cell.file_path) {
                if let Ok(chart_mtime) = meta.modified() {
                    if chart_mtime > cache_mtime {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Cache file format:
    /// `[4B magic "FRC\x01"][4B schema version][32B SHA-256 hash][payload]`
    ///
    /// The schema version is incremented whenever `DrawingInstruction` struct
    /// layout changes, ensuring stale caches are rejected instead of producing
    /// corrupted rendering data.
    const CACHE_MAGIC: &'static [u8; 4] = b"FRC\x01";
    /// Bump this version whenever DrawingInstruction fields change.
    const CACHE_SCHEMA_VERSION: u32 = 2;

    /// Wrap a bincode payload with magic + schema version + SHA-256 corruption-detection hash.
    /// NOTE: This is NOT cryptographic authentication — it detects accidental corruption only.
    fn wrap_cache(payload: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(payload);
        let mut out = Vec::with_capacity(4 + 4 + 32 + payload.len());
        out.extend_from_slice(Self::CACHE_MAGIC);
        out.extend_from_slice(&Self::CACHE_SCHEMA_VERSION.to_le_bytes());
        out.extend_from_slice(&hash);
        out.extend_from_slice(payload);
        out
    }

    /// Verify integrity and deserialize a cache file.
    /// Returns Err if magic/version mismatch or SHA-256 hash doesn't match.
    fn verify_and_deserialize_cache(
        data: &[u8],
    ) -> std::result::Result<Vec<ferrite_render::DrawingInstruction>, String> {
        const HEADER_LEN: usize = 4 + 4 + 32; // magic + version + hash
        if data.len() < HEADER_LEN {
            return Err("cache file too small".into());
        }
        // Check magic
        if &data[..4] != Self::CACHE_MAGIC {
            return Err("invalid cache magic (legacy or corrupted file)".into());
        }
        // Check schema version
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        if version != Self::CACHE_SCHEMA_VERSION {
            return Err(format!(
                "schema version mismatch: file={}, expected={}",
                version,
                Self::CACHE_SCHEMA_VERSION
            ));
        }
        // Verify SHA-256 hash
        let stored_hash = &data[8..40];
        let payload = &data[HEADER_LEN..];
        let computed_hash = Sha256::digest(payload);
        if computed_hash.as_slice() != stored_hash {
            return Err("SHA-256 integrity check failed (file tampered or corrupted)".into());
        }
        // Deserialize
        bincode::deserialize(payload).map_err(|e| format!("deserialization failed: {}", e))
    }

    fn regenerate_instructions(&mut self) -> Result<()> {
        if self.cells.is_empty() {
            return Ok(());
        }

        // Get current viewport size from renderer/window
        let (width, height) = if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            (size.width as f32, size.height as f32)
        } else {
            (1920.0, 1080.0)
        };

        // Clear existing instructions
        self.render_context = RenderContext::new(Viewport::new(width, height));
        self.render_context.set_bounds(self.bounds);

        // Try to load instructions from binary cache
        let cache_path = self.instruction_cache_path();
        let mut cache_loaded = false;

        if let Some(ref cp) = cache_path {
            if self.is_cache_valid(cp) {
                let cache_start = std::time::Instant::now();
                match fs::read(cp) {
                    Ok(data) => match Self::verify_and_deserialize_cache(&data) {
                        Ok(instructions) => {
                            let count = instructions.len();
                            self.render_context
                                .set_instructions_from_cache(instructions);
                            cache_loaded = true;
                            info!(
                                "Loaded {} instructions from cache in {:.1}ms: {}",
                                count,
                                cache_start.elapsed().as_secs_f64() * 1000.0,
                                cp.display()
                            );
                        }
                        Err(e) => {
                            warn!("Cache rejected: {}. Regenerating.", e);
                            let _ = fs::remove_file(cp);
                        }
                    },
                    Err(e) => {
                        warn!("Cache read failed: {}. Regenerating.", e);
                    }
                }
            }
        }

        if !cache_loaded {
            // Get current settings from renderer
            let current_settings = self.renderer.as_ref().map(|r| r.settings().clone());

            // Try Lua portrayal with current color profile and settings
            let lua_result = try_lua_portrayal(
                &self.cells,
                &self.fc,
                &self.pc,
                &mut self.render_context,
                &self.current_profile_name,
                current_settings.as_ref(),
            );

            if let Err(e) = lua_result {
                warn!("Lua portrayal failed: {}. Using default instructions.", e);
                for cell in &self.cells {
                    generate_default_instructions(
                        cell,
                        &mut self.render_context,
                        &self.pc,
                        &self.current_profile_name,
                    );
                }
            }

            // Save instruction cache for next load
            if let Some(ref cp) = cache_path {
                let cache_start = std::time::Instant::now();
                let instructions = self.render_context.raw_instructions();
                match bincode::serialize(instructions) {
                    Ok(payload) => {
                        let signed = Self::wrap_cache(&payload);
                        let size_kb = signed.len() / 1024;
                        match fs::write(cp, &signed) {
                            Ok(_) => {
                                info!(
                                    "Saved instruction cache ({}KB) in {:.1}ms: {}",
                                    size_kb,
                                    cache_start.elapsed().as_secs_f64() * 1000.0,
                                    cp.display()
                                );
                            }
                            Err(e) => warn!("Failed to save instruction cache: {}", e),
                        }
                    }
                    Err(e) => warn!("Failed to serialize instructions: {}", e),
                }
            }
        }

        // Save base instruction count (chart instructions only, before plugin instructions)
        self.base_instruction_count = self.render_context.instruction_count();

        // Update renderer
        // Get color profile and visible viewing groups before mutable borrows
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();

        if let Some(renderer) = &mut self.renderer {
            let size = renderer.window().inner_size();
            self.render_context
                .set_viewport(size.width as f32, size.height as f32);
            self.render_context.zoom_to_fit(self.bounds);

            renderer.begin_frame();
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);
            renderer.add_instructions_with_symbols(
                &mut self.render_context,
                Some(&mut self.symbol_cache),
                color_profile,
                visible_vgs.as_ref(),
            );

            // Rebuild hit testing
            self.build_rendered_symbols();
        }

        Ok(())
    }

    /// Build rendered symbols list for hit testing
    /// Build rendered symbols asynchronously on a background thread.
    /// Results are polled via `poll_hit_test`.
    fn build_rendered_symbols(&mut self) {
        // Collect point data needed for building symbols
        let points: Vec<_> = self
            .render_context
            .get_sorted_instructions()
            .iter()
            .filter_map(|instr| {
                if let ferrite_render::DrawingInstruction::Point(point) = instr {
                    Some((
                        point.symbol_ref.clone(),
                        point.feature_id.unwrap_or(0),
                        point.position,
                        point.priority.0,
                        point.cell_index,
                    ))
                } else {
                    None
                }
            })
            .collect();

        let scaler = self.render_context.scaler.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_hit_test = Some(rx);

        std::thread::spawn(move || {
            let symbols: Vec<RenderedSymbol> = points
                .into_iter()
                .map(|(symbol_ref, feature_id, position, priority, cell_index)| {
                    let screen = scaler.world_to_screen(position);
                    RenderedSymbol {
                        symbol_ref,
                        feature_id,
                        screen_x: screen.x,
                        screen_y: screen.y,
                        world_x: position.x,
                        world_y: position.y,
                        priority,
                        cell_index,
                    }
                })
                .collect();
            let _ = tx.send(symbols);
        });
    }

    /// Poll for completed async hit-test build. Call this each frame.
    fn poll_hit_test(&mut self) {
        if let Some(rx) = &self.pending_hit_test {
            if let Ok(symbols) = rx.try_recv() {
                self.rendered_symbols = symbols;
                self.pending_hit_test = None;
            }
        }
    }

    /// Find symbols near the click position
    /// Sorted by priority (highest first = topmost visible), then by distance (closest first)
    fn find_symbols_at(&self, x: f64, y: f64, radius: f32) -> Vec<&RenderedSymbol> {
        let mut nearby: Vec<_> = self
            .rendered_symbols
            .iter()
            .filter_map(|s| {
                let dx = s.screen_x - x as f32;
                let dy = s.screen_y - y as f32;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq <= radius * radius {
                    Some((s, dist_sq))
                } else {
                    None
                }
            })
            .collect();

        // Sort by priority (highest first = topmost), then by distance (closest first)
        nearby.sort_by(|a, b| {
            // First compare by priority (higher priority = drawn on top)
            match b.0.priority.cmp(&a.0.priority) {
                std::cmp::Ordering::Equal => {
                    // Same priority: prefer closer symbol
                    a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                }
                other => other,
            }
        });

        nearby.into_iter().map(|(s, _)| s).collect()
    }

    /// Update the view based on current zoom and pan
    /// - `rebuild_hit_test`: if false, skip rebuilding the hit-test symbol list
    /// - `preserve_declutter`: if true, preserve symbol declutter grids to avoid flickering
    fn update_view_ex(&mut self, rebuild_hit_test: bool, preserve_declutter: bool) {
        let profiling = ferrite_wgpu::profiler::is_profiling_enabled();
        let update_view_start = if profiling {
            Some(std::time::Instant::now())
        } else {
            None
        };

        // Update viewport to use actual chart area (excluding UI panels)
        if let Some(renderer) = &self.renderer {
            let (x, y, w, h) = renderer.ui_state.chart_area;
            if w > 0.0 && h > 0.0 {
                self.render_context.set_viewport_rect(x, y, w, h);
            }
        }

        // Calculate the zoomed and panned bounds
        // Wrap horizontal pan offset modulo 360° so the viewport always stays near
        // the chart data. Combined with ±360° rendering copies, this enables
        // seamless infinite horizontal panning (Earth is round).
        let base_width = self.bounds.max_x - self.bounds.min_x;
        let base_height = self.bounds.max_y - self.bounds.min_y;
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        let center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0 + wrapped_pan_x;
        let center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0 + self.pan_offset.1;

        let zoomed_width = base_width / self.zoom_level;
        let zoomed_height = base_height / self.zoom_level;

        let new_bounds = GeoBounds {
            min_x: center_x - zoomed_width / 2.0,
            max_x: center_x + zoomed_width / 2.0,
            min_y: center_y - zoomed_height / 2.0,
            max_y: center_y + zoomed_height / 2.0,
        };

        self.render_context.zoom_to_fit(new_bounds);

        // Pre-compute color profile and viewing groups before mutable borrow of renderer
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();

        // Prepare plugin instructions before renderer borrow
        // Always add plugin instructions (route overlays should render even without charts)
        self.render_context
            .truncate_instructions(self.base_instruction_count);
        for instr in self.plugin_system.get_render_instructions() {
            self.render_context.add_instruction(instr);
        }

        if let Some(renderer) = &mut self.renderer {
            // Update zoom level for symbol decluttering and UI
            renderer.set_zoom_level(self.zoom_level);
            renderer.ui_state.zoom_level = self.zoom_level;
            // During animation, preserve declutter state to avoid flickering
            renderer.begin_frame_ex(preserve_declutter);

            // Draw world map coastlines as the lowest layer (before chart data)
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);

            // Chart data + plugin overlay rendering
            renderer.add_instructions_with_symbols(
                &mut self.render_context,
                Some(&mut self.symbol_cache),
                color_profile,
                visible_vgs.as_ref(),
            );
        }

        // Rebuild symbols for hit testing (skip during animation for performance)
        if rebuild_hit_test && self.chart_loaded {
            let hit_test_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };
            self.build_rendered_symbols();
            if let Some(s) = hit_test_start {
                if let Some(renderer) = &mut self.renderer {
                    renderer.cpu_profiler.record("build_hit_test", s.elapsed());
                }
            }
        }

        if let Some(s) = update_view_start {
            let elapsed = s.elapsed();
            tracing::debug!(
                "[PROFILER] update_view_ex: {:.2}ms (hit_test={})",
                elapsed.as_secs_f64() * 1000.0,
                rebuild_hit_test
            );
            if let Some(renderer) = &mut self.renderer {
                renderer.cpu_profiler.record("update_view", elapsed);
            }
        }
    }

    /// Update the view (rebuilds hit-test symbols, clears declutter grids)
    fn update_view(&mut self) {
        self.update_view_ex(true, false);
        // Sync zoom rebuild level so GPU zoom delta resets to 1.0
        self.zoom_rebuilt_level = self.zoom_level;
    }

    /// Recalculate view bounds/scaler without rebuilding geometry.
    /// Used as a lightweight step before adjusting pan offset during zoom.
    fn recalculate_view_bounds(&mut self) {
        if let Some(renderer) = &self.renderer {
            let (x, y, w, h) = renderer.ui_state.chart_area;
            if w > 0.0 && h > 0.0 {
                self.render_context.set_viewport_rect(x, y, w, h);
            }
        }
        let base_width = self.bounds.max_x - self.bounds.min_x;
        let base_height = self.bounds.max_y - self.bounds.min_y;
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        let center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0 + wrapped_pan_x;
        let center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0 + self.pan_offset.1;
        let zoomed_width = base_width / self.zoom_level;
        let zoomed_height = base_height / self.zoom_level;
        let new_bounds = GeoBounds {
            min_x: center_x - zoomed_width / 2.0,
            max_x: center_x + zoomed_width / 2.0,
            min_y: center_y - zoomed_height / 2.0,
            max_y: center_y + zoomed_height / 2.0,
        };
        self.render_context.zoom_to_fit(new_bounds);
    }
}

impl ApplicationHandler for ChartApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            // Load window icon
            let window_icon = load_window_icon();

            let mut window_attrs = Window::default_attributes()
                .with_title(format!("FerriteS100 v{} - S-101 Chart Viewer", VERSION))
                .with_inner_size(winit::dpi::LogicalSize::new(1920, 1080))
                .with_maximized(true);

            if let Some(icon) = window_icon {
                window_attrs = window_attrs.with_window_icon(Some(icon));
            }

            match event_loop.create_window(window_attrs) {
                Ok(window) => {
                    let window = Arc::new(window);
                    self.window = Some(window.clone());

                    // Create renderer asynchronously
                    match pollster::block_on(WgpuRenderer::new(window.clone())) {
                        Ok(mut renderer) => {
                            // Update render context viewport
                            let size = window.inner_size();
                            self.render_context
                                .set_viewport(size.width as f32, size.height as f32);

                            // Initialize UI state
                            renderer.ui_state.version = VERSION.to_string();
                            renderer.ui_state.zoom_level = self.zoom_level;
                            renderer.ui_state.fc_status = self.fc_status.clone();
                            renderer.ui_state.pc_status = self.pc_status.clone();
                            renderer.ui_state.debug_mode = self.debug_mode;
                            renderer.set_color_profile(&self.current_profile_name);

                            // Enable profiling only in debug mode
                            if self.debug_mode {
                                renderer.set_profiling_enabled(true);
                            }

                            // Only add instructions if chart is loaded
                            if self.chart_loaded {
                                let color_profile = self
                                    .pc
                                    .color_profiles
                                    .profiles
                                    .get(&self.current_profile_name);
                                let visible_vgs = self.get_visible_viewing_groups();
                                self.render_context.zoom_to_fit(self.bounds);
                                renderer.begin_frame();
                                renderer.set_lon_wrap_pixels(
                                    360.0 * self.render_context.scaler.scale_x() as f32,
                                );
                                renderer.add_world_map_lines(&self.render_context.scaler);
                                renderer.add_instructions_with_symbols(
                                    &mut self.render_context,
                                    Some(&mut self.symbol_cache),
                                    color_profile,
                                    visible_vgs.as_ref(),
                                );
                                self.build_rendered_symbols();
                            }

                            // Load Natural Earth world map for background rendering
                            let coastlines = parse_world_map_coastlines();
                            renderer.set_world_map(coastlines);

                            // Draw world map immediately (visible even without charts)
                            if !self.chart_loaded {
                                self.render_context.zoom_to_fit(self.bounds);
                                renderer.begin_frame();
                                renderer.set_lon_wrap_pixels(
                                    360.0 * self.render_context.scaler.scale_x() as f32,
                                );
                                renderer.add_world_map_lines(&self.render_context.scaler);
                            }

                            let stats = renderer.statistics();
                            info!(
                                "GPU Renderer initialized: {} (symbols cached: {})",
                                stats,
                                self.symbol_cache.len()
                            );

                            self.renderer = Some(renderer);

                            // Auto-load chart if --chart was specified
                            if !self.pending_auto_chart.is_empty() {
                                let paths = std::mem::take(&mut self.pending_auto_chart);
                                info!("Auto-loading {} chart file(s)", paths.len());
                                if let Err(e) = self.load_charts(&paths) {
                                    error!("Failed to auto-load chart(s): {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format!(
                                "Failed to initialize GPU renderer:\n\n{}\n\n\
                                 This may be caused by missing or outdated graphics drivers.",
                                e
                            );
                            error!("{}", msg);
                            #[cfg(all(windows, not(debug_assertions)))]
                            show_error_dialog("FerriteS100 - Renderer Error", &msg);
                            event_loop.exit();
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to create window:\n\n{}", e);
                    error!("{}", msg);
                    #[cfg(all(windows, not(debug_assertions)))]
                    show_error_dialog("FerriteS100 - Window Error", &msg);
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Intercept Tab key before egui (egui uses Tab for focus navigation)
        if let WindowEvent::KeyboardInput {
            event:
                winit::event::KeyEvent {
                    physical_key: winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Tab),
                    state: winit::event::ElementState::Pressed,
                    repeat: false,
                    ..
                },
            ..
        } = &event
        {
            self.debug_mode = !self.debug_mode;
            if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.debug_mode = self.debug_mode;
                renderer.set_profiling_enabled(self.debug_mode);
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return;
        }

        // Forward events to egui first
        let egui_consumed = if let Some(renderer) = &mut self.renderer {
            renderer.handle_egui_event(&event)
        } else {
            false
        };

        match event {
            WindowEvent::CloseRequested => {
                #[cfg(debug_assertions)]
                info!("Window close requested");
                // Flush profiler report before exit
                if let Some(renderer) = &mut self.renderer {
                    renderer.flush_profiler();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(physical_size) => {
                // Skip resize handling for minimized window (size 0x0)
                if physical_size.width == 0 || physical_size.height == 0 {
                    return;
                }

                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(physical_size);
                    // Reset renderer's screen pan offset (will be recalculated by update_view)
                    renderer.reset_pan_offset();

                    // Update render context viewport (preserve zoom level)
                    self.render_context
                        .set_viewport(physical_size.width as f32, physical_size.height as f32);

                    // Re-apply current view (zoom + pan) instead of resetting
                    // Must always update — world map lines need rebuild with new viewport
                    self.update_view();
                }
            }
            WindowEvent::RedrawRequested => {
                // Poll for completed async hit-test build
                self.poll_hit_test();

                // Process inertia/momentum
                let now = std::time::Instant::now();
                let dt = now.duration_since(self.last_frame_time).as_secs_f64();
                self.last_frame_time = now;

                // Frame profiling: begin frame
                let profiling_enabled = ferrite_wgpu::profiler::is_profiling_enabled();
                if profiling_enabled {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.cpu_profiler.begin_frame();
                    }
                }

                // Apply pan velocity (inertia) — iOS-style deceleration
                // Uses exponential decay: v(t) = v0 * decel^t
                // decel_rate ~0.998 per ms gives natural-feeling momentum
                let velocity_magnitude =
                    (self.pan_velocity.0.powi(2) + self.pan_velocity.1.powi(2)).sqrt();
                if velocity_magnitude > 0.0001 && !self.is_dragging {
                    // Apply velocity to pan offset (using current velocity BEFORE friction)
                    self.pan_offset.0 += self.pan_velocity.0 * dt;
                    self.pan_offset.1 += self.pan_velocity.1 * dt;

                    // Calculate screen-space velocity for GPU pan offset
                    // IMPORTANT: Must use the SAME velocity as pan_offset update (pre-friction)
                    // to keep screen offset and world offset synchronized
                    let screen_vx =
                        -self.pan_velocity.0 * self.render_context.scaler.scale_x() * dt;
                    let screen_vy = self.pan_velocity.1 * self.render_context.scaler.scale_y() * dt;

                    // Decelerate: exponential decay (frame-rate independent)
                    // 0.998^ms ≈ natural iOS-like scroll momentum
                    // At 120Hz (8.33ms): friction = 0.998^8.33 ≈ 0.9834
                    // At 60Hz (16.67ms): friction = 0.998^16.67 ≈ 0.9672
                    let dt_ms = dt * 1000.0;
                    let friction = 0.998_f64.powf(dt_ms);
                    self.pan_velocity.0 *= friction;
                    self.pan_velocity.1 *= friction;

                    // Check if velocity is now very small (stopping)
                    let new_magnitude =
                        (self.pan_velocity.0.powi(2) + self.pan_velocity.1.powi(2)).sqrt();
                    if new_magnitude < 0.00001 {
                        self.pan_velocity = (0.0, 0.0);
                        // Defer rebuild to avoid frame spike on stop frame
                        self.pan_rebuild_phase = 2; // needs phase 1
                        self.pan_rebuild_time = now;
                    } else {
                        // Still moving - use fast GPU pan path
                        if let Some(renderer) = &mut self.renderer {
                            renderer.add_pan_offset(screen_vx as f32, screen_vy as f32);
                        }
                    }
                }

                // Animated zoom: smoothly interpolate toward zoom_target
                if self.zoom_animating {
                    let ratio = self.zoom_target / self.zoom_level;
                    if ratio.abs() < 1e-6 || (ratio - 1.0).abs() < 0.001 {
                        // Close enough — snap to target
                        self.zoom_level = self.zoom_target;
                        self.zoom_animating = false;
                    } else {
                        // Exponential interpolation: lerp in log-space for uniform feel
                        // ~85% toward target per frame → reaches 99% in ~5 frames
                        let t = 1.0 - 0.15_f64.powf(dt * 60.0);
                        let log_current = self.zoom_level.ln();
                        let log_target = self.zoom_target.ln();
                        self.zoom_level = (log_current + (log_target - log_current) * t).exp();
                        self.zoom_level = self.zoom_level.clamp(0.005, 100.0);
                    }

                    // Apply GPU fast-path zoom
                    let (cursor_sx, cursor_sy) = self.zoom_cursor_screen;
                    let gpu_zoom = (self.zoom_level / self.zoom_rebuilt_level) as f32;
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_gpu_zoom(gpu_zoom, cursor_sx, cursor_sy);
                        renderer.ui_state.zoom_level = self.zoom_level;
                    }

                    // Drift-free zoom: directly compute pan_offset from anchor
                    // Step 1: Temporarily zero pan_offset to get the "unshifted" view
                    self.pan_offset = (0.0, 0.0);
                    self.recalculate_view_bounds();
                    // Step 2: Find where cursor maps to world in the unshifted view
                    let screen_pt = ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy);
                    let unshifted_world = self.render_context.scaler.screen_to_world(screen_pt);
                    // Step 3: Set pan_offset so anchor stays under cursor
                    self.pan_offset.0 = self.zoom_anchor_world.0 - unshifted_world.x;
                    self.pan_offset.1 = self.zoom_anchor_world.1 - unshifted_world.y;
                    // Step 4: Re-apply bounds with correct offset
                    self.recalculate_view_bounds();

                    // Keep debounce timer fresh while animating
                    if self.zoom_animating {
                        self.zoom_last_scroll = now;
                    }
                }

                // Zoom debounce: 2-phase rebuild when scrolling/animation stops
                // Phase 2→1 (80ms): Rebuild geometry + declutter, skip hit-test
                // Phase 1→0 (300ms): Rebuild hit-test only (geometry already correct)
                if self.zoom_rebuild_phase == 2 {
                    let elapsed = now.duration_since(self.zoom_last_scroll);
                    if elapsed.as_millis() >= 80 && !self.zoom_animating {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.reset_pan_offset();
                        }
                        self.update_view_ex(false, false);
                        self.zoom_rebuilt_level = self.zoom_level;
                        self.zoom_rebuild_phase = 1;
                    }
                } else if self.zoom_rebuild_phase == 1 {
                    let elapsed = now.duration_since(self.zoom_last_scroll);
                    if elapsed.as_millis() >= 300 {
                        // Hit-test only (geometry unchanged since Phase 1)
                        self.build_rendered_symbols();
                        self.zoom_rebuild_phase = 0;
                    }
                }

                // Deferred pan rebuild after inertia/drag stops
                // Phase 2→1: Rebuild geometry + declutter, skip hit-test
                // Phase 1→0 (150ms): Rebuild hit-test only
                if self.pan_rebuild_phase == 2 {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.update_view_ex(false, false);
                    self.pan_rebuild_phase = 1;
                } else if self.pan_rebuild_phase == 1 {
                    let elapsed = now.duration_since(self.pan_rebuild_time);
                    if elapsed.as_millis() >= 150 {
                        // Hit-test only (geometry unchanged since Phase 1)
                        self.build_rendered_symbols();
                        self.pan_rebuild_phase = 0;
                    }
                }

                // Poll for background loading completion
                if self.loading_state.is_some() {
                    self.poll_loading();
                }

                // Collect UI requests first (to avoid borrow conflicts)
                let (
                    open_file,
                    open_fc,
                    open_pc,
                    screenshot,
                    zoom_in,
                    zoom_out,
                    reset_view,
                    clear_charts,
                    color_change,
                    settings_change,
                    plugin_toggle,
                ) = {
                    if let Some(renderer) = &mut self.renderer {
                        (
                            renderer.take_open_file_request(),
                            renderer.take_open_fc_request(),
                            renderer.take_open_pc_request(),
                            renderer.take_screenshot_request(),
                            renderer.take_zoom_in_request(),
                            renderer.take_zoom_out_request(),
                            renderer.take_reset_view_request(),
                            renderer.take_clear_charts_request(),
                            renderer.take_color_profile_change(),
                            renderer.take_settings_change(),
                            renderer.take_plugin_toggle_request(),
                        )
                    } else {
                        (
                            false, false, false, false, false, false, false, false, None, None,
                            None,
                        )
                    }
                };

                // Process UI requests
                if open_file {
                    let paths = rfd::FileDialog::new()
                        .add_filter("S-101 Chart", &["000"])
                        .set_title("Open S-101 Chart(s) - Hold Ctrl/Shift to select multiple")
                        .pick_files()
                        .unwrap_or_default();

                    #[cfg(debug_assertions)]
                    {
                        info!("File dialog returned {} files", paths.len());
                        for (i, p) in paths.iter().enumerate() {
                            info!("  [{}] {}", i, p.display());
                        }
                    }

                    if !paths.is_empty() {
                        if let Err(e) = self.load_charts(&paths) {
                            error!("Failed to load chart(s): {}", e);
                        }
                    }
                }

                // Handle Feature Catalogue open request
                if open_fc {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Feature Catalogue XML", &["xml"])
                        .set_title("Open Feature Catalogue")
                        .pick_folder()
                    {
                        match load_feature_catalogue(&path) {
                            Ok(new_fc) => {
                                info!("Loaded FC: {} v{}", new_fc.product_id, new_fc.version);
                                self.fc_status = validate_fc(&new_fc, &path);
                                self.fc = Arc::new(new_fc);
                                if let Some(renderer) = &mut self.renderer {
                                    renderer.ui_state.fc_status = self.fc_status.clone();
                                }
                                // Reload charts with new FC if any are loaded
                                if self.chart_loaded {
                                    self.update_view();
                                }
                            }
                            Err(e) => error!("Failed to load Feature Catalogue: {}", e),
                        }
                    }
                }

                // Handle Portrayal Catalogue open request
                if open_pc {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Open Portrayal Catalogue Directory")
                        .pick_folder()
                    {
                        match load_portrayal_catalogue(&path) {
                            Ok(new_pc) => {
                                info!("Loaded PC: {} v{}", new_pc.product_id, new_pc.version);
                                self.pc_status = validate_pc(&new_pc, &path);

                                // Reload symbol cache with new PC
                                let symbols_path = path.join("Symbols");
                                self.symbol_cache = SymbolCache::new(&symbols_path);
                                self.pc = Arc::new(new_pc);

                                if let Some(renderer) = &mut self.renderer {
                                    renderer.ui_state.pc_status = self.pc_status.clone();
                                }
                                // Reload charts with new PC if any are loaded
                                if self.chart_loaded {
                                    self.update_view();
                                }
                            }
                            Err(e) => error!("Failed to load Portrayal Catalogue: {}", e),
                        }
                    }
                }

                if screenshot {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("PNG Image", &["png"])
                        .set_title("Save Screenshot")
                        .save_file()
                    {
                        if let Some(renderer) = &mut self.renderer {
                            match renderer.save_screenshot(&path) {
                                Ok(_) => info!("Screenshot saved to: {}", path.display()),
                                Err(e) => error!("Failed to save screenshot: {}", e),
                            }
                        }
                    }
                }

                if zoom_in {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = (self.zoom_level * 1.5).min(50.0);
                    self.zoom_target = self.zoom_level;
                    self.zoom_animating = false;
                    self.update_view();
                }

                if zoom_out {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = (self.zoom_level / 1.5).max(0.1);
                    self.zoom_target = self.zoom_level;
                    self.zoom_animating = false;
                    self.update_view();
                }

                if reset_view {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = 1.0;
                    self.zoom_target = 1.0;
                    self.zoom_animating = false;
                    self.pan_offset = (0.0, 0.0);
                    self.pan_velocity = (0.0, 0.0); // Stop inertia on reset
                    self.update_view();
                }

                if clear_charts {
                    self.clear_charts();
                    // Deactivate all plugins (close panels) and clear plugin data
                    self.plugin_system.deactivate_all_plugins();
                    self.plugin_system.clear_all_data();
                }

                // Handle color profile change
                if let Some(new_profile) = color_change {
                    self.set_color_profile(&new_profile);
                    // Force re-render with new colors
                    if self.chart_loaded {
                        self.update_view();
                    }
                }

                // Handle settings change (S-101 context parameters)
                if settings_change.is_some() {
                    // Settings have been updated in UI state, regenerate portrayal
                    tracing::info!("Settings changed, regenerating portrayal");
                    if self.chart_loaded {
                        self.regenerate_portrayal();
                        // Pre-compute triangulations for new instructions
                        if let Some(renderer) = &mut self.renderer {
                            renderer
                                .precompute_triangulations(self.render_context.raw_instructions());
                        }
                    }
                }

                // Handle plugin toggle request (only when chart is loaded)
                if let Some(plugin_id) = plugin_toggle {
                    if self.chart_loaded {
                        self.plugin_system.toggle_plugin(&plugin_id);
                    }
                }

                // Handle pan adjustment when panel state changes (to keep chart visually centered)
                if let Some(renderer) = &mut self.renderer {
                    if let Some(adjust_pixels) = renderer.take_pan_adjust_pixels() {
                        // Convert pixel adjustment to world coordinates
                        let world_adjust =
                            adjust_pixels as f64 / self.render_context.scaler.scale_x();
                        self.pan_offset.0 += world_adjust;
                        // Force view update with new pan offset
                        if self.chart_loaded {
                            self.update_view();
                        }
                    }
                }

                // Update plugin toolbar buttons in UI
                if let Some(renderer) = &mut self.renderer {
                    let buttons: Vec<_> = self
                        .plugin_system
                        .get_toolbar_buttons()
                        .into_iter()
                        .map(|btn| ferrite_wgpu::PluginButton {
                            plugin_id: btn.plugin_id,
                            label: btn.label,
                            tooltip: btn.tooltip,
                            active: btn.active,
                        })
                        .collect();
                    renderer.set_plugin_buttons(buttons);

                    // Update plugin UI data
                    let ui_data = self.plugin_system.get_active_plugin_ui_data();
                    renderer.set_plugin_ui_data(ui_data);

                    // Process plugin UI events
                    for (plugin_id, event_json) in renderer.take_plugin_ui_events() {
                        self.plugin_system.send_ui_event(&plugin_id, &event_json);
                        // Update view after UI event
                        if self.chart_loaded {
                            self.update_view();
                        }
                    }
                }

                // Update debug stats
                if self.debug_mode {
                    if let Some(renderer) = &mut self.renderer {
                        // FPS calculation (always update for accurate measurement)
                        self.frame_times.push_back(now);
                        while self.frame_times.len() > 60 {
                            self.frame_times.pop_front();
                        }

                        // Throttle other stats to update every 0.5 seconds
                        let debug_update_interval = std::time::Duration::from_millis(500);
                        let should_update_stats =
                            now.duration_since(self.last_debug_update) >= debug_update_interval;

                        if should_update_stats {
                            self.last_debug_update = now;

                            // Calculate FPS from accumulated frame times
                            if self.frame_times.len() >= 2 {
                                let oldest = self.frame_times.front().unwrap();
                                let elapsed = now.duration_since(*oldest).as_secs_f32();
                                renderer.ui_state.debug_fps =
                                    (self.frame_times.len() - 1) as f32 / elapsed;
                            }

                            // Memory and CPU usage (Windows only)
                            #[cfg(windows)]
                            {
                                use std::mem::MaybeUninit;
                                use windows_sys::Win32::Foundation::FILETIME;
                                use windows_sys::Win32::System::ProcessStatus::{
                                    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
                                };
                                use windows_sys::Win32::System::Threading::{
                                    GetCurrentProcess, GetProcessTimes,
                                };

                                unsafe {
                                    let process = GetCurrentProcess();

                                    // Memory usage (Working Set - physical memory used)
                                    let mut pmc = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
                                    if GetProcessMemoryInfo(
                                        process,
                                        pmc.as_mut_ptr(),
                                        std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                                    ) != 0
                                    {
                                        let pmc = pmc.assume_init();
                                        renderer.ui_state.debug_memory_mb =
                                            pmc.WorkingSetSize as f32 / (1024.0 * 1024.0);
                                    }

                                    // CPU usage calculation
                                    let mut creation_time = MaybeUninit::<FILETIME>::zeroed();
                                    let mut exit_time = MaybeUninit::<FILETIME>::zeroed();
                                    let mut kernel_time = MaybeUninit::<FILETIME>::zeroed();
                                    let mut user_time = MaybeUninit::<FILETIME>::zeroed();

                                    if GetProcessTimes(
                                        process,
                                        creation_time.as_mut_ptr(),
                                        exit_time.as_mut_ptr(),
                                        kernel_time.as_mut_ptr(),
                                        user_time.as_mut_ptr(),
                                    ) != 0
                                    {
                                        let kernel = kernel_time.assume_init();
                                        let user = user_time.assume_init();

                                        // Convert FILETIME to u64 (100-nanosecond intervals)
                                        let kernel_100ns = ((kernel.dwHighDateTime as u64) << 32)
                                            | (kernel.dwLowDateTime as u64);
                                        let user_100ns = ((user.dwHighDateTime as u64) << 32)
                                            | (user.dwLowDateTime as u64);

                                        if let Some((prev_kernel, prev_user, prev_time)) =
                                            self.prev_cpu_times
                                        {
                                            let wall_elapsed =
                                                now.duration_since(prev_time).as_nanos() as u64
                                                    / 100;
                                            if wall_elapsed > 0 {
                                                let cpu_elapsed = (kernel_100ns - prev_kernel)
                                                    + (user_100ns - prev_user);
                                                let num_cpus = std::thread::available_parallelism()
                                                    .map(|n| n.get())
                                                    .unwrap_or(1)
                                                    as f32;
                                                renderer.ui_state.debug_cpu_usage =
                                                    (cpu_elapsed as f32 / wall_elapsed as f32)
                                                        * 100.0
                                                        / num_cpus;
                                            }
                                        }

                                        self.prev_cpu_times = Some((kernel_100ns, user_100ns, now));
                                    }
                                }
                            }

                            // Instruction and symbol counts
                            renderer.ui_state.debug_instruction_count =
                                self.render_context.instruction_count();
                            renderer.ui_state.debug_symbol_count = self.rendered_symbols.len();
                        }
                    }
                }

                // Render
                if let Some(renderer) = &mut self.renderer {
                    if let Err(e) = renderer.render() {
                        error!("Render error: {}", e);
                    }

                    // Frame profiling: end frame (logs periodic report)
                    if profiling_enabled {
                        renderer.cpu_profiler.end_frame();
                    }
                }

                // Auto-screenshot: wait a few frames after load for rendering to stabilize
                if let Some(count) = &mut self.frames_since_loaded {
                    *count += 1;
                    if *count >= 5 {
                        if let Some(path) = self.auto_screenshot.take() {
                            info!("Auto-screenshot: saving to {}", path.display());
                            if let Some(renderer) = &mut self.renderer {
                                match renderer.save_screenshot(&path) {
                                    Ok(_) => info!("Screenshot saved successfully"),
                                    Err(e) => error!("Screenshot failed: {}", e),
                                }
                            }
                            self.frames_since_loaded = None;
                            // Exit after screenshot
                            event_loop.exit();
                            return;
                        }
                    }
                }

                // Request next frame only when needed (on-demand rendering)
                // During drag/inertia: keep requesting frames at VSync rate for smooth motion
                let needs_redraw = {
                    let has_inertia =
                        self.pan_velocity.0.abs() > 0.00001 || self.pan_velocity.1.abs() > 0.00001;
                    let has_loading = self.loading_state.is_some();
                    let has_screenshot_pending = self.frames_since_loaded.is_some();
                    let has_zoom_pending = self.zoom_rebuild_phase > 0;
                    let egui_needs = self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.egui_needs_repaint());
                    let has_pan_rebuild = self.pan_rebuild_phase > 0;
                    self.is_dragging
                        || self.zoom_animating
                        || has_inertia
                        || has_loading
                        || has_screenshot_pending
                        || has_zoom_pending
                        || has_pan_rebuild
                        || egui_needs
                };
                if needs_redraw {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let new_pos = (position.x, position.y);
                let now = std::time::Instant::now();

                // Update UI state with cursor position
                if let Some(renderer) = &mut self.renderer {
                    let screen_pt =
                        ferrite_render::ScreenPoint::new(new_pos.0 as f32, new_pos.1 as f32);
                    let world = self.render_context.scaler.screen_to_world(screen_pt);
                    renderer.set_cursor_world(world.x, world.y);
                    renderer.set_cursor_screen(new_pos.0 as f32, new_pos.1 as f32);
                }

                // Handle panning when dragging (only if egui didn't consume)
                if !egui_consumed && self.is_dragging {
                    let dx = new_pos.0 - self.mouse_pos.0;
                    let dy = new_pos.1 - self.mouse_pos.1;

                    // Track world-space pan offset for final calculation
                    let world_dx = -dx / self.render_context.scaler.scale_x();
                    let world_dy = dy / self.render_context.scaler.scale_y();
                    self.pan_offset.0 += world_dx;
                    self.pan_offset.1 += world_dy;

                    // Track recent positions for velocity calculation (keep last 100ms worth)
                    self.recent_positions.push((new_pos, now));
                    self.recent_positions
                        .retain(|(_, t)| now.duration_since(*t).as_millis() < 100);

                    // Stop any existing inertia when actively dragging
                    self.pan_velocity = (0.0, 0.0);
                    self.pan_rebuild_phase = 0;

                    // FAST PATH: Use GPU pan offset instead of rebuilding vertices
                    // This is much faster than update_view_ex which rebuilds all geometry
                    if let Some(renderer) = &mut self.renderer {
                        renderer.add_pan_offset(dx as f32, dy as f32);
                    }
                }

                self.mouse_pos = new_pos;

                // Request redraw for cursor updates and drag rendering
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !egui_consumed => {
                let scroll_amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(pos) => pos.y / 50.0,
                };

                // Accumulate into zoom target (animated zoom will interpolate toward it)
                let zoom_factor = 1.0 + scroll_amount * 0.15;
                if !self.zoom_animating {
                    self.zoom_target = self.zoom_level;
                }
                self.zoom_target = (self.zoom_target * zoom_factor).clamp(0.005, 100.0);
                self.zoom_animating = true;

                // Record cursor as zoom pivot + anchor world point
                let cursor_sx = self.mouse_pos.0 as f32;
                let cursor_sy = self.mouse_pos.1 as f32;
                self.zoom_cursor_screen = (cursor_sx, cursor_sy);
                // Capture the world point under cursor — used for drift-free zoom
                let anchor = self
                    .render_context
                    .scaler
                    .screen_to_world(ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy));
                self.zoom_anchor_world = (anchor.x, anchor.y);

                // Mark rebuild pending — will execute when animation stops
                self.zoom_last_scroll = std::time::Instant::now();
                self.zoom_rebuild_phase = 2;

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if !egui_consumed => {
                match state {
                    ElementState::Pressed => {
                        // If inertia was active, stop it and rebuild view immediately
                        // so the scaler reflects the actual pan offset (not the stale GPU offset)
                        let had_inertia =
                            self.pan_velocity.0.abs() > 0.001 || self.pan_velocity.1.abs() > 0.001;
                        self.is_dragging = true;
                        self.drag_start = self.mouse_pos;
                        // Clear recent positions and velocity on new drag
                        self.recent_positions.clear();
                        self.pan_velocity = (0.0, 0.0);
                        if had_inertia {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.reset_pan_offset();
                            }
                            self.update_view();
                            self.pan_rebuild_phase = 0;
                        }
                    }
                    ElementState::Released => {
                        let was_dragging = self.is_dragging;
                        self.is_dragging = false;

                        let drag_dist = ((self.mouse_pos.0 - self.drag_start.0).powi(2)
                            + (self.mouse_pos.1 - self.drag_start.1).powi(2))
                        .sqrt();

                        // Calculate velocity for inertia from recent positions
                        let mut inertia_applied = false;
                        if was_dragging && drag_dist >= 5.0 && self.recent_positions.len() >= 2 {
                            // Use positions from recent history to calculate velocity
                            if let (Some(first), Some(last)) =
                                (self.recent_positions.first(), self.recent_positions.last())
                            {
                                let dt = last.1.duration_since(first.1).as_secs_f64();
                                if dt > 0.001 {
                                    // Screen-space velocity
                                    let vx = (last.0 .0 - first.0 .0) / dt;
                                    let vy = (last.0 .1 - first.0 .1) / dt;

                                    // Convert to world-space velocity
                                    let world_vx = -vx / self.render_context.scaler.scale_x();
                                    let world_vy = vy / self.render_context.scaler.scale_y();

                                    // Apply velocity with damping factor for natural feel
                                    // 0.7 gives good momentum while preventing overshoot
                                    self.pan_velocity = (world_vx * 0.7, world_vy * 0.7);
                                    inertia_applied = true;
                                }
                            }
                            self.recent_positions.clear();
                        }

                        // If drag ended without inertia, defer rebuild
                        if was_dragging && !inertia_applied {
                            self.pan_rebuild_phase = 2;
                            self.pan_rebuild_time = std::time::Instant::now();
                        }

                        // Suppress click during inertia — coordinates are stale while map is moving
                        let has_inertia =
                            self.pan_velocity.0.abs() > 0.001 || self.pan_velocity.1.abs() > 0.001;

                        if (!was_dragging || drag_dist < 5.0) && !has_inertia {
                            // Check if egui wants the pointer (click is on UI)
                            let egui_wants = self
                                .renderer
                                .as_ref()
                                .is_some_and(|r| r.egui_wants_pointer());

                            // Skip chart/plugin handling if click was on UI or no chart loaded
                            if !egui_wants && self.chart_loaded {
                                let (x, y) = self.mouse_pos;
                                info!("Click: screen=({:.1}, {:.1})", x, y);
                                let screen_pt =
                                    ferrite_render::ScreenPoint::new(x as f32, y as f32);
                                let world = self.render_context.scaler.screen_to_world(screen_pt);
                                info!("Click: world=({:.6}, {:.6})", world.x, world.y);

                                // Route click to plugins first
                                let plugin_consumed = self.plugin_system.handle_click(
                                    world.x,
                                    world.y,
                                    ferrite_plugin_api::MouseButton::Left,
                                    false,
                                );

                                // If plugin consumed the event, skip default handling but update view
                                if plugin_consumed {
                                    info!(
                                        "Click consumed by plugin at world ({:.6}, {:.6})",
                                        world.x, world.y
                                    );
                                    // Update view to render plugin's new drawing instructions
                                    self.update_view();
                                } else {
                                    // Hit testing (default behavior)
                                    // Find nearby symbols (sorted by priority then distance)
                                    let nearby = self.find_symbols_at(x, y, 20.0);

                                    let selected = nearby.first().map(|sym| {
                                        // Use cell_index to look up feature in the correct cell
                                        // This fixes the bug where multiple cells have the same feature_id
                                        // but different feature types
                                        let feature = if let Some(cell_idx) = sym.cell_index {
                                            // Look up in the specific cell the symbol came from
                                            self.cells
                                                .get(cell_idx as usize)
                                                .and_then(|cell| cell.features.get(&sym.feature_id))
                                        } else {
                                            // Fallback: search all cells (old behavior)
                                            self.cells
                                                .iter()
                                                .find_map(|cell| cell.features.get(&sym.feature_id))
                                        };

                                        let (feature_code, definition) = feature
                                            .map(|f| {
                                                let code = f
                                                    .feature_code
                                                    .as_deref()
                                                    .unwrap_or(&sym.symbol_ref);
                                                // Look up definition from FC
                                                let def = self
                                                    .fc
                                                    .feature_types
                                                    .get(code)
                                                    .and_then(|ft| ft.definition.clone());
                                                (code.to_string(), def)
                                            })
                                            .unwrap_or_else(|| (sym.symbol_ref.clone(), None));

                                        SelectedFeature {
                                            feature_type: feature_code,
                                            feature_id: sym.feature_id,
                                            primitive_type: "Point".to_string(),
                                            attributes: vec![],
                                            world_pos: (sym.world_x, sym.world_y),
                                            definition,
                                            symbol_name: Some(sym.symbol_ref.clone()),
                                        }
                                    });
                                    let nearby_count = nearby.len();
                                    drop(nearby); // Release borrow on self.rendered_symbols

                                    // Update selected feature in UI
                                    if let Some(renderer) = &mut self.renderer {
                                        renderer.ui_state.selected_feature = selected;
                                    }

                                    info!(
                                        "Click at ({:.4}, {:.4}): {} symbols found",
                                        world.x, world.y, nearby_count
                                    );
                                }
                            }
                        } // end if !egui_wants
                    }
                }

                // Request redraw after click/release events
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } if !egui_consumed => {
                // Check if egui wants the pointer (click is on UI)
                let egui_wants = self
                    .renderer
                    .as_ref()
                    .is_some_and(|r| r.egui_wants_pointer());

                // Skip if click was on UI
                if !egui_wants {
                    // Route right-click to plugins first (only when chart is loaded)
                    let plugin_consumed = if self.chart_loaded {
                        let screen_pt = ferrite_render::ScreenPoint::new(
                            self.mouse_pos.0 as f32,
                            self.mouse_pos.1 as f32,
                        );
                        let world = self.render_context.scaler.screen_to_world(screen_pt);
                        let consumed = self.plugin_system.handle_click(
                            world.x,
                            world.y,
                            ferrite_plugin_api::MouseButton::Right,
                            false,
                        );
                        if consumed {
                            info!(
                                "Right-click consumed by plugin at ({:.4}, {:.4})",
                                world.x, world.y
                            );
                            self.update_view();
                        }
                        consumed
                    } else {
                        false
                    };

                    if !plugin_consumed {
                        // Plugin didn't consume, do default behavior (reset view)
                        if let Some(renderer) = &mut self.renderer {
                            renderer.reset_pan_offset();
                        }
                        self.zoom_level = 1.0;
                        self.zoom_target = 1.0;
                        self.zoom_animating = false;
                        self.pan_offset = (0.0, 0.0);
                        self.pan_velocity = (0.0, 0.0); // Stop inertia on reset
                        self.update_view();
                    }
                } // end if !egui_wants

                // Request redraw after right-click
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {
                // For any other window event (keyboard, etc.), request redraw
                // to ensure egui UI updates are rendered
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }
}

fn main() {
    // Install panic hook to show error dialog in release mode (no console window)
    #[cfg(all(windows, not(debug_assertions)))]
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = if let Some(msg) = info.payload().downcast_ref::<&str>() {
                format!("FerriteS100 crashed:\n\n{}", msg)
            } else if let Some(msg) = info.payload().downcast_ref::<String>() {
                format!("FerriteS100 crashed:\n\n{}", msg)
            } else {
                "FerriteS100 crashed with an unknown error.".to_string()
            };
            let message = if let Some(loc) = info.location() {
                format!("{}\n\nLocation: {}:{}", message, loc.file(), loc.line())
            } else {
                message
            };
            show_error_dialog("FerriteS100 - Fatal Error", &message);
            default_hook(info);
        }));
    }

    if let Err(e) = run_app() {
        let msg = format!("FerriteS100 failed to start:\n\n{:?}", e);
        error!("{}", msg);

        #[cfg(all(windows, not(debug_assertions)))]
        show_error_dialog("FerriteS100 - Startup Error", &msg);

        #[cfg(any(not(windows), debug_assertions))]
        eprintln!("{}", msg);

        std::process::exit(1);
    }
}

fn run_app() -> Result<()> {
    let config = AppConfig::from_args();

    // Initialize logging only in debug mode
    if config.debug_mode {
        init_logging(&config.log_path)?;
    }

    info!("========================================");
    info!("FerriteS100 v{} Starting...", VERSION);
    info!("App base directory: {}", get_app_base_dir().display());
    info!("========================================");
    info!("");
    info!("=== Loading Catalogues ===");

    let fc = Arc::new(load_feature_catalogue(&config.fc_path)?);
    let pc = Arc::new(load_portrayal_catalogue(&config.pc_path)?);

    // Validate and create status for FC/PC
    let fc_status = validate_fc(&fc, &config.fc_path);
    let pc_status = validate_pc(&pc, &config.pc_path);

    #[cfg(debug_assertions)]
    {
        info!("");
        info!("Feature Catalogue:");
        info!("  - {} feature types", fc.feature_types.len());
        info!("  - {} simple attributes", fc.simple_attributes.len());
        info!("  - {} complex attributes", fc.complex_attributes.len());
        if let Some(ref msg) = fc_status.validation_message {
            info!("  - Validation: {}", msg);
        }
        info!("");
        info!("Portrayal Catalogue:");
        info!("  - {} color profiles", pc.color_profiles.profiles.len());
        info!("  - {} symbols", pc.symbols.symbols.len());
        info!("  - {} line styles", pc.line_styles.len());
        info!("  - {} area fills", pc.area_fills.len());
        if let Some(ref msg) = pc_status.validation_message {
            info!("  - Validation: {}", msg);
        }
        info!("");
        info!("========================================");
        info!("Starting GUI - Use File > Open to load chart");
        info!("========================================");
    }

    // Create symbol cache for SVG rendering (S-101 only)
    let symbols_path = config.pc_path.join("Symbols");
    let symbol_cache = SymbolCache::new(&symbols_path);
    #[cfg(debug_assertions)]
    info!("Symbol cache initialized: {}", symbols_path.display());

    // Get default color profile name from PC
    let initial_profile = pc
        .color_profiles
        .default_profile
        .clone()
        .unwrap_or_else(|| "Day".to_string());
    #[cfg(debug_assertions)]
    {
        info!(
            "Available color profiles: {:?}",
            pc.color_profiles.profiles.keys().collect::<Vec<_>>()
        );
        info!("Initial color profile: {}", initial_profile);
    }

    let event_loop = EventLoop::new().context("Failed to create event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait); // Use Wait for lower CPU/GPU usage; request_redraw triggers frames on demand

    let mut app = ChartApp::new(
        symbol_cache,
        initial_profile,
        fc,
        pc,
        fc_status,
        pc_status,
        config.debug_mode,
        config.auto_chart,
        config.auto_screenshot,
        config.debug_rings,
        config.auto_zoom,
        config.auto_center,
        config.dev_plugins,
    );

    event_loop.run_app(&mut app).context("Event loop error")?;

    Ok(())
}

/// Try to execute Lua portrayal rules and convert to drawing instructions
/// Context parameters are loaded dynamically from PC XML (no hardcoding)
/// Processes each cell separately to avoid feature ID collisions across cells
fn try_lua_portrayal(
    cells: &[S101Cell],
    fc: &FeatureCatalogue,
    pc: &PortrayalCatalogue,
    render_context: &mut RenderContext,
    profile_name: &str,
    settings: Option<&SettingsState>,
) -> Result<()> {
    let rules_path = pc.root_path.join("Rules");

    if !rules_path.exists() {
        return Err(anyhow::anyhow!(
            "Rules directory not found: {}",
            rules_path.display()
        ));
    }

    info!("Initializing Lua portrayal engine...");

    // Create portrayal engine
    let mut engine =
        PortrayalEngine::new(&rules_path).context("Failed to create portrayal engine")?;

    // Set type catalogue from FC
    let type_catalogue = TypeCatalogue::from_feature_catalogue(fc);
    engine.set_type_catalogue(type_catalogue);
    info!(
        "  Type catalogue loaded: {} feature types, {} attributes",
        fc.feature_types.len(),
        fc.simple_attributes.len()
    );

    // Initialize engine (load main.lua)
    engine
        .initialize()
        .context("Failed to initialize portrayal engine")?;
    info!("  Lua engine initialized");

    // Load context parameters from PC XML (dynamically, no hardcoding)
    let pc_context_params = pc.get_context_parameters();
    let mut context = LuaContextParameters::from_pc_context(pc_context_params);

    // Apply UI settings to context parameters if provided
    if let Some(s) = settings {
        context.safety_depth = s.safety_depth;
        context.safety_contour = s.safety_contour;
        context.shallow_contour = s.shallow_contour;
        context.deep_contour = s.deep_contour;
        context.two_shades = s.two_shades;
        context.simplified_symbols = s.simplified_symbols;
        context.isolated_dangers = s.isolated_dangers;
        context.full_sectors = s.full_light_sectors;
        context.ignore_scale_minimum = s.ignore_scale_minimum;
        context.ignore_scamin = s.ignore_scale_minimum;
        context.symbolized_boundaries = !s.plain_boundaries;
        info!(
            "  Applied UI settings: SafetyDepth={}, SafetyContour={}, TwoShades={}",
            s.safety_depth, s.safety_contour, s.two_shades
        );
    }

    info!(
        "  Context parameters loaded from PC XML: {} parameters",
        pc_context_params.len()
    );

    let mut total_results = 0;

    // Process each cell separately to avoid feature ID collisions
    for (cell_index, cell) in cells.iter().enumerate() {
        debug!(
            "Processing cell {}: {}",
            cell_index,
            cell.file_path.display()
        );

        // Create portrayal context for this cell
        let portrayal_context = PortrayalContext::from_cell(cell, context.clone());
        let cell_data_arc = portrayal_context.cell_data();
        let cell_data_guard = cell_data_arc.read().unwrap();

        // Process cell through Lua
        match engine.process_cell(&cell_data_guard, context.clone()) {
            Ok(results) => {
                debug!(
                    "  Cell {} produced {} portrayal results",
                    cell_index,
                    results.len()
                );
                total_results += results.len();

                // Convert THIS cell's Lua results using ONLY this cell's data
                // Pass cell_index so symbols can be looked up in the correct cell
                convert_lua_results_for_cell(
                    &results,
                    cell,
                    pc,
                    render_context,
                    cell_index,
                    profile_name,
                );
            }
            Err(e) => {
                warn!("  Cell {} portrayal failed: {}", cell_index, e);
            }
        }
    }

    info!("Lua portrayal complete: {} total results", total_results);
    Ok(())
}

/// Convert Lua portrayal results to drawing instructions for a single cell
/// Uses only this cell's data to avoid feature ID collisions across cells
fn convert_lua_results_for_cell(
    results: &[ferrite_lua::PortrayalResult],
    cell: &S101Cell,
    pc: &PortrayalCatalogue,
    context: &mut RenderContext,
    cell_index: usize,
    profile_name: &str,
) {
    use ferrite_lua::DrawingCommand;

    // Helper: convert Lua visibility scale fields to ScaleRange
    let make_scale_range = |vis: &ferrite_lua::VisibilityState| -> ferrite_render::ScaleRange {
        ferrite_render::ScaleRange {
            scale_minimum: vis.scale_minimum,
            scale_maximum: vis.scale_maximum,
        }
    };

    // Helper: convert Lua DisplayPlane to render DisplayPlane
    let make_display_plane = |vis: &ferrite_lua::VisibilityState| -> ferrite_render::DisplayPlane {
        match vis.display_plane {
            ferrite_lua::DisplayPlane::OverRadar => ferrite_render::DisplayPlane::OverRadar,
            _ => ferrite_render::DisplayPlane::UnderRadar,
        }
    };

    // Helper: extract primary viewing group from visibility state
    let make_viewing_group = |vis: &ferrite_lua::VisibilityState| -> u32 {
        vis.viewing_groups.first().copied().unwrap_or(21010)
    };

    // Helper to lookup color from token (from PC colorProfile.xml)
    // Uses the specified profile (Day/Dusk/Night) for color resolution
    let lookup_color = |token: &str| -> Color { lookup_pc_color(pc, token, profile_name) };

    // Helper: collect points from a ring (list of oriented curves)
    let collect_ring_points = |curves: &[ferrite_s100_core::OrientedCurve]| -> Vec<WorldPoint> {
        let mut points = Vec::new();
        for oriented_curve in curves {
            let curve_key = oriented_curve.curve_id.key();
            if let Some(curve) = cell.curves.get(&curve_key) {
                let positions = curve.all_positions();
                if oriented_curve.orientation {
                    for pos in positions {
                        points.push(WorldPoint::new(pos.x, pos.y));
                    }
                } else {
                    for pos in positions.into_iter().rev() {
                        points.push(WorldPoint::new(pos.x, pos.y));
                    }
                }
            } else if let Some(composite) = cell.composite_curves.get(&curve_key) {
                for sub_curve in &composite.curves {
                    let sub_key = sub_curve.curve_id.key();
                    if let Some(curve) = cell.curves.get(&sub_key) {
                        let positions = curve.all_positions();
                        let forward = oriented_curve.orientation == sub_curve.orientation;
                        if forward {
                            for pos in positions {
                                points.push(WorldPoint::new(pos.x, pos.y));
                            }
                        } else {
                            for pos in positions.into_iter().rev() {
                                points.push(WorldPoint::new(pos.x, pos.y));
                            }
                        }
                    }
                }
            }
        }
        // Remove duplicate consecutive points
        let mut cleaned = Vec::with_capacity(points.len());
        for point in points {
            if cleaned.is_empty() {
                cleaned.push(point);
            } else {
                let last = cleaned.last().unwrap();
                if (point.x - last.x).abs() > 1e-9 || (point.y - last.y).abs() > 1e-9 {
                    cleaned.push(point);
                }
            }
        }
        // Remove duplicate closing point
        if cleaned.len() > 3 {
            let first = cleaned.first().unwrap();
            let last = cleaned.last().unwrap();
            if (first.x - last.x).abs() < 1e-9 && (first.y - last.y).abs() < 1e-9 {
                cleaned.pop();
            }
        }
        cleaned
    };

    // Helper: collect exterior + validated interior rings from a surface.
    // Only includes interior rings whose bounding box is fully inside
    // the exterior ring's bounding box (prevents earcut failures).
    let collect_surface_points =
        |surface: &ferrite_s100_core::SurfaceRecord| -> (Vec<WorldPoint>, Vec<Vec<WorldPoint>>) {
            let exterior = collect_ring_points(&surface.exterior_ring);
            if exterior.len() < 3 || surface.interior_rings.is_empty() {
                return (exterior, Vec::new());
            }

            // Compute exterior bounding box
            let (mut ex0, mut ey0, mut ex1, mut ey1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for p in &exterior {
                if p.x < ex0 {
                    ex0 = p.x;
                }
                if p.y < ey0 {
                    ey0 = p.y;
                }
                if p.x > ex1 {
                    ex1 = p.x;
                }
                if p.y > ey1 {
                    ey1 = p.y;
                }
            }

            let mut valid_interiors = Vec::new();
            for ring_curves in &surface.interior_rings {
                if ring_curves.is_empty() {
                    continue;
                }
                // Verify ring closure from raw curve endpoints before collecting points.
                // collect_ring_points removes the closing duplicate so we can't check after.
                let is_closed = {
                    // Get first point of first curve
                    let first_oc = &ring_curves[0];
                    let last_oc = &ring_curves[ring_curves.len() - 1];
                    let first_key = first_oc.curve_id.key();
                    let last_key = last_oc.curve_id.key();

                    let get_positions = |key: i64,
                                         oc: &ferrite_s100_core::OrientedCurve|
                     -> Option<Vec<WorldPoint>> {
                        if let Some(curve) = cell.curves.get(&key) {
                            let pos = curve.all_positions();
                            if pos.is_empty() {
                                return None;
                            }
                            let pts: Vec<WorldPoint> = if oc.orientation {
                                pos.iter().map(|p| WorldPoint::new(p.x, p.y)).collect()
                            } else {
                                pos.iter()
                                    .rev()
                                    .map(|p| WorldPoint::new(p.x, p.y))
                                    .collect()
                            };
                            Some(pts)
                        } else if let Some(composite) = cell.composite_curves.get(&key) {
                            // Get first/last sub-curve points
                            let mut pts = Vec::new();
                            for sub in &composite.curves {
                                let sk = sub.curve_id.key();
                                if let Some(c) = cell.curves.get(&sk) {
                                    let p = c.all_positions();
                                    let forward = oc.orientation == sub.orientation;
                                    if forward {
                                        pts.extend(p.iter().map(|p| WorldPoint::new(p.x, p.y)));
                                    } else {
                                        pts.extend(
                                            p.iter().rev().map(|p| WorldPoint::new(p.x, p.y)),
                                        );
                                    }
                                }
                            }
                            if pts.is_empty() {
                                None
                            } else {
                                Some(pts)
                            }
                        } else {
                            None
                        }
                    };

                    match (
                        get_positions(first_key, first_oc),
                        get_positions(last_key, last_oc),
                    ) {
                        (Some(first_pts), Some(last_pts)) => {
                            let start = first_pts.first().unwrap();
                            let end = last_pts.last().unwrap();
                            (start.x - end.x).abs() < 1e-5 && (start.y - end.y).abs() < 1e-5
                        }
                        _ => false,
                    }
                };

                if !is_closed {
                    continue;
                }

                let ring = collect_ring_points(ring_curves);
                if ring.len() < 3 {
                    continue;
                }
                // Check that ring bbox is inside exterior bbox
                let mut inside = true;
                for p in &ring {
                    if p.x < ex0 || p.x > ex1 || p.y < ey0 || p.y > ey1 {
                        inside = false;
                        break;
                    }
                }
                if inside {
                    valid_interiors.push(ring);
                }
            }
            (exterior, valid_interiors)
        };

    // Helper: convert h_align string to render enum
    let parse_h_align = |s: &str| -> HAlign {
        match s {
            "Center" | "centre" => HAlign::Center,
            "End" | "right" => HAlign::Right,
            _ => HAlign::Left,
        }
    };

    // Helper: convert v_align string to render enum
    let parse_v_align = |s: &str| -> VAlign {
        match s {
            "Top" | "top" => VAlign::Top,
            "Bottom" | "bottom" => VAlign::Bottom,
            _ => VAlign::Middle,
        }
    };

    let mut area_count = 0;
    let mut area_rendered = 0;
    let mut line_count = 0;
    let mut point_count = 0;

    for result in results {
        // Parse feature ID from the result (format: "type|id")
        let feature_id: Option<i64> = result
            .feature_id
            .split('|')
            .next_back()
            .and_then(|s| s.parse().ok());

        // Get feature from THIS cell only (no collision with other cells)
        let feature = feature_id.and_then(|id| cell.features.get(&id));

        for instruction in &result.instructions {
            for cmd in &instruction.commands {
                match cmd {
                    DrawingCommand::PointInstruction {
                        symbol_ref,
                        rotation,
                        scale,
                        position,
                        line_placement,
                        visibility,
                        ..
                    } => {
                        point_count += 1;

                        // If explicit position is available (from AugmentedPoint), use it
                        // This is used for Sounding features where each point has specific coordinates
                        if let Some((x, y)) = position {
                            // For soundings: look up depth from multi_points spatial data
                            // The depth is used for decluttering (keep shallowest for safety)
                            let depth = feature.and_then(|f| {
                                // Find the MultiPoint spatial association
                                for spas in &f.spatial_associations {
                                    if let Some(mp) = cell.multi_points.get(&spas.spatial_id.key())
                                    {
                                        // Find the position with matching coordinates
                                        for coord in &mp.positions {
                                            // Use small epsilon for floating point comparison
                                            if (coord.x - x).abs() < 1e-9
                                                && (coord.y - y).abs() < 1e-9
                                            {
                                                return coord.depth();
                                            }
                                        }
                                    }
                                }
                                None
                            });

                            let mut point_inst =
                                PointInstruction::new(symbol_ref.clone(), WorldPoint::new(*x, *y))
                                    .with_rotation(*rotation)
                                    .with_scale(*scale)
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0))
                                    .with_cell_index(cell_index);

                            // Add depth for sounding decluttering (shallowest wins for safety)
                            if let Some(d) = depth {
                                point_inst = point_inst.with_depth(d);
                            }

                            context.add_instruction(ferrite_render::DrawingInstruction::Point(
                                point_inst,
                            ));
                        } else if let Some(feature) = feature {
                            // S-100 Part 9a LinePlacement: when the feature has curve geometry,
                            // place the symbol at a specific position along the curve.
                            let has_curve_geometry = feature.spatial_associations.iter().any(|s| {
                                let key = s.spatial_id.key();
                                cell.curves.contains_key(&key)
                                    || cell.composite_curves.contains_key(&key)
                            });

                            if has_curve_geometry {
                                if let Some((mode, offset)) = line_placement {
                                    // Collect all curve points from the feature's spatial associations
                                    let mut curve_points: Vec<(f64, f64)> = Vec::new();
                                    for spas in &feature.spatial_associations {
                                        let key = spas.spatial_id.key();
                                        let forward = spas.ornt != 2; // ornt=2 means reverse
                                        if let Some(curve) = cell.curves.get(&key) {
                                            let positions = curve.all_positions();
                                            if forward {
                                                for pos in &positions {
                                                    curve_points.push((pos.x, pos.y));
                                                }
                                            } else {
                                                for pos in positions.iter().rev() {
                                                    curve_points.push((pos.x, pos.y));
                                                }
                                            }
                                        } else if let Some(composite) =
                                            cell.composite_curves.get(&key)
                                        {
                                            for sub_curve in &composite.curves {
                                                let sub_key = sub_curve.curve_id.key();
                                                if let Some(curve) = cell.curves.get(&sub_key) {
                                                    let positions = curve.all_positions();
                                                    let sub_forward =
                                                        forward == sub_curve.orientation;
                                                    if sub_forward {
                                                        for pos in &positions {
                                                            curve_points.push((pos.x, pos.y));
                                                        }
                                                    } else {
                                                        for pos in positions.iter().rev() {
                                                            curve_points.push((pos.x, pos.y));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Remove consecutive duplicates
                                    curve_points.dedup_by(|a, b| {
                                        (a.0 - b.0).abs() < 1e-12 && (a.1 - b.1).abs() < 1e-12
                                    });

                                    if curve_points.len() >= 2 {
                                        // Calculate cumulative segment lengths along the curve
                                        let mut seg_lengths =
                                            Vec::with_capacity(curve_points.len() - 1);
                                        let mut total_length = 0.0_f64;
                                        for i in 1..curve_points.len() {
                                            let dx = curve_points[i].0 - curve_points[i - 1].0;
                                            let dy = curve_points[i].1 - curve_points[i - 1].1;
                                            let len = (dx * dx + dy * dy).sqrt();
                                            seg_lengths.push(len);
                                            total_length += len;
                                        }

                                        if total_length > 0.0 {
                                            // Determine the target distance along the curve
                                            let target_dist = if mode == "Relative" {
                                                offset.clamp(0.0, 1.0) * total_length
                                            } else {
                                                // Absolute mode: offset is in mm.
                                                // Convert mm to approximate geographic degrees.
                                                // At the feature's latitude, 1 degree longitude ~
                                                // 111320 * cos(lat) meters.
                                                // Use midpoint latitude for the conversion.
                                                let mid_lat =
                                                    curve_points.iter().map(|p| p.1).sum::<f64>()
                                                        / curve_points.len() as f64;
                                                let meters_per_deg =
                                                    111_320.0 * mid_lat.to_radians().cos();
                                                let mm_to_deg = 1.0 / (meters_per_deg * 1000.0);
                                                let abs_dist = *offset * mm_to_deg;
                                                abs_dist.min(total_length)
                                            };

                                            // Walk along segments to find the interpolated point
                                            let mut accum = 0.0_f64;
                                            let mut placed = false;
                                            for (i, &seg_len) in seg_lengths.iter().enumerate() {
                                                if accum + seg_len >= target_dist {
                                                    // Interpolate within this segment
                                                    let t = if seg_len > 0.0 {
                                                        (target_dist - accum) / seg_len
                                                    } else {
                                                        0.0
                                                    };
                                                    let px = curve_points[i].0
                                                        + t * (curve_points[i + 1].0
                                                            - curve_points[i].0);
                                                    let py = curve_points[i].1
                                                        + t * (curve_points[i + 1].1
                                                            - curve_points[i].1);

                                                    let point_inst = PointInstruction::new(
                                                        symbol_ref.clone(),
                                                        WorldPoint::new(px, py),
                                                    )
                                                    .with_rotation(*rotation)
                                                    .with_scale(*scale)
                                                    .with_priority(visibility.drawing_priority)
                                                    .with_viewing_group(make_viewing_group(
                                                        visibility,
                                                    ))
                                                    .with_scale_range(make_scale_range(visibility))
                                                    .with_display_plane(make_display_plane(
                                                        visibility,
                                                    ))
                                                    .with_feature_id(feature_id.unwrap_or(0))
                                                    .with_cell_index(cell_index);

                                                    context.add_instruction(
                                                        ferrite_render::DrawingInstruction::Point(
                                                            point_inst,
                                                        ),
                                                    );
                                                    placed = true;
                                                    break;
                                                }
                                                accum += seg_len;
                                            }
                                            // If rounding prevented placement, use last point
                                            if !placed {
                                                let last = curve_points.last().unwrap();
                                                let point_inst = PointInstruction::new(
                                                    symbol_ref.clone(),
                                                    WorldPoint::new(last.0, last.1),
                                                )
                                                .with_rotation(*rotation)
                                                .with_scale(*scale)
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_group(make_viewing_group(visibility))
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0))
                                                .with_cell_index(cell_index);

                                                context.add_instruction(
                                                    ferrite_render::DrawingInstruction::Point(
                                                        point_inst,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Point or Surface geometry: place symbol at point position
                                // or surface centroid (S-100 Part 9a: area features with
                                // PointInstruction place the symbol at the area centroid)
                                let mut placed = false;

                                // Try point geometry first
                                for spas in &feature.spatial_associations {
                                    if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                                        let point_inst = PointInstruction::new(
                                            symbol_ref.clone(),
                                            WorldPoint::new(point.position.x, point.position.y),
                                        )
                                        .with_rotation(*rotation)
                                        .with_scale(*scale)
                                        .with_priority(visibility.drawing_priority)
                                        .with_viewing_group(make_viewing_group(visibility))
                                        .with_scale_range(make_scale_range(visibility))
                                        .with_display_plane(make_display_plane(visibility))
                                        .with_feature_id(feature_id.unwrap_or(0))
                                        .with_cell_index(cell_index);

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Point(point_inst),
                                        );
                                        placed = true;
                                    }
                                }

                                // Surface geometry: compute centroid and place symbol there
                                if !placed {
                                    for spas in &feature.spatial_associations {
                                        if let Some(surface) =
                                            cell.surfaces.get(&spas.spatial_id.key())
                                        {
                                            let ext = collect_ring_points(&surface.exterior_ring);
                                            if ext.len() >= 3 {
                                                // Compute polygon centroid using the shoelace formula
                                                let mut cx = 0.0_f64;
                                                let mut cy = 0.0_f64;
                                                let mut area2 = 0.0_f64;
                                                let n = ext.len();
                                                for i in 0..n {
                                                    let j = (i + 1) % n;
                                                    let cross =
                                                        ext[i].x * ext[j].y - ext[j].x * ext[i].y;
                                                    cx += (ext[i].x + ext[j].x) * cross;
                                                    cy += (ext[i].y + ext[j].y) * cross;
                                                    area2 += cross;
                                                }
                                                if area2.abs() > 1e-15 {
                                                    cx /= 3.0 * area2;
                                                    cy /= 3.0 * area2;
                                                } else {
                                                    // Degenerate polygon: use average of points
                                                    cx = ext.iter().map(|p| p.x).sum::<f64>()
                                                        / n as f64;
                                                    cy = ext.iter().map(|p| p.y).sum::<f64>()
                                                        / n as f64;
                                                }

                                                let point_inst = PointInstruction::new(
                                                    symbol_ref.clone(),
                                                    WorldPoint::new(cx, cy),
                                                )
                                                .with_rotation(*rotation)
                                                .with_scale(*scale)
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_group(make_viewing_group(visibility))
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0))
                                                .with_cell_index(cell_index);

                                                context.add_instruction(
                                                    ferrite_render::DrawingInstruction::Point(
                                                        point_inst,
                                                    ),
                                                );
                                                break; // One centroid symbol per feature
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::LineInstruction {
                        style_refs,
                        simple_style,
                        augmented_ray,
                        augmented_segments,
                        visibility,
                        ..
                    }
                    | DrawingCommand::LineInstructionUnsuppressed {
                        style_refs,
                        simple_style,
                        augmented_ray,
                        augmented_segments,
                        visibility,
                        ..
                    } => {
                        // S-100 Part 9-11.1.9: LineInstructionUnsuppressed cannot be
                        // suppressed by higher-priority lines on the same curve.
                        let unsuppressed =
                            matches!(cmd, DrawingCommand::LineInstructionUnsuppressed { .. });

                        line_count += 1;
                        // Determine line color and width from PC (no hardcoding)
                        let (color, width, line_color_token) =
                            if let Some((w, token)) = simple_style {
                                (lookup_color(token), *w, token.to_string())
                            } else if let Some(ref_name) = style_refs.first() {
                                // Look up from PC line styles
                                if let Some(style) = pc.line_styles.get(ref_name) {
                                    match style {
                                        ferrite_portrayal_catalog::LineStyle::Simple(s) => (
                                            lookup_color(&s.pen.color_token),
                                            s.pen.width as f32,
                                            s.pen.color_token.clone(),
                                        ),
                                        ferrite_portrayal_catalog::LineStyle::Complex(c) => {
                                            if let Some(s) = c.strokes.first() {
                                                (
                                                    lookup_color(&s.pen.color_token),
                                                    s.pen.width as f32,
                                                    s.pen.color_token.clone(),
                                                )
                                            } else {
                                                (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                                            }
                                        }
                                        ferrite_portrayal_catalog::LineStyle::Composite(c) => {
                                            if let Some(s) = c.components.first() {
                                                (
                                                    lookup_color(&s.pen.color_token),
                                                    s.pen.width as f32,
                                                    s.pen.color_token.clone(),
                                                )
                                            } else {
                                                (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                                            }
                                        }
                                    }
                                } else {
                                    (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                                }
                            } else {
                                (lookup_color("CSTLN"), 1.0, "CSTLN".to_string())
                            };

                        // S-100 Part 9a-11.2.15: AugmentedRay — a line from the point
                        // feature's position in a given direction for a given length.
                        // Used for light sector lines, bearing lines, etc.
                        if let Some(ray) = augmented_ray {
                            // Find the feature's point position as the ray origin.
                            let origin = feature.and_then(|f| {
                                for spas in &f.spatial_associations {
                                    if let Some(pt) = cell.points.get(&spas.spatial_id.key()) {
                                        return Some((pt.position.x, pt.position.y));
                                    }
                                }
                                None
                            });

                            if let Some((ox, oy)) = origin {
                                // S-100: direction is degrees clockwise from north
                                // (GeographicCRS) or from positive y-axis (PortrayalCRS/LocalCRS).
                                // The trigonometric conversion is the same for both.
                                let dir_rad = ray.direction.to_radians();

                                // Compute endpoint based on length CRS.
                                let (ex, ey) = if ray.length_crs == "GeographicCRS" {
                                    // Length is in metres; convert to approximate degrees.
                                    // 1 degree latitude ~ 111320 m.
                                    // 1 degree longitude ~ 111320 * cos(lat) m.
                                    let lat_rad = oy.to_radians();
                                    let cos_lat = lat_rad.cos();
                                    let dy_deg = (ray.length * dir_rad.cos()) / 111_320.0;
                                    let dx_deg = if cos_lat.abs() > 1e-10 {
                                        (ray.length * dir_rad.sin()) / (111_320.0 * cos_lat)
                                    } else {
                                        0.0
                                    };
                                    (ox + dx_deg, oy + dy_deg)
                                } else {
                                    // PortrayalCRS or LocalCRS: length is in mm (screen space).
                                    // Convert mm to metres using the cell's compilation scale,
                                    // then to approximate degrees.
                                    let scale = cell.compilation_scale as f64;
                                    let length_m = ray.length * scale / 1000.0;
                                    let lat_rad = oy.to_radians();
                                    let cos_lat = lat_rad.cos();
                                    let dy_deg = (length_m * dir_rad.cos()) / 111_320.0;
                                    let dx_deg = if cos_lat.abs() > 1e-10 {
                                        (length_m * dir_rad.sin()) / (111_320.0 * cos_lat)
                                    } else {
                                        0.0
                                    };
                                    (ox + dx_deg, oy + dy_deg)
                                };

                                let points = vec![WorldPoint::new(ox, oy), WorldPoint::new(ex, ey)];

                                let mut line_inst = LineInstruction::new(points)
                                    .with_style(LineStyle::solid(color, width))
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0));
                                line_inst.color_token = Some(line_color_token.clone());

                                if unsuppressed {
                                    line_inst = line_inst.with_unsuppressed();
                                }

                                context.add_instruction(ferrite_render::DrawingInstruction::Line(
                                    line_inst,
                                ));
                            }
                        } else if !augmented_segments.is_empty() {
                            // S-100 Part 9a-11.2.16: AugmentedPath — line from path segments.
                            // Find the feature's point position as the local origin for
                            // LocalCRS/PortrayalCRS segments.
                            let origin = feature.and_then(|f| {
                                for spas in &f.spatial_associations {
                                    if let Some(pt) = cell.points.get(&spas.spatial_id.key()) {
                                        return Some((pt.position.x, pt.position.y));
                                    }
                                }
                                None
                            });

                            let scale = cell.compilation_scale as f64;

                            for seg in augmented_segments {
                                match seg {
                                    ferrite_lua::PathSegment::Polyline(pts) => {
                                        let points: Vec<WorldPoint> = pts
                                            .iter()
                                            .map(|(x, y)| WorldPoint::new(*x, *y))
                                            .collect();
                                        if points.len() >= 2 {
                                            let mut line_inst = LineInstruction::new(points)
                                                .with_style(LineStyle::solid(color, width))
                                                .with_priority(visibility.drawing_priority)
                                                .with_viewing_group(make_viewing_group(visibility))
                                                .with_scale_range(make_scale_range(visibility))
                                                .with_display_plane(make_display_plane(visibility))
                                                .with_feature_id(feature_id.unwrap_or(0));
                                            line_inst.color_token = Some(line_color_token.clone());
                                            if unsuppressed {
                                                line_inst = line_inst.with_unsuppressed();
                                            }
                                            context.add_instruction(
                                                ferrite_render::DrawingInstruction::Line(line_inst),
                                            );
                                        }
                                    }
                                    ferrite_lua::PathSegment::ArcByRadius {
                                        center,
                                        radius,
                                        start_angle,
                                        angular_distance,
                                    } => {
                                        // Arc center/radius are typically in LocalCRS (mm from
                                        // feature point).  Convert to geographic coordinates.
                                        if let Some((ox, oy)) = origin {
                                            let lat_rad = oy.to_radians();
                                            let cos_lat = lat_rad.cos();
                                            let r_m = radius * scale / 1000.0;
                                            let cx_deg = if cos_lat.abs() > 1e-10 {
                                                ox + (center.0 * scale / 1000.0)
                                                    / (111_320.0 * cos_lat)
                                            } else {
                                                ox
                                            };
                                            let cy_deg =
                                                oy + (center.1 * scale / 1000.0) / 111_320.0;
                                            let r_deg = r_m / 111_320.0;

                                            // Tessellate arc into polyline segments
                                            let step_count = ((angular_distance.abs() / 5.0).ceil()
                                                as usize)
                                                .max(8);
                                            let mut arc_pts = Vec::with_capacity(step_count + 1);
                                            for i in 0..=step_count {
                                                let frac = i as f64 / step_count as f64;
                                                let angle_deg =
                                                    start_angle + angular_distance * frac;
                                                let angle_rad = angle_deg.to_radians();
                                                // Angles are clockwise from north/+Y
                                                let px = if cos_lat.abs() > 1e-10 {
                                                    cx_deg + r_deg * angle_rad.sin() / cos_lat
                                                } else {
                                                    cx_deg
                                                };
                                                let py = cy_deg + r_deg * angle_rad.cos();
                                                arc_pts.push(WorldPoint::new(px, py));
                                            }

                                            if arc_pts.len() >= 2 {
                                                let mut line_inst = LineInstruction::new(arc_pts)
                                                    .with_style(LineStyle::solid(color, width))
                                                    .with_priority(visibility.drawing_priority)
                                                    .with_viewing_group(make_viewing_group(
                                                        visibility,
                                                    ))
                                                    .with_scale_range(make_scale_range(visibility))
                                                    .with_display_plane(make_display_plane(
                                                        visibility,
                                                    ))
                                                    .with_feature_id(feature_id.unwrap_or(0));
                                                line_inst.color_token =
                                                    Some(line_color_token.clone());
                                                if unsuppressed {
                                                    line_inst = line_inst.with_unsuppressed();
                                                }
                                                context.add_instruction(
                                                    ferrite_render::DrawingInstruction::Line(
                                                        line_inst,
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                    ferrite_lua::PathSegment::Arc3Points { start, median, end } => {
                                        // Approximate 3-point arc: compute the circle through
                                        // the three points and tessellate.
                                        let points = vec![
                                            WorldPoint::new(start.0, start.1),
                                            WorldPoint::new(median.0, median.1),
                                            WorldPoint::new(end.0, end.1),
                                        ];
                                        let mut line_inst = LineInstruction::new(points)
                                            .with_style(LineStyle::solid(color, width))
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        line_inst.color_token = Some(line_color_token.clone());
                                        if unsuppressed {
                                            line_inst = line_inst.with_unsuppressed();
                                        }
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Line(line_inst),
                                        );
                                    }
                                    ferrite_lua::PathSegment::Annulus { .. } => {
                                        // Annulus is primarily for area fills; not applicable
                                        // to line rendering.
                                    }
                                }
                            }
                        } else if let Some(feature) = feature {
                            // Default: get coordinates from feature's spatial associations
                            for spas in &feature.spatial_associations {
                                // S-101 4.8.3: Edge masking — check if this curve is
                                // suppressed for this feature.
                                // mask=2 in SPAS means "suppress portrayal" of this edge.
                                if spas.mask == 2 {
                                    continue;
                                }
                                // Also check the MASK field records (MIND=2 = suppress)
                                let masked_by_mask_field = feature.masks.iter().any(|m| {
                                    m.spatial_id.key() == spas.spatial_id.key() && m.mask_type == 2
                                });
                                if masked_by_mask_field {
                                    continue;
                                }

                                if let Some(curve) = cell.curves.get(&spas.spatial_id.key()) {
                                    let points: Vec<WorldPoint> = curve
                                        .all_positions()
                                        .iter()
                                        .map(|c| WorldPoint::new(c.x, c.y))
                                        .collect();

                                    if points.len() >= 2 {
                                        let mut line_inst = LineInstruction::new(points)
                                            .with_style(LineStyle::solid(color, width))
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        line_inst.color_token = Some(line_color_token.clone());

                                        if unsuppressed {
                                            line_inst = line_inst.with_unsuppressed();
                                        }

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Line(line_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::ColorFill {
                        color_token,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        let color = lookup_color(color_token);
                        let draw_priority = visibility.drawing_priority;
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_solid_fill_token(color, color_token)
                                            .with_priority(draw_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::AreaFillReference {
                        reference,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        let draw_priority = visibility.drawing_priority;

                        // Look up fill type from PC
                        let fill = pc.area_fills.get(reference.as_str());

                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior.clone())
                                            .with_interiors(interiors.clone())
                                            .with_priority(draw_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));

                                        let area_inst = if let Some(fill) = fill {
                                            match &fill.fill_type {
                                                ferrite_portrayal_catalog::AreaFillType::Color(c) => {
                                                    area_inst.with_solid_fill_token(lookup_color(&c.color_token), &c.color_token)
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Hatch(h) => {
                                                    area_inst.with_hatch_fill_token(
                                                        lookup_color(&h.line_color),
                                                        &h.line_color,
                                                        h.line_width as f32,
                                                        h.spacing as f32,
                                                        h.angle as f32,
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Symbol(s) => {
                                                    area_inst.with_pattern_fill(
                                                        s.symbol_ref.clone(),
                                                        (s.v1.x as f32, s.v1.y as f32),
                                                        (s.v2.x as f32, s.v2.y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pattern(p) => {
                                                    area_inst.with_pattern_fill(
                                                        p.symbol_ref.clone(),
                                                        (p.spacing_x as f32, 0.0),
                                                        (0.0, p.spacing_y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pixmap(px) => {
                                                    tracing::warn!("AreaFillReference: raster pixmap fill not yet renderable (image: {:?}), falling back to NODTA", px.image_ref);
                                                    area_inst.with_solid_fill_token(lookup_color("NODTA"), "NODTA")
                                                }
                                            }
                                        } else {
                                            area_inst.with_solid_fill_token(
                                                lookup_color("NODTA"),
                                                "NODTA",
                                            )
                                        };

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::PixmapFill {
                        reference,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        let draw_priority = visibility.drawing_priority;

                        let fill = pc.area_fills.get(reference.as_str());

                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior.clone())
                                            .with_interiors(interiors.clone())
                                            .with_priority(draw_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));

                                        let area_inst = if let Some(fill) = fill {
                                            match &fill.fill_type {
                                                ferrite_portrayal_catalog::AreaFillType::Color(c) => {
                                                    area_inst.with_solid_fill_token(lookup_color(&c.color_token), &c.color_token)
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Hatch(h) => {
                                                    area_inst.with_hatch_fill_token(
                                                        lookup_color(&h.line_color),
                                                        &h.line_color,
                                                        h.line_width as f32,
                                                        h.spacing as f32,
                                                        h.angle as f32,
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Symbol(s) => {
                                                    area_inst.with_pattern_fill(
                                                        s.symbol_ref.clone(),
                                                        (s.v1.x as f32, s.v1.y as f32),
                                                        (s.v2.x as f32, s.v2.y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pattern(p) => {
                                                    area_inst.with_pattern_fill(
                                                        p.symbol_ref.clone(),
                                                        (p.spacing_x as f32, 0.0),
                                                        (0.0, p.spacing_y as f32),
                                                    )
                                                }
                                                ferrite_portrayal_catalog::AreaFillType::Pixmap(px) => {
                                                    tracing::warn!("PixmapFill: raster pixmap fill not yet renderable (image: {:?}), falling back to NODTA", px.image_ref);
                                                    area_inst.with_solid_fill_token(lookup_color("NODTA"), "NODTA")
                                                }
                                            }
                                        } else {
                                            area_inst.with_solid_fill_token(
                                                lookup_color("NODTA"),
                                                "NODTA",
                                            )
                                        };

                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::SymbolFill {
                        symbol,
                        v1,
                        v2,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        // S-100 Part 9a: v1/v2 define parallelogram lattice for symbol tiling
                        let v1f = (v1.0 as f32, v1.1 as f32);
                        let v2f = (v2.0 as f32, v2.1 as f32);
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_pattern_fill(symbol.clone(), v1f, v2f)
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::HatchFill {
                        direction,
                        distance,
                        line_styles,
                        visibility,
                        ..
                    } => {
                        area_count += 1;
                        // Look up first line style from PC for color and width
                        let (color, line_width_mm, hatch_token) = line_styles
                            .first()
                            .and_then(|name| pc.line_styles.get(name.as_str()))
                            .map(|style| match style {
                                ferrite_portrayal_catalog::LineStyle::Simple(s) => (
                                    lookup_color(&s.pen.color_token),
                                    s.pen.width as f32,
                                    s.pen.color_token.clone(),
                                ),
                                ferrite_portrayal_catalog::LineStyle::Complex(c) => {
                                    let (col, tok) = c
                                        .strokes
                                        .first()
                                        .map(|s| {
                                            (
                                                lookup_color(&s.pen.color_token),
                                                s.pen.color_token.clone(),
                                            )
                                        })
                                        .unwrap_or_else(|| {
                                            (lookup_color("CSTLN"), "CSTLN".to_string())
                                        });
                                    let w = c
                                        .strokes
                                        .first()
                                        .map(|s| s.pen.width as f32)
                                        .unwrap_or(0.32);
                                    (col, w, tok)
                                }
                                ferrite_portrayal_catalog::LineStyle::Composite(c) => {
                                    let (col, tok) = c
                                        .components
                                        .first()
                                        .map(|s| {
                                            (
                                                lookup_color(&s.pen.color_token),
                                                s.pen.color_token.clone(),
                                            )
                                        })
                                        .unwrap_or_else(|| {
                                            (lookup_color("CSTLN"), "CSTLN".to_string())
                                        });
                                    let w = c
                                        .components
                                        .first()
                                        .map(|s| s.pen.width as f32)
                                        .unwrap_or(0.32);
                                    (col, w, tok)
                                }
                            })
                            .unwrap_or_else(|| (lookup_color("CSTLN"), 0.32, "CSTLN".to_string()));
                        // Compute angle from direction vector (dirX, dirY) in degrees
                        let angle = (direction.1.atan2(direction.0).to_degrees()) as f32;
                        // distance is in mm per S-100 spec
                        let spacing_mm = (*distance as f32).max(0.5);
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_hatch_fill_token(
                                                color,
                                                &hatch_token,
                                                line_width_mm,
                                                spacing_mm,
                                                angle,
                                            )
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::TextInstruction {
                        text,
                        font_size,
                        color_token,
                        bold,
                        italic,
                        h_align,
                        v_align,
                        rotation,
                        local_offset,
                        position,
                        visibility,
                        ..
                    } => {
                        let color = lookup_color(color_token);
                        // If explicit position from AugmentedPoint, use it
                        if let Some((x, y)) = position {
                            let mut text_inst =
                                RenderTextInstruction::new(text.clone(), WorldPoint::new(*x, *y))
                                    .with_font_size(*font_size)
                                    .with_color(color)
                                    .with_alignment(parse_h_align(h_align), parse_v_align(v_align))
                                    .with_rotation(*rotation)
                                    .with_offset(local_offset.0 as f32, local_offset.1 as f32)
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0));
                            text_inst.color_token = Some(color_token.clone());
                            context.add_instruction(ferrite_render::DrawingInstruction::Text(
                                text_inst,
                            ));
                        } else if let Some(feature) = feature {
                            // Place text at feature geometry:
                            // 1. Point geometry → at point position
                            // 2. Surface geometry → at area centroid
                            let mut placed = false;

                            // Try point geometry first
                            for spas in &feature.spatial_associations {
                                if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                                    let mut ti = RenderTextInstruction::new(
                                        text.clone(),
                                        WorldPoint::new(point.position.x, point.position.y),
                                    )
                                    .with_font_size(*font_size)
                                    .with_color(color)
                                    .with_alignment(parse_h_align(h_align), parse_v_align(v_align))
                                    .with_rotation(*rotation)
                                    .with_offset(local_offset.0 as f32, local_offset.1 as f32)
                                    .with_priority(visibility.drawing_priority)
                                    .with_viewing_group(make_viewing_group(visibility))
                                    .with_scale_range(make_scale_range(visibility))
                                    .with_display_plane(make_display_plane(visibility))
                                    .with_feature_id(feature_id.unwrap_or(0));
                                    ti.bold = *bold;
                                    ti.italic = *italic;
                                    ti.color_token = Some(color_token.clone());
                                    context.add_instruction(
                                        ferrite_render::DrawingInstruction::Text(ti),
                                    );
                                    placed = true;
                                }
                            }

                            // Surface geometry: place text at centroid
                            if !placed {
                                for spas in &feature.spatial_associations {
                                    if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key())
                                    {
                                        let ext = collect_ring_points(&surface.exterior_ring);
                                        if ext.len() >= 3 {
                                            let mut cx = 0.0_f64;
                                            let mut cy = 0.0_f64;
                                            let mut area2 = 0.0_f64;
                                            let n = ext.len();
                                            for i in 0..n {
                                                let j = (i + 1) % n;
                                                let cross =
                                                    ext[i].x * ext[j].y - ext[j].x * ext[i].y;
                                                cx += (ext[i].x + ext[j].x) * cross;
                                                cy += (ext[i].y + ext[j].y) * cross;
                                                area2 += cross;
                                            }
                                            if area2.abs() > 1e-15 {
                                                cx /= 3.0 * area2;
                                                cy /= 3.0 * area2;
                                            } else {
                                                cx =
                                                    ext.iter().map(|p| p.x).sum::<f64>() / n as f64;
                                                cy =
                                                    ext.iter().map(|p| p.y).sum::<f64>() / n as f64;
                                            }

                                            let mut ti = RenderTextInstruction::new(
                                                text.clone(),
                                                WorldPoint::new(cx, cy),
                                            )
                                            .with_font_size(*font_size)
                                            .with_color(color)
                                            .with_alignment(
                                                parse_h_align(h_align),
                                                parse_v_align(v_align),
                                            )
                                            .with_rotation(*rotation)
                                            .with_offset(
                                                local_offset.0 as f32,
                                                local_offset.1 as f32,
                                            )
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                            ti.bold = *bold;
                                            ti.italic = *italic;
                                            ti.color_token = Some(color_token.clone());
                                            context.add_instruction(
                                                ferrite_render::DrawingInstruction::Text(ti),
                                            );
                                            break; // One text label per feature
                                        }
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::CoverageFill { visibility, .. } => {
                        // Coverage fills require per-cell attribute grid rendering
                        // Rendered as transparent area placeholder for now
                        area_count += 1;
                        if let Some(feature) = feature {
                            for spas in &feature.spatial_associations {
                                if spas.spatial_id.rcnm != 130 {
                                    continue;
                                }
                                if let Some(surface) = cell.surfaces.get(&spas.spatial_id.key()) {
                                    let (exterior, interiors) = collect_surface_points(surface);
                                    if exterior.len() >= 3 {
                                        area_rendered += 1;
                                        let area_inst = AreaInstruction::new(exterior)
                                            .with_interiors(interiors)
                                            .with_solid_fill(lookup_color("NODTA"))
                                            .with_priority(visibility.drawing_priority)
                                            .with_viewing_group(make_viewing_group(visibility))
                                            .with_scale_range(make_scale_range(visibility))
                                            .with_display_plane(make_display_plane(visibility))
                                            .with_feature_id(feature_id.unwrap_or(0));
                                        context.add_instruction(
                                            ferrite_render::DrawingInstruction::Area(area_inst),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    DrawingCommand::NullInstruction { .. } => {
                        // Feature purposefully not portrayed
                    }
                    DrawingCommand::AlertReference { .. } => {
                        // Alert handling is not part of visual rendering
                    }
                    DrawingCommand::AugmentedPoint { .. }
                    | DrawingCommand::SpatialReference { .. }
                    | DrawingCommand::Dash { .. } => {
                        // State-carrying commands, consumed during parse phase
                    }
                }
            }
        }
    }

    debug!(
        "Cell conversion: {} points, {} lines, {} areas ({} rendered)",
        point_count, line_count, area_count, area_rendered
    );
}

/// Load window icon from icon.ico file
fn load_window_icon() -> Option<Icon> {
    let icon_path = PathBuf::from("./icon.ico");

    if !icon_path.exists() {
        warn!("Icon file not found: {}", icon_path.display());
        return None;
    }

    // Read the ICO file
    match fs::read(&icon_path) {
        Ok(data) => {
            // Parse ICO file to get RGBA data
            // ICO files have a directory structure, we need to extract the image
            match parse_ico_to_rgba(&data) {
                Some((rgba, width, height)) => match Icon::from_rgba(rgba, width, height) {
                    Ok(icon) => {
                        info!("Window icon loaded: {}x{}", width, height);
                        Some(icon)
                    }
                    Err(e) => {
                        warn!("Failed to create icon: {}", e);
                        None
                    }
                },
                None => {
                    warn!("Failed to parse ICO file");
                    None
                }
            }
        }
        Err(e) => {
            warn!("Failed to read icon file: {}", e);
            None
        }
    }
}

/// Parse ICO file and extract RGBA pixel data
fn parse_ico_to_rgba(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    // ICO file structure:
    // - Header (6 bytes): reserved, type, image count
    // - Directory entries (16 bytes each): width, height, colors, reserved, planes, bpp, size, offset
    // - Image data (BMP or PNG format)

    if data.len() < 6 {
        return None;
    }

    // Check ICO header
    let _reserved = u16::from_le_bytes([data[0], data[1]]);
    let image_type = u16::from_le_bytes([data[2], data[3]]);
    let image_count = u16::from_le_bytes([data[4], data[5]]);

    if image_type != 1 || image_count == 0 {
        return None;
    }

    // Find the best (largest) icon
    let mut best_entry: Option<(usize, u32, u32, u32, u32)> = None;

    for i in 0..image_count as usize {
        let entry_offset = 6 + i * 16;
        if entry_offset + 16 > data.len() {
            break;
        }

        // Width and height (0 means 256)
        let width = if data[entry_offset] == 0 {
            256u32
        } else {
            data[entry_offset] as u32
        };
        let height = if data[entry_offset + 1] == 0 {
            256u32
        } else {
            data[entry_offset + 1] as u32
        };
        let size = u32::from_le_bytes([
            data[entry_offset + 8],
            data[entry_offset + 9],
            data[entry_offset + 10],
            data[entry_offset + 11],
        ]);
        let offset = u32::from_le_bytes([
            data[entry_offset + 12],
            data[entry_offset + 13],
            data[entry_offset + 14],
            data[entry_offset + 15],
        ]);

        // Prefer larger icons, but not too large (32x32 or 48x48 is ideal for window icons)
        let score = width * height;
        if best_entry.is_none() || score <= 48 * 48 {
            best_entry = Some((i, width, height, size, offset));
        }
    }

    let (_, _width, _height, size, offset) = best_entry?;
    let offset = offset as usize;
    let size = size as usize;

    if offset + size > data.len() {
        return None;
    }

    let image_data = &data[offset..offset + size];

    // Check if it's PNG (starts with PNG signature)
    if image_data.len() >= 8 && &image_data[0..8] == b"\x89PNG\r\n\x1a\n" {
        // PNG format - use image crate to decode
        use image::GenericImageView;
        match image::load_from_memory(image_data) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = img.dimensions();
                Some((rgba.into_raw(), w, h))
            }
            Err(_) => None,
        }
    } else {
        // BMP format (DIB) - more complex parsing needed
        // For simplicity, try using image crate with BMP header reconstruction
        // Or just decode the DIB directly

        // DIB header starts directly (no BMP file header)
        if image_data.len() < 40 {
            return None;
        }

        let header_size =
            u32::from_le_bytes([image_data[0], image_data[1], image_data[2], image_data[3]]);

        if header_size < 40 {
            return None;
        }

        let dib_width =
            i32::from_le_bytes([image_data[4], image_data[5], image_data[6], image_data[7]]) as u32;

        // Height in DIB is doubled (includes mask)
        let dib_height =
            i32::from_le_bytes([image_data[8], image_data[9], image_data[10], image_data[11]])
                .unsigned_abs()
                / 2;

        let bpp = u16::from_le_bytes([image_data[14], image_data[15]]);

        // Only handle 32-bit BGRA
        if bpp != 32 {
            return None;
        }

        let pixel_offset = header_size as usize;
        let row_size = (dib_width * 4) as usize;
        let pixel_data_size = row_size * dib_height as usize;

        if pixel_offset + pixel_data_size > image_data.len() {
            return None;
        }

        // Convert BGRA to RGBA, and flip vertically (DIB is bottom-up)
        let mut rgba = vec![0u8; (dib_width * dib_height * 4) as usize];

        for y in 0..dib_height {
            let src_y = (dib_height - 1 - y) as usize;
            let src_offset = pixel_offset + src_y * row_size;
            let dst_offset = (y * dib_width * 4) as usize;

            for x in 0..dib_width {
                let src_px = src_offset + (x as usize) * 4;
                let dst_px = dst_offset + (x as usize) * 4;

                if src_px + 4 <= image_data.len() {
                    // BGRA -> RGBA
                    rgba[dst_px] = image_data[src_px + 2]; // R
                    rgba[dst_px + 1] = image_data[src_px + 1]; // G
                    rgba[dst_px + 2] = image_data[src_px]; // B
                    rgba[dst_px + 3] = image_data[src_px + 3]; // A
                }
            }
        }

        Some((rgba, dib_width, dib_height))
    }
}

/// Initialize logging with file and console output
#[allow(dead_code)]
fn init_logging(log_path: &Path) -> Result<()> {
    // Create log directory
    fs::create_dir_all(log_path)
        .with_context(|| format!("Failed to create log directory: {}", log_path.display()))?;

    // File appender
    let file_appender = RollingFileAppender::new(Rotation::NEVER, log_path, "ferrite_debug.log");

    // Console layer
    let console_layer = fmt::layer().with_target(false).with_level(true);

    // File layer
    let file_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_writer(file_appender);

    // Environment filter - default to info, debug for core modules.
    // wgpu_hal::vulkan::conv emits a benign "Unrecognized present mode 1000361000"
    // warning at startup on some Vulkan drivers; downgrade it to error so it
    // doesn't drown out the log on every adapter probe.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,ferrite_s100=debug,ferrite_s100_core=debug,ferrite_plugin_loader=debug,ferrite_wgpu=debug,wgpu_hal::vulkan::conv=error",
        )
    });

    // Initialize subscriber
    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    info!(
        "Logging initialized: {}/ferrite_debug.log",
        log_path.display()
    );

    Ok(())
}

/// Validate Feature Catalogue and create status
fn validate_fc(fc: &FeatureCatalogue, path: &Path) -> CatalogueStatus {
    let mut validation_messages = Vec::new();

    // Check product ID
    if fc.product_id.is_empty() {
        validation_messages.push("Missing product ID");
    } else if !fc.product_id.contains("S-101") && fc.product_id != "S-101" {
        validation_messages.push("Product ID is not S-101");
    }

    // Check required content
    if fc.feature_types.is_empty() {
        validation_messages.push("No feature types defined");
    }
    if fc.simple_attributes.is_empty() {
        validation_messages.push("No simple attributes defined");
    }

    // Check for essential S-101 feature types
    let essential_features = ["DepthArea", "LandArea", "Coastline", "Sounding"];
    let missing: Vec<_> = essential_features
        .iter()
        .filter(|f| !fc.feature_types.contains_key(**f))
        .collect();
    if !missing.is_empty() {
        validation_messages.push("Missing essential feature types");
    }

    let validation_message = if validation_messages.is_empty() {
        Some("Valid S-101 Feature Catalogue".to_string())
    } else {
        Some(format!("Warning: {}", validation_messages.join(", ")))
    };

    CatalogueStatus {
        loaded: true,
        product_id: fc.product_id.clone(),
        version: fc.version.clone(),
        path: path.display().to_string(),
        item_count: fc.feature_types.len(),
        validation_message,
    }
}

/// Validate Portrayal Catalogue and create status
fn validate_pc(pc: &PortrayalCatalogue, path: &Path) -> CatalogueStatus {
    let mut validation_messages = Vec::new();

    // Check product ID
    if pc.product_id.is_empty() {
        validation_messages.push("Missing product ID");
    } else if !pc.product_id.contains("S-101") && pc.product_id != "S-101" {
        validation_messages.push("Product ID is not S-101");
    }

    // Check required content
    if pc.color_profiles.profiles.is_empty() {
        validation_messages.push("No color profiles defined");
    }
    if pc.symbols.symbols.is_empty() {
        validation_messages.push("No symbols defined");
    }

    // Check for required color profiles (Day, Dusk, Night)
    let required_profiles = ["Day", "Dusk", "Night"];
    let missing: Vec<_> = required_profiles
        .iter()
        .filter(|p| !pc.color_profiles.profiles.contains_key(**p))
        .collect();
    if !missing.is_empty() {
        validation_messages.push("Missing required color profiles");
    }

    let validation_message = if validation_messages.is_empty() {
        Some("Valid S-101 Portrayal Catalogue".to_string())
    } else {
        Some(format!("Warning: {}", validation_messages.join(", ")))
    };

    CatalogueStatus {
        loaded: true,
        product_id: pc.product_id.clone(),
        version: pc.version.clone(),
        path: path.display().to_string(),
        item_count: pc.symbols.symbols.len(),
        validation_message,
    }
}

/// Load Feature Catalogue from XML file
fn load_feature_catalogue(path: &Path) -> Result<FeatureCatalogue> {
    info!("Loading Feature Catalogue: {}", path.display());

    if !path.exists() {
        anyhow::bail!(
            "Feature Catalogue directory not found: {}\n\
             Expected layout: <app>/Catalogues/FC/S-101/*Feature_Catalogue*.xml.\n\
             If you launched the executable from a build/output directory, ensure the \
             Catalogues/ folder is present alongside it.",
            path.display()
        );
    }

    // If path is a directory, scan for FC XML file
    let fc_file = if path.is_dir() {
        // Look for Feature Catalogue XML in the directory
        let mut found_fc = None;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let file_path = entry.path();
            if file_path.is_file() {
                if let Some(name) = file_path.file_name().and_then(|n| n.to_str()) {
                    // Look for FC XML files (e.g., "101_Feature_Catalogue_*.xml" or "*_FC.xml")
                    if name.contains("Feature_Catalogue") && name.ends_with(".xml") {
                        info!("Found FC XML: {}", file_path.display());
                        found_fc = Some(file_path);
                        break;
                    }
                }
            }
        }
        found_fc.unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };

    let fc = FeatureCatalogue::load(&fc_file)
        .with_context(|| format!("Failed to load Feature Catalogue: {}", fc_file.display()))?;

    info!("FC loaded successfully");
    debug!("  Product: {}", fc.product_id);
    debug!("  Version: {}", fc.version);
    debug!("  Feature types: {}", fc.feature_types.len());
    debug!("  Simple attributes: {}", fc.simple_attributes.len());
    debug!("  Complex attributes: {}", fc.complex_attributes.len());
    debug!("  Information types: {}", fc.information_types.len());

    Ok(fc)
}

/// Load Portrayal Catalogue from directory.
///
/// Failure here is fatal: an empty PC means no color profiles and no symbols,
/// which would silently degrade every chart render to placeholder fallbacks.
/// Surfacing the error early forces the user to fix the install/path instead
/// of seeing a broken render.
fn load_portrayal_catalogue(path: &Path) -> Result<PortrayalCatalogue> {
    info!("Loading Portrayal Catalogue: {}", path.display());

    if !path.exists() {
        anyhow::bail!(
            "Portrayal Catalogue directory not found: {}\n\
             Expected layout: <app>/Catalogues/PC/S-101/ with ColorProfiles/, Symbols/, Rules/.\n\
             If you launched the executable from a build/output directory, ensure the \
             Catalogues/ folder is present alongside it.",
            path.display()
        );
    }

    let pc = PortrayalCatalogue::load(path)
        .with_context(|| format!("Failed to load Portrayal Catalogue: {}", path.display()))?;

    if pc.color_profiles.profiles.is_empty() {
        anyhow::bail!(
            "Portrayal Catalogue at {} contains no color profiles. \
             Check that ColorProfiles/*.xml exists and is well-formed.",
            path.display()
        );
    }
    if pc.symbols.symbols.is_empty() {
        anyhow::bail!(
            "Portrayal Catalogue at {} contains no symbols. \
             Check that Symbols/*.svg exists.",
            path.display()
        );
    }

    info!("PC loaded successfully");
    debug!("  Product: {}", pc.product_id);
    debug!("  Version: {}", pc.version);
    debug!("  Color profiles: {}", pc.color_profiles.profiles.len());
    for (profile_id, profile) in &pc.color_profiles.profiles {
        info!(
            "  Color profile '{}' (name: '{}'): {} colors",
            profile_id,
            profile.name,
            profile.colors.len()
        );
        // Log some sample colors
        for token in ["DEPVS", "DEPMS", "DEPMD", "DEPDW", "DEPIT", "LANDA"] {
            if let Some(color) = profile.get_srgb(token) {
                debug!("    {}: RGB({}, {}, {})", token, color.r, color.g, color.b);
            }
        }
    }
    debug!("  Symbols: {}", pc.symbols.symbols.len());
    debug!("  Line styles: {}", pc.line_styles.len());
    debug!("  Area fills: {}", pc.area_fills.len());

    Ok(pc)
}

/// Load all chart data from directory
#[allow(dead_code)]
fn load_chart_data(path: &Path) -> Result<Vec<S101Cell>> {
    info!("Scanning ChartData folder: {}", path.display());

    if !path.exists() {
        warn!("ChartData directory not found: {}", path.display());
        warn!("Creating directory...");
        fs::create_dir_all(path)
            .with_context(|| format!("Failed to create ChartData directory: {}", path.display()))?;
        return Ok(Vec::new());
    }

    // Find all .000 files (filter to specific file for testing)
    let chart_files: Vec<PathBuf> = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "000")
                // Filter to specific file for testing
                && e.path().file_name().is_some_and(|name| name == "101GB00GB302045.000")
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    info!("Found {} chart files", chart_files.len());

    // Load each chart
    let mut cells = Vec::new();

    for chart_path in &chart_files {
        info!("Loading: {}", chart_path.display());

        match S101Cell::load(chart_path) {
            Ok(cell) => {
                let stats = cell.statistics();
                debug!("  -> {}", stats);
                cells.push(cell);
            }
            Err(e) => {
                error!("Failed to load {}: {}", chart_path.display(), e);
            }
        }
    }

    info!("Loaded {} cells successfully", cells.len());

    Ok(cells)
}

/// Generate default drawing instructions for a cell (simplified portrayal)
/// Uses PC color lookup - no hardcoded colors
fn generate_default_instructions(
    cell: &S101Cell,
    context: &mut RenderContext,
    pc: &PortrayalCatalogue,
    profile_name: &str,
) {
    // Process each feature and generate default instructions based on type
    for (key, feature) in &cell.features {
        let feature_code = feature.feature_code.as_deref().unwrap_or("UNKNOWN");

        // Get color token and priority based on feature type, then look up color from PC
        let (color_token, priority) = get_feature_color_token(feature_code);
        let color = lookup_pc_color(pc, color_token, profile_name);

        match feature.primitive_type {
            SpatialPrimitiveType::Point => {
                // Get point coordinates from spatial associations
                for spas in &feature.spatial_associations {
                    if let Some(point) = cell.points.get(&spas.spatial_id.key()) {
                        let instruction = PointInstruction::new(
                            feature_code.to_string(),
                            WorldPoint::new(point.position.x, point.position.y),
                        )
                        .with_priority(priority)
                        .with_feature_id(*key);

                        context.add_instruction(ferrite_render::DrawingInstruction::Point(
                            instruction,
                        ));
                    }
                }
            }
            SpatialPrimitiveType::Curve | SpatialPrimitiveType::CompositeCurve => {
                // Get curve coordinates
                for spas in &feature.spatial_associations {
                    // S-101 4.8.3: Edge masking — skip suppressed edges
                    if spas.mask == 2 {
                        continue;
                    }
                    let masked_by_mask_field = feature
                        .masks
                        .iter()
                        .any(|m| m.spatial_id.key() == spas.spatial_id.key() && m.mask_type == 2);
                    if masked_by_mask_field {
                        continue;
                    }

                    if let Some(curve) = cell.curves.get(&spas.spatial_id.key()) {
                        let points: Vec<WorldPoint> = curve
                            .all_positions()
                            .iter()
                            .map(|c| WorldPoint::new(c.x, c.y))
                            .collect();

                        if points.len() >= 2 {
                            let instruction = LineInstruction::new(points)
                                .with_style(LineStyle::solid(color, 1.0))
                                .with_priority(priority)
                                .with_feature_id(*key);

                            context.add_instruction(ferrite_render::DrawingInstruction::Line(
                                instruction,
                            ));
                        }
                    }
                }
            }
            SpatialPrimitiveType::Surface => {
                // Get surface boundary from surface records
                for spas in &feature.spatial_associations {
                    // Get the surface record
                    let surface_key = spas.spatial_id.key();
                    if let Some(surface) = cell.surfaces.get(&surface_key) {
                        let mut exterior_points = Vec::new();

                        // Collect points from exterior ring curves
                        for oriented_curve in &surface.exterior_ring {
                            let curve_key = oriented_curve.curve_id.key();

                            // Try to get curve from curves collection
                            if let Some(curve) = cell.curves.get(&curve_key) {
                                let positions = curve.all_positions();
                                if oriented_curve.orientation {
                                    for pos in positions {
                                        exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                    }
                                } else {
                                    // Reverse orientation
                                    for pos in positions.into_iter().rev() {
                                        exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                    }
                                }
                            }
                            // Also check composite curves
                            else if let Some(composite) = cell.composite_curves.get(&curve_key) {
                                for sub_curve in &composite.curves {
                                    if let Some(curve) = cell.curves.get(&sub_curve.curve_id.key())
                                    {
                                        let positions = curve.all_positions();
                                        let forward =
                                            oriented_curve.orientation == sub_curve.orientation;
                                        if forward {
                                            for pos in positions {
                                                exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                            }
                                        } else {
                                            for pos in positions.into_iter().rev() {
                                                exterior_points.push(WorldPoint::new(pos.x, pos.y));
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Remove duplicate consecutive points (curves share endpoints)
                        let mut cleaned_points = Vec::with_capacity(exterior_points.len());
                        for point in exterior_points {
                            if cleaned_points.is_empty() {
                                cleaned_points.push(point);
                            } else {
                                let last = cleaned_points.last().unwrap();
                                let dx = (point.x - last.x).abs();
                                let dy = (point.y - last.y).abs();
                                if dx > 1e-9 || dy > 1e-9 {
                                    cleaned_points.push(point);
                                }
                            }
                        }

                        // Remove duplicate closing point if present
                        if cleaned_points.len() > 3 {
                            let first = cleaned_points.first().unwrap();
                            let last = cleaned_points.last().unwrap();
                            let dx = (first.x - last.x).abs();
                            let dy = (first.y - last.y).abs();
                            if dx < 1e-9 && dy < 1e-9 {
                                cleaned_points.pop();
                            }
                        }

                        if cleaned_points.len() >= 3 {
                            let fill_color = color.with_alpha(0.3);
                            let instruction = AreaInstruction::new(cleaned_points)
                                .with_solid_fill(fill_color)
                                .with_outline(LineStyle::solid(color, 0.5))
                                .with_priority(priority)
                                .with_feature_id(*key);

                            context.add_instruction(ferrite_render::DrawingInstruction::Area(
                                instruction,
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Get color token and priority for feature type (maps feature code to PC color token)
/// Returns (color_token, priority) - color_token should be looked up from PC color profile
fn get_feature_color_token(feature_code: &str) -> (&'static str, i32) {
    match feature_code {
        // Land features - use PC tokens
        "LandArea" => ("LANDA", 1),
        "BuiltUpArea" => ("CHBRN", 2),

        // Depth features - use PC tokens
        "DepthArea" => ("DEPVS", 1),     // Very shallow water
        "DepthContour" => ("DEPCN", 10), // Depth contour
        "DredgedArea" => ("DEPMD", 3),   // Medium depth

        // Coastline
        "Coastline" => ("CSTLN", 15),

        // Navigation features
        "Light" | "LightAllAround" | "LightSectored" => ("LITRD", 20),
        "Buoy" | "LateralBuoy" | "CardinalBuoy" | "IsolatedDangerBuoy" => ("LITRD", 18),
        "Beacon" | "LateralBeacon" | "CardinalBeacon" => ("LITRD", 18),

        // Obstructions and dangers
        "Wreck" => ("DEPVS", 25),
        "Obstruction" => ("CHGRD", 25),
        "Rock" | "UnderwaterRock" => ("CHGRD", 22),

        // Anchorage
        "AnchorageArea" => ("CHMGD", 8),
        "AnchorBerth" => ("CHMGD", 12),

        // Traffic
        "TrafficSeparationScheme" | "TrafficSeparationZone" => ("TRFCD", 5),

        // Default - use CHGRD (chart grid color)
        _ => ("CHGRD", 5),
    }
}

/// Look up color from PC color profile by token
/// Uses the specified profile name (Day, Dusk, Night) for color resolution
fn lookup_pc_color(pc: &PortrayalCatalogue, token: &str, profile_name: &str) -> Color {
    // Try to get the specified profile, fallback to default or first available
    let profile = pc
        .color_profiles
        .profiles
        .get(profile_name)
        .or_else(|| {
            pc.color_profiles
                .default_profile
                .as_ref()
                .and_then(|name| pc.color_profiles.profiles.get(name))
        })
        .or_else(|| pc.color_profiles.profiles.values().next());

    if let Some(profile) = profile {
        if let Some(srgb) = profile.get_srgb(token) {
            return Color::rgb(
                srgb.r as f32 / 255.0,
                srgb.g as f32 / 255.0,
                srgb.b as f32 / 255.0,
            );
        }
    }

    // Color not found in PC - log warning and return gray
    tracing::warn!("Color token '{}' not found in PC color profile", token);
    Color::rgb(0.5, 0.5, 0.5)
}
