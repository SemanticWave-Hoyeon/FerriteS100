//! Command-line argument parsing into `AppConfig`.
//!
//! Path defaults are resolved against the application base directory
//! (see `path_resolution::get_app_base_dir`). The flag set is intentionally
//! small — interactive features are configured at runtime through the GUI,
//! and these flags exist mainly to drive non-interactive flows
//! (`--screenshot`, `--zoom`, `--chart`, `--center`).

use std::path::PathBuf;

use crate::app::path_resolution::get_app_base_dir;

#[derive(Debug)]
pub struct AppConfig {
    /// Path to Feature Catalogue XML directory
    pub fc_path: PathBuf,
    /// Path to Portrayal Catalogue directory
    pub pc_path: PathBuf,
    /// Path to log directory
    pub log_path: PathBuf,
    /// Debug mode enabled (--debug flag, or implied by --screenshot / --debug-rings)
    pub debug_mode: bool,
    /// Auto-load chart file(s) on startup
    pub auto_chart: Vec<PathBuf>,
    /// Auto-save screenshot after loading (then exit)
    pub auto_screenshot: Option<PathBuf>,
    /// Debug interior rings: log detailed ring info
    pub debug_rings: bool,
    /// Override zoom level for auto-screenshot (1.0 = fit to window)
    pub auto_zoom: Option<f64>,
    /// Override center position for auto-screenshot (lat,lon in degrees)
    pub auto_center: Option<(f64, f64)>,
}

impl AppConfig {
    pub fn from_args() -> Self {
        let base = get_app_base_dir();
        let args: Vec<String> = std::env::args().collect();
        let debug_mode = args.iter().any(|arg| arg == "--debug" || arg == "--DEBUG");
        let debug_rings = args.iter().any(|arg| arg == "--debug-rings");

        // --chart <path> may repeat. A directory expands to all `.000` files inside.
        let mut auto_chart = Vec::new();
        let mut i = 1;
        while i < args.len() {
            if args[i] == "--chart" {
                if let Some(path_str) = args.get(i + 1) {
                    let path = PathBuf::from(path_str);
                    if path.is_dir() {
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

        let auto_screenshot = args
            .windows(2)
            .find(|w| w[0] == "--screenshot")
            .map(|w| PathBuf::from(&w[1]));

        let auto_zoom = args
            .windows(2)
            .find(|w| w[0] == "--zoom")
            .and_then(|w| w[1].parse::<f64>().ok());

        // --center "<lat>,<lon>" e.g. --center 50.7908,-1.1135
        let auto_center = args.windows(2).find(|w| w[0] == "--center").and_then(|w| {
            let parts: Vec<&str> = w[1].split(',').collect();
            if parts.len() == 2 {
                if let (Ok(lat), Ok(lon)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    return Some((lat, lon));
                }
            }
            None
        });

        // Force debug mode if --debug-rings or --screenshot is set so the
        // resulting render captures the diagnostics those flags depend on.
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
        }
    }
}
