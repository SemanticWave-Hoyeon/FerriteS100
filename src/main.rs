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

mod app;
mod plugins;

use app::catalogue::{load_feature_catalogue, load_portrayal_catalogue, validate_fc, validate_pc};
use app::config::AppConfig;
#[cfg(all(windows, not(debug_assertions)))]
use app::error_dialog::show_error_dialog;
use app::logging::init_logging;
use app::path_resolution::get_app_base_dir;

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use anyhow::{Context, Result};

use tracing::{error, info};
use winit::{
    event_loop::{ControlFlow, EventLoop},
    window::Window,
};

use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{GeoBounds, RenderContext, Viewport};
use ferrite_s100_core::S101Cell;
use ferrite_wgpu::{CatalogueStatus, SymbolCache, WgpuRenderer};

/// Result of background chart loading
pub(crate) struct ChartLoadResult {
    pub(crate) path: PathBuf,
    pub(crate) cell: S101Cell,
}

/// Background loading state
pub(crate) struct BackgroundLoadingState {
    /// Number of files being loaded
    pub(crate) total_files: usize,
    /// Number of files loaded so far
    pub(crate) loaded_count: usize,
    /// Receiver for loaded cells
    pub(crate) receiver: Receiver<Option<ChartLoadResult>>,
}

/// Rendered symbol info for hit testing
/// Fields ordered by size (largest first) for optimal memory layout
#[derive(Clone, Debug)]
pub(crate) struct RenderedSymbol {
    pub(crate) world_x: f64,
    pub(crate) world_y: f64,
    pub(crate) feature_id: i64,
    pub(crate) screen_x: f32,
    pub(crate) screen_y: f32,
    /// Drawing priority (higher = drawn on top, should be selected first)
    pub(crate) priority: i32,
    pub(crate) symbol_ref: String,
    /// Cell index this symbol belongs to (for correct feature lookup in multi-cell scenarios)
    pub(crate) cell_index: Option<u32>,
}

/// Chart viewer application for winit
pub(crate) struct ChartApp {
    pub(crate) window: Option<Arc<Window>>,
    pub(crate) renderer: Option<WgpuRenderer>,
    pub(crate) render_context: RenderContext,
    pub(crate) bounds: GeoBounds,
    /// Symbol cache for SVG symbol rendering
    pub(crate) symbol_cache: SymbolCache,
    /// Current color profile name (Day, Dusk, Night)
    pub(crate) current_profile_name: String,
    /// Current mouse position
    pub(crate) mouse_pos: (f64, f64),
    /// Rendered symbols for hit testing
    pub(crate) rendered_symbols: Vec<RenderedSymbol>,
    /// Pending async hit-test build result
    pub(crate) pending_hit_test: Option<Receiver<Vec<RenderedSymbol>>>,
    /// Is mouse being dragged for panning
    pub(crate) is_dragging: bool,
    /// Last drag position
    pub(crate) drag_start: (f64, f64),
    /// Current zoom level (1.0 = fit to window)
    pub(crate) zoom_level: f64,
    /// Pan offset in world coordinates
    pub(crate) pan_offset: (f64, f64),
    /// Pan velocity for inertia (world coordinates per second)
    pub(crate) pan_velocity: (f64, f64),
    /// Last frame time for velocity calculation
    pub(crate) last_frame_time: std::time::Instant,
    /// Recent mouse positions for velocity calculation (screen coords, time)
    pub(crate) recent_positions: Vec<((f64, f64), std::time::Instant)>,
    /// Feature Catalogue reference for attribute lookup
    pub(crate) fc: Arc<FeatureCatalogue>,
    /// Portrayal Catalogue reference
    pub(crate) pc: Arc<PortrayalCatalogue>,
    /// Feature Catalogue status (for UI display)
    pub(crate) fc_status: CatalogueStatus,
    /// Portrayal Catalogue status (for UI display)
    pub(crate) pc_status: CatalogueStatus,
    /// All loaded S101 cells
    pub(crate) cells: Vec<S101Cell>,
    /// Whether chart data is loaded
    pub(crate) chart_loaded: bool,
    /// Paths of already loaded chart files (to prevent duplicates)
    pub(crate) loaded_paths: std::collections::HashSet<PathBuf>,
    /// Background loading state (Some if loading in progress)
    pub(crate) loading_state: Option<BackgroundLoadingState>,
    /// Plugin system
    pub(crate) plugin_system: plugins::PluginSystem,
    /// Base instruction count (chart instructions only, before plugin instructions)
    pub(crate) base_instruction_count: usize,
    /// Debug mode enabled
    pub(crate) debug_mode: bool,
    /// Charts to auto-load on startup
    pub(crate) pending_auto_chart: Vec<PathBuf>,
    /// Auto-screenshot path (take screenshot after load, then exit)
    pub(crate) auto_screenshot: Option<PathBuf>,
    /// Debug interior rings
    pub(crate) debug_rings: bool,
    /// Override zoom level for auto-screenshot
    pub(crate) auto_zoom: Option<f64>,
    /// Override center position for auto-screenshot (lat, lon)
    pub(crate) auto_center: Option<(f64, f64)>,
    /// Frame count since load completed (for auto-screenshot timing)
    pub(crate) frames_since_loaded: Option<u32>,
    /// Frame times for FPS calculation
    pub(crate) frame_times: std::collections::VecDeque<std::time::Instant>,
    /// Previous CPU time measurement (kernel_time, user_time, wall_time) in 100-nanosecond intervals
    #[cfg(windows)]
    pub(crate) prev_cpu_times: Option<(u64, u64, std::time::Instant)>,
    /// Last debug stats update time (for throttling to 0.5s intervals)
    pub(crate) last_debug_update: std::time::Instant,
    /// Zoom debounce: time of last scroll event (for deferred geometry rebuild)
    pub(crate) zoom_last_scroll: std::time::Instant,
    /// Zoom debounce: the zoom level at which geometry was last rebuilt
    pub(crate) zoom_rebuilt_level: f64,
    /// Zoom debounce phase: 0=none, 2=needs phase1, 1=phase1 done waiting for phase2
    pub(crate) zoom_rebuild_phase: u8,
    /// Zoom debounce: cursor position during zoom (for pivot)
    pub(crate) zoom_cursor_screen: (f32, f32),
    /// Pan rebuild pending: deferred rebuild after inertia/drag stops
    /// 0 = none, 1 = phase 1 done (waiting for phase 2), 2 = needs phase 1
    pub(crate) pan_rebuild_phase: u8,
    /// Time when pan rebuild was requested
    pub(crate) pan_rebuild_time: std::time::Instant,
    /// Animated zoom: target zoom level (we interpolate zoom_level toward this)
    pub(crate) zoom_target: f64,
    /// Whether zoom animation is active
    pub(crate) zoom_animating: bool,
    /// Anchor world point: the world position under cursor at zoom start.
    /// Used for drift-free zoom by directly computing pan_offset each frame
    /// instead of accumulating floating-point deltas.
    pub(crate) zoom_anchor_world: (f64, f64),
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
            loaded_paths: std::collections::HashSet::new(),
            loading_state: None,
            plugin_system: {
                let base = get_app_base_dir();
                // Check both plugin directory names:
                // - "plugins_out" for dev builds (cargo run)
                // - "plugins" for distribution packages
                let plugins_path = if base.join("plugins_out").exists() {
                    base.join("plugins_out")
                } else {
                    base.join("plugins")
                };
                info!("Plugin directory: {}", plugins_path.display());

                let mut ps = plugins::PluginSystem::new(plugins_path, VERSION);
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
    );

    event_loop.run_app(&mut app).context("Event loop error")?;

    Ok(())
}
