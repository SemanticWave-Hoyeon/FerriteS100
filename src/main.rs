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

fn prepare_flat_coverage(
    inventory: Option<&ferrite_s101::coverage_projection::GeographicCoverageInventory>,
    context: &mut RenderContext,
    extent: winit::dpi::PhysicalSize<u32>,
    density: f64,
) -> Result<()> {
    context.scaler.set_pixel_ratio(density);
    context.get_sorted_instructions();
    let Some(inventory) = inventory else {
        return Ok(());
    };
    if inventory.current_dataset_count() == 0 {
        context.clear_prepared_coverage();
        return Ok(());
    }
    context.require_prepared_coverage();
    let exemptions = context
        .raw_instructions()
        .iter()
        .enumerate()
        .filter_map(|(i, command)| {
            matches!(
                command.portrayal_origin(),
                ferrite_render::PortrayalOrigin::CoverageExempt
            )
            .then_some(i)
        })
        .collect();
    match inventory.prepare_flat(
        context,
        &exemptions,
        [extent.width, extent.height],
        true,
        128 * 1024 * 1024,
    )? {
        Some(prepared) => context.set_prepared_coverage(prepared)?,
        None => context.clear_prepared_coverage(),
    }
    Ok(())
}

struct CoverageLifecycleResize {
    output: PathBuf,
    original: winit::dpi::PhysicalSize<u32>,
    requested: winit::dpi::PhysicalSize<u32>,
    restore: bool,
    observed: Option<winit::dpi::PhysicalSize<u32>>,
    started: std::time::Instant,
    next_poll: std::time::Instant,
}

impl CoverageLifecycleResize {
    fn ready(&self,actual:winit::dpi::PhysicalSize<u32>)->bool {
        actual.width>0 && actual.height>0 && self.observed==Some(actual)
            && if self.restore {actual==self.original} else {actual!=self.original}
    }
}

/// Fresh provider-local policy. A zero opt-out selects the original cold path.
/// No process-global state is mutated; tests exercise this parser directly.
fn coverage_cache_policy(geometry: Option<&str>, midpoint: Option<&str>) -> (bool, bool) {
    let enabled = geometry != Some("0");
    (enabled, enabled && midpoint != Some("0"))
}
fn new_coverage_geometry_cache() -> Arc<std::sync::Mutex<ferrite_wgpu::globe_coverage_projection::CoverageGeometryCache>> {
    let geometry = std::env::var("FERRITE_GLOBE_COVERAGE_GEOMETRY_CACHE").ok();
    let midpoint = std::env::var("FERRITE_GLOBE_COVERAGE_MIDPOINT_REUSE").ok();
    let (enabled, reuse) = coverage_cache_policy(geometry.as_deref(), midpoint.as_deref());
    coverage_geometry_cache_with_policy(enabled, reuse)
}
fn coverage_geometry_cache_with_policy(enabled: bool, reuse: bool) -> Arc<std::sync::Mutex<ferrite_wgpu::globe_coverage_projection::CoverageGeometryCache>> {
    let mut cache = ferrite_wgpu::globe_coverage_projection::CoverageGeometryCache::default();
    cache.set_enabled(enabled);
    cache.set_midpoint_reuse(reuse);
    Arc::new(std::sync::Mutex::new(cache))
}

/// Application wiring: S-101 selects coverage; the renderer supplies its camera.
struct S101GlobeCoverageProvider {
    geometry_cache:
        Arc<std::sync::Mutex<ferrite_wgpu::globe_coverage_projection::CoverageGeometryCache>>,
    inventory: Arc<ferrite_s101::coverage_projection::GeographicCoverageInventory>,
}
impl ferrite_render::GlobeCoverageProvider for S101GlobeCoverageProvider {
    fn prepare(
        &self,
        context: &RenderContext,
        view: ferrite_render::GlobeCoverageView<'_>,
    ) -> ferrite_render::Result<Option<ferrite_render::PreparedCoverage>> {
        let prepare = || -> Result<_> {
            use ferrite_kernel::coverage_selection::Region;
            let [w, h] = view.extent;
            let viewport = Region::from_rings(
                &[
                    [0., 0.],
                    [w as f64, 0.],
                    [w as f64, h as f64],
                    [0., h as f64],
                    [0., 0.],
                ],
                &[],
            )?;
            let exemptions = context
                .raw_instructions()
                .iter()
                .enumerate()
                .filter_map(|(i, command)| {
                    matches!(
                        command.portrayal_origin(),
                        ferrite_render::PortrayalOrigin::CoverageExempt
                    )
                    .then_some(i)
                })
                .collect();
            self.inventory.prepare_projected(context, &exemptions,
                ferrite_s101::coverage_projection::ProjectedCoverageView {
                    viewport: &viewport, extent: view.extent,
                    display_scale: view.display_scale, pixel_budget: 128 * 1024 * 1024,
                },
                |dataset, coverage_id, surfaces| {
                    let mut regions = Vec::with_capacity(surfaces.len());
                    for (surface_index,surface) in surfaces.iter().enumerate() {
                        let mut cache=match self.geometry_cache.lock(){Ok(cache)=>cache,Err(poison)=>{let mut cache=poison.into_inner();cache.set_enabled(false);cache}};
                        let region=cache.project_rings((dataset,coverage_id,surface_index),&surface.exterior,&surface.holes,
                            view.camera,ferrite_wgpu::globe_portrayal::DrapingLimits::default(),
                            ferrite_kernel::globe_coverage_projection::CoverageProjectionLimits::default()).map_err(anyhow::Error::msg)?;
                        regions.push(region);
                    }
                    Region::union_projected(&regions, 1048576)
                },
                |_, origin| Ok(ferrite_render::PortrayalOrigin::project_globe_source(origin, view.camera, view.pixels_per_mm)?),
            )
        };
        prepare().map_err(|e| {
            ferrite_render::RenderError::Render(format!("S-101 globe coverage: {e:#}"))
        })
    }
}

mod cell_source_identity;
mod chart_publication;
mod dataset_discovery;
mod dataset_signature_policy;
mod interoperability;
mod navigation;
mod plugins;
mod process_stats;
mod s101_lifecycle_metadata;
mod s101_update_plan;
mod s102_depth_policy;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Audit/capture requests must fail without waiting for interactive dismissal.
fn noninteractive_diagnostics(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    arguments.into_iter().any(|argument| {
        matches!(
            argument.as_ref(),
            "--screenshot"
                | "--portrayal-audit"
                | "--ic-audit"
                | "--ic-transition-audit"
                | "--selection-audit"
                | "--bathymetry-audit"
                | "--animation-audit"
        )
    })
}
#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    #[test]
    fn invalid_audit_requests_do_not_require_interactive_error_dismissal() {
        for flag in [
            "--screenshot",
            "--portrayal-audit",
            "--ic-audit",
            "--ic-transition-audit",
            "--selection-audit",
            "--bathymetry-audit",
            "--animation-audit",
        ] {
            assert!(noninteractive_diagnostics(["ferrite-s100", flag, "output"]));
        }
        assert!(!noninteractive_diagnostics([
            "ferrite-s100",
            "--chart",
            "example.000"
        ]));
        assert!(!noninteractive_diagnostics([
            "ferrite-s100",
            "--portrayal-audit-example"
        ]));
    }
}

/// Show a native Windows error dialog (release mode only, no-op on other platforms)
#[cfg(all(windows, not(debug_assertions)))]
fn show_error_dialog(title: &str, message: &str) {
    // Automated captures must report failure and exit instead of waiting for
    // a user to dismiss a modal dialog in a remote desktop session.
    if noninteractive_diagnostics(std::env::args()) {
        return;
    }
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
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Icon, Window, WindowId},
};

use ferrite_feature_catalog::{BoundFeatureCatalogue, FeatureCatalogue};
use ferrite_kernel::{depth_selection::DepthReference, NumericCoverageSource};
use ferrite_lua::{
    ContextParameters as LuaContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue,
};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, PortrayalCatalogue};
use ferrite_render::{DrawingInstruction, GeoBounds, RenderContext, Viewport, WorldPoint};
use ferrite_s100_core::{CellSourceIdentity, S101Cell};
use ferrite_s101::{convert_lua_results_for_cell, lookup_pc_color};
use ferrite_s102::{
    BathymetryCoverage, BathymetryPortrayal, ConservativeCoverage, DatumCoverage, DepthSettings,
};
use ferrite_security::{
    authorize_datasets, AuthenticatedSnapshot, AuthorizedDatasets, TrustAnchors,
    UnauthenticatedSnapshot, UnsignedPolicy,
};
use ferrite_wgpu::{
    CatalogueStatus, DisplayMode, SelectedFeature, SettingsState, SymbolCache, WgpuRenderer,
};
use s102_depth_policy::DepthPolicy;

/// Embedded public-domain reference coastlines (map scales, not accuracy).
/// Source: https://www.naturalearthdata.com/ (Public Domain)
const WORLD_MAP_GEOJSON: &str = include_str!("../assets/ne_110m_coastline.geojson");

/// Parse Natural Earth GeoJSON coastlines into line segments.
/// Returns Vec of line strings, each being a list of [longitude, latitude] pairs.
fn parse_world_map_coastlines(data: &str) -> Vec<Vec<[f64; 2]>> {
    let parsed: serde_json::Value = match serde_json::from_str(data) {
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
    auto_s102: Vec<PathBuf>,
    s102_pc_path: PathBuf,
    s102_adjustments_path: Option<PathBuf>,
    require_signatures: bool,
    ic_path: Option<PathBuf>,
    initial_interoperability_enabled: bool,
    ic_transition_audit: Option<PathBuf>,
    ic_trust_root: Option<PathBuf>,
    ic_administrator: String,
    ic_audit: Option<PathBuf>,
    portrayal_audit: Option<PathBuf>,
    /// Select the same catalogue display preset exposed by the settings UI.
    initial_display_mode: Option<String>,
    initial_viewing_layers: Vec<String>,
    initial_globe_samples: Option<String>,
    /// Auto-save screenshot after loading (then exit)
    auto_screenshot: Option<PathBuf>,
    selection_audit: Option<PathBuf>,
    bathymetry_audit: Option<PathBuf>,
    animation_audit: Option<PathBuf>,
    viewing_date: Option<String>,
    viewing_instant: Option<String>,
    local_time_offset: Option<String>,
    /// Debug interior rings: log detailed ring info
    debug_rings: bool,
    /// Override zoom level for auto-screenshot (1.0 = fit to window)
    auto_zoom: Option<f64>,
    /// Override center position for auto-screenshot (lat,lon in degrees)
    auto_center: Option<(f64, f64)>,
}

impl AppConfig {
    fn from_args() -> Self {
        let base = get_app_base_dir();
        let args: Vec<String> = std::env::args().collect();
        let debug_mode = args.iter().any(|arg| arg == "--debug" || arg == "--DEBUG");
        let debug_rings = args.iter().any(|arg| arg == "--debug-rings");

        // Parse --chart <path> (can appear multiple times or use glob)
        let mut auto_chart = Vec::new();
        let mut i = 1;
        while i < args.len() {
            if args[i] == "--chart" {
                if let Some(path_str) = args.get(i + 1) {
                    let path = PathBuf::from(path_str);
                    if path.is_dir() {
                        // Exchange sets store cells in nested directories.
                        let mut charts: Vec<_> = walkdir::WalkDir::new(&path)
                            .follow_links(true)
                            .into_iter()
                            .filter_map(|entry| entry.ok())
                            .filter(|entry| entry.file_type().is_file())
                            .map(|entry| entry.into_path())
                            .filter(|p| s101_update_plan::is_chart_file(p))
                            .collect();
                        charts.sort();
                        auto_chart.extend(charts);
                    } else {
                        auto_chart.push(path);
                    }
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }

        let mut auto_s102 = Vec::new();
        for pair in args.windows(2).filter(|p| p[0] == "--s102") {
            let path = PathBuf::from(&pair[1]);
            if path.is_dir() {
                auto_s102.extend(
                    walkdir::WalkDir::new(path)
                        .into_iter()
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().is_file())
                        .map(|e| e.into_path())
                        .filter(|p| dataset_discovery::is_dataset_file(p, "h5")),
                );
            } else {
                auto_s102.push(path);
            }
        }
        auto_s102.sort();
        // Parse --screenshot <path>
        let auto_screenshot = args
            .windows(2)
            .find(|w| w[0] == "--screenshot")
            .map(|w| PathBuf::from(&w[1]));

        // Parse --zoom <level>
        let auto_zoom = args
            .windows(2)
            .find(|w| w[0] == "--zoom")
            .and_then(|w| w[1].parse::<f64>().ok())
            .and_then(navigation::bounded_zoom);

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
            fc_path: args
                .windows(2)
                .find(|w| w[0] == "--fc")
                .map(|w| PathBuf::from(&w[1]))
                .unwrap_or_else(|| base.join("Catalogues/FC/S-101")),
            pc_path: args
                .windows(2)
                .find(|w| w[0] == "--pc")
                .map(|w| PathBuf::from(&w[1]))
                .unwrap_or_else(|| base.join("Catalogues/PC/S-101")),
            log_path: base.join("logs"),
            debug_mode,
            auto_chart,
            auto_s102,
            s102_adjustments_path: args
                .windows(2)
                .find(|p| p[0] == "--s102-datum-adjustments")
                .map(|p| PathBuf::from(&p[1])),
            require_signatures: args.iter().any(|a| a == "--require-signatures"),
            initial_interoperability_enabled: !args.iter().any(|a| a == "--no-interoperability"),
            ic_transition_audit: args
                .windows(2)
                .find(|p| p[0] == "--ic-transition-audit")
                .map(|p| PathBuf::from(&p[1])),
            ic_path: args
                .windows(2)
                .find(|p| p[0] == "--ic")
                .map(|p| PathBuf::from(&p[1])),
            ic_trust_root: args
                .windows(2)
                .find(|p| p[0] == "--ic-trust-root")
                .map(|p| PathBuf::from(&p[1])),
            ic_administrator: args
                .windows(2)
                .find(|p| p[0] == "--ic-administrator")
                .map(|p| p[1].clone())
                .unwrap_or_else(|| "IHO".into()),
            portrayal_audit: args
                .windows(2)
                .find(|p| p[0] == "--portrayal-audit")
                .map(|p| PathBuf::from(&p[1])),
            ic_audit: args
                .windows(2)
                .find(|p| p[0] == "--ic-audit")
                .map(|p| PathBuf::from(&p[1])),
            s102_pc_path: args
                .windows(2)
                .find(|p| p[0] == "--s102-pc")
                .map(|p| PathBuf::from(&p[1]))
                .unwrap_or_else(|| base.join("Catalogues/PC/S-102")),
            initial_display_mode: args
                .iter()
                .position(|a| a == "--display-mode")
                .map(|i| args.get(i + 1).cloned().unwrap_or_default()),
            initial_viewing_layers: args
                .iter()
                .enumerate()
                .filter(|(_, a)| a.as_str() == "--viewing-layer")
                .map(|(i, _)| args.get(i + 1).cloned().unwrap_or_default())
                .collect(),
            initial_globe_samples: args
                .iter()
                .position(|a| a == "--globe-samples")
                .map(|i| args.get(i + 1).cloned().unwrap_or_default()),
            auto_screenshot,
            viewing_date: args
                .windows(2)
                .find(|a| a[0] == "--viewing-date")
                .map(|a| a[1].clone()),
            viewing_instant: args
                .windows(2)
                .find(|a| a[0] == "--viewing-instant")
                .map(|a| a[1].clone()),
            local_time_offset: args
                .windows(2)
                .find(|a| a[0] == "--local-time-offset")
                .map(|a| a[1].clone()),
            selection_audit: args
                .windows(2)
                .find(|a| a[0] == "--selection-audit")
                .map(|a| PathBuf::from(&a[1])),
            bathymetry_audit: args
                .windows(2)
                .find(|a| a[0] == "--bathymetry-audit")
                .map(|a| PathBuf::from(&a[1])),
            animation_audit: args
                .windows(2)
                .find(|a| a[0] == "--animation-audit")
                .map(|a| PathBuf::from(&a[1])),
            debug_rings,
            auto_zoom,
            auto_center,
        }
    }
}

/// Rebuild raw data extents after a cell replacement; raster tile bounds remain
/// part of the same mixed-product view. No temporary flattened coordinate arrays.
fn chart_data_bounds(
    cells: &[S101Cell],
    rasters: impl IntoIterator<Item = GeoBounds>,
) -> GeoBounds {
    let mut bounds = GeoBounds::default();
    for cell in cells {
        for point in cell.points.values() {
            bounds.expand(WorldPoint::new(point.position.x, point.position.y));
        }
        for points in cell.multi_points.values() {
            for pos in &points.positions {
                bounds.expand(WorldPoint::new(pos.x, pos.y));
            }
        }
        for curve in cell.curves.values() {
            for segment in &curve.segments {
                for pos in &segment.positions {
                    bounds.expand(WorldPoint::new(pos.x, pos.y));
                }
            }
        }
    }
    for raster in rasters {
        bounds.expand(WorldPoint::new(raster.min_x, raster.min_y));
        bounds.expand(WorldPoint::new(raster.max_x, raster.max_y));
    }
    bounds
}

/// Result of background chart loading
struct ChartLoadResult {
    base_metadata: Option<s101_lifecycle_metadata::MetadataEvidence>,
    metadata: Option<s101_lifecycle_metadata::MetadataEvidence>,
    input_paths: Vec<PathBuf>,
    cell: S101Cell,
    source_identity: CellSourceIdentity,
}

enum ChartPublicationResult {
    Load(ChartLoadResult),
    Cancel(chart_publication::ValidatedRemoval),
}
struct PreparedPortrayal {
    context: RenderContext,
    coverage: Arc<ferrite_s101::coverage_projection::GeographicCoverageInventory>,
    ic_changed: usize,
    geometry_cache:
        Arc<std::sync::Mutex<ferrite_wgpu::globe_coverage_projection::CoverageGeometryCache>>,
    globe: Option<ferrite_wgpu::PreparedGlobePublication>,
}

/// State belonging to the displayed datasets before an attempted load.
struct LoadCheckpoint {
    verified_count: usize,
    unsigned_count: usize,
    security_ui: Option<(String, String)>,
    frames_since_loaded: Option<u32>,
    signature_mode: bool,
    catalogue_identity: ([u8; 32], [u8; 32]),
}

/// Background loading state
struct BackgroundLoadingState {
    /// Number of files being loaded
    total_files: usize,
    /// Number of files loaded so far
    loaded_count: usize,
    pending: Vec<ChartPublicationResult>,
    failed: bool,
    checkpoint: LoadCheckpoint,
    /// Receiver for loaded cells
    receiver: Receiver<Result<ChartPublicationResult>>,
}

/// Rendered symbol info for hit testing
/// Fields ordered by size (largest first) for optimal memory layout
#[derive(Clone, Debug, PartialEq)]
struct RenderedSymbol {
    source: Option<usize>,
    plane: ferrite_kernel::CompositionPlane,
    /// Point = 0, line = 1, area = 2; geometry breaks ties within a display plane/priority.
    kind: u8,
    world_x: f64,
    world_y: f64,
    longitude_shift: f64,
    feature_id: i64,
    screen_x: f32,
    screen_y: f32,
    /// Drawing priority (higher = drawn on top, should be selected first)
    priority: i32,
    symbol_ref: String,
    /// Cell index this symbol belongs to (for correct feature lookup in multi-cell scenarios)
    cell_index: Option<u32>,
}

impl RenderedSymbol {
    fn compare_hits(a: &(Self, f64), b: &(Self, f64)) -> std::cmp::Ordering {
        b.0.plane
            .cmp(&a.0.plane)
            .then(b.0.priority.cmp(&a.0.priority))
            .then(a.0.kind.cmp(&b.0.kind))
            .then(a.1.total_cmp(&b.1))
            .then(a.0.cell_index.cmp(&b.0.cell_index))
            .then(a.0.feature_id.cmp(&b.0.feature_id))
    }
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
    touch_navigation: navigation::TouchNavigation,
    pinch_ownership: navigation::GestureOwnership,
    pan_gesture_ownership: navigation::GestureOwnership,
    navigation_modifiers: winit::keyboard::ModifiersState,
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
    next_temporal_wake: Option<std::time::Instant>,
    /// Recent mouse positions for velocity calculation (screen coords, time)
    recent_positions: Vec<((f64, f64), std::time::Instant)>,
    /// Feature Catalogue reference for attribute lookup
    fc: Arc<BoundFeatureCatalogue>,
    /// Portrayal Catalogue reference
    pc: Arc<BoundPortrayalCatalogue>,
    /// Feature Catalogue status (for UI display)
    fc_status: CatalogueStatus,
    /// Portrayal Catalogue status (for UI display)
    pc_status: CatalogueStatus,
    /// All loaded S101 cells
    cells: Vec<S101Cell>,
    loaded_source_identities: cell_source_identity::LoadedSourceIdentities,
    loaded_chain_paths: Vec<Vec<PathBuf>>,
    loaded_discovery: std::collections::BTreeMap<
        (String, String),
        Option<s101_lifecycle_metadata::MetadataEvidence>,
    >,
    cancellation_history_path: PathBuf,
    coverage_inventory: Option<Arc<ferrite_s101::coverage_projection::GeographicCoverageInventory>>,
    coverage_geometry_cache: Option<
        Arc<std::sync::Mutex<ferrite_wgpu::globe_coverage_projection::CoverageGeometryCache>>,
    >,
    ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
    ic_assigned_vectors: usize,
    initial_interoperability_enabled: bool,
    ic_transition_audit: Option<PathBuf>,
    ic_audit: Option<PathBuf>,
    portrayal_audit: Option<PathBuf>,
    bathymetry: Vec<(
        PathBuf,
        BathymetryCoverage,
        Option<Arc<AuthenticatedSnapshot>>,
    )>,
    bathymetry_bounds: std::collections::HashMap<PathBuf, GeoBounds>,
    depth_policies: std::collections::HashMap<PathBuf, DepthPolicy>,
    depth_inputs: std::collections::HashMap<PathBuf, Arc<UnauthenticatedSnapshot>>,
    require_signatures: bool,
    verified_count: usize,
    unsigned_count: usize,
    startup_error: Option<String>,
    publication_test_fail_before_commit: bool,
    pending_auto_s102: Vec<PathBuf>,
    s102_pc_path: PathBuf,
    s102_adjustments_path: Option<PathBuf>,
    /// Whether chart data is loaded
    chart_loaded: bool,
    /// Paths of already loaded chart files (to prevent duplicates)
    /// Background loading state (Some if loading in progress)
    loading_state: Option<BackgroundLoadingState>,
    /// Plugin system
    plugin_system: plugins::PluginSystem,
    /// Base instruction count (chart instructions only, before plugin instructions)
    base_instruction_count: usize,
    applied_settings: SettingsState,
    initial_globe_samples: u32,
    /// Debug mode enabled
    debug_mode: bool,
    /// Charts to auto-load on startup
    pending_auto_chart: Vec<PathBuf>,
    /// Auto-screenshot path (take screenshot after load, then exit)
    auto_screenshot: Option<PathBuf>,
    selection_audit: Option<PathBuf>,
    bathymetry_audit: Option<PathBuf>,
    animation_audit: Option<PathBuf>,
    /// Debug interior rings
    debug_rings: bool,
    /// Override zoom level for auto-screenshot
    auto_zoom: Option<f64>,
    /// Override center position for auto-screenshot (lat, lon)
    auto_center: Option<(f64, f64)>,
    /// Frame count since load completed (for auto-screenshot timing)
    frames_since_loaded: Option<u32>,
    coverage_lifecycle_resize: Option<CoverageLifecycleResize>,
    /// Frame times for FPS calculation
    frame_times: std::collections::VecDeque<std::time::Instant>,
    process_stats: process_stats::ProcessStats,
    debug_stats_audit: Option<PathBuf>,
    debug_stats_samples: Vec<serde_json::Value>,
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
        fc: Arc<BoundFeatureCatalogue>,
        pc: Arc<BoundPortrayalCatalogue>,
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
            render_context: {
                let mut context = RenderContext::new(Viewport::new(1920.0, 1080.0));
                context
                    .scaler
                    .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
                context
            },
            bounds: GeoBounds::new(-180.0, -90.0, 180.0, 90.0),
            symbol_cache,
            current_profile_name: initial_profile,
            mouse_pos: (0.0, 0.0),
            rendered_symbols: Vec::new(),
            pending_hit_test: None,
            is_dragging: false,
            touch_navigation: navigation::TouchNavigation::default(),
            pinch_ownership: navigation::GestureOwnership::default(),
            pan_gesture_ownership: navigation::GestureOwnership::default(),
            navigation_modifiers: winit::keyboard::ModifiersState::empty(),
            drag_start: (0.0, 0.0),
            zoom_level: 1.0,
            pan_offset: (0.0, 0.0),
            pan_velocity: (0.0, 0.0),
            last_frame_time: std::time::Instant::now(),
            next_temporal_wake: None,
            recent_positions: Vec::new(),
            fc,
            pc,
            fc_status,
            pc_status,
            cells: Vec::new(),
            loaded_source_identities: Default::default(),
            loaded_chain_paths: Vec::new(),
            loaded_discovery: Default::default(),
            cancellation_history_path: chart_publication::default_history_path(),
            coverage_inventory: None,
            coverage_geometry_cache: None,
            ic: None,
            ic_assigned_vectors: 0,
            initial_interoperability_enabled: true,
            ic_transition_audit: None,
            ic_audit: None,
            portrayal_audit: None,
            bathymetry: Vec::new(),
            bathymetry_bounds: Default::default(),
            depth_policies: std::collections::HashMap::new(),
            depth_inputs: std::collections::HashMap::new(),
            require_signatures: false,
            verified_count: 0,
            unsigned_count: 0,
            startup_error: None,
            publication_test_fail_before_commit: false,
            pending_auto_s102: Vec::new(),
            s102_adjustments_path: None,
            s102_pc_path: get_app_base_dir().join("Catalogues/PC/S-102"),
            chart_loaded: false,
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
            applied_settings: SettingsState::default(),
            initial_globe_samples: 4,
            debug_mode,
            pending_auto_chart,
            auto_screenshot,
            selection_audit: None,
            bathymetry_audit: None,
            animation_audit: None,
            debug_rings,
            auto_zoom,
            auto_center,
            frames_since_loaded: None,
            coverage_lifecycle_resize: None,
            frame_times: std::collections::VecDeque::with_capacity(60),
            process_stats: process_stats::ProcessStats::default(),
            debug_stats_audit: ferrite_wgpu::background_test::enabled()
                .then(|| std::env::var_os("FERRITE_DEBUG_STATS_AUDIT").map(PathBuf::from))
                .flatten(),
            debug_stats_samples: Vec::new(),
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
                    renderer.background_color = lookup_pc_color(&self.pc, "DEPDW", profile_name);
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

    /// Validate and stage both products before replacing any active portrayal or settings.
    fn apply_portrayal_settings(&mut self) -> Result<()> {
        let candidate = self.renderer.as_ref().map(|r| r.settings().clone());
        validated_lua_context(&self.pc, candidate.as_ref())?;
        let mut next = self.render_context.empty_for_rebuild();
        if !self.cells.is_empty() {
            try_lua_portrayal(
                &self.cells,
                &self.fc,
                &self.pc,
                &mut next,
                &self.current_profile_name,
                candidate.as_ref(),
            )?;
        }
        let ic_changed = if let Some(ic) = self.active_ic() {
            interoperability::compose_vectors(&ic, &self.cells, &self.fc, &mut next)?
        } else {
            0
        };
        // raster_batch commits only when every candidate tile has been prepared.
        // The following vector swap is infallible, so a failing product changes neither display.
        self.recolor_bathymetry()?;
        self.render_context = next;
        self.ic_assigned_vectors = ic_changed;
        self.base_instruction_count = self.render_context.instruction_count();
        if let Some(candidate) = candidate {
            self.applied_settings = candidate;
        }
        self.update_interoperability_status();
        Ok(())
    }

    fn active_ic(&self) -> Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>> {
        let enabled = self
            .renderer
            .as_ref()
            .map(|r| r.settings().interoperability_enabled)
            .unwrap_or(self.initial_interoperability_enabled);
        if enabled {
            self.ic.clone()
        } else {
            None
        }
    }
    fn update_interoperability_status(&mut self) {
        if let (Some(ic), Some(r)) = (&self.ic, &mut self.renderer) {
            r.ui_state.interoperability_active = r.settings().interoperability_enabled;
            r.ui_state.interoperability_status = format!(
                "Interoperability {}: {} {} (signature verified)",
                if r.settings().interoperability_enabled {
                    "on"
                } else {
                    "off"
                },
                ic.catalogue.name,
                ic.catalogue.version
            );
        }
    }
    /// Drive the same staged Settings Apply path; this is not OS mouse automation.
    fn audit_interoperability_transitions(&mut self, out: &Path) -> Result<()> {
        fs::create_dir_all(out)?;
        anyhow::ensure!(
            self.ic.is_some(),
            "IC transition audit requires authenticated catalogue"
        );
        for (name, enabled) in [("on", true), ("off", false), ("restored", true)] {
            self.renderer
                .as_mut()
                .context("Renderer not initialized")?
                .ui_state
                .settings
                .interoperability_enabled = enabled;
            self.apply_portrayal_settings()?;
            self.renderer
                .as_mut()
                .unwrap()
                .precompute_triangulations(&self.render_context);
            self.update_view();
            self.audit_interoperability(&out.join(format!("{name}.json")))?;
            self.renderer
                .as_mut()
                .unwrap()
                .save_screenshot(out.join(format!("{name}.png")))?;
        }
        Ok(())
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
            .unwrap_or(self.applied_settings.display_mode);

        let preset = match display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        // Every catalogue installation validates these mappings before commit.
        let mut visible = ferrite_s101::viewing_groups_for_preset(&self.pc, preset)
            .expect("Installed S-101 display modes were prevalidated");
        let settings = self
            .renderer
            .as_ref()
            .map(|r| r.settings())
            .unwrap_or(&self.applied_settings);
        visible.extend(
            ferrite_s101::viewing_groups_for_layers(
                &self.pc,
                settings.viewing_layers.iter().map(String::as_str),
            )
            .expect("Selected S-101 layers were prevalidated"),
        );
        visible.insert(21010); // Existing plugin overlay group.
        Some(visible)
    }

    fn signature_verification_enabled(&self) -> bool {
        self.renderer
            .as_ref()
            .map(|r| r.ui_state.verify_dataset_signatures)
            .unwrap_or(self.require_signatures)
    }

    fn authenticate_paths(&mut self, paths: &[PathBuf]) -> Result<AuthorizedDatasets> {
        let verify_signatures = self.signature_verification_enabled();
        if !verify_signatures {
            let result = dataset_signature_policy::unchecked_datasets(paths);
            if let Some(r) = &mut self.renderer {
                r.ui_state.security_status = if result.is_ok() {
                    "Digital signature verification OFF: datasets opened without verification"
                        .into()
                } else {
                    "Dataset could not be opened".into()
                };
                r.ui_state.security_details = result
                    .as_ref()
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_default();
            }
            return result;
        }
        let mut anchors = TrustAnchors::default();
        let root = get_app_base_dir().join("Trust/IHO-S100-5.2.pem");
        if root.exists() {
            anchors.install_pem("IHO", &fs::read(root)?)?;
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        let result = authorize_datasets(paths, &anchors, time, UnsignedPolicy::Reject);
        match &result {
            Ok(report) => {
                self.verified_count += report.signed_count;
                self.unsigned_count += report.unsigned_count;
                info!(
                    "Authentication: {} verified, {} unsigned evaluation datasets",
                    report.signed_count, report.unsigned_count
                );
                for warning in &report.metadata_warnings {
                    warn!("Signature metadata: {warning}");
                }
                if let Some(r) = &mut self.renderer {
                    r.ui_state.security_status = format!(
                        "Data signatures: {} verified / {} unsigned evaluation",
                        self.verified_count, self.unsigned_count
                    );
                    r.ui_state.security_details = report.metadata_warnings.join("\n");
                }
            }
            Err(e) => {
                if let Some(r) = &mut self.renderer {
                    r.ui_state.security_status = "Data authentication failed; load rejected".into();
                    r.ui_state.security_details = format!("{e:#}");
                }
            }
        }
        result
    }

    fn bathymetry_portrayal(&self) -> Result<BathymetryPortrayal> {
        let pc = PortrayalCatalogue::load(&self.s102_pc_path)?;
        let s = self
            .renderer
            .as_ref()
            .context("Renderer not initialized")?
            .settings();
        BathymetryPortrayal::from_catalogue(
            &pc,
            &self.current_profile_name,
            DepthSettings {
                safety_contour: s.safety_contour,
                shallow_contour: s.shallow_contour,
                deep_contour: s.deep_contour,
                four_shades: !s.two_shades,
            },
        )
    }

    fn load_bathymetry(&mut self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            self.loading_state.is_none(),
            "Finish or clear the current S-101 load before adding bathymetry"
        );
        let paths: Vec<_> = paths
            .iter()
            .filter(|p| {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                !self
                    .bathymetry
                    .iter()
                    .any(|(loaded, _, _)| *loaded == canonical)
            })
            .cloned()
            .collect();
        if paths.is_empty() {
            return Ok(());
        }
        let require_signature = self.signature_verification_enabled();
        let authorization = self.authenticate_paths(&paths)?;
        let portrayal = self.bathymetry_portrayal()?;
        for path in &paths {
            let path = path
                .canonicalize()
                .with_context(|| format!("Cannot open {}", path.display()))?;
            if self.bathymetry.iter().any(|(p, _, _)| p == &path) {
                continue;
            }
            let snapshot = dataset_signature_policy::dataset_snapshot(
                &authorization,
                &path,
                require_signature,
            )?;
            let unsigned_input = if snapshot.is_none() && self.s102_adjustments_path.is_some() {
                Some(Arc::new(UnauthenticatedSnapshot::copy(&path)?))
            } else {
                None
            };
            let data_path = snapshot
                .as_ref()
                .map(|s| s.path())
                .or_else(|| unsigned_input.as_ref().map(|s| s.path()))
                .unwrap_or(&path);
            let coverages = BathymetryCoverage::open(data_path)?;
            let policy = DepthPolicy::load(
                self.s102_adjustments_path.as_deref(),
                data_path,
                &coverages
                    .iter()
                    .map(|c| c.vertical_datum)
                    .collect::<Vec<_>>(),
            )?;
            let mosaic = ConservativeCoverage::new(
                coverages
                    .iter()
                    .map(|c| DatumCoverage {
                        coverage: c,
                        reference: DepthReference(c.vertical_datum as u64),
                    })
                    .collect(),
                DepthReference(policy.target as u64),
                &policy,
            )?;
            let raw = coverages.len() == 1 && policy.identity(coverages[0].vertical_datum);
            let numeric: &dyn NumericCoverageSource = if raw { &coverages[0] } else { &mosaic };
            if !raw {
                anyhow::ensure!(
                    numeric
                        .numeric_geometry()
                        .width
                        .checked_mul(numeric.numeric_geometry().height)
                        .is_some_and(|n| n <= 64 * 1024 * 1024),
                    "Composed depth output exceeds 64M-cell application work budget"
                );
            }
            let assignment = self
                .active_ic()
                .map(|ic| coverages[0].interoperability_assignment(&ic.catalogue))
                .transpose()?
                .flatten();
            if let Some(ic) = self.active_ic() {
                for c in &coverages {
                    anyhow::ensure!(
                        c.interoperability_assignment(&ic.catalogue)? == assignment,
                        "Cannot compose S102 instances with different interoperability assignments"
                    );
                }
            }
            let edge = self.bathymetry_tile_edge()?;
            let mut tile_bounds: Option<GeoBounds> = None;
            self.renderer
                .as_mut()
                .context("Renderer not initialized")?
                .raster_batch(&self.render_context.scaler, false, |upload| {
                    for window in numeric.numeric_geometry().windows(edge, edge)? {
                        let layer = interoperability::compose_raster(
                            portrayal.raster_window(
                                numeric,
                                window,
                                format!(
                                    "{}:composed:{}:{}",
                                    path.display(),
                                    window.column,
                                    window.row
                                ),
                            )?,
                            assignment.as_ref(),
                        )?;
                        let b = layer.bounds;
                        if let Some(bounds) = &mut tile_bounds {
                            bounds.min_x = bounds.min_x.min(b.min_x);
                            bounds.min_y = bounds.min_y.min(b.min_y);
                            bounds.max_x = bounds.max_x.max(b.max_x);
                            bounds.max_y = bounds.max_y.max(b.max_y);
                        } else {
                            tile_bounds = Some(b);
                        }
                        upload(layer)?;
                    }
                    Ok::<_, anyhow::Error>(())
                })?;
            let bounds = tile_bounds.context("No coverage tiles")?;
            self.bathymetry_bounds.insert(path.clone(), bounds);
            if !self.chart_loaded && self.bathymetry.is_empty() {
                self.bounds = bounds;
            } else {
                self.bounds.min_x = self.bounds.min_x.min(bounds.min_x);
                self.bounds.min_y = self.bounds.min_y.min(bounds.min_y);
                self.bounds.max_x = self.bounds.max_x.max(bounds.max_x);
                self.bounds.max_y = self.bounds.max_y.max(bounds.max_y);
            }
            drop(mosaic);
            for c in &coverages {
                if c.observed_range_violations().iter().any(|v| *v) {
                    warn!("S-102 {} {}: decoded values outside declared extrema; original samples retained",path.display(),c.instance_name);
                }
                if !c.root_enclosure.encoding_compatible() {
                    warn!(
                        "S-102 {} {}: root geographic enclosure {:?}; raw values retained",
                        path.display(),
                        c.instance_name,
                        c.root_enclosure
                    );
                }
                if c.observed_depth_centroids_outside_domain() {
                    warn!("S-102 {} {}: populated original depth sample positions outside instance validity; raw samples retained",path.display(),c.instance_name);
                }
            }
            if let Some(input) = unsigned_input {
                self.depth_inputs.insert(path.clone(), input);
            }
            self.depth_policies.insert(path.clone(), policy);
            self.bathymetry.extend(
                coverages
                    .into_iter()
                    .map(|c| (path.clone(), c, snapshot.clone())),
            );
            self.chart_loaded = true;
        }
        if let Some(r) = &mut self.renderer {
            r.ui_state.bathymetry_count = self.bathymetry.len();
            r.ui_state.chart_count = self.cells.len() + self.bathymetry.len();
        }
        if self.auto_screenshot.is_some() && self.loading_state.is_none() {
            if let Some((lat, lon)) = self.auto_center {
                if let Some(pan) = self
                    .render_context
                    .scaler
                    .projection()
                    .pan_to(self.bounds, WorldPoint::new(lon, lat))
                {
                    self.pan_offset = (pan[0], pan[1]);
                }
            }
            if let Some(zoom) = self.auto_zoom {
                self.zoom_level = zoom;
                self.zoom_target = zoom;
            }
            self.frames_since_loaded = Some(0);
        }
        self.update_view();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        info!("S-102 loaded: {} coverages", self.bathymetry.len());
        Ok(())
    }

    fn bathymetry_tile_edge(&self) -> Result<usize> {
        let limit = self
            .renderer
            .as_ref()
            .context("Renderer not initialized")?
            .raster_texture_limit() as usize;
        let requested = match std::env::var("FERRITE_RASTER_TILE_EDGE") {
            Ok(value) => value
                .parse::<usize>()
                .context("Invalid FERRITE_RASTER_TILE_EDGE")?,
            Err(std::env::VarError::NotPresent) => 2048,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(requested > 0, "FERRITE_RASTER_TILE_EDGE must be positive");
        Ok(requested.min(limit))
    }

    fn recolor_bathymetry(&mut self) -> Result<()> {
        if self.bathymetry.is_empty() {
            return Ok(());
        }
        let portrayal = self.bathymetry_portrayal()?;
        let edge = self.bathymetry_tile_edge()?;
        let active_ic = self.active_ic();
        if let Some(r) = &mut self.renderer {
            r.raster_batch(&self.render_context.scaler, true, |upload| {
                for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0) {
                    let (p, c, _) = &group[0];
                    let policy = self
                        .depth_policies
                        .get(p)
                        .context("Missing loaded depth policy")?;
                    let mosaic = ConservativeCoverage::new(
                        group
                            .iter()
                            .map(|(_, c, _)| DatumCoverage {
                                coverage: c,
                                reference: DepthReference(c.vertical_datum as u64),
                            })
                            .collect(),
                        DepthReference(policy.target as u64),
                        policy,
                    )?;
                    let numeric: &dyn NumericCoverageSource =
                        if group.len() == 1 && policy.identity(c.vertical_datum) {
                            c
                        } else {
                            &mosaic
                        };
                    let assignment = active_ic
                        .as_ref()
                        .map(|ic| c.interoperability_assignment(&ic.catalogue))
                        .transpose()?
                        .flatten();
                    if let Some(ic) = active_ic.as_ref() {
                        for (_, c, _) in group {
                            anyhow::ensure!(
                                c.interoperability_assignment(&ic.catalogue)? == assignment,
                                "Cannot compose differing interoperability assignments"
                            );
                        }
                    }
                    for window in numeric.numeric_geometry().windows(edge, edge)? {
                        upload(interoperability::compose_raster(
                            portrayal.raster_window(
                                numeric,
                                window,
                                format!(
                                    "{}:composed:{}:{}",
                                    p.display(),
                                    window.column,
                                    window.row
                                ),
                            )?,
                            assignment.as_ref(),
                        )?)?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Ok(())
    }

    fn inspect_bathymetry(&self, lon: f64, lat: f64) -> Result<Option<String>> {
        let mut missing = None;
        let wrapping = self
            .renderer
            .as_ref()
            .is_some_and(|r| r.longitude_wrapping_enabled());
        for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0).rev() {
            let path = &group[0].0;
            let policy = self
                .depth_policies
                .get(path)
                .context("Missing loaded depth policy")?;
            let mosaic = ConservativeCoverage::new(
                group
                    .iter()
                    .map(|(_, c, _)| DatumCoverage {
                        coverage: c,
                        reference: DepthReference(c.vertical_datum as u64),
                    })
                    .collect(),
                DepthReference(policy.target as u64),
                policy,
            )?;
            for shift in &([0., -360., 360.])[..if wrapping { 3 } else { 1 }] {
                let Some(selected) = mosaic.query_nearest(lon - shift, lat)? else {
                    if mosaic.covers_position(lon - shift, lat)? {
                        missing = Some(format!("S-102 depth: No data\n{}", path.display()));
                    }
                    continue;
                };
                let sample = selected.candidate;
                let c = &group[sample.source.instance].1;
                let uncertainty = sample
                    .uncertainty
                    .map(|v| format!("{v:.2} m"))
                    .unwrap_or_else(|| "No data".into());
                let coordinate_uncertainty = |value: f32| {
                    if value == -1. {
                        "Unknown".to_owned()
                    } else {
                        format!("{value:.2} m")
                    }
                };
                let mut info=format!("S-102 depth: {:.2} m\nCell depth uncertainty: {uncertainty}\nContainer horizontal coordinate uncertainty: {}\nContainer vertical coordinate uncertainty: {}\nVertical CRS: {} / datum code: {}\n{}",
                    selected.adjusted_depth,coordinate_uncertainty(c.horizontal_position_uncertainty),coordinate_uncertainty(c.vertical_position_uncertainty),
                    c.vertical_crs,c.vertical_datum,path.file_name().unwrap_or_default().to_string_lossy());
                info.push_str(&format!(
                    "\nProduct issue date: {}\nProduct issue time: {}\nEncoded axes: {} / {}",
                    c.issue.date,
                    c.issue.time.as_deref().unwrap_or("Not provided"),
                    c.axes.names[0],
                    c.axes.names[1]
                ));
                info.push_str(&format!(
                    "\nDeclared S-102 features: {}",
                    c.feature_metadata.declared_features.join(", ")
                ));
                for diagnostic in &c.feature_metadata.diagnostics {
                    info.push_str(&format!("\nFeature metadata diagnostic: {diagnostic:?}"));
                }
                for definition in c
                    .feature_metadata
                    .bathymetry
                    .iter()
                    .chain(c.feature_metadata.quality.iter())
                {
                    info.push_str(&format!(
                        "\nDefinition {}: unit '{}', {}, fill {}, {} [{}, {}]",
                        definition.code,
                        definition.unit,
                        definition.datatype,
                        definition.fill_value,
                        definition.closure,
                        definition.lower,
                        definition.upper
                    ));
                }
                if !c.axes.matches_generic_array_major_order()
                    || c.quality
                        .as_ref()
                        .is_some_and(|q| !q.axes.matches_generic_array_major_order())
                {
                    info.push_str("\nAxis metadata diagnostic: declared axis order differs from generic S-100 array major order. Original S-102 east-then-north samples are retained; no automatic transpose.");
                }
                info.push_str(&format!(
                    "\nRoot geographic extent assessment: grid {:?}, domain {:?}",
                    c.root_enclosure.full_grid, c.root_enclosure.declared_domain
                ));
                info.push_str(&format!("\nTarget datum code: {}\nRaw source depth: {:.8} m\nPositive-down correction: {:+.8} m\nCorrection source: {}\nHost-supplied correction; not dataset-authenticated transformation evidence.\nSource instance: {}",
                    policy.target,sample.raw_depth.unwrap(),selected.adjustment.correction_metres,policy.source(selected.adjustment.provenance),c.instance_name));
                info.push_str(&format!("\nQuery longitude / latitude: {lon:.8} / {lat:.8}\nSample longitude / latitude: {:.8} / {:.8}\nGrid column / row (zero-based): {} / {}\nNearest grid node; no spatial interpolation. Shared boundaries use the least adjusted depth.",sample.x,sample.y,sample.source.column,sample.source.row));
                if *shift != 0. {
                    info.push_str(&format!("\nDisplay longitude copy: {shift:+.0}°; sample coordinates are from the source."));
                }
                let metadata_issues = group
                    .iter()
                    .filter_map(|(_, source, _)| {
                        let issues = source.observed_range_violations();
                        issues.iter().any(|v| *v).then(|| {
                            format!(
                                "{} ({})",
                                source.instance_name,
                                if issues[0] && issues[1] {
                                    "depth and uncertainty"
                                } else if issues[0] {
                                    "depth"
                                } else {
                                    "uncertainty"
                                }
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                if !metadata_issues.is_empty() {
                    info.push_str(&format!("\nDataset metadata warning: {} values outside declared extrema. Original samples retained.",metadata_issues.join(", ")));
                }
                let domain_issues = group
                    .iter()
                    .filter(|(_, source, _)| source.observed_depth_centroids_outside_domain())
                    .map(|(_, source, _)| source.instance_name.as_str())
                    .collect::<Vec<_>>();
                if !domain_issues.is_empty() {
                    info.push_str(&format!("\nValidity diagnostic: {} populated depth sample positions outside their own domain. Raw samples retained; this is not an entire-cell intersection test.",domain_issues.join(", ")));
                }
                let quality = match &c.quality {
                    Some(q) => {
                        let mut description=match q.sample_nearest(lon-shift,lat)? {
                            Some(record)=>record.description(),None=>"Quality: no record at this location (fill ID 0 or outside its own validity domain)".into(),
                        };
                        if q.observed_id_centroids_outside_domain() {
                            description.push_str("\nQuality validity diagnostic: nonzero original ID sample positions outside its own domain; IDs retained.");
                        }
                        description
                    }
                    None => "Quality coverage: not supplied".into(),
                };
                info.push_str("\n");
                info.push_str(&quality);
                return Ok(Some(info));
            }
        }
        Ok(missing)
    }

    /// Exercise the same coverage query used by mouse picking, without OS input.
    fn audit_interoperability(&self, output: &Path) -> Result<()> {
        let ic = self.ic.as_ref().context("No authenticated IC active")?;
        let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
        let vector_planes=self.render_context.raw_instructions().iter().filter_map(|i| {
            if let ferrite_render::DisplayPlane::Interoperability(order)=i.display_plane() {
                Some(serde_json::json!({"cell":i.cell_index(),"feature":i.feature_id(),"plane":order.get(),"priority":i.priority().0,"groups":i.viewing_groups().map(|g|g.0).collect::<Vec<_>>()}))
            }else{None}
        }).collect::<Vec<_>>();
        let raster=renderer.raster_composition_metadata().map(|(plane,priority,groups,visible)|
            serde_json::json!({"plane":plane,"priority":priority,"groups":groups,"visible":visible})).collect::<Vec<_>>();
        // Opt-in diagnostic snapshot: distinguish changed product output from GPU state.
        // This runs only for an explicit IC audit, never in the interactive render loop.
        let instructions = bincode::serialize(self.render_context.raw_instructions())?;
        let instructions_sha256 = format!("{:x}", Sha256::digest(&instructions));
        fs::write(output.with_extension("instructions.bin"), &instructions)?;
        let displayed_symbols = renderer.displayed_symbols();
        let render_stats = format!("{:?}", renderer.statistics());

        fs::write(
            output,
            serde_json::to_vec_pretty(&serde_json::json!({
                "app_ic_activated":self.active_ic().is_some(),"ui_ic_activated":renderer.ui_state.interoperability_active,"catalogue_name":ic.catalogue.name,"catalogue_version":ic.catalogue.version,
                "authenticated_metadata":ic.authorization.metadata,"source_sha384":ic.authorization.resource.sha384,
                "trust_anchor_sha256":ic.authorization.trust_anchor_sha256,"revocation_checked":ic.authorization.revocation_checked,
                "assigned_vectors":self.ic_assigned_vectors,"vector_planes":vector_planes,"raster_layers":raster,
                "full_s98_verified":false,"native_mouse_and_menu_verified":false,
                "instructions_sha256":instructions_sha256,"instruction_count":self.render_context.instruction_count(),
                "displayed_symbols":displayed_symbols,"render_stats":render_stats,"symbol_draw_batches":renderer.symbol_draw_batch_count()
            }))?,
        )?;
        Ok(())
    }

    /// Product-neutral opt-in trace. It does not require or activate an IC.
    fn audit_portrayal(&self, directory: &Path) -> Result<()> {
        fs::create_dir_all(directory)?;
        let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
        let instructions = bincode::serialize(self.render_context.raw_instructions())?;
        fs::write(directory.join("instructions.bin"), &instructions)?;
        fs::write(
            directory.join("snapshot.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "instruction_count": self.render_context.instruction_count(),
                "instructions_sha256": format!("{:x}",Sha256::digest(&instructions)),
                "displayed_symbols": renderer.displayed_symbols(),
                "settings": format!("{:?}",self.render_context.settings),
                "applied_settings": format!("{:?}",self.applied_settings),
                "digital_signature_verification_enabled": renderer.ui_state.verify_dataset_signatures,
                "digital_signature_status": renderer.ui_state.security_status,
                "verified_dataset_count": self.verified_count,
                "unsigned_evaluation_dataset_count": self.unsigned_count,
                "profile": self.current_profile_name,
                "viewport": renderer.chart_viewport_pixels(),
                "zoom": self.zoom_level,
                "render_stats": format!("{:?}",renderer.statistics()),
                "source_cells": self.cells.iter().map(|c| &c.file_path).collect::<Vec<_>>(),
                "native_physical_input_verified": false,
                "debug_statistics": {"cpu_percent": renderer.ui_state.debug_cpu_usage,
                    "resident_mib": renderer.ui_state.debug_memory_mb,
                    "logical_cpus": self.process_stats.logical_cpus(),
                    "updates": self.process_stats.updates()},
                "background_test": ferrite_wgpu::background_test::enabled(),
                "native_window_visible": self.window.as_ref().and_then(|w|w.is_visible()),
                "native_window_has_focus": self.window.as_ref().is_some_and(|w|w.has_focus()),
            }))?,
        )?;
        renderer.audit_geometry_buffers(&directory.join("gpu"))?;
        Ok(())
    }

    fn audit_bathymetry(&mut self, output: &Path) -> Result<()> {
        let mut probes = Vec::new();
        for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0) {
            let path = &group[0].0;
            let policy = self
                .depth_policies
                .get(path)
                .context("Missing loaded depth policy")?;
            let mosaic = ConservativeCoverage::new(
                group
                    .iter()
                    .map(|(_, c, _)| DatumCoverage {
                        coverage: c,
                        reference: DepthReference(c.vertical_datum as u64),
                    })
                    .collect(),
                DepthReference(policy.target as u64),
                policy,
            )?;
            let g = mosaic.numeric_geometry();
            let limit = if g.width.checked_mul(g.height).is_some_and(|n| n <= 4096) {
                g.width * g.height
            } else {
                2
            };
            let mut cells = Vec::new();
            for row in 0..g.height {
                for column in (0..g.width).step_by(2048) {
                    mosaic.visit_window_values(
                        ferrite_kernel::GridWindow {
                            column,
                            row,
                            width: 2048.min(g.width - column),
                            height: 1,
                        },
                        &mut |index, value| {
                            if let Some(value) = value {
                                if cells.len() < limit {
                                    cells.push((column + index, row, value));
                                }
                            }
                            Ok(())
                        },
                    )?;
                    if cells.len() == limit {
                        break;
                    }
                }
                if cells.len() == limit {
                    break;
                }
            }
            anyhow::ensure!(!cells.is_empty(), "Coverage has no valid depth to probe");
            for (column, row, value) in cells {
                let (lon, lat) = g.position(column, row).unwrap();
                let queries = [
                    (lon, lat),
                    (lon + g.spacing_x * 0.24, lat + g.spacing_y * 0.24),
                ];
                for (qx, qy, shift) in [0., -360., 360.]
                    .into_iter()
                    .flat_map(|shift| queries.map(|(x, y)| (x + shift, y, shift)))
                {
                    let expected = mosaic
                        .query_nearest(qx - shift, qy)?
                        .context("Missing composed query")?;
                    anyhow::ensure!(
                        expected.adjusted_depth == value,
                        "Numeric raster value and interior query differ"
                    );
                    let sample = expected.candidate;
                    let source = &group[sample.source.instance].1;
                    let record = source
                        .quality
                        .as_ref()
                        .map(|q| q.sample_nearest(qx - shift, qy))
                        .transpose()?
                        .flatten();
                    let info = self
                        .inspect_bathymetry(qx, qy)?
                        .context("Coverage absent from App query")?;
                    anyhow::ensure!(
                        info.contains(&format!("S-102 depth: {value:.2} m")),
                        "App query depth differs"
                    );
                    anyhow::ensure!(
                        info.contains(&format!(
                            "Sample longitude / latitude: {:.8} / {:.8}",
                            sample.x, sample.y
                        )),
                        "App lost source position"
                    );
                    anyhow::ensure!(
                        info.contains(&format!("Query longitude / latitude: {qx:.8} / {qy:.8}")),
                        "App lost query position"
                    );
                    if shift != 0. {
                        anyhow::ensure!(
                            info.contains(&format!("Display longitude copy: {shift:+.0}°")),
                            "Wrong longitude copy"
                        );
                    }
                    if let Some(record) = record {
                        anyhow::ensure!(
                            info.contains(&record.description()),
                            "App changed source quality attributes"
                        );
                    }
                    probes.push(serde_json::json!({"source":path,"longitude_shift":shift,"position":[qx,qy],"sample_position":[sample.x,sample.y],"grid_column":sample.source.column,"grid_row":sample.source.row,"source_instance":source.instance_name,"raw_depth":sample.raw_depth,"target_datum":policy.target,"correction":expected.adjustment.correction_metres,"depth":value,"uncertainty":sample.uncertainty,"quality_record_id":record.map(|r|r.id),"metadata_range_violations":source.observed_range_violations(),"root_full_grid_enclosure":format!("{:?}",source.root_enclosure.full_grid),"root_declared_domain_enclosure":format!("{:?}",source.root_enclosure.declared_domain),"populated_depth_centroids_outside_domain":source.observed_depth_centroids_outside_domain(),"quality_id_centroids_outside_own_domain":source.quality.as_ref().map(|q|q.observed_id_centroids_outside_domain()),"displayed_information":info}));
                }
            }
        }
        let before = self
            .renderer
            .as_ref()
            .context("No renderer for recolor audit")?
            .raster_composition_metadata()
            .map(|(p, o, g, v)| (p, o, g.to_vec(), v))
            .collect::<Vec<_>>();
        self.recolor_bathymetry()?;
        // Recolour invalidates prepared GPU panes; rebuild the same view before capture.
        self.update_view();
        if let Some(r) = &self.renderer {
            anyhow::ensure!(
                !r.ui_state.globe_preview || r.globe_preview_diagnostics().is_some(),
                "Recolor audit could not restore the globe view"
            );
        }
        let after = self
            .renderer
            .as_ref()
            .context("No renderer after recolor audit")?
            .raster_composition_metadata()
            .map(|(p, o, g, v)| (p, o, g.to_vec(), v))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            before == after,
            "Recolor changed raster composition metadata"
        );
        for probe in &probes {
            let x = probe["position"][0].as_f64().unwrap();
            let y = probe["position"][1].as_f64().unwrap();
            anyhow::ensure!(
                self.inspect_bathymetry(x, y)?.as_deref()
                    == probe["displayed_information"].as_str(),
                "Recolor changed depth selection or source provenance"
            );
        }
        anyhow::ensure!(!probes.is_empty(), "No loaded bathymetry coverage");
        std::fs::write(
            output,
            serde_json::to_vec_pretty(
                &serde_json::json!({"probes":probes,"recolor_app_path_exercised":true,"recolor_composition_and_queries_equal":true,"native_mouse_and_menu_interaction_verified":false}),
            )?,
        )?;
        Ok(())
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

        let checkpoint = self.load_checkpoint();
        let prepared = (|| -> Result<_> {
            ferrite_s101::validate_catalogue_pair(&self.fc, &self.pc)?;
            let loaded_inputs: Vec<_> = self.loaded_chain_paths.iter().flatten().cloned().collect();
            let candidates = s101_update_plan::expand_candidates(paths, &loaded_inputs)?;
            let require_signature = self.signature_verification_enabled();
            // Retain all authenticated private inputs through background parsing.
            let authorization = self.authenticate_paths(&candidates)?;
            let mut loaded = std::collections::BTreeMap::new();
            for (index, cell) in self.cells.iter().enumerate() {
                let key = s101_update_plan::dataset_key(&cell.dsid)?;
                let metadata = self.loaded_discovery.get(&key).cloned().flatten();
                let identity = self
                    .loaded_source_identities
                    .get(index)
                    .context("Missing loaded cell identity")?;
                anyhow::ensure!(
                    loaded
                        .insert(key, (cell.dsid.clone(), identity, metadata))
                        .is_none(),
                    "Duplicate loaded logical dataset"
                );
            }
            let history =
                chart_publication::CancellationHistory::read(&self.cancellation_history_path)?;
            let (plans, cancellations) = s101_update_plan::authorized_batch(
                candidates,
                paths,
                &authorization,
                require_signature,
                &loaded,
                &history,
            )?;
            Ok((authorization, plans, cancellations))
        })();
        let (authorization, plans, cancellations) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.restore_load_checkpoint(checkpoint);
                return Err(error);
            }
        };
        self.frames_since_loaded = None;
        let total_files = plans.len() + cancellations.len();
        if total_files == 0 {
            self.restore_load_checkpoint(checkpoint);
            return Ok(());
        }
        info!(
            "Starting background load of {} complete S-101 chain(s)",
            total_files
        );

        // Create channel for receiving loaded cells
        let (tx, rx) = mpsc::channel();

        // Catalogue pair was validated before load state changed.
        let fc = Arc::clone(&self.fc);
        let pc_product = self.pc.product_id.clone();
        let pc_version = self.pc.version.clone();

        // Spawn background thread for loading
        std::thread::spawn(move || {
            let fc_feature_codes = fc.feature_type_codes();

            // Retain all authenticated snapshots throughout owned parsing.
            let _authorization = authorization;
            for plan in plans {
                let path = plan.base.original.clone();
                let result = plan.load().and_then(|(mut cell, source_identity)| {
                    ferrite_s101::validate_dataset_catalogues(
                        &cell.dsid,
                        &fc,
                        &pc_product,
                        &pc_version,
                    )
                    .with_context(|| format!("Incompatible S-101 dataset {}", path.display()))?;
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

                    Ok(ChartPublicationResult::Load(ChartLoadResult {
                        base_metadata: plan.base.metadata.clone(),
                        metadata: plan.ending_metadata().cloned(),
                        input_paths: plan.input_paths(),
                        cell,
                        source_identity,
                    }))
                });

                // Send result (even None to track progress)
                if tx.send(result).is_err() {
                    // Receiver dropped, stop loading
                    break;
                }
            }
            for cancellation in cancellations {
                if tx
                    .send(cancellation.load().map(ChartPublicationResult::Cancel))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Set loading state
        self.loading_state = Some(BackgroundLoadingState {
            total_files,
            loaded_count: 0,
            pending: Vec::new(),
            failed: false,
            checkpoint,
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

        // Non-blocking receive of all available results
        loop {
            match loading_state.receiver.try_recv() {
                Ok(result) => {
                    loading_state.loaded_count += 1;

                    match result {
                        Ok(load_result) => {
                            loading_state.pending.push(load_result);
                        }
                        Err(error) => {
                            loading_state.failed = true;
                            let message = format!("S-101 load failed: {error:#}");
                            error!("{message}");
                            if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.notice = Some(message.clone());
                            }
                            if self.auto_screenshot.is_some() {
                                self.startup_error = Some(message);
                            }
                        }
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
                    if loading_state.loaded_count < loading_state.total_files {
                        loading_state.failed = true;
                        let message = format!(
                            "S-101 worker ended after {} of {} chains",
                            loading_state.loaded_count, loading_state.total_files
                        );
                        error!("{message}");
                        if let Some(renderer) = &mut self.renderer {
                            renderer.ui_state.notice = Some(message.clone());
                        }
                        if self.auto_screenshot.is_some() {
                            self.startup_error = Some(message);
                        }
                    }
                    completed = true;
                    break;
                }
            }
        }

        // Update UI progress
        if let Some(renderer) = &mut self.renderer {
            if let Some(state) = &self.loading_state {
                renderer.ui_state.loading_progress = Some((state.total_files, state.loaded_count));
            }
        }

        if completed {
            let state = self
                .loading_state
                .take()
                .expect("Completed load state exists");
            if state.failed || state.pending.is_empty() {
                // A batch is all-or-nothing. Successful parses remain private if
                // another chain failed; current cells, extent and selection survive.
                self.restore_load_checkpoint(state.checkpoint);
                if let Some(renderer) = &mut self.renderer {
                    renderer.ui_state.loading_progress = None;
                }
            } else if state.checkpoint.signature_mode != self.signature_verification_enabled()
                || state.checkpoint.catalogue_identity != self.bound_catalogue_identity()
            {
                self.restore_load_checkpoint(state.checkpoint);
                self.load_error(anyhow::anyhow!("Verification policy or bound catalogues changed while loading; reopen the datasets"));
            } else {
                self.finalize_loading(state.pending, state.checkpoint);
            }
        }
        completed
    }

    fn load_checkpoint(&self) -> LoadCheckpoint {
        LoadCheckpoint {
            verified_count: self.verified_count,
            unsigned_count: self.unsigned_count,
            security_ui: self.renderer.as_ref().map(|r| {
                (
                    r.ui_state.security_status.clone(),
                    r.ui_state.security_details.clone(),
                )
            }),
            frames_since_loaded: self.frames_since_loaded,
            signature_mode: self.signature_verification_enabled(),
            catalogue_identity: self.bound_catalogue_identity(),
        }
    }
    fn bound_catalogue_identity(&self) -> ([u8; 32], [u8; 32]) {
        (*self.fc.source_digest(), *self.pc.source_digest())
    }
    fn restore_load_checkpoint(&mut self, checkpoint: LoadCheckpoint) {
        self.verified_count = checkpoint.verified_count;
        self.unsigned_count = checkpoint.unsigned_count;
        self.frames_since_loaded = checkpoint.frames_since_loaded;
        let current_mode = self.signature_verification_enabled();
        if let (Some(renderer), Some((status, details))) =
            (&mut self.renderer, checkpoint.security_ui)
        {
            renderer.ui_state.security_status = if current_mode == checkpoint.signature_mode {
                status
            } else {
                format!("Signature policy {}: retained results {} verified / {} unsigned evaluation; existing data was not re-verified", if current_mode {"ON"} else {"OFF"}, self.verified_count, self.unsigned_count)
            };
            renderer.ui_state.security_details = details;
        }
    }
    fn load_error(&mut self, error: anyhow::Error) {
        let message = format!("S-101 load rejected; previous chart retained: {error:#}");
        error!("{message}");
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.loading_progress = None;
            renderer.ui_state.notice = Some(message.clone());
        }
        if self.auto_screenshot.is_some() {
            self.startup_error = Some(message);
        }
    }

    /// Publish only after every fallible data/portrayal/coverage preparation succeeds.
    fn finalize_loading(
        &mut self,
        operations: Vec<ChartPublicationResult>,
        checkpoint: LoadCheckpoint,
    ) {
        let mut incoming = Vec::new();
        let mut removals = Vec::new();
        for operation in operations {
            match operation {
                ChartPublicationResult::Load(cell) => incoming.push(cell),
                ChartPublicationResult::Cancel(removal) => removals.push(removal),
            }
        }
        let cancelled_count = removals.len();
        let mut history =
            match chart_publication::HistoryStore::begin(&self.cancellation_history_path) {
                Ok(history) => history,
                Err(error) => {
                    self.restore_load_checkpoint(checkpoint);
                    self.load_error(error);
                    return;
                }
            };
        for removal in &removals {
            if let Err(error) = history.history.validate_cancellation(removal.record()) {
                self.restore_load_checkpoint(checkpoint);
                self.load_error(error);
                return;
            }
        }
        let announcement_only =
            incoming.is_empty() && removals.iter().all(|r| !r.removes_content());
        let mut next_metadata = self.loaded_discovery.clone();
        for removal in &removals {
            next_metadata.remove(removal.key());
        }
        for result in &incoming {
            let key = match s101_update_plan::dataset_key(&result.cell.dsid) {
                Ok(key) => key,
                Err(error) => {
                    self.restore_load_checkpoint(checkpoint);
                    self.load_error(error);
                    return;
                }
            };
            if let Err(error) = history.history.validate_reuse(
                &result.cell.dsid,
                result.base_metadata.as_ref().map(|m| m.issue_date),
            ) {
                self.restore_load_checkpoint(checkpoint);
                self.load_error(error);
                return;
            }
            next_metadata.insert(key, result.metadata.clone());
        }
        let transaction = match chart_publication::CellPublication::stage_changes(
            &mut self.cells,
            &mut self.loaded_source_identities,
            &mut self.loaded_chain_paths,
            incoming,
            removals,
        ) {
            Ok(value) => value,
            Err(error) => {
                self.restore_load_checkpoint(checkpoint);
                self.load_error(error);
                return;
            }
        };
        if announcement_only {
            if let Err(error) = history.persist(transaction.cancellations()) {
                transaction.rollback(
                    &mut self.cells,
                    &mut self.loaded_source_identities,
                    &mut self.loaded_chain_paths,
                );
                self.restore_load_checkpoint(checkpoint);
                self.load_error(error);
                return;
            }
            drop(transaction.commit());
            self.loaded_discovery = next_metadata;
            // A notice about an absent cell must not replace the retained
            // chart's authentication results or status.
            self.restore_load_checkpoint(checkpoint);
            if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.loading_progress = None;
                renderer.ui_state.notice = Some(format!("Recorded {cancelled_count} S-101 cancellation announcement(s); no stored content matched"));
            }
            return;
        }
        let previous_bounds = self.bounds;
        if !self.cells.is_empty() || !self.bathymetry.is_empty() {
            self.bounds = chart_data_bounds(&self.cells, self.bathymetry_bounds.values().copied());
            self.bounds.expand_by_percent(0.1);
        }
        let prepared = match self.prepare_instructions(cancelled_count > 0) {
            Ok(prepared) => prepared,
            Err(error) => {
                transaction.rollback(
                    &mut self.cells,
                    &mut self.loaded_source_identities,
                    &mut self.loaded_chain_paths,
                );
                self.bounds = previous_bounds;
                self.restore_load_checkpoint(checkpoint);
                self.load_error(error);
                return;
            }
        };
        // Every recoverable portrayal error precedes durable and visible commits.
        // The OS lock keeps history/name reuse checks current across instances.
        if let Err(error) = history.persist(transaction.cancellations()) {
            transaction.rollback(
                &mut self.cells,
                &mut self.loaded_source_identities,
                &mut self.loaded_chain_paths,
            );
            self.bounds = previous_bounds;
            self.restore_load_checkpoint(checkpoint);
            self.load_error(error);
            return;
        }
        self.publish_instructions(prepared);
        drop(transaction.commit());
        self.loaded_discovery = next_metadata;
        self.chart_loaded = !self.cells.is_empty() || !self.bathymetry.is_empty();

        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.selected_feature = None;
            renderer.ui_state.selection_candidates.clear();
            renderer.ui_state.selection_requested = None;
            renderer.update_selection(&self.render_context.scaler);
            renderer.precompute_triangulations(&self.render_context);
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
                renderer.ui_state.chart_count = self.cells.len() + self.bathymetry.len();

                // The reference comes from producer DataCoverage attributes.
                // A legacy dataset without optimumDisplayScale has no current
                // reference; zero denotes unknown, never an invented filename scale.
                let reference_scale = self
                    .cells
                    .iter()
                    .filter_map(
                        |cell| match ferrite_s101::coverage_scale::dataset_reference_scale(cell) {
                            Ok(value) => value,
                            Err(error) => {
                                warn!(
                                    "Invalid dataset scale metadata {}: {error:#}",
                                    cell.file_path.display()
                                );
                                None
                            }
                        },
                    )
                    .min();
                renderer.set_compilation_scale(reference_scale.unwrap_or(0));

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
            } else {
                renderer.ui_state.loaded_chart = None;
                renderer.ui_state.feature_count = 0;
                renderer.ui_state.chart_count = 0;
                renderer.ui_state.coverage_info = None;
                renderer.set_world_map_chart_boxes(Vec::new());
                renderer.set_compilation_scale(0);
            }
            if cancelled_count > 0 {
                renderer.ui_state.notice = Some(format!(
                    "Cancelled {cancelled_count} S-101 dataset(s); cancellation history retained"
                ));
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
                if let Some(pan) = self
                    .render_context
                    .scaler
                    .projection()
                    .pan_to(self.bounds, WorldPoint::new(lon, lat))
                {
                    self.pan_offset = (pan[0], pan[1]);
                }
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

    fn clear_charts(&mut self) {
        // Drop the receiver so a previously started load cannot repopulate cleared charts.
        self.loading_state = None;
        if let Some(r) = &mut self.renderer {
            r.ui_state.globe_pose = None;
            r.ui_state.globe_range_factor = 1.;
        }
        #[cfg(debug_assertions)]
        info!("Clearing all charts");

        self.verified_count = 0;
        self.unsigned_count = 0;
        self.cells.clear();
        self.loaded_source_identities.clear();
        self.loaded_chain_paths.clear();
        self.loaded_discovery.clear();
        self.bathymetry.clear();
        self.bathymetry_bounds.clear();
        self.depth_policies.clear();
        self.depth_inputs.clear();
        self.bounds = GeoBounds::new(-180.0, -90.0, 180.0, 90.0);
        self.chart_loaded = false;
        self.zoom_level = 1.0;
        self.zoom_target = 1.0;
        self.zoom_animating = false;
        self.pan_offset = (0.0, 0.0);
        self.rendered_symbols.clear();

        // Clear render context and base instruction count
        if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            let display_settings = self.render_context.settings.clone();
            self.render_context =
                RenderContext::new(Viewport::new(size.width as f32, size.height as f32));
            self.render_context.settings = display_settings;
        }
        self.base_instruction_count = 0;
        self.coverage_inventory = None;
        self.coverage_geometry_cache = None;
        if let Some(renderer) = &mut self.renderer {
            renderer.set_globe_coverage_provider(None);
        }

        // Update UI state
        if let Some(renderer) = &mut self.renderer {
            renderer.clear_raster_layers();
            renderer.ui_state.bathymetry_count = 0;
            renderer.ui_state.coverage_info = None;
            renderer.ui_state.security_status.clear();
            renderer.ui_state.security_details.clear();
            renderer.ui_state.loaded_chart = None;
            renderer.ui_state.feature_count = 0;
            renderer.ui_state.chart_count = 0;
            renderer.ui_state.selected_feature = None;
            renderer.ui_state.selection_candidates.clear();
            renderer.ui_state.selection_requested = None;
            renderer.update_selection(&self.render_context.scaler);

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
        if self.cells.is_empty() || std::env::var_os("FERRITE_NO_CACHE").is_some() {
            return None;
        }
        let first_path = &self.cells[0].file_path;
        let cache_dir = first_path.parent()?;

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (cell_index, cell) in self.cells.iter().enumerate() {
            let canonical =
                std::fs::canonicalize(&cell.file_path).unwrap_or_else(|_| cell.file_path.clone());
            // Lowercase on Windows: NTFS is case-insensitive but path strings
            // can vary in case, which would otherwise produce different hashes.
            #[cfg(windows)]
            let key = canonical.to_string_lossy().to_lowercase();
            #[cfg(not(windows))]
            let key = canonical.to_string_lossy().into_owned();
            key.hash(&mut hasher);
            // Use the exact bytes parsed into this loaded Cell, including authenticated
            // private snapshots. Reopening a later-modified live path could key old
            // portrayal output by new bytes. Missing loaded identity disables caching.
            self.loaded_source_identities
                .hash_for_cell(cell_index, &mut hasher)?;
        }
        // Catalogue rules and context parameters determine the generated output.
        // Hash their bytes and current settings rather than only chart paths.
        Self::CACHE_SCHEMA_VERSION.hash(&mut hasher);
        // Fail closed if the linked runtime cannot identify itself. A major/minor
        // label alone cannot invalidate output from another Lua patch or build.
        ferrite_lua::runtime_identity()?.hash(&mut hasher);
        hash_bound_catalogue_inputs(&self.fc, &self.pc, &mut hasher);
        if let Some(renderer) = &self.renderer {
            let mut pc_settings = renderer.settings().clone();
            pc_settings.interoperability_enabled = true; // IC composition is outside the ordinary PC cache.
            format!("{:?}", pc_settings).hash(&mut hasher);
        }
        let mut group_identity = self
            .pc
            .viewing_groups
            .identifiers
            .iter()
            .collect::<Vec<_>>();
        group_identity.sort_unstable_by(|a, b| a.0.cmp(b.0));
        group_identity.hash(&mut hasher);
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
    const CACHE_SCHEMA_VERSION: u32 = 37;

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
        let prepared = self.prepare_instructions(false)?;
        self.publish_instructions(prepared);
        Ok(())
    }
    fn prepare_instructions(&mut self, preserve_view: bool) -> Result<PreparedPortrayal> {
        ferrite_s101::validate_catalogue_pair(&self.fc, &self.pc)?;
        let coverage_inventory =
            ferrite_s101::coverage_projection::GeographicCoverageInventory::from_cells(
                &self.cells,
            )?;
        for cell in &self.cells {
            ferrite_s101::validate_dataset_catalogues(
                &cell.dsid,
                &self.fc,
                &self.pc.product_id,
                &self.pc.version,
            )?;
            // Validate mandatory producer scale metadata even on portrayal-cache hits.
            ferrite_s101::coverage_scale::dataset_reference_scale(cell).with_context(|| {
                format!(
                    "Invalid DataCoverage scale metadata: {}",
                    cell.file_path.display()
                )
            })?;
        }

        let current_settings = self.renderer.as_ref().map(|r| r.settings().clone());
        validated_lua_context(&self.pc, current_settings.as_ref())?; // Also validate cache hits.
        let mut next = self.render_context.empty_for_rebuild();
        next.set_bounds(self.bounds);

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
                            next.set_instructions_from_cache(instructions);
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
            try_lua_portrayal(
                &self.cells,
                &self.fc,
                &self.pc,
                &mut next,
                &self.current_profile_name,
                current_settings.as_ref(),
            )?;

            // Save instruction cache for next load
            if let Some(ref cp) = cache_path {
                let cache_start = std::time::Instant::now();
                let instructions = next.raw_instructions();
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

        let ic_changed = if let Some(ic) = self.active_ic() {
            interoperability::compose_vectors(&ic, &self.cells, &self.fc, &mut next)?
        } else {
            0
        };
        let coverage_inventory = Arc::new(coverage_inventory);
        if let Some(renderer) = &self.renderer {
            let size = renderer.window().inner_size();
            next.set_viewport(size.width as f32, size.height as f32);
            if preserve_view {
                next.scaler = self.render_context.scaler.clone();
            } else {
                next.zoom_to_fit(self.bounds);
            }
            prepare_flat_coverage(
                Some(coverage_inventory.as_ref()),
                &mut next,
                size,
                renderer.window().scale_factor(),
            )?;
        }
        let geometry_cache = new_coverage_geometry_cache();
        let provider: Arc<dyn ferrite_render::GlobeCoverageProvider> =
            Arc::new(S101GlobeCoverageProvider {
                inventory: Arc::clone(&coverage_inventory),
                geometry_cache: Arc::clone(&geometry_cache),
            });
        let visible_vgs = self.get_visible_viewing_groups();
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let globe = if let Some(renderer) = &self.renderer {
            if renderer.ui_state.globe_preview {
                let capsule = renderer.prepare_globe_publication(
                    &mut next,
                    visible_vgs.as_ref(),
                    &mut self.symbol_cache,
                    color_profile,
                    provider,
                )?;
                renderer.validate_globe_publication(&capsule)?;
                Some(capsule)
            } else {
                None
            }
        } else {
            None
        };
        if self.publication_test_fail_before_commit {
            anyhow::ensure!(
                ferrite_wgpu::background_test::enabled(),
                "Publication failure injection requires hidden background test"
            );
            anyhow::bail!(
                "Injected failure after Lua/IC/flat coverage preparation, before publication"
            );
        }
        Ok(PreparedPortrayal {
            context: next,
            coverage: coverage_inventory,
            ic_changed,
            geometry_cache,
            globe,
        })
    }
    fn publish_instructions(&mut self, prepared: PreparedPortrayal) {
        let PreparedPortrayal {
            context: next,
            coverage: coverage_inventory,
            ic_changed,
            geometry_cache,
            globe,
        } = prepared;
        self.render_context = next;
        self.coverage_geometry_cache = Some(Arc::clone(&geometry_cache));
        if globe.is_none() {
            if let Some(renderer) = &mut self.renderer {
                renderer.set_globe_coverage_provider(Some(Arc::new(S101GlobeCoverageProvider {
                    inventory: Arc::clone(&coverage_inventory),
                    geometry_cache,
                })));
            }
        }
        self.coverage_inventory = Some(coverage_inventory);
        self.ic_assigned_vectors = ic_changed;

        // Save base instruction count (chart instructions only, before plugin instructions)
        self.base_instruction_count = self.render_context.instruction_count();
        if let Some(renderer) = &self.renderer {
            self.applied_settings = renderer.settings().clone();
        }

        // Update renderer
        // Get color profile and visible viewing groups before mutable borrows
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();

        if let Some(renderer) = &mut self.renderer {
            renderer.begin_frame();
            if let Some(globe) = globe {
                // The prepared pane is installed last. No provider setter or
                // fallible globe preparation may invalidate it after history.
                renderer.commit_globe_publication(globe);
            } else {
                renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
                renderer.add_world_map_lines(&self.render_context.scaler);
                renderer.add_instructions_with_symbols(
                    &mut self.render_context,
                    Some(&mut self.symbol_cache),
                    color_profile,
                    visible_vgs.as_ref(),
                );
            }
            self.build_rendered_symbols();
        }
    }

    /// Build rendered symbols list for hit testing
    /// Build rendered symbols asynchronously on a background thread.
    /// Results are polled via `poll_hit_test`.
    fn build_rendered_symbols(&mut self) {
        self.pending_hit_test = None;
        if let Some(renderer) = &self.renderer {
            self.rendered_symbols = renderer
                .displayed_symbols_with_sources()
                .into_iter()
                .map(
                    |(
                        symbol_ref,
                        feature_id,
                        position,
                        screen,
                        priority,
                        cell_index,
                        plane,
                        source,
                    )| {
                        RenderedSymbol {
                            source,
                            plane,
                            kind: 0,
                            symbol_ref,
                            feature_id: feature_id.unwrap_or(0),
                            screen_x: screen.x,
                            screen_y: screen.y,
                            world_x: position.x,
                            world_y: position.y,
                            longitude_shift: 0.,
                            priority,
                            cell_index,
                        }
                    },
                )
                .collect();
        }
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
    /// Sorted by display plane, priority, geometry, then distance. Higher planes win even at lower priority.
    fn find_features_at(&self, x: f64, y: f64, radius: f32) -> Vec<RenderedSymbol> {
        self.find_features_at_impl(x, y, radius, true)
    }
    fn find_features_at_impl(
        &self,
        x: f64,
        y: f64,
        radius: f32,
        indexed: bool,
    ) -> Vec<RenderedSymbol> {
        if !x.is_finite() || !y.is_finite() || !radius.is_finite() || radius < 0. {
            return Vec::new();
        }

        let wrapping = self
            .renderer
            .as_ref()
            .is_some_and(|r| r.longitude_wrapping_enabled());
        let offsets = [0., -360., 360.];
        let mut nearby = Vec::new();
        for symbol in &self.rendered_symbols {
            for &shift in &offsets[..if wrapping { 3 } else { 1 }] {
                let pass = if shift < 0. {
                    1
                } else if shift > 0. {
                    2
                } else {
                    0
                };
                let screen = match &self.renderer {
                    Some(renderer) => match renderer
                        .displayed_symbol_screen([symbol.screen_x, symbol.screen_y], pass)
                    {
                        Some(screen) => screen,
                        None => continue,
                    },
                    None => [symbol.screen_x, symbol.screen_y],
                };
                let distance = (screen[0] - x as f32).hypot(screen[1] - y as f32);
                if distance <= radius {
                    if let (Some(renderer), Some(source)) = (&self.renderer, symbol.source) {
                        if !renderer.coverage_fragment_visible(source, pass, [x as f32, y as f32]) {
                            continue;
                        }
                    }
                    let mut hit = symbol.clone();
                    hit.screen_x = screen[0];
                    hit.screen_y = screen[1];
                    hit.longitude_shift = shift;
                    nearby.push((hit, distance as f64));
                }
            }
        }
        if let Some(renderer) = &self.renderer {
            let candidates: std::borrow::Cow<'_, [usize]> = if indexed {
                std::borrow::Cow::Owned(renderer.selection_candidates_in_context(
                    &self.render_context,
                    ferrite_render::ScreenPoint::new(x as f32, y as f32),
                    radius as f64 * 0.4,
                ))
            } else {
                std::borrow::Cow::Borrowed(renderer.displayed_geometry())
            };
            for &index in candidates.iter() {
                let Some(instruction) = self.render_context.raw_instructions().get(index) else {
                    continue;
                };
                let (kind, cell_index, feature_id) = match instruction {
                    DrawingInstruction::Line(line) => (1, line.cell_index, line.feature_id),
                    DrawingInstruction::Area(area) => (2, area.cell_index, area.feature_id),
                    _ => continue,
                };
                let Some(feature_id) = feature_id else {
                    continue;
                };
                if let Some(wrapped_hit) = ferrite_render::hit_geometry_wrapped_visible(
                    instruction,
                    &self.render_context.scaler,
                    ferrite_render::ScreenPoint::new(x as f32, y as f32),
                    radius as f64 * 0.4,
                    renderer.displayed_line_spans(index),
                    wrapping,
                ) {
                    let hit = wrapped_hit.hit;
                    let coverage_pass = if wrapped_hit.longitude_shift < 0. {
                        1
                    } else if wrapped_hit.longitude_shift > 0. {
                        2
                    } else {
                        0
                    };
                    if !renderer.coverage_fragment_visible(
                        index,
                        coverage_pass,
                        [hit.nearest.x, hit.nearest.y],
                    ) {
                        continue;
                    }
                    let world = self.render_context.scaler.screen_to_world(hit.nearest);
                    nearby.push((
                        RenderedSymbol {
                            source: Some(index),
                            plane: instruction
                                .display_plane()
                                .composition_plane(ferrite_kernel::CompositionStage::Chart),
                            kind,
                            cell_index,
                            feature_id,
                            world_x: world.x - wrapped_hit.longitude_shift,
                            world_y: world.y,
                            longitude_shift: wrapped_hit.longitude_shift,
                            screen_x: hit.nearest.x,
                            screen_y: hit.nearest.y,
                            priority: instruction.priority().0,
                            symbol_ref: String::new(),
                        },
                        hit.distance,
                    ));
                }
            }
        }
        nearby.sort_by(RenderedSymbol::compare_hits);
        let mut seen = std::collections::HashSet::new();
        nearby
            .into_iter()
            .filter_map(|(hit, _)| seen.insert((hit.cell_index, hit.feature_id)).then_some(hit))
            .collect()
    }

    fn describe_hit(&self, sym: &RenderedSymbol) -> SelectedFeature {
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
                let code = f.feature_code.as_deref().unwrap_or(&sym.symbol_ref);
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
            foid: feature
                .and_then(|f| f.foid.as_ref())
                .map(ToString::to_string),
            cell_index: sym.cell_index,
            primitive_type: feature
                .map(|f| format!("{:?}", f.primitive_type))
                .unwrap_or_else(|| "Unknown".into()),
            source: sym
                .cell_index
                .and_then(|i| self.cells.get(i as usize))
                .map(|c| c.file_path.display().to_string()),
            attributes: feature
                .map(|f| {
                    ferrite_s101::pick_report_attributes(
                        &self.fc,
                        &f.feature_code.clone().unwrap_or_default(),
                        &f.attributes,
                    )
                })
                .unwrap_or_default(),
            world_pos: (sym.world_x, sym.world_y),
            longitude_shift: sym.longitude_shift,
            definition,
            symbol_name: (sym.kind == 0).then(|| sym.symbol_ref.clone()),
        }
    }

    /// Diagnostic snapshot of the live model, excluding intentional error notices.
    fn publication_model(&self) -> Result<serde_json::Value> {
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let cells: Vec<_> = self.cells.iter().enumerate().map(|(index, cell)| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            self.loaded_source_identities.hash_for_cell(index, &mut hash).expect("Loaded source aligned");
            serde_json::json!({"path":cell.file_path,"dataset":cell.dsid.dataset_name,
                "edition":cell.dsid.edition_number,"update":cell.dsid.update_number,"features":cell.features.len(),
                "source_key":std::hash::Hasher::finish(&hash),
                "complete_cell_debug_sha256":format!("{:x}",Sha256::digest(format!("{cell:?}").as_bytes()))})
        }).collect();
        Ok(
            serde_json::json!({"cells":cells,"chain_paths":self.loaded_chain_paths,
            "bounds_bits":[self.bounds.min_x.to_bits(),self.bounds.min_y.to_bits(),self.bounds.max_x.to_bits(),self.bounds.max_y.to_bits()],
            "chart_loaded":self.chart_loaded,"frames_since_loaded":self.frames_since_loaded,"base_instruction_count":self.base_instruction_count,
            "instructions_sha256":format!("{:x}",Sha256::digest(bincode::serialize(self.render_context.raw_instructions())?)),
            "verified":self.verified_count,"unsigned":self.unsigned_count,
            "security_status":renderer.ui_state.security_status,"security_details":renderer.ui_state.security_details,
            "selection":format!("{:?}",renderer.ui_state.selected_feature),
            "selection_candidates":format!("{:?}",renderer.ui_state.selection_candidates),
            "rendered_symbols":format!("{:?}",self.rendered_symbols),
            "scaler":format!("{:?}",self.render_context.scaler),"applied_settings":format!("{:?}",self.applied_settings)}),
        )
    }
    fn wait_publication_test_load(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while self.loading_state.is_some() {
            self.poll_loading();
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Publication worker timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }
    /// Reproduce ordinary last-cell cancellation without a screenshot-triggered
    /// update_view repair. All inputs/history belong to this hidden test case.
    fn audit_root_cancellation(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden test required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused cancellation audit forbidden"
        );
        anyhow::ensure!(
            self.cells.len() == 1 && self.bathymetry.is_empty(),
            "Audit requires one vector cell and no raster"
        );
        let selected = PathBuf::from(
            std::env::var_os("FERRITE_ROOT_CANCELLATION_INPUT")
                .context("Cancellation path required")?,
        )
        .canonicalize()?;
        let private = output
            .parent()
            .context("Audit parent")?
            .join("input")
            .canonicalize()?;
        anyhow::ensure!(
            selected.parent() == Some(private.as_path())
                && self.cells[0].file_path.canonicalize()?.parent() == Some(private.as_path()),
            "Cancellation audit must use private input copies"
        );
        anyhow::ensure!(
            self.cancellation_history_path
                .starts_with(output.parent().context("Audit parent")?),
            "Audit history must belong to this private case"
        );
        anyhow::ensure!(
            !self.cancellation_history_path.exists(),
            "Audit requires fresh private history"
        );
        fs::create_dir_all(output)?;
        if std::env::var_os("FERRITE_ROOT_CANCELLATION_ANNOUNCEMENT").is_some() {
            return self.audit_root_cancellation_announcement(output, selected);
        }
        let previous_id = self.cells[0].dsid.clone();
        let previous_date = self
            .loaded_discovery
            .get(&s101_update_plan::dataset_key(&previous_id)?)
            .and_then(Option::as_ref)
            .context("Captured prior metadata required")?
            .issue_date;
        let viewport = self.renderer.as_ref().unwrap().chart_viewport_pixels();
        for y in 1..=4 {
            for x in 1..=6 {
                if self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selected_feature
                    .is_none()
                {
                    self.chart_click((
                        viewport.0 as f64 + viewport.2 as f64 * x as f64 / 7.,
                        viewport.1 as f64 + viewport.3 as f64 * y as f64 / 5.,
                    ));
                }
            }
        }
        self.renderer.as_mut().unwrap().render()?;
        let original = self.publication_model()?;
        self.audit_portrayal(&output.join("before"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("before.png"))?;
        // Exercise the ordinary interactive branch, not the auto-screenshot
        // branch that otherwise repairs a missing globe pane after commit.
        self.publication_test_fail_before_commit = true;
        self.load_charts(&[selected.clone()])?;
        self.wait_publication_test_load()?;
        self.publication_test_fail_before_commit = false;
        let failure = self
            .startup_error
            .take()
            .context("Late cancellation fault did not reject")?;
        anyhow::ensure!(
            failure.contains("Injected failure after Lua"),
            "Unexpected cancellation rejection: {failure}"
        );
        anyhow::ensure!(
            self.publication_model()? == original && !self.cancellation_history_path.exists(),
            "Rejected cancellation changed model/history"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-late-failure"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("after-late-failure.png"))?;
        let saved_screenshot = self.auto_screenshot.take();
        self.load_charts(&[selected])?;
        self.wait_publication_test_load()?;
        anyhow::ensure!(
            self.startup_error.is_none(),
            "Cancellation failed: {:?}",
            self.startup_error
        );
        anyhow::ensure!(
            self.cells.is_empty()
                && self.loaded_chain_paths.is_empty()
                && self.loaded_discovery.is_empty()
                && !self.chart_loaded,
            "Cancelled last dataset survived publication"
        );
        anyhow::ensure!(
            self.rendered_symbols.is_empty() && self.render_context.instruction_count() == 0,
            "Cancelled instructions/picking survived"
        );
        anyhow::ensure!(
            self.publication_model()?["scaler"] == original["scaler"],
            "Cancellation moved camera"
        );
        let renderer = self.renderer.as_ref().unwrap();
        anyhow::ensure!(
            renderer.ui_state.selected_feature.is_none()
                && renderer.ui_state.selection_candidates.is_empty(),
            "Cancelled selection survived"
        );
        anyhow::ensure!(
            renderer.geometry_matches_view(&self.render_context.scaler),
            "Cancellation committed without a prepared current-view pane"
        );
        // Deliberately no update_view here: the next visible frame must already
        // contain the committed empty scene, with no stale image or object IDs.
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-success"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("after-success.png"))?;
        let mut direct_pick_probes = Vec::new();
        for y in 1..=4 {
            for x in 1..=6 {
                let point = ferrite_render::ScreenPoint::new(
                    viewport.0 + viewport.2 * x as f32 / 7.,
                    viewport.1 + viewport.3 * y as f32 / 5.,
                );
                let renderer = self.renderer.as_mut().unwrap();
                let globe = renderer.ui_state.globe_preview;
                let candidates = if globe {
                    // This invokes the real cropped GPU ID readback, even
                    // though the application has chart_loaded=false.
                    renderer
                        .globe_feature_candidates(&self.render_context, point, 4.)?
                        .len()
                } else {
                    renderer
                        .selection_candidates_in_context(&self.render_context, point, 4.)
                        .len()
                };
                anyhow::ensure!(
                    candidates == 0,
                    "Cancelled object remained in renderer picking"
                );
                direct_pick_probes.push(serde_json::json!({"point":[point.x,point.y],"backend":if globe {"GPU cropped feature IDs"} else {"flat selection index"},"candidates":candidates}));
            }
        }
        let history =
            chart_publication::CancellationHistory::read(&self.cancellation_history_path)?;
        anyhow::ensure!(
            history
                .validate_reuse(&previous_id, Some(previous_date))
                .is_err(),
            "Persisted cancellation allowed old-date reuse"
        );
        self.auto_screenshot = saved_screenshot;
        fs::write(
            output.join("cancellation.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "hidden":true,"focused":false,"original":original,"cancelled":self.publication_model()?,
                "late_failure_preserved_model_and_history":true,"last_cell_removed":true,
                "direct_postcommit_frame_without_update_view":true,"old_camera_preserved":true,
                "cancelled_selection_and_24_pick_positions_empty":true,"direct_renderer_pick_probes":direct_pick_probes,"reopened_history_rejects_old_date":true,
                "signature":"OFF; structural synthetic cancellation and copied official SHOM chain",
                "scope":"No signed cancellation, partial deletion, retained raster, gesture or FPS claim"
            }))?,
        )?;
        Ok(())
    }

    /// Exercise absence-only notices through the real worker and persisted
    /// history, with an unrelated loaded scene retained across first/replay.
    fn audit_root_cancellation_announcement(
        &mut self,
        output: &Path,
        selected: PathBuf,
    ) -> Result<()> {
        let cancellation = ferrite_s101::cancellation::Cancellation::parse(&fs::read(&selected)?)?;
        let cancelled_id = ferrite_s100_core::DatasetIdentification {
            product_identifier: "INT.IHO.S-101.2.0".into(),
            dataset_name: cancellation.dataset_name().into(),
            application_profile: "1".into(),
            edition_number: 4,
            ..Default::default()
        };
        let key = s101_update_plan::dataset_key(&cancelled_id)?;
        anyhow::ensure!(
            self.cells
                .iter()
                .all(|c| s101_update_plan::dataset_key(&c.dsid).ok().as_ref() != Some(&key)),
            "Announcement must concern an absent cell"
        );
        let viewport = self
            .renderer
            .as_ref()
            .context("Renderer")?
            .chart_viewport_pixels();
        for y in 1..=4 {
            for x in 1..=6 {
                if self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selected_feature
                    .is_none()
                {
                    self.chart_click((
                        viewport.0 as f64 + viewport.2 as f64 * x as f64 / 7.,
                        viewport.1 as f64 + viewport.3 as f64 * y as f64 / 5.,
                    ));
                }
            }
        }
        self.renderer.as_mut().unwrap().render()?;
        let original = self.publication_model()?;
        let cache = self
            .coverage_geometry_cache
            .clone()
            .context("Coverage cache")?;
        self.audit_portrayal(&output.join("before"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("before.png"))?;
        // Force the ordinary interactive finalize branch for both receipts.
        let saved_screenshot = self.auto_screenshot.take();
        let mut phases = Vec::new();
        for phase in ["first", "replay"] {
            self.load_charts(&[selected.clone()])?;
            self.wait_publication_test_load()?;
            anyhow::ensure!(
                self.startup_error.is_none(),
                "Announcement failed: {:?}",
                self.startup_error
            );
            anyhow::ensure!(
                self.cancellation_history_path.is_file()
                    && self
                        .renderer
                        .as_ref()
                        .unwrap()
                        .ui_state
                        .notice
                        .as_deref()
                        .is_some_and(|notice| notice
                            .starts_with("Recorded 1 S-101 cancellation announcement")),
                "Announcement did not commit its private history receipt"
            );
            anyhow::ensure!(
                self.publication_model()? == original,
                "Announcement changed retained model/selection/scaler/security"
            );
            anyhow::ensure!(
                Arc::ptr_eq(
                    &cache,
                    self.coverage_geometry_cache
                        .as_ref()
                        .context("Retained cache")?
                ),
                "Announcement replaced the retained geometry cache"
            );
            anyhow::ensure!(
                self.renderer
                    .as_ref()
                    .unwrap()
                    .geometry_matches_view(&self.render_context.scaler),
                "Announcement invalidated the current-view pane"
            );
            self.renderer.as_mut().unwrap().render()?;
            self.audit_portrayal(&output.join(phase))?;
            self.renderer
                .as_mut()
                .unwrap()
                .save_screenshot(&output.join(format!("{phase}.png")))?;
            phases.push(serde_json::json!({"phase":phase,"model_unchanged":true,
                "cache_arc_unchanged":true,"direct_frame_without_update_view":true}));
        }
        let history =
            chart_publication::CancellationHistory::read(&self.cancellation_history_path)?;
        let cancelled_date = chrono::NaiveDate::from_ymd_opt(2026, 6, 19).unwrap();
        anyhow::ensure!(
            history
                .validate_reuse(&cancelled_id, Some(cancelled_date))
                .is_err(),
            "Reopened history accepted old-date reuse"
        );
        anyhow::ensure!(
            history
                .validate_reuse(&cancelled_id, Some(cancelled_date.succ_opt().unwrap()))
                .is_ok(),
            "Reopened history rejected newer base"
        );
        self.auto_screenshot = saved_screenshot;
        fs::write(
            output.join("announcement.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "hidden":true,"focused":false,"original":original,"retained":self.publication_model()?,
                "phases":phases,"reopened_history_rejects_old_date":true,
                "scope":"OFF physical synthetic B7 notice, unrelated official SHOM full chain; no signed notice or predecessor verification claim"
            }))?,
        )?;
        Ok(())
    }

    /// Real load_charts -> worker -> poll_loading -> Lua/IC/coverage -> commit path.
    /// Runs only in hidden test mode, using copied official fixture bytes.
    /// Diagnostic-only lifecycle gate; commands/materials are never filtered.
    fn capture_coverage_lifecycle(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(ferrite_wgpu::background_test::enabled(), "Hidden lifecycle audit required");
        fs::create_dir_all(output)?;
        self.select_feature(None);
        self.renderer.as_mut().context("Renderer missing")?.ui_state.selection_candidates.clear();
        std::env::set_var("FERRITE_ROOT_KEY_PROOF_COVERAGE", output.join("coverage"));
        self.update_view();
        std::env::remove_var("FERRITE_ROOT_KEY_PROOF_COVERAGE");
        anyhow::ensure!(self.startup_error.is_none(), "Lifecycle update failed {:?}", self.startup_error);
        let r=self.renderer.as_mut().unwrap();
        r.render()?;r.wait_hidden_key_frame()?;
        anyhow::ensure!(r.window().is_visible()==Some(false)&&!r.window().has_focus(), "Visible/focused lifecycle forbidden");
        let viewport=r.chart_viewport_pixels();
        r.save_screenshot(output.join("chart.png"))?;
        self.audit_portrayal(&output.join("audit"))?;
        if !self.renderer.as_ref().unwrap().ui_state.globe_preview {
            self.renderer.as_ref().unwrap().export_hidden_key_flat_coverage(&output.join("coverage"),&self.render_context)?;
        }
        let mut picks=Vec::new();
        for y in 1..=2 {for x in 1..=3 {
            let point=(viewport.0 as f64+viewport.2 as f64*x as f64/4.,viewport.1 as f64+viewport.3 as f64*y as f64/3.);
            self.chart_click(point);
            let selected=self.renderer.as_ref().unwrap().ui_state.selected_feature.as_ref().map(|f|serde_json::json!({"feature_id":f.feature_id,"cell_index":f.cell_index,"source":f.source,"full_attributes":f.attributes,"foid":f.foid,"definition":f.definition,"primitive_type":f.primitive_type,"symbol_name":f.symbol_name,"world_position_bits":[f.world_pos.0.to_bits(),f.world_pos.1.to_bits()],"longitude_shift_bits":f.longitude_shift.to_bits()}));
            picks.push(serde_json::json!({"pixel":point,"selected":selected}));
        }}
        self.select_feature(None);self.renderer.as_mut().unwrap().ui_state.selection_candidates.clear();
        fs::write(output.join("semantic.json"),serde_json::to_vec_pretty(&serde_json::json!({"picks":picks,"profile":self.current_profile_name,"source_cells":self.cells.iter().map(|c|&c.file_path).collect::<Vec<_>>(),"window_visible":false,"window_focus":false}))?)?;
        fs::write(output.join("cache.json"),serde_json::to_vec_pretty(&self.coverage_geometry_cache.as_ref().context("Cache missing")?.lock().map_err(|_|anyhow::anyhow!("Cache poisoned"))?.diagnostics())?)?;
        Ok(())
    }
    fn regenerate_coverage_lifecycle(&mut self) -> Result<()> {
        let old=Arc::clone(self.coverage_geometry_cache.as_ref().context("Cache missing")?);
        let prepared=self.prepare_instructions(true)?;
        anyhow::ensure!(!Arc::ptr_eq(&old,&prepared.geometry_cache),"New source preparation reused old provider cache");
        self.publish_instructions(prepared);
        Ok(())
    }
    fn audit_coverage_cache_lifecycle(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(ferrite_wgpu::background_test::enabled(),"Hidden lifecycle required");
        anyhow::ensure!(self.cells.len()==4,"Lifecycle expects original four SHOM cells");
        self.capture_coverage_lifecycle(&output.join("00-initial-cold"))?;
        self.capture_coverage_lifecycle(&output.join("01-initial-warm"))?;
        self.cells.reverse();
        let reordered=self.regenerate_coverage_lifecycle().and_then(|_|self.capture_coverage_lifecycle(&output.join("02-source-reordered")));
        self.cells.reverse();self.regenerate_coverage_lifecycle()?;reordered?;
        let removed=self.cells.pop().unwrap();
        let removal=self.regenerate_coverage_lifecycle().and_then(|_|self.capture_coverage_lifecycle(&output.join("03-source-removed")));
        self.cells.push(removed);self.regenerate_coverage_lifecycle()?;removal?;
        self.capture_coverage_lifecycle(&output.join("04-source-restored"))?;
        let oldcache=Arc::clone(self.coverage_geometry_cache.as_ref().unwrap());
        let original=self.publication_model()?;
        self.publication_test_fail_before_commit=true;
        let rejected=self.prepare_instructions(true);
        self.publication_test_fail_before_commit=false;
        anyhow::ensure!(rejected.is_err(),"Injected publication unexpectedly accepted");
        anyhow::ensure!(Arc::ptr_eq(&oldcache,self.coverage_geometry_cache.as_ref().unwrap()),"Rejected publication replaced cache");
        anyhow::ensure!(self.publication_model()?==original,"Rejected publication changed original state");
        // Render the retained publication directly; no update_view repair.
        self.renderer.as_mut().unwrap().render()?;
        self.renderer.as_mut().unwrap().wait_hidden_key_frame()?;
        self.renderer.as_mut().unwrap().save_screenshot(output.join("05-rejected-publication-direct.png"))?;
        self.capture_coverage_lifecycle(&output.join("06-rejected-publication"))?;
        let initial=self.current_profile_name.clone();
        let mut profiles:Vec<String>=self.pc.color_profiles.profiles.keys().cloned().collect();profiles.sort();
        for (i,profile) in profiles.iter().enumerate() {
            self.set_color_profile(profile);self.regenerate_coverage_lifecycle()?;
            self.capture_coverage_lifecycle(&output.join(format!("07-profile-{i}")))?;
        }
        self.set_color_profile(&initial);self.regenerate_coverage_lifecycle()?;
        // Reload the actual bound FC/PC inputs rather than changing a name key.
        let fc=load_feature_catalogue(Path::new(&std::env::var("FERRITE_COVERAGE_LIFECYCLE_FC")?))?;
        let pc=load_portrayal_catalogue(Path::new(&std::env::var("FERRITE_COVERAGE_LIFECYCLE_PC")?))?;
        ferrite_s101::validate_catalogue_pair(&fc,&pc)?;
        self.fc=Arc::new(fc);self.pc=Arc::new(pc);self.symbol_cache.clear();
        self.renderer.as_mut().unwrap().clear_symbol_textures();
        self.regenerate_coverage_lifecycle()?;
        self.capture_coverage_lifecycle(&output.join("08-bound-fcpc-reloaded"))?;
        let base_pose=self.renderer.as_ref().unwrap().globe_pose();
        let old_zoom=self.zoom_level;let old_pan=self.pan_offset;
        for (i,zoom) in [1.,2.,20.,200.,20.,2.,1.].into_iter().enumerate() {
            if let Some(base)=base_pose {
                let mut pose=base;pose.range_m=base.range_m/zoom;
                pose.focus=ferrite_kernel::geodesy::GeographicPosition::new(base.focus.latitude()+0.2*(i as f64).sin(),base.focus.longitude()+0.5*(i as f64).sin())?;
                pose.heading_deg=base.heading_deg+15.*(i as f64).sin();
                self.renderer.as_mut().unwrap().ui_state.globe_pose=Some(pose);
            } else {self.zoom_level=old_zoom*zoom;self.pan_offset=(old_pan.0+1000.*(i as f64).sin(),old_pan.1);}
            self.capture_coverage_lifecycle(&output.join(format!("09-camera-zoom-{i}")))?;
        }
        self.renderer.as_mut().unwrap().ui_state.globe_pose=base_pose;
        self.zoom_level=old_zoom;self.pan_offset=old_pan;
        let original_size=self.window.as_ref().unwrap().inner_size();
        let smaller=winit::dpi::PhysicalSize::new((original_size.width*3/4).max(640),(original_size.height*3/4).max(480));
        anyhow::ensure!(smaller!=original_size,"Resize must alter native extent");
        let now=std::time::Instant::now();
        self.coverage_lifecycle_resize=Some(CoverageLifecycleResize {output:output.to_path_buf(),original:original_size,requested:smaller,restore:false,observed:None,started:now,next_poll:now});
        let _=self.window.as_ref().unwrap().request_inner_size(smaller);
        self.window.as_ref().unwrap().request_redraw();
        Ok(())
    }
    fn resume_coverage_lifecycle_resize(&mut self) -> Result<bool> {
        let mut pending=self.coverage_lifecycle_resize.take().context("Resize state missing")?;
        let window=self.window.as_ref().context("Window missing")?;
        anyhow::ensure!(window.is_visible()==Some(false)&&!window.has_focus(),"Resize exposed/focused window");
        let actual=window.inner_size();
        let ready=pending.ready(actual);
        if !ready {
            anyhow::ensure!(pending.started.elapsed()<std::time::Duration::from_secs(10),"Native Resized→Redraw timeout: requested {:?}, actual {:?}, observed {:?}",pending.requested,actual,pending.observed);
            self.coverage_lifecycle_resize=Some(pending);return Ok(false);
        }
        // The normal Resized handler has resized GPU/context and rebuilt coverage.
        let i=usize::from(pending.restore);
        self.capture_coverage_lifecycle(&pending.output.join(format!("10-viewport-{i}")))?;
        fs::write(pending.output.join(format!("10-viewport-{i}-event.json")),serde_json::to_vec_pretty(&serde_json::json!({"requested":[pending.requested.width,pending.requested.height],"actual_window":[actual.width,actual.height],"observed_resized":[actual.width,actual.height],"after_redraw":true,"window_visible":false,"window_focus":false}))?)?;
        if !pending.restore {
            pending.restore=true;pending.requested=pending.original;pending.observed=None;
            pending.started=std::time::Instant::now();pending.next_poll=pending.started;
            let original=pending.original;self.coverage_lifecycle_resize=Some(pending);
            let _=self.window.as_ref().unwrap().request_inner_size(original);
            self.window.as_ref().unwrap().request_redraw();return Ok(false);
        }
        self.complete_coverage_cache_lifecycle(&pending.output)?;
        Ok(true)
    }
    fn complete_coverage_cache_lifecycle(&mut self, output:&Path) -> Result<()> {
        let old_danger=self.renderer.as_ref().unwrap().settings().isolated_dangers;
        for (i,enabled) in [true,false,true].into_iter().enumerate() {
            self.renderer.as_mut().unwrap().ui_state.settings.isolated_dangers=enabled;
            self.regenerate_coverage_lifecycle()?;
            self.capture_coverage_lifecycle(&output.join(format!("09-shallow-water-dangers-{i}")))?;
        }
        self.renderer.as_mut().unwrap().ui_state.settings.isolated_dangers=old_danger;
        self.regenerate_coverage_lifecycle()?;
        fs::write(output.join("scope.json"),serde_json::to_vec_pretty(&serde_json::json!({"source_reorder":true,"source_remove_restore":true,"fcpc_actual_reload":true,"profile_actual_change":true,"publication_rejection_cache_identity":true,"shallow_water_dangers_regenerated":[true,false,true],"hardware_dpi_changed":false,"viewport_actual_mutated":true,"camera_zoom_path":[1,2,20,200,20,2,1],"official_update_source_replacement":"separate Root publication audit required","window_visible":false,"window_focus":false}))?)?;
        Ok(())
    }

    fn audit_root_publication(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden background test required"
        );
        anyhow::ensure!(
            self.cells.len() == 1 && self.cells[0].dsid.update_number == 0,
            "Audit starts with one original base"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused audit forbidden"
        );
        fs::create_dir_all(output)?;
        let originals: Vec<PathBuf> =
            serde_json::from_str(&std::env::var("FERRITE_ROOT_PUBLICATION_INPUTS")?)?;
        anyhow::ensure!(!originals.is_empty(), "Audit needs official updates");
        let folder = self.cells[0]
            .file_path
            .parent()
            .context("Base folder")?
            .to_path_buf();
        let expected_private = output
            .parent()
            .context("Audit parent")?
            .join("input")
            .canonicalize()?;
        anyhow::ensure!(
            folder.canonicalize()? == expected_private,
            "Audit requires its private input folder"
        );
        let mut copied = Vec::new();
        for source in &originals {
            let path = folder.join(source.file_name().context("Update filename")?);
            anyhow::ensure!(
                !path.exists() && fs::symlink_metadata(&path).is_err(),
                "Audit destination must be new"
            );
            anyhow::ensure!(
                source.canonicalize()?.parent() != Some(expected_private.as_path()),
                "Audit source must be outside private input folder"
            );
            fs::copy(source, &path)?;
            copied.push(path);
        }
        let selected = copied.last().context("Last update")?.clone();
        // Preserve an actual pick when the current camera has a selectable object.
        let v = self.renderer.as_ref().unwrap().chart_viewport_pixels();
        for y in 1..=4 {
            for x in 1..=6 {
                if self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selected_feature
                    .is_none()
                {
                    self.chart_click((
                        v.0 as f64 + v.2 as f64 * x as f64 / 7.,
                        v.1 as f64 + v.3 as f64 * y as f64 / 5.,
                    ));
                }
            }
        }
        self.renderer.as_mut().unwrap().render()?;
        let original = self.publication_model()?;
        self.audit_portrayal(&output.join("before"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("before.png"))?;
        self.publication_test_fail_before_commit = true;
        self.load_charts(&[selected.clone()])?;
        let raster_error = self
            .load_bathymetry(&[folder.join("not-opened.h5")])
            .unwrap_err();
        anyhow::ensure!(
            raster_error.to_string().contains("Finish or clear"),
            "Concurrent raster load was not isolated"
        );
        self.wait_publication_test_load()?;
        self.publication_test_fail_before_commit = false;
        let late_error = self
            .startup_error
            .take()
            .context("Injected late failure did not reject load")?;
        anyhow::ensure!(
            late_error.contains("Injected failure after Lua"),
            "Unexpected late failure: {late_error}"
        );
        anyhow::ensure!(
            self.publication_model()? == original,
            "Late failure altered displayed model"
        );
        // No update_view repair: the unchanged provider/scene must render directly.
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-late-failure"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("after-late-failure.png"))?;
        anyhow::ensure!(
            self.publication_model()? == original,
            "Rendering after rejection changed model"
        );

        self.load_charts(&[selected.clone()])?;
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .verify_dataset_signatures = true;
        self.wait_publication_test_load()?;
        let policy_error = self
            .startup_error
            .take()
            .context("Changed verification policy did not reject old-policy load")?;
        anyhow::ensure!(
            policy_error.contains("Verification policy or bound catalogues changed"),
            "Unexpected policy error"
        );
        let policy_state = self.publication_model()?;
        for key in [
            "cells",
            "chain_paths",
            "bounds_bits",
            "chart_loaded",
            "base_instruction_count",
            "instructions_sha256",
            "selection",
            "selection_candidates",
            "rendered_symbols",
            "scaler",
        ] {
            anyhow::ensure!(
                policy_state[key] == original[key],
                "Policy change rejection changed {key}"
            );
        }
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .ui_state
                .verify_dataset_signatures,
            "Rollback changed the user's new policy"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-policy-rejection"))?;
        // Restore the diagnostic's initial policy, without reopening an input.
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.verify_dataset_signatures = false;
            renderer.ui_state.security_status =
                original["security_status"].as_str().unwrap().into();
            renderer.ui_state.security_details =
                original["security_details"].as_str().unwrap().into();
        }

        // A first successful parse followed by another chain's malformed body
        // must not publish a partial batch. Its first metadata record is valid.
        let base = fs::read(&self.cells[0].file_path)?;
        let ddr: usize = std::str::from_utf8(&base[..5])?.parse()?;
        let first: usize = std::str::from_utf8(&base[ddr..ddr + 5])?.parse()?;
        let mut bad = base[..ddr + first].to_vec();
        let old_name = self.cells[0].dsid.dataset_name.as_bytes();
        let replacement = b"101ZZ99FAILXX.000";
        anyhow::ensure!(
            old_name.len() == replacement.len(),
            "Synthetic diagnostic name width"
        );
        let location = bad
            .windows(old_name.len())
            .position(|b| b == old_name)
            .context("DSID name")?;
        bad[location..location + old_name.len()].copy_from_slice(replacement);
        bad.extend_from_slice(b"malformed trailing body");
        let bad_path = folder.join("101ZZ99FAILXX.000");
        fs::write(&bad_path, &bad)?;
        self.load_charts(&[selected.clone(), bad_path.clone()])?;
        self.wait_publication_test_load()?;
        let parse_error = self
            .startup_error
            .take()
            .context("Malformed second chain did not reject batch")?;
        anyhow::ensure!(
            self.publication_model()? == original,
            "Partial failed batch altered displayed model"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-batch-failure"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("after-batch-failure.png"))?;
        fs::remove_file(bad_path)?;
        // Successful retry publishes the same official updated chain once.
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.notice = None;
        }
        self.load_charts(&[selected])?;
        self.wait_publication_test_load()?;
        anyhow::ensure!(
            self.startup_error.is_none(),
            "Retry rejected: {:?}",
            self.startup_error
        );
        anyhow::ensure!(
            self.cells.len() == 1 && self.cells[0].dsid.update_number == originals.len() as u16,
            "Retry did not replace base at one index"
        );
        self.update_view();
        self.renderer.as_mut().unwrap().render()?;
        let published = self.publication_model()?;
        anyhow::ensure!(
            published["cells"][0]["source_key"] != original["cells"][0]["source_key"],
            "Updated bytes did not invalidate cache key"
        );
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .ui_state
                .selected_feature
                .is_none(),
            "Old selection survived successful replacement"
        );
        self.audit_portrayal(&output.join("after-success"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(&output.join("after-success.png"))?;
        fs::write(
            output.join("publication.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
            "before":original,"published":published,"late_failure":late_error,"batch_failure":parse_error,"policy_failure":policy_error,
            "new_signature_policy_preserved_and_old_policy_load_rejected":true,"concurrent_raster_authentication_was_blocked":true,
            "same_real_app_load_and_poll_paths":true,"late_failure_retained_complete_model":true,
            "actual_original_selection_present":original["selection"]!="None",
            "partial_batch_rejected":true,"successful_retry_replaced_one_index_and_invalidated_cache":true,
            "hidden":self.window.as_ref().and_then(|w|w.is_visible())==Some(false),
            "focused":self.window.as_ref().is_some_and(|w|w.has_focus()),"signature_mode":"defaultOFF"}))?,
        )?;
        Ok(())
    }

    fn audit_root_source_topology(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Background test required"
        );
        let r = self.renderer.as_ref().context("Renderer missing")?;
        anyhow::ensure!(
            !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
            "Visible/focused test forbidden"
        );
        let globe = r.ui_state.globe_preview;
        let base_pose = r.globe_pose();
        let old_zoom = self.zoom_level;
        let old_pan = self.pan_offset;
        let old_pose = r.ui_state.globe_pose;
        let epoch = self.render_context.geometry_revision();
        let viewport = r.chart_viewport_pixels();
        let profile = self.current_profile_name.clone();
        self.renderer.as_mut().unwrap().set_profiling_enabled(true);
        let mut rows = Vec::new();
        std::fs::create_dir_all(output)?;
        anyhow::ensure!(
            std::env::var_os("FERRITE_ROOT_KEY_PROOF_COVERAGE").is_none(),
            "Proof coverage env must be unset before harness"
        );
        for (mode, source_mode) in [0u8, 1, 2, 2, 1, 0].into_iter().enumerate() {
            let enabled = false;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_source_cache_diagnostic(source_mode)?;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_pattern_key_diagnostic(true)?;
            // Original boundary behavior unchanged; no negative allocation candidate applied.
            for cycle in 0..4 {
                for frame in 0..25usize {
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    self.renderer.as_mut().unwrap().set_globe_selection_click(
                        [0., 0.],
                        false,
                        &self.render_context.scaler,
                    );
                    let phase = frame as f64 / 24.;
                    let zoom = 200f64.powf(1. - (2. * phase - 1.).abs());
                    if globe {
                        let base = base_pose.context("Globe pose missing")?;
                        let mut pose = base;
                        pose.range_m = base.range_m / zoom;
                        pose.focus = ferrite_kernel::geodesy::GeographicPosition::new(
                            base.focus.latitude() + 0.35 * (phase * std::f64::consts::TAU).sin(),
                            base.focus.longitude() + 0.7 * (phase * std::f64::consts::TAU).sin(),
                        )?;
                        pose.heading_deg =
                            base.heading_deg + 30. * (phase * std::f64::consts::TAU).sin();
                        self.renderer.as_mut().unwrap().ui_state.globe_pose = Some(pose);
                    } else {
                        self.zoom_level = old_zoom * zoom;
                        self.pan_offset = old_pan;
                    }
                    let proof = output.join(format!("mode{mode}/cycle{cycle}-frame{frame:02}"));
                    if cycle == 3 {
                        std::fs::create_dir_all(&proof)?;
                        std::env::set_var(
                            "FERRITE_ROOT_KEY_PROOF_COVERAGE",
                            proof.join("coverage"),
                        );
                    }
                    let start = std::time::Instant::now();
                    self.update_view();
                    std::env::remove_var("FERRITE_ROOT_KEY_PROOF_COVERAGE");
                    anyhow::ensure!(
                        self.startup_error.is_none(),
                        "Prepare failed {:?}",
                        self.startup_error
                    );
                    let prepare_ms = start.elapsed().as_secs_f64() * 1000.;
                    let r = self.renderer.as_mut().unwrap();
                    r.render()?;
                    let wait = std::time::Instant::now();
                    r.wait_hidden_key_frame()?;
                    let wait_ms = wait.elapsed().as_secs_f64() * 1000.;
                    let service_ms = start.elapsed().as_secs_f64() * 1000.;
                    anyhow::ensure!(
                        !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
                        "Visible/focused frame"
                    );
                    anyhow::ensure!(
                        r.chart_viewport_pixels() == viewport
                            && self.current_profile_name == profile,
                        "Viewport/palette changed"
                    );
                    anyhow::ensure!(
                        self.render_context.geometry_revision() == epoch,
                        "Source epoch changed"
                    );
                    let preparation = r.globe_preview_diagnostics().map(|d| d.preparation.clone());
                    rows.push(serde_json::json!({"mode":mode,"source_cache_mode":source_mode,"boundary_storage":enabled,"request_keys":true,"cycle":cycle,"frame":frame,"zoom":zoom,"warmup":cycle==0,"correctness_replay":cycle==3,"geometry_revision":epoch,"globe":globe,"viewport":[viewport.0,viewport.1,viewport.2,viewport.3],"prepare_cpu_wall_ms":prepare_ms,"serialized_frame_service_wall_ms":service_ms,"residual_completion_wait_wall_ms":wait_ms,"preparation_diagnostics":preparation,"window_visible":false,"window_focus":false}));
                    if cycle != 3 {
                        continue;
                    }
                    // Proof only: source/instructions/current GPU CPU payloads plus actual image/ID clicks.
                    self.audit_portrayal(&proof.join("audit"))?;
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .save_screenshot(proof.join("chart.png"))?;
                    if !globe {
                        self.renderer
                            .as_ref()
                            .unwrap()
                            .export_hidden_key_flat_coverage(
                                &proof.join("coverage"),
                                &self.render_context,
                            )?;
                    }
                    let mut picks = Vec::new();
                    for y in 1..=2 {
                        for x in 1..=3 {
                            let point = (
                                viewport.0 as f64 + viewport.2 as f64 * x as f64 / 4.,
                                viewport.1 as f64 + viewport.3 as f64 * y as f64 / 3.,
                            );
                            self.chart_click(point);
                            let selected=self.renderer.as_ref().unwrap().ui_state.selected_feature.as_ref().map(|f|serde_json::json!({"feature_id":f.feature_id,"cell_index":f.cell_index,"source":f.source,"full_attributes":f.attributes,"foid":f.foid,"definition":f.definition,"primitive_type":f.primitive_type,"symbol_name":f.symbol_name,"world_position_bits":[f.world_pos.0.to_bits(),f.world_pos.1.to_bits()],"longitude_shift_bits":f.longitude_shift.to_bits()}));
                            picks.push(serde_json::json!({"pixel":point,"selected":selected}));
                        }
                    }
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    std::fs::write(
                        proof.join("semantic.json"),
                        serde_json::to_vec_pretty(
                            &serde_json::json!({"globe":globe,"frame":frame,"zoom":zoom,"picks":picks,"profile":profile,"geometry_revision":epoch,"source_cells":self.cells.iter().map(|c|&c.file_path).collect::<Vec<_>>(),"window_visible":false,"window_focus":false}),
                        )?,
                    )?;
                }
            }
        }
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_source_cache_diagnostic(0)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_pattern_key_diagnostic(false)?;
        self.zoom_level = old_zoom;
        self.pan_offset = old_pan;
        self.renderer.as_mut().unwrap().ui_state.globe_pose = old_pose;
        self.select_feature(None);
        self.update_view();
        std::fs::write(
            output.join("frames.json"),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        Ok(())
    }

    fn audit_root_retained_passing(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Background test required"
        );
        let r = self.renderer.as_ref().context("Renderer missing")?;
        anyhow::ensure!(
            !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
            "Visible/focused test forbidden"
        );
        let globe = r.ui_state.globe_preview;
        let base_pose = r.globe_pose();
        let old_zoom = self.zoom_level;
        let old_pan = self.pan_offset;
        let old_pose = r.ui_state.globe_pose;
        let epoch = self.render_context.geometry_revision();
        let viewport = r.chart_viewport_pixels();
        let profile = self.current_profile_name.clone();
        self.renderer.as_mut().unwrap().set_profiling_enabled(true);
        let mut rows = Vec::new();
        std::fs::create_dir_all(output)?;
        anyhow::ensure!(
            std::env::var_os("FERRITE_ROOT_KEY_PROOF_COVERAGE").is_none(),
            "Proof coverage env must be unset before harness"
        );
        for (mode, retained_passing) in [false, true, true, false].into_iter().enumerate() {
            let source_mode = 1u8;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_retained_passing_diagnostic(retained_passing)?;
            let enabled = false;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_source_cache_diagnostic(source_mode)?;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_pattern_key_diagnostic(true)?;
            // Original boundary behavior unchanged; no negative allocation candidate applied.
            for cycle in 0..4 {
                for frame in 0..25usize {
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    self.renderer.as_mut().unwrap().set_globe_selection_click(
                        [0., 0.],
                        false,
                        &self.render_context.scaler,
                    );
                    let phase = frame as f64 / 24.;
                    let zoom = 200f64.powf(1. - (2. * phase - 1.).abs());
                    if globe {
                        let base = base_pose.context("Globe pose missing")?;
                        let mut pose = base;
                        pose.range_m = base.range_m / zoom;
                        pose.focus = ferrite_kernel::geodesy::GeographicPosition::new(
                            base.focus.latitude() + 0.35 * (phase * std::f64::consts::TAU).sin(),
                            base.focus.longitude() + 0.7 * (phase * std::f64::consts::TAU).sin(),
                        )?;
                        pose.heading_deg =
                            base.heading_deg + 30. * (phase * std::f64::consts::TAU).sin();
                        self.renderer.as_mut().unwrap().ui_state.globe_pose = Some(pose);
                    } else {
                        self.zoom_level = old_zoom * zoom;
                        self.pan_offset = old_pan;
                    }
                    let proof = output.join(format!("mode{mode}/cycle{cycle}-frame{frame:02}"));
                    if cycle == 3 {
                        std::fs::create_dir_all(&proof)?;
                        std::env::set_var(
                            "FERRITE_ROOT_KEY_PROOF_COVERAGE",
                            proof.join("coverage"),
                        );
                    }
                    let start = std::time::Instant::now();
                    self.update_view();
                    std::env::remove_var("FERRITE_ROOT_KEY_PROOF_COVERAGE");
                    anyhow::ensure!(
                        self.startup_error.is_none(),
                        "Prepare failed {:?}",
                        self.startup_error
                    );
                    let prepare_ms = start.elapsed().as_secs_f64() * 1000.;
                    let r = self.renderer.as_mut().unwrap();
                    r.render()?;
                    let wait = std::time::Instant::now();
                    r.wait_hidden_key_frame()?;
                    let wait_ms = wait.elapsed().as_secs_f64() * 1000.;
                    let service_ms = start.elapsed().as_secs_f64() * 1000.;
                    anyhow::ensure!(
                        !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
                        "Visible/focused frame"
                    );
                    anyhow::ensure!(
                        r.chart_viewport_pixels() == viewport
                            && self.current_profile_name == profile,
                        "Viewport/palette changed"
                    );
                    anyhow::ensure!(
                        self.render_context.geometry_revision() == epoch,
                        "Source epoch changed"
                    );
                    let preparation = r.globe_preview_diagnostics().map(|d| d.preparation.clone());
                    rows.push(serde_json::json!({"mode":mode,"retained_passing":retained_passing,"source_cache_mode":source_mode,"boundary_storage":enabled,"request_keys":true,"cycle":cycle,"frame":frame,"zoom":zoom,"warmup":cycle==0,"correctness_replay":cycle==3,"geometry_revision":epoch,"globe":globe,"viewport":[viewport.0,viewport.1,viewport.2,viewport.3],"prepare_cpu_wall_ms":prepare_ms,"serialized_frame_service_wall_ms":service_ms,"residual_completion_wait_wall_ms":wait_ms,"preparation_diagnostics":preparation,"window_visible":false,"window_focus":false}));
                    if cycle != 3 {
                        continue;
                    }
                    // Proof only: source/instructions/current GPU CPU payloads plus actual image/ID clicks.
                    self.audit_portrayal(&proof.join("audit"))?;
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .save_screenshot(proof.join("chart.png"))?;
                    if !globe {
                        self.renderer
                            .as_ref()
                            .unwrap()
                            .export_hidden_key_flat_coverage(
                                &proof.join("coverage"),
                                &self.render_context,
                            )?;
                    }
                    let mut picks = Vec::new();
                    for y in 1..=2 {
                        for x in 1..=3 {
                            let point = (
                                viewport.0 as f64 + viewport.2 as f64 * x as f64 / 4.,
                                viewport.1 as f64 + viewport.3 as f64 * y as f64 / 3.,
                            );
                            self.chart_click(point);
                            let selected=self.renderer.as_ref().unwrap().ui_state.selected_feature.as_ref().map(|f|serde_json::json!({"feature_id":f.feature_id,"cell_index":f.cell_index,"source":f.source,"full_attributes":f.attributes,"foid":f.foid,"definition":f.definition,"primitive_type":f.primitive_type,"symbol_name":f.symbol_name,"world_position_bits":[f.world_pos.0.to_bits(),f.world_pos.1.to_bits()],"longitude_shift_bits":f.longitude_shift.to_bits()}));
                            picks.push(serde_json::json!({"pixel":point,"selected":selected}));
                        }
                    }
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    std::fs::write(
                        proof.join("semantic.json"),
                        serde_json::to_vec_pretty(
                            &serde_json::json!({"globe":globe,"frame":frame,"zoom":zoom,"picks":picks,"profile":profile,"geometry_revision":epoch,"source_cells":self.cells.iter().map(|c|&c.file_path).collect::<Vec<_>>(),"window_visible":false,"window_focus":false}),
                        )?,
                    )?;
                }
            }
        }
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_retained_passing_diagnostic(false)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_source_cache_diagnostic(0)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_pattern_key_diagnostic(false)?;
        self.zoom_level = old_zoom;
        self.pan_offset = old_pan;
        self.renderer.as_mut().unwrap().ui_state.globe_pose = old_pose;
        self.select_feature(None);
        self.update_view();
        std::fs::write(
            output.join("frames.json"),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        Ok(())
    }

    fn audit_root_resource_epoch(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Background test required"
        );
        let r = self.renderer.as_ref().context("Renderer missing")?;
        anyhow::ensure!(
            !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
            "Visible/focused test forbidden"
        );
        let globe = r.ui_state.globe_preview;
        let base_pose = r.globe_pose();
        let old_zoom = self.zoom_level;
        let old_pan = self.pan_offset;
        let old_pose = r.ui_state.globe_pose;
        let epoch = self.render_context.geometry_revision();
        let viewport = r.chart_viewport_pixels();
        let profile = self.current_profile_name.clone();
        self.renderer.as_mut().unwrap().set_profiling_enabled(true);
        let mut rows = Vec::new();
        std::fs::create_dir_all(output)?;
        anyhow::ensure!(
            std::env::var_os("FERRITE_ROOT_KEY_PROOF_COVERAGE").is_none(),
            "Proof coverage env must be unset before harness"
        );
        for (mode, resource_reuse) in [false, true, true, false].into_iter().enumerate() {
            let retained_passing = true;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_prepared_symbol_resources(resource_reuse)?;
            let source_mode = 1u8;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_retained_passing_diagnostic(retained_passing)?;
            let enabled = false;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_source_cache_diagnostic(source_mode)?;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_pattern_key_diagnostic(true)?;
            // Original boundary behavior unchanged; no negative allocation candidate applied.
            for cycle in 0..4 {
                for frame in 0..25usize {
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    self.renderer.as_mut().unwrap().set_globe_selection_click(
                        [0., 0.],
                        false,
                        &self.render_context.scaler,
                    );
                    let phase = frame as f64 / 24.;
                    let zoom = 200f64.powf(1. - (2. * phase - 1.).abs());
                    if globe {
                        let base = base_pose.context("Globe pose missing")?;
                        let mut pose = base;
                        pose.range_m = base.range_m / zoom;
                        pose.focus = ferrite_kernel::geodesy::GeographicPosition::new(
                            base.focus.latitude() + 0.35 * (phase * std::f64::consts::TAU).sin(),
                            base.focus.longitude() + 0.7 * (phase * std::f64::consts::TAU).sin(),
                        )?;
                        pose.heading_deg =
                            base.heading_deg + 30. * (phase * std::f64::consts::TAU).sin();
                        self.renderer.as_mut().unwrap().ui_state.globe_pose = Some(pose);
                    } else {
                        self.zoom_level = old_zoom * zoom;
                        self.pan_offset = old_pan;
                    }
                    let proof = output.join(format!("mode{mode}/cycle{cycle}-frame{frame:02}"));
                    if cycle == 3 {
                        std::fs::create_dir_all(&proof)?;
                        std::env::set_var(
                            "FERRITE_ROOT_KEY_PROOF_COVERAGE",
                            proof.join("coverage"),
                        );
                    }
                    let start = std::time::Instant::now();
                    self.update_view();
                    std::env::remove_var("FERRITE_ROOT_KEY_PROOF_COVERAGE");
                    anyhow::ensure!(
                        self.startup_error.is_none(),
                        "Prepare failed {:?}",
                        self.startup_error
                    );
                    let prepare_ms = start.elapsed().as_secs_f64() * 1000.;
                    let r = self.renderer.as_mut().unwrap();
                    r.render()?;
                    let wait = std::time::Instant::now();
                    r.wait_hidden_key_frame()?;
                    let wait_ms = wait.elapsed().as_secs_f64() * 1000.;
                    let service_ms = start.elapsed().as_secs_f64() * 1000.;
                    anyhow::ensure!(
                        !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
                        "Visible/focused frame"
                    );
                    anyhow::ensure!(
                        r.chart_viewport_pixels() == viewport
                            && self.current_profile_name == profile,
                        "Viewport/palette changed"
                    );
                    anyhow::ensure!(
                        self.render_context.geometry_revision() == epoch,
                        "Source epoch changed"
                    );
                    let preparation = r.globe_preview_diagnostics().map(|d| d.preparation.clone());
                    rows.push(serde_json::json!({"mode":mode,"retained_passing":retained_passing,"resource_reuse":resource_reuse,"source_cache_mode":source_mode,"boundary_storage":enabled,"request_keys":true,"cycle":cycle,"frame":frame,"zoom":zoom,"warmup":cycle==0,"correctness_replay":cycle==3,"geometry_revision":epoch,"globe":globe,"viewport":[viewport.0,viewport.1,viewport.2,viewport.3],"prepare_cpu_wall_ms":prepare_ms,"serialized_frame_service_wall_ms":service_ms,"residual_completion_wait_wall_ms":wait_ms,"preparation_diagnostics":preparation,"window_visible":false,"window_focus":false}));
                    if cycle != 3 {
                        continue;
                    }
                    // Proof only: source/instructions/current GPU CPU payloads plus actual image/ID clicks.
                    self.audit_portrayal(&proof.join("audit"))?;
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .save_screenshot(proof.join("chart.png"))?;
                    if !globe {
                        self.renderer
                            .as_ref()
                            .unwrap()
                            .export_hidden_key_flat_coverage(
                                &proof.join("coverage"),
                                &self.render_context,
                            )?;
                    }
                    let mut picks = Vec::new();
                    for y in 1..=2 {
                        for x in 1..=3 {
                            let point = (
                                viewport.0 as f64 + viewport.2 as f64 * x as f64 / 4.,
                                viewport.1 as f64 + viewport.3 as f64 * y as f64 / 3.,
                            );
                            self.chart_click(point);
                            let selected=self.renderer.as_ref().unwrap().ui_state.selected_feature.as_ref().map(|f|serde_json::json!({"feature_id":f.feature_id,"cell_index":f.cell_index,"source":f.source,"full_attributes":f.attributes,"foid":f.foid,"definition":f.definition,"primitive_type":f.primitive_type,"symbol_name":f.symbol_name,"world_position_bits":[f.world_pos.0.to_bits(),f.world_pos.1.to_bits()],"longitude_shift_bits":f.longitude_shift.to_bits()}));
                            picks.push(serde_json::json!({"pixel":point,"selected":selected}));
                        }
                    }
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    std::fs::write(
                        proof.join("semantic.json"),
                        serde_json::to_vec_pretty(
                            &serde_json::json!({"globe":globe,"frame":frame,"zoom":zoom,"picks":picks,"profile":profile,"geometry_revision":epoch,"source_cells":self.cells.iter().map(|c|&c.file_path).collect::<Vec<_>>(),"window_visible":false,"window_focus":false}),
                        )?,
                    )?;
                }
            }
        }
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_prepared_symbol_resources(false)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_retained_passing_diagnostic(false)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_source_cache_diagnostic(0)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_pattern_key_diagnostic(false)?;
        self.zoom_level = old_zoom;
        self.pan_offset = old_pan;
        self.renderer.as_mut().unwrap().ui_state.globe_pose = old_pose;
        self.select_feature(None);
        self.update_view();
        std::fs::write(
            output.join("frames.json"),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        Ok(())
    }

    fn audit_root_coverage_geometry_epoch(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Background test required"
        );
        let r = self.renderer.as_ref().context("Renderer missing")?;
        anyhow::ensure!(
            !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
            "Visible/focused test forbidden"
        );
        let globe = r.ui_state.globe_preview;
        let base_pose = r.globe_pose();
        let old_zoom = self.zoom_level;
        let old_pan = self.pan_offset;
        let old_pose = r.ui_state.globe_pose;
        let epoch = self.render_context.geometry_revision();
        let viewport = r.chart_viewport_pixels();
        let profile = self.current_profile_name.clone();
        self.renderer.as_mut().unwrap().set_profiling_enabled(true);
        let mut rows = Vec::new();
        std::fs::create_dir_all(output)?;
        anyhow::ensure!(
            std::env::var_os("FERRITE_ROOT_KEY_PROOF_COVERAGE").is_none(),
            "Proof coverage env must be unset before harness"
        );
        for (mode, coverage_geometry_reuse) in [false, true, true, false].into_iter().enumerate() {
            let resource_reuse = true;
            self.coverage_geometry_cache
                .as_ref()
                .context("Coverage cache missing")?
                .lock()
                .map_err(|_| anyhow::anyhow!("Coverage geometry cache poisoned"))?
                .set_enabled(coverage_geometry_reuse);
            let retained_passing = true;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_prepared_symbol_resources(resource_reuse)?;
            let source_mode = 1u8;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_retained_passing_diagnostic(retained_passing)?;
            let enabled = false;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_source_cache_diagnostic(source_mode)?;
            self.renderer
                .as_mut()
                .unwrap()
                .configure_hidden_pattern_key_diagnostic(true)?;
            // Original boundary behavior unchanged; no negative allocation candidate applied.
            for cycle in 0..4 {
                for frame in 0..25usize {
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    self.renderer.as_mut().unwrap().set_globe_selection_click(
                        [0., 0.],
                        false,
                        &self.render_context.scaler,
                    );
                    let phase = frame as f64 / 24.;
                    let zoom = 200f64.powf(1. - (2. * phase - 1.).abs());
                    if globe {
                        let base = base_pose.context("Globe pose missing")?;
                        let mut pose = base;
                        pose.range_m = base.range_m / zoom;
                        pose.focus = ferrite_kernel::geodesy::GeographicPosition::new(
                            base.focus.latitude() + 0.35 * (phase * std::f64::consts::TAU).sin(),
                            base.focus.longitude() + 0.7 * (phase * std::f64::consts::TAU).sin(),
                        )?;
                        pose.heading_deg =
                            base.heading_deg + 30. * (phase * std::f64::consts::TAU).sin();
                        self.renderer.as_mut().unwrap().ui_state.globe_pose = Some(pose);
                    } else {
                        self.zoom_level = old_zoom * zoom;
                        self.pan_offset = old_pan;
                    }
                    let proof = output.join(format!("mode{mode}/cycle{cycle}-frame{frame:02}"));
                    if cycle == 3 {
                        std::fs::create_dir_all(&proof)?;
                        std::env::set_var(
                            "FERRITE_ROOT_KEY_PROOF_COVERAGE",
                            proof.join("coverage"),
                        );
                    }
                    let start = std::time::Instant::now();
                    self.update_view();
                    std::env::remove_var("FERRITE_ROOT_KEY_PROOF_COVERAGE");
                    anyhow::ensure!(
                        self.startup_error.is_none(),
                        "Prepare failed {:?}",
                        self.startup_error
                    );
                    let prepare_ms = start.elapsed().as_secs_f64() * 1000.;
                    let r = self.renderer.as_mut().unwrap();
                    r.render()?;
                    let wait = std::time::Instant::now();
                    r.wait_hidden_key_frame()?;
                    let wait_ms = wait.elapsed().as_secs_f64() * 1000.;
                    let service_ms = start.elapsed().as_secs_f64() * 1000.;
                    anyhow::ensure!(
                        !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
                        "Visible/focused frame"
                    );
                    anyhow::ensure!(
                        r.chart_viewport_pixels() == viewport
                            && self.current_profile_name == profile,
                        "Viewport/palette changed"
                    );
                    anyhow::ensure!(
                        self.render_context.geometry_revision() == epoch,
                        "Source epoch changed"
                    );
                    let preparation = r.globe_preview_diagnostics().map(|d| d.preparation.clone());
                    rows.push(serde_json::json!({"mode":mode,"coverage_geometry_reuse":coverage_geometry_reuse,"coverage_geometry_cache":self.coverage_geometry_cache.as_ref().unwrap().lock().unwrap().diagnostics(),"retained_passing":retained_passing,"resource_reuse":resource_reuse,"source_cache_mode":source_mode,"boundary_storage":enabled,"request_keys":true,"cycle":cycle,"frame":frame,"zoom":zoom,"warmup":cycle==0,"correctness_replay":cycle==3,"geometry_revision":epoch,"globe":globe,"viewport":[viewport.0,viewport.1,viewport.2,viewport.3],"prepare_cpu_wall_ms":prepare_ms,"serialized_frame_service_wall_ms":service_ms,"residual_completion_wait_wall_ms":wait_ms,"preparation_diagnostics":preparation,"window_visible":false,"window_focus":false}));
                    if cycle != 3 {
                        continue;
                    }
                    // Proof only: source/instructions/current GPU CPU payloads plus actual image/ID clicks.
                    self.audit_portrayal(&proof.join("audit"))?;
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .save_screenshot(proof.join("chart.png"))?;
                    if !globe {
                        self.renderer
                            .as_ref()
                            .unwrap()
                            .export_hidden_key_flat_coverage(
                                &proof.join("coverage"),
                                &self.render_context,
                            )?;
                    }
                    let mut picks = Vec::new();
                    for y in 1..=2 {
                        for x in 1..=3 {
                            let point = (
                                viewport.0 as f64 + viewport.2 as f64 * x as f64 / 4.,
                                viewport.1 as f64 + viewport.3 as f64 * y as f64 / 3.,
                            );
                            self.chart_click(point);
                            let selected=self.renderer.as_ref().unwrap().ui_state.selected_feature.as_ref().map(|f|serde_json::json!({"feature_id":f.feature_id,"cell_index":f.cell_index,"source":f.source,"full_attributes":f.attributes,"foid":f.foid,"definition":f.definition,"primitive_type":f.primitive_type,"symbol_name":f.symbol_name,"world_position_bits":[f.world_pos.0.to_bits(),f.world_pos.1.to_bits()],"longitude_shift_bits":f.longitude_shift.to_bits()}));
                            picks.push(serde_json::json!({"pixel":point,"selected":selected}));
                        }
                    }
                    self.select_feature(None);
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .ui_state
                        .selection_candidates
                        .clear();
                    std::fs::write(
                        proof.join("semantic.json"),
                        serde_json::to_vec_pretty(
                            &serde_json::json!({"globe":globe,"frame":frame,"zoom":zoom,"picks":picks,"profile":profile,"geometry_revision":epoch,"source_cells":self.cells.iter().map(|c|&c.file_path).collect::<Vec<_>>(),"window_visible":false,"window_focus":false}),
                        )?,
                    )?;
                }
            }
        }
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_prepared_symbol_resources(false)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_retained_passing_diagnostic(false)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_source_cache_diagnostic(0)?;
        self.renderer
            .as_mut()
            .unwrap()
            .configure_hidden_pattern_key_diagnostic(false)?;
        self.zoom_level = old_zoom;
        self.pan_offset = old_pan;
        self.renderer.as_mut().unwrap().ui_state.globe_pose = old_pose;
        self.select_feature(None);
        self.update_view();
        std::fs::write(
            output.join("frames.json"),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        Ok(())
    }

    fn audit_animation(&mut self, output: &Path) -> Result<()> {
        let mut rows = Vec::new();
        let mut expected = None;
        for animation in [false, true, false] {
            self.renderer
                .as_mut()
                .context("No renderer")?
                .set_animation_mode(animation);
            let start = std::time::Instant::now();
            self.update_view();
            let renderer = self.renderer.as_ref().unwrap();
            let stats = renderer.statistics();
            let visible = renderer.displayed_geometry().to_vec();
            let symbols = renderer.displayed_symbols();
            let fingerprint = format!("{stats:?}|{visible:?}|{symbols:?}");
            if let Some(expected) = &expected {
                anyhow::ensure!(
                    expected == &fingerprint,
                    "Animation altered the displayed instruction set"
                );
            } else {
                expected = Some(fingerprint);
            }
            rows.push(serde_json::json!({"animation":animation,"update_seconds":start.elapsed().as_secs_f64(),"area_triangles":stats.area_triangles,
                "line_triangles":stats.line_triangles,"line_visibility_counts":renderer.line_visibility_counts(),"temporal_visibility_counts":renderer.temporal_visibility_counts(),"symbols":stats.symbol_instances,"text_labels":stats.text_labels,"displayed_geometries":visible.len()}));
        }
        std::fs::write(output, serde_json::to_vec_pretty(&rows)?)?;
        info!("Animation audit: stationary/animated/stationary geometry, symbols and text counts match");
        Ok(())
    }

    /// Opt-in runtime audit against the same displayed geometry and pick path as mouse input.
    fn audit_globe_selection(&mut self, output: &Path) -> Result<()> {
        let diagnostics = self
            .renderer
            .as_ref()
            .and_then(|r| r.globe_preview_diagnostics())
            .context("Globe selection audit has no actual frame")?;
        let v = diagnostics.viewport;
        let mut probes: Vec<[f64; 2]> = diagnostics
            .symbol_footprints_px
            .iter()
            .take(12)
            .map(|b| {
                [
                    (b[0] as f64 + b[2] as f64) / 2.,
                    (b[1] as f64 + b[3] as f64) / 2.,
                ]
            })
            .collect();
        for y in 1..=3 {
            for x in 1..=4 {
                probes.push([v[0] + v[2] * x as f64 / 5., v[1] + v[3] * y as f64 / 4.]);
            }
        }
        let mut rows = Vec::new();
        let mut selected_count = 0;
        let mut last_selected = None;
        for p in probes {
            if !self.chart_contains((p[0], p[1])) {
                continue;
            }
            self.globe_chart_click((p[0], p[1]));
            let renderer = self.renderer.as_ref().unwrap();
            if let Some(feature) = &renderer.ui_state.selected_feature {
                let cell = feature
                    .cell_index
                    .and_then(|i| self.cells.get(i as usize))
                    .context("Globe pick has no product cell")?;
                anyhow::ensure!(
                    cell.features.contains_key(&feature.feature_id),
                    "Globe pick fabricated feature identity"
                );
                anyhow::ensure!(
                    feature.source.as_deref() == Some(cell.file_path.to_string_lossy().as_ref()),
                    "Globe pick mixed source cells"
                );
                selected_count += 1;
                last_selected = Some(feature.clone());
                rows.push(serde_json::json!({"pixel":p,"feature_id":feature.feature_id,"cell_index":feature.cell_index,"feature_type":feature.feature_type,"attributes":feature.attributes.len(),"source":feature.source,"candidates":renderer.ui_state.selection_candidates.len(),"world_position":feature.world_pos}));
            } else {
                rows.push(serde_json::json!({"pixel":p,"selected":false}));
            }
        }
        anyhow::ensure!(selected_count > 0, "No actual globe objects selected");
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.select_feature(last_selected);
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot_with_ui(output.with_file_name("selection-ui.png"))?;
        std::fs::write(
            output,
            serde_json::to_vec_pretty(
                &serde_json::json!({"same_click_handler_as_pointer":true,"selected_probes":selected_count,"probes":rows,"viewport":v,"physical_pixel_radius_maximum":64,"single_sample_picking":true,"coverage_3d_verified":false}),
            )?,
        )?;
        Ok(())
    }

    fn audit_selection(&mut self, output: &Path) -> Result<()> {
        if self
            .renderer
            .as_ref()
            .is_some_and(|r| r.ui_state.globe_preview)
        {
            return self.audit_globe_selection(output);
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let renderer = self.renderer.as_ref().context("No renderer")?;
        let (vx, vy, vw, vh) = renderer.chart_viewport_pixels();
        anyhow::ensure!(
            self.render_context
                .scaler
                .viewport
                .matches_physical_rect((vx, vy, vw, vh)),
            "Selection audit has unsynchronized chart layout"
        );
        let (pan, fast_zoom, pivot) = renderer.fast_view_transform();
        anyhow::ensure!(
            pan == (0., 0.) && fast_zoom == 1.,
            "Selection audit has unreconciled GPU transform"
        );
        let layout = serde_json::json!({"viewport_physical_pixels":[vx,vy,vw,vh],"ui_pixels_per_point":renderer.ui_pixels_per_point(),"native_pixels_per_point":self.window.as_ref().map(|w|w.scale_factor()),"fast_pan":pan,"fast_zoom":fast_zoom,"fast_pivot":pivot,"flat_projection":format!("{:?}",self.render_context.scaler.projection()),"geographic_bounds":[self.render_context.scaler.geo_bounds.min_x,self.render_context.scaler.geo_bounds.min_y,self.render_context.scaler.geo_bounds.max_x,self.render_context.scaler.geo_bounds.max_y],"zoom":self.zoom_level});

        let on_screen = |p: ferrite_render::ScreenPoint| {
            p.x >= vx && p.x <= vx + vw && p.y >= vy && p.y <= vy + vh
        };
        let scaler = &self.render_context.scaler;
        let mut probes = Vec::new();
        let mut counts = [0usize; 3];
        let mut per_cell = std::collections::HashMap::<(usize, u32), usize>::new();
        let mut seen_probes = std::collections::HashSet::new();
        for &index in renderer.displayed_geometry() {
            let instruction = &self.render_context.raw_instructions()[index];
            let (kind, cell, id, probe) = match instruction {
                DrawingInstruction::Line(line) => {
                    let midpoint = |(a, b): (WorldPoint, WorldPoint)| {
                        scaler
                            .world_to_screen(WorldPoint::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5))
                    };
                    let p = line.render_paths(scaler).find_map(|points| {
                        let dash = ferrite_render::dash_line_spans(
                            &points,
                            scaler,
                            &line.style,
                            renderer.displayed_line_spans(index),
                        );
                        if let Some(spans) =
                            dash.as_deref().or(renderer.displayed_line_spans(index))
                        {
                            spans
                                .iter()
                                .filter_map(|s| s.endpoints(&points))
                                .map(midpoint)
                                .find(|p| on_screen(*p))
                        } else {
                            points
                                .windows(2)
                                .map(|w| midpoint((w[0], w[1])))
                                .find(|p| on_screen(*p))
                        }
                    });
                    (1, line.cell_index, line.feature_id, p)
                }
                DrawingInstruction::Area(area) => {
                    let center = ferrite_render::ScreenPoint::new(vx + vw * 0.5, vy + vh * 0.5);
                    let p = if ferrite_render::hit_geometry(instruction, scaler, center, 0.)
                        .is_some()
                    {
                        Some(center)
                    } else {
                        area.exterior
                            .iter()
                            .map(|p| scaler.world_to_screen(*p))
                            .find(|p| on_screen(*p))
                    };
                    (2, area.cell_index, area.feature_id, p)
                }
                _ => continue,
            };
            if let (Some(cell), Some(id), Some(point)) = (cell, id, probe) {
                if !renderer.coverage_fragment_visible(index, 0, [point.x, point.y]) {
                    continue;
                }
                if per_cell.get(&(kind, cell)).copied().unwrap_or(0) >= 3
                    || !seen_probes.insert((kind, cell, id))
                {
                    continue;
                }
                *per_cell.entry((kind, cell)).or_default() += 1;
                probes.push((kind, cell, id, point));
                counts[kind] += 1;
            }
        }
        for symbol in &self.rendered_symbols {
            let p = ferrite_render::ScreenPoint::new(symbol.screen_x, symbol.screen_y);
            if let Some(cell) = symbol.cell_index.filter(|_| on_screen(p)) {
                if symbol
                    .source
                    .is_some_and(|i| !renderer.coverage_fragment_visible(i, 0, [p.x, p.y]))
                {
                    continue;
                }
                if per_cell.get(&(0, cell)).copied().unwrap_or(0) >= 3
                    || !seen_probes.insert((0, cell, symbol.feature_id))
                {
                    continue;
                }
                *per_cell.entry((0, cell)).or_default() += 1;
                probes.push((0, cell, symbol.feature_id, p));
                counts[0] += 1;
            }
        }
        anyhow::ensure!(
            counts[1] > 0 && counts[2] > 0,
            "No visible line/area probes"
        );
        let radius = (20. * self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.)) as f32;
        let mut sample_points: Vec<_> = probes.iter().map(|p| p.3).collect();
        for row in 0..6 {
            for col in 0..8 {
                sample_points.push(ferrite_render::ScreenPoint::new(
                    vx + vw * (col as f32 + 0.5) / 8.,
                    vy + vh * (row as f32 + 0.5) / 6.,
                ));
            }
        }
        for &point in &sample_points {
            for shift in [-360., 0., 360.] {
                let x = point.x as f64 + shift * self.render_context.scaler.scale_x();
                anyhow::ensure!(self.find_features_at_impl(x, point.y as f64, radius, false)
                    == self.find_features_at_impl(x, point.y as f64, radius, true),
                    "Indexed selection changed candidates, order, nearest position or longitude copy");
            }
        }
        let mut baseline_times = Vec::new();
        let mut indexed_times = Vec::new();
        for run in 0..3 {
            // Alternate measurement order to reduce systematic warm-cache bias.
            for indexed in if run % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = std::time::Instant::now();
                for point in &sample_points {
                    std::hint::black_box(self.find_features_at_impl(
                        point.x as f64,
                        point.y as f64,
                        radius,
                        indexed,
                    ));
                }
                let seconds = start.elapsed().as_secs_f64();
                if indexed {
                    indexed_times.push(seconds);
                } else {
                    baseline_times.push(seconds);
                }
            }
        }
        let renderer = self.renderer.as_ref().unwrap();
        let candidate_counts: Vec<_> = sample_points
            .iter()
            .map(|p| {
                renderer
                    .selection_candidates_in_context(&self.render_context, *p, radius as f64 * 0.4)
                    .len()
            })
            .collect();
        let performance = serde_json::json!({"oracle_queries":sample_points.len()*3,
            "timed_queries_per_run":sample_points.len(),"baseline_seconds":baseline_times,
            "indexed_seconds":indexed_times,"all_candidates_order_and_coordinates_equal":true,
            "displayed_geometry":renderer.displayed_geometry().len(),"candidate_counts":candidate_counts,
            "index":renderer.selection_index_stats(),"symbol_scan_unchanged":true});
        let mut reports = Vec::new();
        for (kind, cell, id, point) in probes {
            let candidates = self.find_features_at(point.x as f64, point.y as f64, radius);
            let expected = candidates
                .iter()
                .find(|s| s.cell_index == Some(cell) && s.feature_id == id)
                .with_context(|| format!("Visible probe absent from pick candidates: kind={kind} cell={cell} id={id} point={point:?} candidates={candidates:?}"))?;
            let selected = self.describe_hit(expected);
            let source = self.cells.get(cell as usize).context("Wrong source cell")?;
            let feature = source.features.get(&id).context("Wrong source feature")?;
            anyhow::ensure!(
                selected.source.as_deref() == Some(source.file_path.to_string_lossy().as_ref()),
                "Wrong source path"
            );
            anyhow::ensure!(
                selected.feature_type
                    == feature
                        .feature_code
                        .as_deref()
                        .unwrap_or(&expected.symbol_ref),
                "Wrong source type"
            );
            let source_path = selected.source.clone();
            let feature_type = selected.feature_type.clone();
            let attributes = selected.attributes.len();
            let foid = selected.foid.clone();
            anyhow::ensure!(
                foid == feature.foid.as_ref().map(ToString::to_string),
                "Wrong source FOID"
            );
            let mut wrapped_checks = Vec::new();
            for shift in [-360., 360.] {
                let shifted_x = point.x as f64 + shift * self.render_context.scaler.scale_x();
                let wrapped = self.find_features_at(shifted_x, point.y as f64, radius);
                let source_index = expected
                    .source
                    .context("Selection probe has no source instruction")?;
                let renderer = self.renderer.as_ref().unwrap();
                let raw_hit = ferrite_render::hit_geometry_wrapped_visible(
                    &self.render_context.raw_instructions()[source_index],
                    &self.render_context.scaler,
                    ferrite_render::ScreenPoint::new(shifted_x as f32, point.y),
                    radius as f64 * 0.4,
                    renderer.displayed_line_spans(source_index),
                    renderer.longitude_wrapping_enabled(),
                );
                let visible = if expected.kind == 0 {
                    renderer
                        .displayed_symbol_screen(
                            [expected.screen_x, expected.screen_y],
                            if shift < 0. { 1 } else { 2 },
                        )
                        .is_some_and(|p| {
                            (p[0] - shifted_x as f32).hypot(p[1] - point.y) <= radius
                                && renderer.coverage_fragment_visible(
                                    source_index,
                                    if shift < 0. { 1 } else { 2 },
                                    [shifted_x as f32, point.y],
                                )
                        })
                } else {
                    raw_hit.as_ref().is_some_and(|h| {
                        h.longitude_shift == shift
                            && renderer.coverage_fragment_visible(
                                source_index,
                                if shift < 0. { 1 } else { 2 },
                                [h.hit.nearest.x, h.hit.nearest.y],
                            )
                    })
                };
                let hit = wrapped.iter().find(|h| {
                    h.cell_index == Some(cell) && h.feature_id == id && h.longitude_shift == shift
                });
                if visible {
                    let hit = hit.context("Visible longitude copy absent from application pick")?;
                    let copy = self.describe_hit(hit);
                    anyhow::ensure!(
                        copy.foid == foid,
                        "Wrapped selection changed source identity"
                    );
                    self.select_feature(Some(copy));
                } else {
                    anyhow::ensure!(
                        !wrapped
                            .iter()
                            .any(|h| h.source == Some(source_index) && h.longitude_shift == shift),
                        "Masked longitude copy remained selectable"
                    );
                }
                wrapped_checks.push(serde_json::json!({"longitude_shift":shift,"source_visible":visible,"feature_selected":hit.is_some(),"query_physical_pixels":[shifted_x,point.y]}));
            }
            self.select_feature(Some(selected));
            reports.push(serde_json::json!({"wrapped_checks":wrapped_checks,"kind":kind,"cell":cell,"feature_id":id,"source":source_path,"feature_type":feature_type,"attributes":attributes,"foid":foid,"candidates":candidates.len(),"query_physical_pixels":[point.x,point.y],"candidate_identities":candidates.iter().map(|c|serde_json::json!({"cell":c.cell_index,"feature_id":c.feature_id,"kind":c.kind,"plane":c.plane,"priority":c.priority,"longitude_shift":c.longitude_shift})).collect::<Vec<_>>(),
                "selected_geometry_vertices":self.renderer.as_ref().unwrap().selected_geometry_vertex_count()}));
            if candidates.len() > 1 {
                self.select_feature(Some(self.describe_hit(&candidates[1])));
            }
        }
        std::fs::write(
            output,
            serde_json::to_vec_pretty(
                &serde_json::json!({"counts":counts,"probes":reports,"layout":layout,"performance":performance,"native_mouse_and_menu_interaction_verified":false}),
            )?,
        )?;
        info!(
            "Selection audit: {} points, {} lines, {} areas",
            counts[0], counts[1], counts[2]
        );
        Ok(())
    }

    fn refresh_visible_selection(&mut self) {
        // Remove stale selection/overlap candidates after a temporal expiry.
        let mut visible = std::collections::HashSet::new();
        for symbol in &self.rendered_symbols {
            visible.insert((symbol.cell_index, symbol.feature_id));
        }
        if let Some(renderer) = &self.renderer {
            for &index in renderer.displayed_geometry() {
                let instruction = &self.render_context.raw_instructions()[index];
                let source = match instruction {
                    DrawingInstruction::Line(i) => i.cell_index,
                    DrawingInstruction::Area(i) => i.cell_index,
                    _ => None,
                };
                if let Some(id) = instruction.feature_id() {
                    visible.insert((source, id));
                }
            }
        }
        if self
            .renderer
            .as_ref()
            .is_some_and(|r| r.ui_state.globe_preview)
        {
            visible.clear();
            for &source in self.renderer.as_ref().unwrap().globe_displayed_sources() {
                if let Some(instruction) = self.render_context.raw_instructions().get(source) {
                    if let Some(id) = instruction.feature_id() {
                        visible.insert((instruction.cell_index(), id));
                    }
                }
            }
        }
        let selected = self
            .renderer
            .as_ref()
            .and_then(|r| r.ui_state.selected_feature.clone());
        if selected
            .as_ref()
            .is_some_and(|s| !visible.contains(&(s.cell_index, s.feature_id)))
        {
            self.select_feature(None);
        } else if selected.is_some() {
            self.select_feature(selected);
        }
        if let Some(renderer) = &mut self.renderer {
            renderer
                .ui_state
                .selection_candidates
                .retain(|s| visible.contains(&(s.cell_index, s.feature_id)));
        }
    }

    fn select_feature(&mut self, selected: Option<SelectedFeature>) {
        let mut geometry = Vec::new();
        if let (Some(feature), Some(renderer)) = (&selected, &self.renderer) {
            for &index in renderer.displayed_geometry() {
                let instruction = &self.render_context.raw_instructions()[index];
                if instruction.feature_id() != Some(feature.feature_id) {
                    continue;
                }
                match instruction {
                    DrawingInstruction::Line(line) if line.cell_index == feature.cell_index => {
                        if let Some(spans) = renderer.displayed_line_spans(index) {
                            for span in spans {
                                if let Some((a, b)) = span.endpoints(&line.points) {
                                    geometry.push(vec![a, b]);
                                }
                            }
                        } else {
                            geometry.extend(
                                line.render_paths(&self.render_context.scaler)
                                    .into_iter()
                                    .map(|p| p.into_owned()),
                            );
                        }
                    }
                    DrawingInstruction::Area(area) if area.cell_index == feature.cell_index => {
                        for ring in std::iter::once(&area.exterior).chain(&area.interiors) {
                            let mut path = ring.clone();
                            if let Some(first) = path.first().copied() {
                                if path.last() != Some(&first) {
                                    path.push(first);
                                }
                            }
                            geometry.push(path);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.selected_feature = selected;
            renderer.set_selection_geometry(geometry, &self.render_context.scaler);
        }
    }

    fn chart_contains(&self, point: (f64, f64)) -> bool {
        self.renderer.as_ref().is_some_and(|r| {
            let (x, y, w, h) = r.chart_viewport_pixels();
            point.0 >= x as f64
                && point.1 >= y as f64
                && point.0 < (x + w) as f64
                && point.1 < (y + h) as f64
        })
    }

    fn chart_click(&mut self, position: (f64, f64)) {
        if self
            .renderer
            .as_ref()
            .is_some_and(|r| r.ui_state.globe_preview)
        {
            self.globe_chart_click(position);
            return;
        }
        if !self.chart_loaded {
            return;
        }
        if self.zoom_animating || self.zoom_rebuild_phase != 0 || self.pan_rebuild_phase != 0 {
            self.zoom_animating = false;
            self.zoom_target = self.zoom_level;
            self.zoom_rebuild_phase = 0;
            self.pan_rebuild_phase = 0;
            self.update_view();
        }
        let (x, y) = position;
        info!("Click: screen=({:.1}, {:.1})", x, y);
        let screen_pt = ferrite_render::ScreenPoint::new(x as f32, y as f32);
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
            let pixel_ratio = self
                .window
                .as_ref()
                .map(|w| w.scale_factor())
                .unwrap_or(1.0);
            let nearby = self.find_features_at(x, y, (20.0 * pixel_ratio) as f32);

            let candidates: Vec<_> = nearby.iter().map(|sym| self.describe_hit(sym)).collect();
            let selected = candidates.first().cloned();
            let nearby_count = nearby.len();
            drop(nearby); // Release borrow on self.rendered_symbols

            let coverage_info = match self.inspect_bathymetry(world.x, world.y) {
                Ok(info) => info,
                Err(e) => Some(format!("S-102 query failed: {e:#}")),
            };
            // Update selected feature in UI
            if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.selection_candidates = candidates;
                renderer.ui_state.selection_requested = None;
                renderer.ui_state.coverage_info = coverage_info;
            }

            self.select_feature(selected);
            info!(
                "Click at ({:.4}, {:.4}): {} unique objects found",
                world.x, world.y, nearby_count
            );
        }
    }

    fn globe_chart_click(&mut self, position: (f64, f64)) {
        if !self.chart_loaded || !self.chart_contains(position) {
            return;
        }
        if self.zoom_animating || self.zoom_rebuild_phase != 0 || self.pan_rebuild_phase != 0 {
            self.zoom_animating = false;
            self.zoom_target = self.zoom_level;
            self.zoom_rebuild_phase = 0;
            self.pan_rebuild_phase = 0;
            self.update_view();
        }
        let screen = ferrite_render::ScreenPoint::new(position.0 as f32, position.1 as f32);
        let world = self
            .renderer
            .as_ref()
            .and_then(|r| r.globe_world_at(screen));
        if let Some(world) = world {
            if self.plugin_system.handle_click(
                world.x,
                world.y,
                ferrite_plugin_api::MouseButton::Left,
                false,
            ) {
                self.update_view();
                return;
            }
        }
        let radius = (20. * self.window.as_ref().map_or(1., |w| w.scale_factor())).min(64.);
        let hits = match self.renderer.as_mut().unwrap().globe_feature_candidates(
            &self.render_context,
            screen,
            radius,
        ) {
            Ok(hits) => hits,
            Err(e) => {
                self.renderer.as_mut().unwrap().ui_state.notice =
                    Some(format!("Globe selection failed: {e}"));
                return;
            }
        };
        let mut nearby = Vec::new();
        for (source, hit) in hits {
            let Some(instruction) = self.render_context.raw_instructions().get(source) else {
                continue;
            };
            let Some(feature_id) = instruction.feature_id() else {
                continue;
            };
            let cell_index = instruction.cell_index();
            let at = self.renderer.as_ref().and_then(|r| {
                r.globe_world_at(ferrite_render::ScreenPoint::new(
                    hit.pixel[0] as f32,
                    hit.pixel[1] as f32,
                ))
            });
            let reference = cell_index
                .and_then(|c| self.cells.get(c as usize))
                .and_then(|c| {
                    c.features.get(&feature_id).and_then(|f| {
                        f.spatial_associations
                            .iter()
                            .find_map(|a| c.points.get(&a.spatial_id.key()))
                    })
                })
                .map(|p| WorldPoint::new(p.position.x, p.position.y));
            let device = instruction.portrayal_origin().is_device_fixed();
            let authored_reference = match instruction.portrayal_origin() {
                ferrite_render::PortrayalOrigin::Point(source) => match source.as_ref() {
                    ferrite_render::PointOriginGeometry::FeaturePoint(p) => Some(*p),
                    ferrite_render::PointOriginGeometry::AugmentedLocalPoint {
                        reference_point,
                        ..
                    } => Some(*reference_point),
                    _ => None,
                },
                _ => None,
            };
            let coordinate = if matches!(
                instruction,
                DrawingInstruction::Point(_) | DrawingInstruction::Text(_)
            ) {
                authored_reference
                    .or(reference)
                    .or(if device { None } else { at })
            } else {
                at.or(reference)
            };
            let Some(coordinate) = coordinate else {
                continue;
            };
            nearby.push((
                RenderedSymbol {
                    source: Some(source),
                    plane: instruction
                        .display_plane()
                        .composition_plane(ferrite_kernel::CompositionStage::Chart),
                    kind: match instruction {
                        DrawingInstruction::Line(_) => 1,
                        DrawingInstruction::Area(_) => 2,
                        _ => 0,
                    },
                    world_x: coordinate.x,
                    world_y: coordinate.y,
                    longitude_shift: 0.,
                    feature_id,
                    screen_x: hit.pixel[0] as f32,
                    screen_y: hit.pixel[1] as f32,
                    priority: instruction.priority().0,
                    symbol_ref: if let DrawingInstruction::Point(p) = instruction {
                        p.symbol_ref.clone()
                    } else {
                        String::new()
                    },
                    cell_index,
                },
                hit.distance_px,
            ));
        }
        nearby.sort_by(RenderedSymbol::compare_hits);
        let mut seen = std::collections::HashSet::new();
        nearby.retain(|(h, _)| seen.insert((h.cell_index, h.feature_id)));
        let candidates: Vec<_> = nearby.iter().map(|(h, _)| self.describe_hit(h)).collect();
        let first = nearby
            .first()
            .map(|(h, _)| (h.source.unwrap(), [h.screen_x as f64, h.screen_y as f64]));
        let selected = candidates.first().cloned();
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .selection_candidates = candidates;
        self.renderer.as_mut().unwrap().ui_state.selection_requested = None;
        self.renderer.as_mut().unwrap().ui_state.coverage_info =
            world.and_then(|w| self.inspect_bathymetry(w.x, w.y).ok().flatten());
        self.select_feature(selected);
        if let Some((source, pixel)) = first {
            let device = self.render_context.raw_instructions()[source]
                .portrayal_origin()
                .is_device_fixed();
            self.renderer.as_mut().unwrap().set_globe_selection_click(
                pixel,
                device,
                &self.render_context.scaler,
            );
        }
    }

    fn sync_globe_controller(&mut self) {
        let Some(pose) = self.renderer.as_ref().and_then(|r| r.ui_state.globe_pose) else {
            return;
        };
        let viewport = self.render_context.scaler.viewport;
        if let Some(bounds) = ferrite_render::anchored_zoom_bounds_projected(
            self.render_context.scaler.projection(),
            self.bounds,
            viewport,
            self.zoom_level,
            WorldPoint::new(pose.focus.longitude(), pose.focus.latitude()),
            viewport.center(),
            pose.focus.latitude(),
        ) {
            if let Some(pan) = self
                .render_context
                .scaler
                .projection()
                .pan_between(self.bounds, bounds)
            {
                self.pan_offset = (pan[0], pan[1]);
            }
            self.render_context.zoom_to_fit(bounds);
        }
    }
    fn globe_gesture(&mut self, motion: navigation::GestureMotion) -> bool {
        if !self
            .renderer
            .as_ref()
            .is_some_and(|r| r.ui_state.globe_preview)
        {
            return false;
        }
        let Some(zoom) = navigation::bounded_zoom(self.zoom_level * motion.ratio) else {
            return true;
        };
        let ratio = zoom / self.zoom_level;
        let from = ferrite_render::ScreenPoint::new(motion.from.0 as f32, motion.from.1 as f32);
        let to = ferrite_render::ScreenPoint::new(motion.to.0 as f32, motion.to.1 as f32);
        let r = self.renderer.as_mut().unwrap();
        if let Some(anchor) = r.globe_world_at(from) {
            if r.move_globe_anchor(anchor, to, ratio) {
                self.zoom_level = zoom;
                self.zoom_target = zoom;
                self.zoom_animating = false;
                self.pan_velocity = (0., 0.);
                self.sync_globe_controller();
                self.update_view();
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        true
    }
    fn apply_gesture_motion(&mut self, motion: navigation::GestureMotion) {
        if self.globe_gesture(motion) {
            return;
        }
        // Settle only legacy mouse pan: its CPU scaler lags the GPU transform.
        // Continuous gestures keep the current CPU and GPU cameras synchronized.
        if self.pan_rebuild_phase != 0 || self.is_dragging || self.pan_velocity != (0., 0.) {
            self.pan_velocity = (0., 0.);
            self.is_dragging = false;
            self.pan_rebuild_phase = 0;
            self.update_view();
        }
        let Some((zoom, bounds)) = navigation::gesture_camera(
            self.bounds,
            &self.render_context.scaler,
            self.zoom_level,
            motion,
        ) else {
            return;
        };
        self.zoom_animating = false;
        self.zoom_level = zoom;
        self.zoom_target = zoom;
        if let Some(pan) = self
            .render_context
            .scaler
            .projection()
            .pan_between(self.bounds, bounds)
        {
            self.pan_offset = (pan[0], pan[1]);
        }
        self.render_context.zoom_to_fit(bounds);
        if let Some(renderer) = &mut self.renderer {
            if !renderer.set_gpu_view_scaler(&self.render_context.scaler) {
                self.update_view();
            } else {
                renderer.ui_state.zoom_level = zoom;
            }
        }
        self.zoom_last_scroll = std::time::Instant::now();
        self.zoom_rebuild_phase = 2;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Pan in the selected projection's angular coordinates. The CPU camera
    /// stays current while GPU geometry retains its captured source transform.
    fn pan_camera_by(&mut self, delta: [f64; 2]) {
        let projection = self.render_context.scaler.projection();
        let pan = [self.pan_offset.0 + delta[0], self.pan_offset.1 + delta[1]];
        let wrapped = pan[0] - (pan[0] / 360.).round() * 360.;
        let Some(bounds) = projection.view_bounds(self.bounds, self.zoom_level, [wrapped, pan[1]])
        else {
            return;
        };
        let Some(actual) = projection.pan_between(self.bounds, bounds) else {
            return;
        };
        self.pan_offset = (actual[0], actual[1]);
        self.render_context.zoom_to_fit(bounds);
        if let Some(renderer) = &mut self.renderer {
            if !renderer.set_gpu_view_scaler(&self.render_context.scaler) {
                self.update_view_ex(false, false);
            }
        }
    }

    /// Update the view based on current zoom and pan
    /// - `rebuild_hit_test`: if false, skip rebuilding the hit-test symbol list
    /// - `preserve_declutter`: if true, preserve symbol declutter grids to avoid flickering
    fn update_view_ex(&mut self, rebuild_hit_test: bool, preserve_declutter: bool) {
        self.next_temporal_wake = None;
        // A rebuild already includes the current world pan/zoom in its scaler.
        // Discard the previous fast transform so it is never applied twice.
        if let Some(renderer) = &mut self.renderer {
            renderer.reset_pan_offset();
        }
        self.zoom_rebuilt_level = self.zoom_level;
        let profiling = ferrite_wgpu::profiler::is_profiling_enabled();
        let update_view_start = if profiling {
            Some(std::time::Instant::now())
        } else {
            None
        };

        // Update viewport to use actual chart area (excluding UI panels)
        if let Some(renderer) = &self.renderer {
            let (x, y, w, h) = renderer.chart_viewport_pixels();
            if w > 0.0 && h > 0.0 {
                self.render_context.set_viewport_rect(x, y, w, h);
            }
        }

        // Calculate the zoomed and panned bounds
        // Wrap horizontal pan offset modulo 360° so the viewport always stays near
        // the chart data. Combined with ±360° rendering copies, this enables
        // seamless infinite horizontal panning (Earth is round).
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        let Some(new_bounds) = self.render_context.scaler.projection().view_bounds(
            self.bounds,
            self.zoom_level,
            [wrapped_pan_x, self.pan_offset.1],
        ) else {
            return;
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
        self.render_context.remove_coverage_exempt_instructions();
        for mut instr in self.plugin_system.get_render_instructions() {
            instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
            self.render_context.add_instruction(instr);
        }
        if let Some(renderer) = &self.renderer {
            if renderer.ui_state.globe_preview {
                self.render_context.clear_prepared_coverage();
            } else if let Err(error) = prepare_flat_coverage(
                self.coverage_inventory.as_deref(),
                &mut self.render_context,
                renderer.window().inner_size(),
                renderer.window().scale_factor(),
            ) {
                let message = format!("Coverage preparation failed: {error}");
                tracing::error!("{message}");
                if self.auto_screenshot.is_some() {
                    self.startup_error = Some(message.clone());
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.begin_frame();
                    renderer.ui_state.notice = Some(message);
                }
                return;
            }
        }

        if let Some(renderer) = &mut self.renderer {
            // Update zoom level for symbol decluttering and UI
            renderer.set_zoom_level(self.zoom_level);
            renderer.ui_state.zoom_level = self.zoom_level;
            // During animation, preserve declutter state to avoid flickering
            renderer.begin_frame_ex(preserve_declutter);
            if renderer.ui_state.globe_preview {
                if let Err(error) = renderer.prepare_globe_with_symbols(
                    &mut self.render_context,
                    visible_vgs.as_ref(),
                    &mut self.symbol_cache,
                    color_profile,
                ) {
                    renderer.ui_state.globe_summary = format!("Globe preparation failed: {error}");
                    tracing::error!("Globe preparation failed: {error}");
                    if self.auto_screenshot.is_some() {
                        self.startup_error = Some(format!("Globe preparation failed: {error}"));
                    }
                }
                return;
            }
            renderer.clear_globe_preview();
            renderer.update_raster_view(&self.render_context.scaler);
            renderer.update_selection(&self.render_context.scaler);

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

    /// Panel layout is measured during rendering. Rebuild on any changed axis,
    /// not only the left panel origin, before accepting an automated pick/export.
    fn sync_chart_layout(&mut self) -> bool {
        let Some(renderer) = &self.renderer else {
            return false;
        };
        let rect = renderer.chart_viewport_pixels();
        let (fast_pan, fast_zoom, _) = renderer.fast_view_transform();
        let fast_transform_active =
            fast_pan != (0., 0.) || fast_zoom != 1. || renderer.fast_view_scales().1 != 1.;
        if ![rect.0, rect.1, rect.2, rect.3]
            .iter()
            .all(|v| v.is_finite())
            || rect.2 <= 0.
            || rect.3 <= 0.
            || (self
                .render_context
                .scaler
                .viewport
                .matches_physical_rect(rect)
                && (!self.chart_loaded
                    || fast_transform_active
                    || renderer.geometry_matches_view(&self.render_context.scaler)))
        {
            return false;
        }
        self.update_view();
        self.refresh_visible_selection();
        true
    }

    /// Update the view (rebuilds hit-test symbols, clears declutter grids)
    fn update_view(&mut self) {
        self.update_view_ex(true, false);
        // Sync zoom rebuild level so GPU zoom delta resets to 1.0
        self.zoom_rebuilt_level = self.zoom_level;
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

            window_attrs = ferrite_wgpu::background_test::window_attributes(window_attrs);
            if ferrite_wgpu::background_test::enabled() {
                window_attrs =
                    window_attrs.with_inner_size(winit::dpi::PhysicalSize::new(1920, 1018));
            }
            match event_loop.create_window(window_attrs) {
                Ok(window) => {
                    let window = Arc::new(window);
                    self.window = Some(window.clone());

                    // Create renderer asynchronously
                    match pollster::block_on(WgpuRenderer::new(window.clone())) {
                        Ok(mut renderer) => {
                            renderer
                                .set_globe_sample_count(self.initial_globe_samples)
                                .expect("Startup globe sampling was prevalidated");
                            // Update render context viewport
                            let size = window.inner_size();
                            self.render_context
                                .set_viewport(size.width as f32, size.height as f32);

                            // Initialize UI state
                            renderer.ui_state.verify_dataset_signatures = self.require_signatures;
                            renderer.ui_state.settings.viewing_layers =
                                self.applied_settings.viewing_layers.clone();
                            renderer.ui_state.optional_viewing_layers =
                                ferrite_s101::optional_viewing_layers(&self.pc);
                            renderer.ui_state.settings.display_mode =
                                self.applied_settings.display_mode;
                            renderer.ui_state.version = VERSION.to_string();
                            renderer.ui_state.zoom_level = self.zoom_level;
                            renderer.ui_state.fc_status = self.fc_status.clone();
                            renderer.ui_state.pc_status = self.pc_status.clone();
                            renderer.ui_state.debug_mode = self.debug_mode;
                            renderer.ui_state.settings.interoperability_enabled =
                                self.initial_interoperability_enabled;
                            renderer.ui_state.interoperability_available = self.ic.is_some();
                            renderer.ui_state.interoperability_active =
                                self.ic.is_some() && self.initial_interoperability_enabled;
                            if let Some(ic) = &self.ic {
                                renderer.ui_state.interoperability_status = format!(
                                    "Interoperability {}: {} {} (signature verified)",
                                    if self.initial_interoperability_enabled {
                                        "on"
                                    } else {
                                        "off"
                                    },
                                    ic.catalogue.name,
                                    ic.catalogue.version
                                );
                            }
                            renderer.ui_state.temporal_view =
                                ferrite_render::TemporalView::from_settings(
                                    &self.render_context.settings,
                                );
                            renderer.set_color_profile(&self.current_profile_name);
                            renderer.background_color =
                                lookup_pc_color(&self.pc, "DEPDW", &self.current_profile_name);

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
                                if let Err(error) = prepare_flat_coverage(
                                    self.coverage_inventory.as_deref(),
                                    &mut self.render_context,
                                    renderer.window().inner_size(),
                                    renderer.window().scale_factor(),
                                ) {
                                    self.startup_error =
                                        Some(format!("Coverage preparation failed: {error}"));
                                }
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
                            let coastlines = parse_world_map_coastlines(WORLD_MAP_GEOJSON);
                            renderer.set_world_map(coastlines);
                            renderer.set_world_map_detailed(parse_world_map_coastlines(
                                include_str!("../assets/ne_10m_coastline.geojson"),
                            ));

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

                            renderer.ui_state.globe_preview =
                                std::env::args().any(|a| a == "--globe");
                            let args: Vec<_> = std::env::args().collect();
                            renderer.ui_state.globe_range_factor = args
                                .windows(2)
                                .find(|a| a[0] == "--globe-range-factor")
                                .and_then(|a| a[1].parse::<f64>().ok())
                                .filter(|v| v.is_finite() && *v > 0. && *v <= 10000.)
                                .unwrap_or(1.);
                            renderer.ui_state.globe_tilt_deg = args
                                .windows(2)
                                .find(|a| a[0] == "--globe-tilt")
                                .and_then(|a| a[1].parse::<f64>().ok())
                                .filter(|v| v.is_finite() && (0. ..=70.).contains(v))
                                .unwrap_or(0.);
                            self.renderer = Some(renderer);

                            if !self.pending_auto_s102.is_empty() {
                                let paths = std::mem::take(&mut self.pending_auto_s102);
                                if let Err(e) = self.load_bathymetry(&paths) {
                                    error!("Failed to load S-102: {e:#}");
                                    if self.auto_screenshot.is_some() {
                                        self.startup_error =
                                            Some(format!("Failed to load S-102: {e:#}"));
                                        event_loop.exit();
                                        return;
                                    }
                                    if let Some(r) = &mut self.renderer {
                                        r.ui_state.coverage_info =
                                            Some(format!("S-102 load failed: {e:#}"));
                                    }
                                }
                            }
                            // Auto-load chart if --chart was specified
                            if !self.pending_auto_chart.is_empty() {
                                let paths = std::mem::take(&mut self.pending_auto_chart);
                                info!("Auto-loading {} chart file(s)", paths.len());
                                if let Err(e) = self.load_charts(&paths) {
                                    error!("Failed to auto-load chart(s): {}", e);
                                    if self.auto_screenshot.is_some() {
                                        self.startup_error =
                                            Some(format!("Failed to load chart(s): {e:#}"));
                                        event_loop.exit();
                                        return;
                                    }
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
        // A requested automatic capture owns its view until export and exit.
        // OS navigation input must not alter the prescribed comparison condition.
        // Resize/DPI/redraw and close events still follow the normal lifecycle.
        if self.auto_screenshot.is_some()
            && matches!(
                &event,
                WindowEvent::MouseWheel { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::CursorMoved { .. }
                    | WindowEvent::KeyboardInput { .. }
                    | WindowEvent::PinchGesture { .. }
                    | WindowEvent::PanGesture { .. }
                    | WindowEvent::DoubleTapGesture { .. }
                    | WindowEvent::Touch(_)
                    | WindowEvent::Ime(_)
            )
        {
            return;
        }

        // F12 toggles profiling. Tab/Shift+Tab remain available for egui keyboard focus.
        if let WindowEvent::KeyboardInput {
            event:
                winit::event::KeyEvent {
                    physical_key: winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::F12),
                    state: winit::event::ElementState::Pressed,
                    repeat: false,
                    ..
                },
            ..
        } = &event
        {
            self.debug_mode = !self.debug_mode;
            self.process_stats.reset();
            self.frame_times.clear();
            if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.debug_mode = self.debug_mode;
                renderer.set_profiling_enabled(self.debug_mode);
                renderer.ui_state.debug_cpu_usage = None;
                renderer.ui_state.debug_memory_mb = None;
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
            WindowEvent::ModifiersChanged(modifiers) => {
                self.navigation_modifiers = modifiers.state();
            }
            WindowEvent::Focused(false) => {
                self.touch_navigation.reset();
                self.pinch_ownership.reset();
                self.pan_gesture_ownership.reset();
                self.navigation_modifiers = winit::keyboard::ModifiersState::empty();
                self.is_dragging = false;
                self.pan_velocity = (0., 0.);
                self.recent_positions.clear();
                if self.pan_rebuild_phase != 0 || self.zoom_rebuild_phase != 0 {
                    self.update_view();
                }
                self.pan_rebuild_phase = 0;
                self.zoom_animating = false;
                self.zoom_target = self.zoom_level;
                self.zoom_rebuild_phase = 0;
            }
            WindowEvent::Touch(touch) => {
                let point = (touch.location.x, touch.location.y);
                let density = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.);
                let chart_allowed = !egui_consumed && self.chart_contains(point);
                if touch.phase == winit::event::TouchPhase::Started
                    && !self.touch_navigation.active()
                    && chart_allowed
                {
                    self.pan_velocity = (0., 0.);
                    self.is_dragging = false;
                    self.recent_positions.clear();
                    self.zoom_animating = false;
                    self.zoom_target = self.zoom_level;
                    self.update_view();
                    self.pan_rebuild_phase = 0;
                }
                let action = self.touch_navigation.event(
                    (touch.device_id, touch.id),
                    touch.phase,
                    point,
                    chart_allowed,
                    density,
                );
                match action {
                    Some(navigation::TouchAction::Motion(motion)) => {
                        self.apply_gesture_motion(motion)
                    }
                    Some(navigation::TouchAction::Tap(point)) => {
                        self.chart_click(point);
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    None => {}
                }
            }
            WindowEvent::PinchGesture { delta, phase, .. } => {
                let allowed = !egui_consumed && self.chart_contains(self.mouse_pos);
                if self.pinch_ownership.event(phase, allowed) {
                    if let Some(motion) = navigation::pinch_motion(self.mouse_pos, delta) {
                        self.apply_gesture_motion(motion);
                    }
                }
            }
            WindowEvent::PanGesture { delta, phase, .. } => {
                let allowed = !egui_consumed && self.chart_contains(self.mouse_pos);
                if self.pan_gesture_ownership.event(phase, allowed) {
                    let from = self.mouse_pos;
                    self.apply_gesture_motion(navigation::GestureMotion {
                        from,
                        to: (from.0 + delta.x as f64, from.1 + delta.y as f64),
                        ratio: 1.,
                    });
                }
            }
            WindowEvent::CloseRequested => {
                #[cfg(debug_assertions)]
                info!("Window close requested");
                // Flush profiler report before exit
                if let Some(renderer) = &mut self.renderer {
                    renderer.flush_profiler();
                }
                event_loop.exit();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // Native DPI changes affect fixed-size strokes/symbols and SCAMIN
                // even when the physical window dimensions stay the same.
                self.update_view();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Resized(physical_size) => {
                if let Some(pending)=self.coverage_lifecycle_resize.as_mut() {
                    if physical_size.width>0 && physical_size.height>0 {pending.observed=Some(physical_size);}
                }
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
                if self.coverage_lifecycle_resize.is_some() {
                    match self.resume_coverage_lifecycle_resize() {
                        Ok(true)=>{if let Some(w)=&self.window {w.request_redraw();}},
                        Ok(false)=>{},
                        Err(e)=>{self.startup_error=Some(format!("Coverage resize lifecycle failed: {e:#}"));event_loop.exit();},
                    }
                    return;
                }
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
                    self.pan_camera_by([self.pan_velocity.0 * dt, self.pan_velocity.1 * dt]);

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
                        // pan_camera_by already synchronized the affine fast path.
                    }
                }

                // Animated zoom: smoothly interpolate toward zoom_target
                if self.zoom_animating {
                    let previous_zoom = self.zoom_level;
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
                        self.zoom_level = self
                            .zoom_level
                            .clamp(navigation::MIN_ZOOM, navigation::MAX_ZOOM);
                    }

                    // The CPU camera and GPU fast path share the same final view.
                    // The anchor solver accounts for latitude-dependent horizontal
                    // scale; zeroing pan and solving once caused the old end jump.
                    let (cursor_sx, cursor_sy) = self.zoom_cursor_screen;
                    let viewport = self.render_context.scaler.viewport;
                    if self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.ui_state.globe_preview)
                    {
                        let moved = self.renderer.as_mut().unwrap().move_globe_anchor(
                            WorldPoint::new(self.zoom_anchor_world.0, self.zoom_anchor_world.1),
                            ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy),
                            self.zoom_level / previous_zoom,
                        );
                        if moved {
                            self.sync_globe_controller();
                            self.update_view();
                        } else {
                            self.zoom_level = previous_zoom;
                            self.zoom_target = previous_zoom;
                            self.zoom_animating = false;
                        }
                    } else if let Some(bounds) = ferrite_render::anchored_zoom_bounds_projected(
                        self.render_context.scaler.projection(),
                        self.bounds,
                        viewport,
                        self.zoom_level,
                        WorldPoint::new(self.zoom_anchor_world.0, self.zoom_anchor_world.1),
                        ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy),
                        self.render_context.scaler.geo_bounds.center().y,
                    ) {
                        if let Some(pan) = self
                            .render_context
                            .scaler
                            .projection()
                            .pan_between(self.bounds, bounds)
                        {
                            self.pan_offset = (pan[0], pan[1]);
                        }
                        self.render_context.zoom_to_fit(bounds);
                        if let Some(renderer) = &mut self.renderer {
                            if !renderer.set_gpu_view_scaler(&self.render_context.scaler) {
                                // View-dependent placement/parent visibility cannot
                                // be obtained by scaling old screen anchors.
                                self.update_view_ex(false, true);
                            } else {
                                renderer.ui_state.zoom_level = self.zoom_level;
                            }
                        }
                    } else {
                        // Preserve the last valid camera if an extreme or invalid
                        // anchor cannot be solved; do not commit a zoom-only jump.
                        self.zoom_level = previous_zoom;
                        self.zoom_target = previous_zoom;
                        self.zoom_animating = false;
                    }

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
                    if self.startup_error.is_some() {
                        event_loop.exit();
                        return;
                    }
                }

                let candidate = self.renderer.as_mut().and_then(|renderer| {
                    renderer
                        .ui_state
                        .selection_requested
                        .take()
                        .and_then(|index| {
                            renderer.ui_state.selection_candidates.get(index).cloned()
                        })
                });
                if let Some(candidate) = candidate {
                    self.select_feature(Some(candidate));
                }
                if let Some(r) = &mut self.renderer {
                    if std::mem::take(&mut r.ui_state.fit_globe_requested) {
                        if let Some(d) = r.globe_preview_diagnostics() {
                            let factor = if r.ui_state.globe_range_factor > 0. {
                                r.ui_state.globe_range_factor
                            } else {
                                1.
                            };
                            r.ui_state.globe_range_factor = 20_000_000. / (d.range_m / factor);
                            if let Some(mut pose) = r.globe_pose() {
                                pose.range_m = 20_000_000.;
                                pose.tilt_deg = 0.;
                                r.ui_state.globe_pose = Some(pose);
                            }
                            r.ui_state.globe_tilt_deg = 0.;
                            r.ui_state.view_mode_changed = true;
                        }
                    }
                }
                let view_changed = self
                    .renderer
                    .as_mut()
                    .is_some_and(|r| std::mem::take(&mut r.ui_state.view_mode_changed));
                if view_changed {
                    self.recent_positions.clear();
                    self.zoom_animating = false;
                    self.pan_velocity = (0., 0.);
                    self.select_feature(None);
                    if let Some(r) = &mut self.renderer {
                        r.ui_state.selection_candidates.clear();
                        r.ui_state.selection_requested = None;
                    }
                    self.update_view();
                }
                let temporal_request = self.renderer.as_mut().and_then(|renderer| {
                    if std::mem::take(&mut renderer.ui_state.temporal_changed) {
                        Some(renderer.ui_state.temporal_view.clone())
                    } else {
                        None
                    }
                });
                if let Some(view) = temporal_request {
                    match view.apply(&mut self.render_context.settings) {
                        Ok(()) => {
                            self.update_view();
                            self.refresh_visible_selection();
                        }
                        Err(error) => {
                            tracing::warn!("Viewing-time change rejected: {error}");
                            if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.temporal_view =
                                    ferrite_render::TemporalView::from_settings(
                                        &self.render_context.settings,
                                    );
                            }
                        }
                    }
                }
                let (open_exchange, close_requested) = self
                    .renderer
                    .as_mut()
                    .map(|r| {
                        (
                            std::mem::take(&mut r.ui_state.open_exchange_requested),
                            std::mem::take(&mut r.ui_state.close_requested),
                        )
                    })
                    .unwrap_or((false, false));
                if close_requested {
                    if let Some(r) = &mut self.renderer {
                        r.flush_profiler();
                    }
                    event_loop.exit();
                    return;
                }
                if open_exchange {
                    if let Some(folder) = rfd::FileDialog::new()
                        .set_title("Open S-101 / S-102 exchange set folder")
                        .pick_folder()
                    {
                        match dataset_discovery::discover_exchange_folder(&folder) {
                            Ok((charts, rasters)) => {
                                if let Some(r) = &mut self.renderer {
                                    r.ui_state.notice = None;
                                }
                                let result = self
                                    .load_bathymetry(&rasters)
                                    .and_then(|_| self.load_charts(&charts));
                                if let Err(error) = result {
                                    error!("Exchange set load failed: {error:#}");
                                    if let Some(r) = &mut self.renderer {
                                        r.ui_state.notice =
                                            Some(format!("Could not open exchange set: {error:#}"));
                                    }
                                }
                            }
                            Err(error) => {
                                if let Some(r) = &mut self.renderer {
                                    r.ui_state.notice =
                                        Some(format!("Could not open exchange set: {error:#}"));
                                }
                            }
                        }
                    }
                }
                let open_catalogue_set = self
                    .renderer
                    .as_mut()
                    .is_some_and(|r| std::mem::take(&mut r.ui_state.open_catalogue_set_requested));
                if open_catalogue_set {
                    if let Some(folder) = rfd::FileDialog::new()
                        .set_title("Open catalogue version folder containing FC and PC")
                        .pick_folder()
                    {
                        let candidate = (|| -> Result<_> {
                            anyhow::ensure!(
                                self.loading_state.is_none(),
                                "Wait for dataset loading to finish before changing catalogues"
                            );
                            let mut files = fs::read_dir(folder.join("FC"))?
                                .map(|entry| entry.map(|e| e.path()))
                                .collect::<std::io::Result<Vec<_>>>()?;
                            files.retain(|p| {
                                p.is_file()
                                    && !p.file_name().unwrap().to_string_lossy().starts_with("._")
                                    && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml"))
                            });
                            anyhow::ensure!(
                                files.len() == 1,
                                "Catalogue set FC folder must contain exactly one catalogue XML"
                            );
                            let fc_path = &files[0];
                            let pc_path = folder.join("PC");
                            let fc = load_feature_catalogue(fc_path)?;
                            let pc = load_portrayal_catalogue(&pc_path)?;
                            ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
                            for cell in &self.cells {
                                ferrite_s101::validate_dataset_catalogues(
                                    &cell.dsid,
                                    &fc,
                                    &pc.product_id,
                                    &pc.version,
                                )?;
                            }
                            ferrite_s101::viewing_groups_for_layers(
                                &pc,
                                self.applied_settings
                                    .viewing_layers
                                    .iter()
                                    .map(String::as_str),
                            )?;
                            Ok((fc, pc))
                        })();
                        match candidate {
                            Ok((fc, pc)) => {
                                self.fc_status = validate_fc(&fc, &fc.source_path);
                                self.pc_status = validate_pc(&pc, &pc.root_path);
                                self.symbol_cache = SymbolCache::new_with_sources(
                                    pc.root_path.join("Symbols"),
                                    pc.sources(),
                                );
                                self.fc = Arc::new(fc);
                                self.pc = Arc::new(pc);
                                if let Some(r) = &mut self.renderer {
                                    r.clear_symbol_textures();
                                    r.ui_state.fc_status = self.fc_status.clone();
                                    r.ui_state.pc_status = self.pc_status.clone();
                                    r.ui_state.optional_viewing_layers =
                                        ferrite_s101::optional_viewing_layers(&self.pc);
                                    r.ui_state.notice = None;
                                }
                                if self.chart_loaded {
                                    let codes = self.fc.feature_type_codes();
                                    for cell in &mut self.cells {
                                        cell.normalize_feature_codes(&codes);
                                    }
                                    if let Err(error) = self.regenerate_instructions() {
                                        if let Some(r) = &mut self.renderer {
                                            r.ui_state.notice = Some(format!(
                                                "Catalogue set portrayal failed: {error:#}"
                                            ));
                                        }
                                    }
                                }
                                self.update_view();
                            }
                            Err(error) => {
                                if let Some(r) = &mut self.renderer {
                                    r.ui_state.notice =
                                        Some(format!("Could not open catalogue set: {error:#}"));
                                }
                            }
                        }
                    }
                }
                // The toolbar edits the same mode used by F12 and --debug.
                if let Some(renderer) = &mut self.renderer {
                    if self.debug_mode != renderer.ui_state.debug_mode {
                        self.debug_mode = renderer.ui_state.debug_mode;
                        self.process_stats.reset();
                        self.frame_times.clear();
                        renderer.set_profiling_enabled(self.debug_mode);
                        renderer.ui_state.debug_cpu_usage = None;
                        renderer.ui_state.debug_memory_mb = None;
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
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
                        .add_filter("S-101 / S-102", &["000", "h5", "H5"])
                        .add_filter(
                            "S-101 updates / reissues (select numeric extension)",
                            &["*"],
                        )
                        .set_title("Open S-101 charts or S-102 bathymetry")
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
                        if let Some(r) = &mut self.renderer {
                            r.ui_state.notice = None;
                        }
                        let (raster_paths, chart_paths): (Vec<_>, Vec<_>) =
                            paths.into_iter().partition(|p| {
                                p.extension().is_some_and(|e| e.eq_ignore_ascii_case("h5"))
                            });
                        if !raster_paths.is_empty() {
                            if let Err(e) = self.load_bathymetry(&raster_paths) {
                                error!("S-102 load failed: {e:#}");
                                if let Some(r) = &mut self.renderer {
                                    r.ui_state.notice = Some(format!("S-102 load failed: {e:#}"));
                                }
                            }
                        }
                        if let Err(e) = self.load_charts(&chart_paths) {
                            error!("Failed to load chart(s): {}", e);
                            if let Some(r) = &mut self.renderer {
                                r.ui_state.notice = Some(format!("S-101 load failed: {e:#}"));
                            }
                        }
                    }
                }

                // Handle Feature Catalogue open request
                if open_fc {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Feature Catalogue XML", &["xml"])
                        .set_title("Open Feature Catalogue XML")
                        .pick_file()
                    {
                        let candidate = load_feature_catalogue(&path).and_then(|new_fc| {
                            anyhow::ensure!(
                                self.loading_state.is_none(),
                                "Wait for dataset loading to finish before changing catalogues"
                            );
                            ferrite_s101::validate_catalogue_pair(&new_fc, &self.pc)?;
                            for cell in &self.cells {
                                ferrite_s101::validate_dataset_catalogues(
                                    &cell.dsid,
                                    &new_fc,
                                    &self.pc.product_id,
                                    &self.pc.version,
                                )?;
                            }
                            Ok(new_fc)
                        });
                        match candidate {
                            Ok(new_fc) => {
                                info!("Loaded FC: {} v{}", new_fc.product_id, new_fc.version);
                                self.fc_status = validate_fc(&new_fc, &path);
                                self.fc = Arc::new(new_fc);
                                if let Some(renderer) = &mut self.renderer {
                                    renderer.ui_state.fc_status = self.fc_status.clone();
                                }
                                if self.chart_loaded {
                                    let codes = self.fc.feature_type_codes();
                                    for cell in &mut self.cells {
                                        cell.normalize_feature_codes(&codes);
                                    }
                                    if let Err(error) = self.regenerate_instructions() {
                                        error!("FC portrayal rebuild failed: {error:#}");
                                        if let Some(r) = &mut self.renderer {
                                            r.ui_state.notice = Some(format!(
                                                "Feature catalogue portrayal failed: {error:#}"
                                            ));
                                        }
                                    }
                                    self.update_view();
                                }
                            }
                            Err(e) => {
                                error!("Failed to load Feature Catalogue: {}", e);
                                if let Some(r) = &mut self.renderer {
                                    r.ui_state.notice =
                                        Some(format!("Could not open feature catalogue: {e:#}"));
                                }
                            }
                        }
                    }
                }

                // Handle Portrayal Catalogue open request
                if open_pc {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Open Portrayal Catalogue Directory")
                        .pick_folder()
                    {
                        let candidate = load_portrayal_catalogue(&path).and_then(|new_pc| {
                            anyhow::ensure!(
                                self.loading_state.is_none(),
                                "Wait for dataset loading to finish before changing catalogues"
                            );
                            ferrite_s101::validate_catalogue_pair(&self.fc, &new_pc)?;
                            for cell in &self.cells {
                                ferrite_s101::validate_dataset_catalogues(
                                    &cell.dsid,
                                    &self.fc,
                                    &new_pc.product_id,
                                    &new_pc.version,
                                )?;
                            }
                            ferrite_s101::viewing_groups_for_layers(
                                &new_pc,
                                self.applied_settings
                                    .viewing_layers
                                    .iter()
                                    .map(String::as_str),
                            )?;
                            Ok(new_pc)
                        });
                        match candidate {
                            Ok(new_pc) => {
                                info!("Loaded PC: {} v{}", new_pc.product_id, new_pc.version);
                                self.pc_status = validate_pc(&new_pc, &path);

                                // Reload symbol cache with new PC
                                let symbols_path = path.join("Symbols");
                                self.symbol_cache =
                                    SymbolCache::new_with_sources(&symbols_path, new_pc.sources());
                                self.pc = Arc::new(new_pc);

                                if let Some(renderer) = &mut self.renderer {
                                    renderer.clear_symbol_textures();
                                    renderer.ui_state.pc_status = self.pc_status.clone();
                                    renderer.ui_state.optional_viewing_layers =
                                        ferrite_s101::optional_viewing_layers(&self.pc);
                                }
                                if self.chart_loaded {
                                    if let Err(error) = self.regenerate_instructions() {
                                        error!("PC portrayal rebuild failed: {error:#}");
                                        if let Some(r) = &mut self.renderer {
                                            r.ui_state.notice = Some(format!(
                                                "Portrayal catalogue rebuild failed: {error:#}"
                                            ));
                                        }
                                    }
                                    self.update_view();
                                }
                            }
                            Err(e) => {
                                error!("Failed to load Portrayal Catalogue: {}", e);
                                if let Some(r) = &mut self.renderer {
                                    r.ui_state.notice =
                                        Some(format!("Could not open portrayal catalogue: {e:#}"));
                                }
                            }
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

                for (requested, steps) in [(zoom_in, 1.), (zoom_out, -1.)] {
                    if !requested {
                        continue;
                    }
                    let target = navigation::zoom_by_steps(self.zoom_level, steps, 1.5)
                        .unwrap_or(self.zoom_level);
                    let centre = self.render_context.scaler.viewport.center();
                    if !self.globe_gesture(navigation::GestureMotion {
                        from: (centre.x as f64, centre.y as f64),
                        to: (centre.x as f64, centre.y as f64),
                        ratio: target / self.zoom_level,
                    }) {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.reset_pan_offset();
                        }
                        self.zoom_level = target;
                        self.zoom_target = target;
                        self.zoom_animating = false;
                        self.update_view();
                    }
                }

                if reset_view {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.ui_state.globe_pose = None;
                        renderer.ui_state.globe_range_factor = 1.;
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
                    if let Err(e) = self.recolor_bathymetry() {
                        error!("S-102 portrayal failed: {e:#}");
                    }
                    // Force re-render with new colors
                    if self.chart_loaded {
                        self.update_view();
                    }
                }

                // Validate the complete candidate context before Lua, bathymetry, or GPU changes.
                if settings_change.is_some() {
                    let result = self.apply_portrayal_settings();
                    match result {
                        Ok(()) => {
                            if let Some(renderer) = &mut self.renderer {
                                self.applied_settings = renderer.settings().clone();
                                renderer.precompute_triangulations(&self.render_context);
                            }
                        }
                        Err(error) => {
                            let message = format!("Settings were not applied: {error:#}. Previous portrayal retained.");
                            error!("{message}");
                            if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.settings = self.applied_settings.clone();
                                renderer.ui_state.notice = Some(message);
                            }
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
                        let debug_update_interval = process_stats::UPDATE_INTERVAL;
                        let should_update_stats =
                            now.duration_since(self.last_debug_update) >= debug_update_interval;

                        if should_update_stats {
                            let stats_elapsed = now.duration_since(self.last_debug_update);
                            self.last_debug_update = now;

                            // Calculate FPS from accumulated frame times
                            if self.frame_times.len() >= 2 {
                                let oldest = self.frame_times.front().unwrap();
                                let elapsed = now.duration_since(*oldest).as_secs_f32();
                                renderer.ui_state.debug_fps =
                                    (self.frame_times.len() - 1) as f32 / elapsed;
                            }

                            let sample = self.process_stats.sample();
                            renderer.ui_state.debug_cpu_usage = sample.cpu_percent;
                            renderer.ui_state.debug_memory_mb = sample.resident_mib;
                            if let Some(path) = &self.debug_stats_audit {
                                self.debug_stats_samples.push(serde_json::json!({
                                    "cpu_percent": sample.cpu_percent,
                                    "resident_mib": sample.resident_mib,
                                    "logical_cpus": self.process_stats.logical_cpus(),
                                    "updates": self.process_stats.updates(),
                                    "elapsed_since_previous_ms": stats_elapsed.as_millis(),
                                    "native_window_visible": self.window.as_ref().and_then(|w| w.is_visible()),
                                    "native_window_has_focus": self.window.as_ref().is_some_and(|w| w.has_focus()),
                                }));
                                if self.debug_stats_samples.len() >= 3 {
                                    if let Err(error) = fs::write(
                                        path,
                                        serde_json::to_vec_pretty(&self.debug_stats_samples)
                                            .unwrap(),
                                    ) {
                                        self.startup_error =
                                            Some(format!("Debug statistics audit failed: {error}"));
                                    }
                                    event_loop.exit();
                                    return;
                                }
                            }

                            // Instruction and symbol counts
                            renderer.ui_state.debug_instruction_count =
                                self.render_context.instruction_count();
                            renderer.ui_state.debug_symbol_count = self.rendered_symbols.len();
                        }
                    }
                }

                // Camera animations and inertia also change the geographic
                // position beneath a stationary cursor; refresh every frame.
                if let Some(renderer) = &mut self.renderer {
                    let p = self.render_context.scaler.screen_to_world(
                        ferrite_render::ScreenPoint::new(
                            self.mouse_pos.0 as f32,
                            self.mouse_pos.1 as f32,
                        ),
                    );
                    let p = if renderer.ui_state.globe_preview {
                        renderer
                            .globe_world_at(ferrite_render::ScreenPoint::new(
                                self.mouse_pos.0 as f32,
                                self.mouse_pos.1 as f32,
                            ))
                            .unwrap_or(ferrite_render::WorldPoint::new(f64::NAN, f64::NAN))
                    } else {
                        p
                    };
                    renderer.set_cursor_world(p.x, p.y);
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

                if self.startup_error.is_some() {
                    event_loop.exit();
                    return;
                }
                if self.sync_chart_layout() {
                    if self.frames_since_loaded.is_some() {
                        self.frames_since_loaded = Some(0);
                    }
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return;
                }

                // Export/picking must wait for the interactive fast transform to be
                // reconciled with rebuilt geometry; frame count alone is insufficient.
                let view_settled = !self.zoom_animating
                    && self.zoom_rebuild_phase == 0
                    && self.pan_rebuild_phase == 0
                    && self.renderer.as_ref().is_none_or(|r| {
                        let (pan, zoom, _) = r.fast_view_transform();
                        pan == (0., 0.)
                            && zoom == 1.
                            && r.geometry_matches_view(&self.render_context.scaler)
                    });
                if self.frames_since_loaded.is_some() && !view_settled {
                    self.frames_since_loaded = Some(0);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                // Auto-screenshot: wait a few frames after load for rendering to stabilize
                if let Some(count) = &mut self.frames_since_loaded {
                    *count += 1;
                    if *count >= 5 {
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_CANCELLATION_AUDIT") {
                            if let Err(error) = self.audit_root_cancellation(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("Cancellation audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_COVERAGE_CACHE_LIFECYCLE_AUDIT") {
                            std::env::remove_var("FERRITE_COVERAGE_CACHE_LIFECYCLE_AUDIT");
                            if let Err(error) = self.audit_coverage_cache_lifecycle(&PathBuf::from(path)) {
                                self.startup_error = Some(format!("Coverage lifecycle audit failed: {error:#}"));
                                event_loop.exit(); return;
                            }
                            if self.coverage_lifecycle_resize.is_some() {return;}
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_PUBLICATION_AUDIT") {
                            if let Err(error) = self.audit_root_publication(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("Publication audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) =
                            std::env::var_os("FERRITE_ROOT_COVERAGE_GEOMETRY_EPOCH_AUDIT")
                        {
                            if let Err(error) =
                                self.audit_root_coverage_geometry_epoch(&PathBuf::from(path))
                            {
                                self.startup_error = Some(format!(
                                    "Coverage geometry epoch audit failed: {error:#}"
                                ));
                                event_loop.exit();
                                return;
                            }
                            event_loop.exit();
                            return;
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_RESOURCE_EPOCH_AUDIT") {
                            if let Err(error) = self.audit_root_resource_epoch(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Resource epoch audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_RETAINED_PASSING_AUDIT")
                        {
                            if let Err(error) =
                                self.audit_root_retained_passing(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Retained passing audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_SOURCE_TOPOLOGY_AUDIT") {
                            if let Err(error) =
                                self.audit_root_source_topology(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Source topology audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = self.animation_audit.take() {
                            if let Err(error) = self.audit_animation(&path) {
                                self.startup_error =
                                    Some(format!("Animation audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = self.ic_transition_audit.take() {
                            if let Err(error) = self.audit_interoperability_transitions(&path) {
                                self.startup_error =
                                    Some(format!("IC transition audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = self.ic_audit.take() {
                            if let Err(error) = self.audit_interoperability(&path) {
                                self.startup_error = Some(format!("IC audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = self.bathymetry_audit.take() {
                            if let Err(error) = self.audit_bathymetry(&path) {
                                self.startup_error =
                                    Some(format!("Bathymetry audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = self.selection_audit.take() {
                            if let Err(error) = self.audit_selection(&path) {
                                self.startup_error =
                                    Some(format!("Selection audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = self.auto_screenshot.take() {
                            info!("Auto-screenshot: saving to {}", path.display());
                            if let Some(renderer) = &mut self.renderer {
                                if renderer.ui_state.globe_preview {
                                    let Some(diagnostics) = renderer.globe_preview_diagnostics()
                                    else {
                                        self.startup_error = Some(
                                            "Globe camera did not prepare successfully".into(),
                                        );
                                        event_loop.exit();
                                        return;
                                    };
                                    let audit_path = path.with_extension("globe.json");
                                    if let Err(error) = std::fs::write(
                                        &audit_path,
                                        serde_json::to_vec_pretty(&diagnostics.as_json()).unwrap(),
                                    ) {
                                        self.startup_error =
                                            Some(format!("Globe audit export failed: {error}"));
                                        event_loop.exit();
                                        return;
                                    }
                                }
                                match renderer.save_screenshot(&path) {
                                    Ok(_) => info!("Screenshot saved successfully"),
                                    Err(e) => {
                                        error!("Screenshot failed: {}", e);
                                        self.startup_error =
                                            Some(format!("Screenshot failed: {e:#}"));
                                    }
                                }
                            }
                            if let Some(path) = self.portrayal_audit.take() {
                                if let Err(error) = self.audit_portrayal(&path) {
                                    self.startup_error =
                                        Some(format!("Portrayal audit failed: {error:#}"));
                                    event_loop.exit();
                                    return;
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

                // Handle panning when dragging (only if egui didn't consume)
                if !egui_consumed && self.is_dragging && !self.touch_navigation.active() {
                    let dx = new_pos.0 - self.mouse_pos.0;
                    let dy = new_pos.1 - self.mouse_pos.1;

                    // Track world-space pan offset for final calculation
                    let world_dx = -dx / self.render_context.scaler.scale_x();
                    let world_dy = dy / self.render_context.scaler.scale_y();
                    let globe = self.globe_gesture(navigation::GestureMotion {
                        from: self.mouse_pos,
                        to: new_pos,
                        ratio: 1.,
                    });
                    if !globe {
                        self.pan_camera_by([world_dx, world_dy]);
                    }

                    // Track recent positions for velocity calculation (keep last 100ms worth)
                    if !globe {
                        self.recent_positions.push((new_pos, now));
                    }
                    self.recent_positions
                        .retain(|(_, t)| now.duration_since(*t).as_millis() < 100);

                    // Stop any existing inertia when actively dragging
                    self.pan_velocity = (0.0, 0.0);
                    self.pan_rebuild_phase = 0;

                    // pan_camera_by already synchronized CPU and GPU navigation.
                }

                // Read the cursor coordinate after applying this event's camera motion
                if let Some(renderer) = &mut self.renderer {
                    let screen_pt =
                        ferrite_render::ScreenPoint::new(new_pos.0 as f32, new_pos.1 as f32);
                    let world = self.render_context.scaler.screen_to_world(screen_pt);
                    let world = if renderer.ui_state.globe_preview {
                        renderer
                            .globe_world_at(screen_pt)
                            .unwrap_or(ferrite_render::WorldPoint::new(f64::NAN, f64::NAN))
                    } else {
                        world
                    };
                    renderer.set_cursor_world(world.x, world.y);
                    renderer.set_cursor_screen(new_pos.0 as f32, new_pos.1 as f32);
                }

                self.mouse_pos = new_pos;

                // Request redraw for cursor updates and drag rendering
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. }
                if !egui_consumed && !self.touch_navigation.active() =>
            {
                if let winit::event::MouseScrollDelta::PixelDelta(position) = delta {
                    if !self.navigation_modifiers.control_key()
                        && !self.navigation_modifiers.super_key()
                    {
                        let from = self.mouse_pos;
                        if self.chart_contains(from) {
                            self.apply_gesture_motion(navigation::GestureMotion {
                                from,
                                to: (from.0 + position.x, from.1 + position.y),
                                ratio: 1.,
                            });
                        }
                        return;
                    }
                }
                let current_target = if self.zoom_animating {
                    self.zoom_target
                } else {
                    self.zoom_level
                };
                let native_scale = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.);
                let Some(target) =
                    navigation::scroll_zoom_target(current_target, delta, native_scale)
                else {
                    warn!("Ignoring invalid chart zoom input");
                    return;
                };
                if target == current_target {
                    return;
                }
                // Pan/inertia can leave the CPU scaler behind the visible map.
                // Settle that transform before reading a fresh scroll anchor.
                if self.pan_rebuild_phase != 0 || self.is_dragging || self.pan_velocity != (0., 0.)
                {
                    self.pan_velocity = (0., 0.);
                    self.pan_rebuild_phase = 0;
                    self.update_view();
                }
                self.zoom_target = target;
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
                let anchor = if self
                    .renderer
                    .as_ref()
                    .is_some_and(|r| r.ui_state.globe_preview)
                {
                    let Some(a) = self
                        .renderer
                        .as_ref()
                        .unwrap()
                        .globe_world_at(ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy))
                    else {
                        self.zoom_animating = false;
                        self.zoom_target = self.zoom_level;
                        return;
                    };
                    a
                } else {
                    anchor
                };
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
            } => {
                let chart_pointer = !egui_consumed
                    && !self.touch_navigation.active()
                    && self.chart_contains(self.mouse_pos)
                    && !self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.egui_wants_pointer());
                match state {
                    ElementState::Pressed if chart_pointer => {
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
                    ElementState::Pressed => {
                        self.is_dragging = false;
                        self.recent_positions.clear();
                    }
                    ElementState::Released => {
                        // A chart drag must end even if its release is consumed
                        // by a toolbar, dialog or details pane. Otherwise later
                        // hover movements silently pan the chart.

                        let was_dragging = self.is_dragging;
                        self.is_dragging = false;

                        let drag_dist = ((self.mouse_pos.0 - self.drag_start.0).powi(2)
                            + (self.mouse_pos.1 - self.drag_start.1).powi(2))
                        .sqrt();

                        // Calculate velocity for inertia from recent positions
                        let mut inertia_applied = false;
                        if was_dragging
                            && chart_pointer
                            && drag_dist >= 5.0
                            && self.recent_positions.len() >= 2
                        {
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

                        if was_dragging && chart_pointer && drag_dist < 5.0 && !has_inertia {
                            // Check if egui wants the pointer (click is on UI)
                            let egui_wants = self
                                .renderer
                                .as_ref()
                                .is_some_and(|r| r.egui_wants_pointer());

                            // Skip chart/plugin handling if click was on UI or no chart loaded
                            if !egui_wants && self.chart_loaded {
                                self.chart_click(self.mouse_pos);
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
                            renderer.ui_state.globe_pose = None;
                            renderer.ui_state.globe_range_factor = 1.;
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
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(pending)=self.coverage_lifecycle_resize.as_mut() {
            let now=std::time::Instant::now();
            if now>=pending.next_poll {
                if let Some(w)=&self.window {w.request_redraw();}
                pending.next_poll=now+std::time::Duration::from_millis(100);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(pending.next_poll));return;
        }
        let now = std::time::Instant::now();
        let debug_wake =
            process_stats::debug_deadline(self.debug_mode, self.last_debug_update, now);
        if self.debug_mode
            && now.duration_since(self.last_debug_update) >= process_stats::UPDATE_INTERVAL
        {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if !self.chart_loaded
            || self.loading_state.is_some()
            || !self.render_context.has_live_temporal_conditions()
        {
            self.next_temporal_wake = None;
            event_loop.set_control_flow(
                debug_wake
                    .map(ControlFlow::WaitUntil)
                    .unwrap_or(ControlFlow::Wait),
            );
            return;
        }
        let now = std::time::Instant::now();
        if self
            .next_temporal_wake
            .is_some_and(|deadline| deadline <= now)
        {
            self.next_temporal_wake = None;
            let changed = self
                .renderer
                .as_ref()
                .is_some_and(|r| r.temporal_conditions_changed(&self.render_context));
            if changed {
                self.update_view();
                self.refresh_visible_selection();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
        if self.next_temporal_wake.is_none() {
            if let Some(deadline) = self.render_context.next_live_temporal_change() {
                let delta = (deadline - chrono::Utc::now().fixed_offset())
                    .to_std()
                    .unwrap_or_default()
                    .min(std::time::Duration::from_secs(60));
                self.next_temporal_wake = std::time::Instant::now()
                    .checked_add(delta.max(std::time::Duration::from_nanos(1)));
            }
        }
        // Also detect backward wall-clock changes after all finite intervals expired.
        if self.next_temporal_wake.is_none() {
            self.next_temporal_wake =
                std::time::Instant::now().checked_add(std::time::Duration::from_secs(60));
        }
        event_loop.set_control_flow(
            [self.next_temporal_wake, debug_wake]
                .into_iter()
                .flatten()
                .min()
                .map(ControlFlow::WaitUntil)
                .unwrap_or(ControlFlow::Wait),
        );
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

        // stderr remains available to redirected CLI/audit callers on Windows.
        eprintln!("{}", msg);

        std::process::exit(1);
    }
}

fn run_app() -> Result<()> {
    let config = AppConfig::from_args();
    anyhow::ensure!(
        config.viewing_date.is_none() || config.viewing_instant.is_none(),
        "choose --viewing-date or --viewing-instant"
    );
    let local_time_offset = ferrite_kernel::parse_local_time_offset(
        config.local_time_offset.as_deref().unwrap_or("Z"),
    )?;
    if let Some(instant) = &config.viewing_instant {
        ferrite_kernel::parse_viewing_instant(instant)?;
    }
    if let Some(date) = &config.viewing_date {
        ferrite_kernel::parse_viewing_date(date)?;
    }

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

    anyhow::ensure!(
        config.ic_path.is_some()
            || (config.ic_trust_root.is_none()
                && config.ic_audit.is_none()
                && config.ic_transition_audit.is_none()
                && config.initial_interoperability_enabled),
        "IC options require --ic"
    );
    anyhow::ensure!(
        config.portrayal_audit.is_none() || config.auto_screenshot.is_some(),
        "--portrayal-audit requires --screenshot"
    );
    let initial_display_mode = match config.initial_display_mode.as_deref() {
        None | Some("standard") => DisplayMode::Standard,
        Some("base") => DisplayMode::Base,
        Some("all") => DisplayMode::All,
        Some(value) => {
            anyhow::bail!("--display-mode requires base, standard or all; got {value:?}")
        }
    };
    let initial_globe_samples = match config.initial_globe_samples.as_deref() {
        None | Some("4") => 4,
        Some("1") => 1,
        Some(value) => anyhow::bail!("--globe-samples requires 1 or 4; got {value:?}"),
    };
    let authenticated_ic = if let Some(path) = &config.ic_path {
        let mut anchors = TrustAnchors::default();
        let root = config
            .ic_trust_root
            .clone()
            .unwrap_or_else(|| get_app_base_dir().join("Trust/IHO-S100-5.2.pem"));
        anchors.install_pem(&config.ic_administrator, &fs::read(root)?)?;
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        let ic = ferrite_interoperability::AuthenticatedCatalogue::load(path, &anchors, time)?;
        anyhow::ensure!(
            ic.catalogue
                .products
                .iter()
                .all(|p| ["S-101", "S-102"].contains(&p.code.as_str())),
            "IC includes a product not supported by this app composition path"
        );
        info!(
            "Authenticated IC: {} {} (supported plane rules; full S-98 not verified)",
            ic.catalogue.name, ic.catalogue.version
        );
        Some(Arc::new(ic))
    } else {
        None
    };

    let fc = Arc::new(load_feature_catalogue(&config.fc_path)?);
    let pc = Arc::new(load_portrayal_catalogue(&config.pc_path)?);
    ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
    ferrite_s101::viewing_groups_for_layers(
        &pc,
        config.initial_viewing_layers.iter().map(String::as_str),
    )?;

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
    let symbol_cache = SymbolCache::new_with_sources(&symbols_path, pc.sources());
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

    let mut event_loop_builder = EventLoop::builder();
    ferrite_wgpu::background_test::configure_event_loop(&mut event_loop_builder);
    #[cfg(windows)]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        event_loop_builder.with_msg_hook(|raw| {
            use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageExtraInfo, MSG};
            // The hook runs before dispatch, while extra info belongs to this
            // MSG. Reading it later from a queued winit event is unreliable.
            let message = unsafe { &*(raw as *const MSG) };
            navigation::touch_promoted_mouse(message.message, unsafe { GetMessageExtraInfo() }
                as u64)
        });
    }
    let event_loop = event_loop_builder
        .build()
        .context("Failed to create event loop")?;
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

    app.initial_globe_samples = initial_globe_samples;
    app.applied_settings.display_mode = initial_display_mode;
    app.applied_settings.viewing_layers = config.initial_viewing_layers.into_iter().collect();
    app.ic = authenticated_ic;
    app.ic_audit = config.ic_audit;
    app.portrayal_audit = config.portrayal_audit;
    app.selection_audit = config.selection_audit;
    app.bathymetry_audit = config.bathymetry_audit;
    app.animation_audit = config.animation_audit;
    if let Some(date) = config.viewing_date {
        app.render_context.settings.current_date = Some(date);
    }
    app.render_context.settings.local_time_offset_seconds = local_time_offset.local_minus_utc();
    app.render_context.settings.current_datetime = config.viewing_instant;
    app.require_signatures = config.require_signatures;
    app.pending_auto_s102 = config.auto_s102;
    app.s102_pc_path = config.s102_pc_path;
    app.s102_adjustments_path = config.s102_adjustments_path;
    app.initial_interoperability_enabled = config.initial_interoperability_enabled;
    app.ic_transition_audit = config.ic_transition_audit;
    event_loop.run_app(&mut app).context("Event loop error")?;
    if let Some(error) = app.startup_error {
        anyhow::bail!(error);
    }
    Ok(())
}

/// Try to execute Lua portrayal rules and convert to drawing instructions
/// Context parameters are loaded dynamically from PC XML (no hardcoding)
/// Processes each cell separately to avoid feature ID collisions across cells
fn validated_lua_context(
    pc: &PortrayalCatalogue,
    settings: Option<&SettingsState>,
) -> Result<LuaContextParameters> {
    if let Some(settings) = settings {
        ferrite_s101::viewing_groups_for_layers(
            pc,
            settings.viewing_layers.iter().map(String::as_str),
        )?;
    }
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
        // The IHO PC uses ShallowWaterDangers; IsolatedDangers is a legacy
        // host parameter and does not control UDWHAZ05 in current catalogues.
        context.shallow_water_dangers = s.isolated_dangers;
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

    ferrite_s101::synchronize_legacy_context(pc, &mut context);
    let (_, diagnostics) = ferrite_s101::validate_portrayal_context(pc, &context)
        .context("PC portrayal input validation failed")?;
    for diagnostic in diagnostics {
        warn!("{diagnostic}");
    }
    Ok(context)
}

fn try_lua_portrayal(
    cells: &[S101Cell],
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    render_context: &mut RenderContext,
    profile_name: &str,
    settings: Option<&SettingsState>,
) -> Result<()> {
    let context = validated_lua_context(pc, settings)?;

    let rules_path = pc.root_path.join("Rules");

    if pc
        .sources()
        .read_relative(Path::new("Rules/main.lua"))
        .is_err()
    {
        return Err(anyhow::anyhow!(
            "Rules directory not found: {}",
            rules_path.display()
        ));
    }

    info!("Initializing Lua portrayal engine...");

    // Create portrayal engine
    let mut engine = PortrayalEngine::new_with_sources(pc.sources())
        .context("Failed to create portrayal engine")?;

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

        let results = engine
            .process_cell(&cell_data_guard, context.clone())
            .with_context(|| {
                format!(
                    "Cell {} portrayal failed: {}",
                    cell_index,
                    cell.file_path.display()
                )
            })?;
        total_results += results.len();
        convert_lua_results_for_cell(&results, cell, pc, render_context, cell_index, profile_name)?;
    }

    info!("Lua portrayal complete: {} total results", total_results);
    Ok(())
}

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

    if let Ok((_, diagnostics)) = ferrite_s101::context_validation_parameters(pc) {
        if !diagnostics.is_empty() {
            validation_messages
                .push("Known language-rule defect corrected locally; original catalogue retained");
        }
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
fn load_feature_catalogue(path: &Path) -> Result<BoundFeatureCatalogue> {
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
        dataset_discovery::feature_catalogue_file(path)?
    } else {
        path.to_path_buf()
    };

    let fc = FeatureCatalogue::load_bound(&fc_file)
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
fn load_portrayal_catalogue(path: &Path) -> Result<BoundPortrayalCatalogue> {
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

    let pc = PortrayalCatalogue::load_bound(path)
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
#[cfg(test)]
mod picking_plane_tests {
    use super::*;
    fn hit(kind: u8, order: i32, priority: i32, id: i64) -> (RenderedSymbol, f64) {
        (
            RenderedSymbol {
                source: None,
                plane: ferrite_kernel::CompositionPlane::new(
                    ferrite_kernel::CompositionStage::Chart,
                    std::num::NonZeroI32::new(order).unwrap(),
                ),
                kind,
                world_x: 0.,
                world_y: 0.,
                longitude_shift: 0.,
                feature_id: id,
                screen_x: 0.,
                screen_y: 0.,
                priority,
                symbol_ref: String::new(),
                cell_index: Some(0),
            },
            0.,
        )
    }
    #[test]
    fn displayed_plane_and_priority_precede_geometry_preference() {
        let mut hits = vec![
            hit(0, -10000, 99, 1),
            hit(2, -100, 0, 2),
            hit(0, -100, 0, 3),
            hit(1, -100, 1, 4),
        ];
        hits.sort_by(RenderedSymbol::compare_hits);
        assert_eq!(
            hits.iter().map(|h| h.0.feature_id).collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
    }
}

/// The persistent cache uses retained parser/runtime inputs, never the live tree.
fn hash_bound_catalogue_inputs<H: std::hash::Hasher>(
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    hash: &mut H,
) {
    use std::hash::Hash;
    fc.version.hash(hash);
    fc.source_digest().hash(hash);
    pc.source_digest().hash(hash);
}

#[cfg(test)]
mod bound_catalogue_cache_tests {
    use super::*;
    #[test]
    fn actual_catalogue_key_component_follows_bound_objects_not_live_edits() {
        use std::hash::Hasher;
        let root = std::env::temp_dir().join(format!(
            "ferrite-app-catalogue-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("PC/Rules")).unwrap();
        let fc_path = root.join("FC.xml");
        let pc_path = root.join("PC");
        let xml = |name: &str| {
            format!("<S100_FC_FeatureCatalogue><name>{name}</name><versionNumber>2.0</versionNumber><productId>S-101</productId></S100_FC_FeatureCatalogue>")
        };
        std::fs::write(&fc_path, xml("A")).unwrap();
        std::fs::write(pc_path.join("portrayal_catalogue.xml"), "<portrayalCatalog productId='S-101' version='2.0'><foundationMode/><displayPlanes><displayPlane id='OverRadar' order='1'/></displayPlanes></portrayalCatalog>").unwrap();
        std::fs::write(pc_path.join("Rules/main.lua"), "return 'A'").unwrap();
        let fc_a = FeatureCatalogue::load_bound(&fc_path).unwrap();
        let pc_a = PortrayalCatalogue::load_bound(&pc_path).unwrap();
        let key = |fc: &BoundFeatureCatalogue, pc: &BoundPortrayalCatalogue| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            hash_bound_catalogue_inputs(fc, pc, &mut hash);
            hash.finish()
        };
        let a = key(&fc_a, &pc_a);
        std::fs::write(&fc_path, xml("B")).unwrap();
        std::fs::write(pc_path.join("Rules/main.lua"), "return 'B'").unwrap();
        assert_eq!(key(&fc_a, &pc_a), a);
        let fc_b = FeatureCatalogue::load_bound(&fc_path).unwrap();
        let pc_b = PortrayalCatalogue::load_bound(&pc_path).unwrap();
        assert_ne!(key(&fc_b, &pc_a), a);
        assert_ne!(key(&fc_a, &pc_b), a);
        let b = key(&fc_b, &pc_b);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(key(&fc_a, &pc_a), a);
        assert_eq!(key(&fc_b, &pc_b), b);
    }
}

#[cfg(test)]
mod instruction_cache_schema_tests {
    use super::ChartApp;
    use ferrite_render::{AreaInstruction, DrawingInstruction, PatternCrs, WorldPoint};

    #[test]
    fn schema37_round_trip_preserves_both_clip_modes_and_source() {
        let instructions: Vec<_> = [true, false]
            .into_iter()
            .map(|clip| {
                DrawingInstruction::Area(
                    AreaInstruction::new(vec![
                        WorldPoint::new(0., 0.),
                        WorldPoint::new(1., 0.),
                        WorldPoint::new(1., 1.),
                        WorldPoint::new(0., 0.),
                    ])
                    .with_pattern_fill("motif".into(), (-2., 1.), (1., 3.))
                    .with_pattern_crs(PatternCrs::LocalGeometry)
                    .with_pattern_clip_symbols(clip)
                    .with_feature_id(42)
                    .with_cell_index(3),
                )
            })
            .collect();
        let payload = bincode::serialize(&instructions).unwrap();
        let file = ChartApp::wrap_cache(&payload);
        assert_eq!(&file[4..8], &37u32.to_le_bytes());
        let decoded = ChartApp::verify_and_deserialize_cache(&file).unwrap();
        assert_eq!(bincode::serialize(&decoded).unwrap(), payload);
        for (instruction, clip) in decoded.iter().zip([true, false]) {
            let DrawingInstruction::Area(area) = instruction else {
                panic!("not area")
            };
            assert_eq!(area.pattern_clip_symbols, clip);
            assert_eq!(area.pattern_crs, PatternCrs::LocalGeometry);
            assert_eq!(instruction.cell_index(), Some(3));
            assert_eq!(instruction.feature_id(), Some(42));
        }
    }

    #[test]
    fn rejects_legacy_schema_before_hash_or_decode_and_rejects_bad_headers() {
        let mut file = ChartApp::wrap_cache(&[1, 0, 0, 0, 0, 0, 0, 0]);
        file[4..8].copy_from_slice(&36u32.to_le_bytes());
        file[8] ^= 1; // Invalid hash must not bypass the schema gate.
        assert_eq!(
            ChartApp::verify_and_deserialize_cache(&file).unwrap_err(),
            "schema version mismatch: file=36, expected=37"
        );
        assert_eq!(
            ChartApp::verify_and_deserialize_cache(&file[..39]).unwrap_err(),
            "cache file too small"
        );
        file[0] ^= 1;
        assert!(ChartApp::verify_and_deserialize_cache(&file)
            .unwrap_err()
            .contains("magic"));
    }

    #[test]
    fn current_schema_checks_corruption_before_deserialization() {
        let payload = bincode::serialize(&Vec::<DrawingInstruction>::new()).unwrap();
        let mut file = ChartApp::wrap_cache(&payload);
        file[40] ^= 1;
        assert!(ChartApp::verify_and_deserialize_cache(&file)
            .unwrap_err()
            .contains("SHA-256"));
        let valid_hash_bad_payload = ChartApp::wrap_cache(&[1, 0, 0, 0, 0, 0, 0, 0]);
        assert!(
            ChartApp::verify_and_deserialize_cache(&valid_hash_bad_payload)
                .unwrap_err()
                .contains("deserialization failed")
        );
    }

    #[test]
    #[ignore = "requires separately SHA-guarded actual schema36 and schema37 SHOM payloads"]
    fn actual_shom_payloads_reject_36_and_round_trip_37() {
        let legacy = std::fs::read(std::env::var("FERRITE_CACHE36_PAYLOAD").unwrap()).unwrap();
        let mut old_file = ChartApp::wrap_cache(&legacy);
        old_file[4..8].copy_from_slice(&36u32.to_le_bytes());
        assert_eq!(
            ChartApp::verify_and_deserialize_cache(&old_file).unwrap_err(),
            "schema version mismatch: file=36, expected=37"
        );
        let current = std::fs::read(std::env::var("FERRITE_CACHE37_PAYLOAD").unwrap()).unwrap();
        let decoded =
            ChartApp::verify_and_deserialize_cache(&ChartApp::wrap_cache(&current)).unwrap();
        assert_eq!(decoded.len(), 69_570);
        assert_eq!(bincode::serialize(&decoded).unwrap(), current);
        let pattern_count = decoded
            .iter()
            .filter(|instruction| {
                matches!(instruction, DrawingInstruction::Area(a)
                if matches!(a.fill, ferrite_render::AreaFillType::Pattern { .. }))
            })
            .count();
        assert_eq!(pattern_count, 5_375);
        println!(
            "actual cache37: {} instructions / {} patterns; legacy36 rejected",
            decoded.len(),
            pattern_count
        );
    }
}

#[cfg(test)]
mod updated_chart_bounds_tests {
    use super::*;
    #[test]
    fn retains_raster_extent_and_drops_deleted_vector_extent_without_margin_growth() {
        let path = std::env::temp_dir().join(format!("ferrite-bounds-{}.000", std::process::id()));
        std::fs::write(&path, b"000253LE1 0000025 ! 1104\x1e").unwrap();
        let (mut cell, _) = S101Cell::load_from_with_identity(&path, &path).unwrap();
        std::fs::remove_file(path).unwrap();
        let id = ferrite_s100_core::RecordId::new(110, 1);
        cell.points.insert(
            id.key(),
            ferrite_s100_core::PointRecord {
                id,
                position: ferrite_s100_core::Coordinate::new(70., 80.),
                update_instruction: 1,
            },
        );
        let raster = GeoBounds::new(-10., -20., 10., 20.);
        let cells = vec![cell];
        let before = chart_data_bounds(&cells, [raster]);
        assert_eq!(
            (before.min_x, before.min_y, before.max_x, before.max_y),
            (-10., -20., 70., 80.)
        );
        let mut cells = cells;
        cells[0].points.clear();
        let after = chart_data_bounds(&cells, [raster]);
        assert_eq!(
            (after.min_x, after.min_y, after.max_x, after.max_y),
            (-10., -20., 10., 20.)
        );
        let again = chart_data_bounds(&cells, [raster]);
        assert_eq!(
            (again.min_x, again.min_y, again.max_x, again.max_y),
            (-10., -20., 10., 20.)
        );
    }
}

#[cfg(test)]
mod coverage_production_policy_tests {
    use super::coverage_cache_policy;
    #[test]
    fn defaults_on_and_independent_original_path_opt_out() {
        assert_eq!(coverage_cache_policy(None,None),(true,true));
        assert_eq!(coverage_cache_policy(Some("0"),None),(false,false));
        assert_eq!(coverage_cache_policy(None,Some("0")),(true,false));
        assert_eq!(coverage_cache_policy(Some("1"),Some("1")),(true,true));
    }
    #[test]
    fn each_precommit_provider_owns_fresh_cache_not_a_shared_epoch() {
        let a=super::coverage_geometry_cache_with_policy(true,true);
        let b=super::coverage_geometry_cache_with_policy(true,true);
        assert!(!std::sync::Arc::ptr_eq(&a,&b));
        a.lock().unwrap().set_enabled(false);
        let d=b.lock().unwrap().diagnostics();
        assert_eq!(d["enabled"],true);assert_eq!(d["midpoint_reuse"],true);
        assert_eq!(d["entries"],0);assert_eq!(d["retained_bytes"],0);
        assert_eq!(d["budget_bytes"],16*1024*1024);
    }
    #[test]
    fn explicit_cold_optout_has_no_optional_record_or_reuse() {
        let a=super::coverage_geometry_cache_with_policy(false,false);
        let d=a.lock().unwrap().diagnostics();
        assert_eq!(d["enabled"],false);assert_eq!(d["midpoint_reuse"],false);
        assert_eq!(d["entries"],0);assert_eq!(d["retained_bytes"],0);
    }
    #[test]
    fn no_eager_validation_or_global_env_mutation() {
        for value in ["", "false", "unknown"] {
            assert_eq!(coverage_cache_policy(Some(value),Some(value)),(true,true));
        }
    }
}

#[cfg(test)]
mod coverage_resize_event_tests {
    use super::CoverageLifecycleResize;
    use winit::dpi::PhysicalSize;
    fn state()->CoverageLifecycleResize {
        let now=std::time::Instant::now();
        CoverageLifecycleResize {output:std::path::PathBuf::new(),original:PhysicalSize::new(1280,950),requested:PhysicalSize::new(960,712),restore:false,observed:None,started:now,next_poll:now}
    }
    #[test]
    fn requested_extent_without_resized_event_is_not_a_proof() {
        let s=state();assert!(!s.ready(s.requested));
    }
    #[test]
    fn resized_and_actual_must_match_and_really_change_viewport() {
        let mut s=state();s.observed=Some(s.original);assert!(!s.ready(s.original));
        s.observed=Some(s.requested);assert!(!s.ready(s.original));assert!(s.ready(s.requested));
        s.observed=Some(PhysicalSize::new(0,0));assert!(!s.ready(PhysicalSize::new(0,0)));
    }
    #[test]
    fn restoration_requires_its_own_native_event_and_original_extent() {
        let mut s=state();s.restore=true;s.observed=Some(s.requested);assert!(!s.ready(s.requested));
        s.observed=Some(s.original);assert!(s.ready(s.original));
    }
}
