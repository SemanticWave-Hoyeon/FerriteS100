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
    let exemptions = if context.may_have_coverage_exempt_instructions() {
        context
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
            .collect()
    } else {
        std::collections::BTreeSet::new()
    };
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
    fn ready(&self, actual: winit::dpi::PhysicalSize<u32>) -> bool {
        actual.width > 0
            && actual.height > 0
            && self.observed == Some(actual)
            && if self.restore {
                actual == self.original
            } else {
                actual != self.original
            }
    }
}

mod cell_source_identity;
mod chart_publication;
mod dataset_discovery;
mod dataset_open_plan;
mod dataset_pick_fields;
mod dataset_signature_policy;
mod decoded_chart_cache;
mod diagnostics_tracing;
mod flat_event_burst;
mod flat_event_coalescing;
mod gpu_usage;
mod interoperability;
mod native_mcp;
mod native_s421;
mod navigation;
mod plugins;
mod point_retention_audit;
mod primary_viewing_layers;
mod process_stats;
mod s101_catalogue_selection;
mod s101_lifecycle_metadata;
mod s101_update_plan;
mod s101_xc_capture_pool;
mod s101_xc_consistency;
mod s101_xc_coverage;
mod s101_xc_region;
mod s102_depth_policy;
mod s102_input_capture;
mod s102_journal_admission;
mod s102_original_inputs;
mod s102_quality_worker;
mod s421_dataset_input;
mod secure_instruction_cache;
mod thread_stats;

mod flat_eventloop_diagnostics;
mod flat_service_trajectory;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Audit/capture requests must fail without waiting for interactive dismissal.
#[cfg(any(test, all(windows, not(debug_assertions))))]
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
use ferrite_lua::{ContextParameters as LuaContextParameters, PortrayalEngine, TypeCatalogue};
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

/// Bundles carry their own version inventory; developer checkouts keep the
/// historical sibling location. Neither depends on the launcher's working directory.
fn default_catalogue_inventory(base: &std::path::Path) -> PathBuf {
    let bundled = base.join("S101-Catalogues");
    if bundled.is_dir() {
        bundled
    } else {
        base.parent().unwrap_or(base).join("S101-Catalogues")
    }
}

fn default_log_directory(base: &std::path::Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    if base.file_name().is_some_and(|name| name == "Resources")
        && base
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|name| name == "Contents"))
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Logs/FerriteS100");
        }
    }
    base.join("logs")
}

/// Application configuration
struct AppConfig {
    /// Path to Feature Catalogue XML
    fc_path: PathBuf,
    /// Path to Portrayal Catalogue directory
    pc_path: PathBuf,
    /// Explicit local multi-version inventory; raw bytes remain source-bound.
    catalogue_inventory: PathBuf,
    /// Path to log directory (used only in debug builds)
    log_path: PathBuf,
    /// Debug mode enabled (--debug flag)
    debug_mode: bool,
    /// Auto-load chart file(s) on startup
    auto_chart: Vec<PathBuf>,
    auto_s102: Vec<PathBuf>,
    auto_s421: Vec<PathBuf>,
    s102_pc_path: PathBuf,
    s102_adjustments_path: Option<PathBuf>,
    require_signatures: bool,
    operational_mode: bool,
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
            catalogue_inventory: args
                .windows(2)
                .find(|w| w[0] == "--catalogue-inventory")
                .map(|w| PathBuf::from(&w[1]))
                .unwrap_or_else(|| default_catalogue_inventory(&base)),
            log_path: default_log_directory(&base),
            debug_mode,
            auto_chart,
            auto_s421: args
                .windows(2)
                .filter(|p| p[0] == "--s421")
                .map(|p| PathBuf::from(&p[1]))
                .collect(),
            auto_s102,
            s102_adjustments_path: args
                .windows(2)
                .find(|p| p[0] == "--s102-datum-adjustments")
                .map(|p| PathBuf::from(&p[1])),
            require_signatures: args
                .iter()
                .any(|a| a == "--require-signatures" || a == "--operational"),
            operational_mode: args.iter().any(|a| a == "--operational"),
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
    Load(Box<ChartLoadResult>),
    Cancel(chart_publication::ValidatedRemoval),
}
struct PreparedPortrayal {
    context: RenderContext,
    catalogue_registry: s101_catalogue_selection::DatasetCatalogueRegistry,
    cell_resources: Option<ferrite_wgpu::CellPortrayalResources>,
    coverage: Arc<ferrite_s101::coverage_projection::GeographicCoverageInventory>,
    ic_changed: usize,

    raster_scene: Option<ferrite_wgpu::PreparedRasterScenePublication>,
}

/// Source-only additive candidate. UI routing is deliberately not wired yet.
#[derive(Clone)]
struct PortrayalChangeRequest {
    profile: String,
    settings: SettingsState,
}
fn coalesce_portrayal_request(
    pending: Option<PortrayalChangeRequest>,
    profile: &str,
    settings: &SettingsState,
    next_profile: Option<String>,
    next_settings: Option<SettingsState>,
) -> PortrayalChangeRequest {
    let previous = pending.unwrap_or(PortrayalChangeRequest {
        profile: profile.into(),
        settings: settings.clone(),
    });
    PortrayalChangeRequest {
        profile: next_profile.unwrap_or(previous.profile),
        settings: next_settings.unwrap_or(previous.settings),
    }
}
fn portrayal_navigation_pending(
    loading: bool,
    dragging: bool,
    touch: bool,
    zoom: bool,
    pan_phase: u8,
    zoom_phase: u8,
    velocity: (f64, f64),
) -> bool {
    loading
        || dragging
        || touch
        || zoom
        || pan_phase != 0
        || zoom_phase != 0
        || velocity.0.abs() > 0.00001
        || velocity.1.abs() > 0.00001
}
// Hidden qualification only: owns sealed TARGET preparation; never activates it.
enum TargetAuditCapsule {
    TargetOwned(ferrite_wgpu::PreparedTargetVectorRaster),
    TargetSingle(ferrite_wgpu::PreparedTargetSinglePcVectorRaster),
}

struct PreparedPortrayalChange {
    base_instruction_count: usize,
    portrayal: PreparedPortrayal,
    symbols: SymbolCache,
    request: PortrayalChangeRequest,
    background: ferrite_render::Color,
}

struct PreparedBathymetryInput {
    capture: s102_input_capture::CapturedInput,
    original_input: Option<Arc<s102_original_inputs::OwnedOriginalInput>>,
    path: PathBuf,
    coverages: Vec<BathymetryCoverage>,
    policy: DepthPolicy,
    snapshot: Option<Arc<AuthenticatedSnapshot>>,
    unsigned_input: Option<Arc<UnauthenticatedSnapshot>>,
    bounds: Option<GeoBounds>,
}

/// Required SVGs are resolved in a new CPU cache; live material caches are not
/// touched. Device texture admission precedes infallible upload at publication.
fn preflight_candidate_symbols(
    context: &RenderContext,
    symbols: &mut SymbolCache,
    profile: &ferrite_portrayal_catalog::ColorProfile,
    texture_limit: u32,
) -> Result<()> {
    for instruction in context.raw_instructions() {
        let reference = match instruction {
            DrawingInstruction::Point(point) => Some(point.symbol_ref.as_str()),
            DrawingInstruction::Area(area) => match &area.fill {
                ferrite_render::AreaFillType::Pattern { symbol_ref, .. }
                | ferrite_render::AreaFillType::CentroidSymbol(symbol_ref) => {
                    Some(symbol_ref.as_str())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(reference) = reference.filter(|r| !r.is_empty()) {
            let geometry = symbols.get_symbol(reference, profile).with_context(|| {
                format!("Candidate PC resource could not be rendered: {reference}")
            })?;
            anyhow::ensure!(
                geometry.width <= texture_limit && geometry.height <= texture_limit,
                "Candidate SVG exceeds GPU texture limit: {reference}"
            );
        }
    }
    Ok(())
}

/// State belonging to the displayed datasets before an attempted load.
struct LoadCheckpoint {
    verified_count: usize,
    unsigned_count: usize,
    security_ui: Option<(String, String)>,
    frames_since_loaded: Option<u32>,
    signature_mode: bool,
    catalogue_identity: ([u8; 32], [u8; 32]),
    registry_checkpoint: s101_catalogue_selection::DatasetCatalogueCheckpoint,
    catalogue_routes: Vec<(Option<String>, Option<[u8; 32]>)>,
}

/// Background loading state
/// Private catalogue pair selected from captured, authenticated-or-explicit-OFF DSIDs.
/// Not installed until complete portrayal/resources and durable history succeed.
struct CandidateCatalogues {
    fc: Arc<BoundFeatureCatalogue>,
    pc: Arc<BoundPortrayalCatalogue>,
    fc_status: CatalogueStatus,
    pc_status: CatalogueStatus,
    notice: String,
}
/// Only the fields modified by normalize_feature_codes; graph, source and order
/// remain owned by the loaded cell. Used for synchronous preparation rollback.
struct CatalogueNormalization {
    feature_codes: Vec<(i64, Option<String>)>,
    mapping: ferrite_s100_core::CodeMapping,
}
impl CatalogueNormalization {
    fn capture(cell: &S101Cell) -> Self {
        Self {
            feature_codes: cell
                .features
                .iter()
                .map(|(id, f)| (*id, f.feature_code.clone()))
                .collect(),
            mapping: cell.code_mappings.feature_types.clone(),
        }
    }
    fn restore(self, cell: &mut S101Cell) {
        for (id, code) in self.feature_codes {
            cell.features
                .get_mut(&id)
                .expect("Preparation preserves feature ownership")
                .feature_code = code;
        }
        cell.code_mappings.feature_types = self.mapping;
    }
}
#[derive(Clone, Copy)]
enum RasterPreparation {
    Rebuild,
    /// The S102 append transaction stages and validates its own complete scene.
    CallerStagedRaster,
}
struct CatalogueRasterPreparation {
    registry: Option<s101_catalogue_selection::DatasetCatalogueRegistry>,
    raster: RasterPreparation,
}
enum CataloguePreparation {
    RetainedView,
    Loaded { preserve_view: bool },
}
enum PreparedLoadPortrayal {
    Current(Box<PreparedPortrayal>),
    Catalogue {
        owner: Box<CandidateCatalogues>,
        change: Box<PreparedPortrayalChange>,
    },
}
struct BackgroundLoadingState {
    /// Number of files being loaded
    total_files: usize,
    /// Number of files loaded so far
    loaded_count: usize,
    pending: Vec<ChartPublicationResult>,
    failed: bool,
    allow_partial: bool,
    checkpoint: LoadCheckpoint,
    candidate_catalogues: Option<CandidateCatalogues>,
    candidate_registry: Option<s101_catalogue_selection::DatasetCatalogueRegistry>,
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

fn chart_viewport_ready(view: (f32, f32, f32, f32)) -> bool {
    let (x, y, width, height) = view;
    [x, y, width, height].iter().all(|v| v.is_finite())
        && width > 0.
        && height > 0.
        && (x + width).is_finite()
        && (y + height).is_finite()
        && x + width > x
        && y + height > y
}

fn chart_batch_publishable(allow_partial: bool, failed: bool, complete_datasets: usize) -> bool {
    complete_datasets > 0 && (allow_partial || !failed)
}

#[derive(Default)]
struct DatasetOpenSession {
    routes: std::collections::VecDeque<s421_dataset_input::CapturedRouteInput>,
    routes_loaded: usize,
    routes_existing: usize,
    routes_failed: usize,
    rasters: std::collections::VecDeque<PathBuf>,
    charts: Option<Vec<PathBuf>>,
    selected_charts: Vec<PathBuf>,
    notices: Vec<String>,
    raster_loaded: usize,
    raster_existing: usize,
    raster_failed: usize,
    chart_started: bool,
    chart_failed: bool,
    chart_partial: bool,
}

impl DatasetOpenSession {
    fn summary(&self, chart_count: usize) -> String {
        let charts = if !self.chart_started {
            "not requested"
        } else if self.chart_failed && self.chart_partial {
            "completed with failed datasets"
        } else if self.chart_failed {
            "failed; previous charts retained"
        } else {
            "completed"
        };
        format!(
            "Dataset loading complete. S-101: {charts} ({chart_count} charts available). S-102: {} loaded, {} already open, {} failed. S-421: {} loaded, {} already open, {} failed. {} skipped or failed entries. See Logs for details.",
            self.raster_loaded,
            self.raster_existing,
            self.raster_failed,
            self.routes_loaded,
            self.routes_existing,
            self.routes_failed,
            self.notices.len()
        )
    }
}

/// Chart viewer application for winit
struct ChartApp {
    s100_mcp: native_mcp::Controller,
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    render_context: RenderContext,
    bounds: GeoBounds,
    /// Symbol cache for SVG symbol rendering
    symbol_cache: SymbolCache,
    cell_resources: Option<ferrite_wgpu::CellPortrayalResources>,
    /// Current color profile name (Day, Dusk, Night)
    pending_portrayal_change: Option<PortrayalChangeRequest>,
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
    catalogue_inventory: PathBuf,
    catalogue_registry: s101_catalogue_selection::DatasetCatalogueRegistry,
    /// Feature Catalogue status (for UI display)
    fc_status: CatalogueStatus,
    /// Portrayal Catalogue status (for UI display)
    pc_status: CatalogueStatus,
    s102_pc_status: CatalogueStatus,
    s102_fc_status: CatalogueStatus,
    s102_fc: Option<Arc<BoundFeatureCatalogue>>,
    s102_pc: Option<Arc<BoundPortrayalCatalogue>>,
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
    s102_original_inputs: s102_original_inputs::Registry,
    s102_journal_gate: Option<s102_journal_admission::Gate>,
    require_signatures: bool,
    operational_mode: bool,
    verified_count: usize,
    unsigned_count: usize,
    startup_error: Option<String>,
    publication_test_fail_before_commit: bool,
    s102_raster_rebuild_count: u64,
    catalogue_test_fail_history: bool,
    pending_auto_s102: Vec<PathBuf>,
    s102_pc_path: PathBuf,
    s102_adjustments_path: Option<PathBuf>,
    /// Whether chart data is loaded
    chart_loaded: bool,
    /// Paths of already loaded chart files (to prevent duplicates)
    /// Background loading state (Some if loading in progress)
    loading_state: Option<BackgroundLoadingState>,
    decoded_chart_cache: decoded_chart_cache::SharedDecodedChartCache,
    dataset_open_discovery: Option<Receiver<Result<dataset_open_plan::Plan>>>,
    dataset_open_session: Option<DatasetOpenSession>,
    /// Plugin system
    plugin_system: plugins::PluginSystem,
    native_s421: native_s421::NativeS421,
    native_s421_ui_revision: u64,
    pending_auto_s421: Vec<PathBuf>,
    /// Base instruction count (chart instructions only, before plugin instructions)
    base_instruction_count: usize,
    applied_settings: SettingsState,

    /// Debug mode enabled
    debug_mode: bool,
    navigation_scene_pending: flat_event_coalescing::Pending,
    /// Navigation scene emission running on a worker; owns `cell_resources`.
    scene_build: Option<std::thread::JoinHandle<ferrite_wgpu::BuiltScene>>,
    /// `FERRITE_BACKGROUND_SCENE_BUILD=0` (or `FERRITE_MOTION_PREVIEW=0`) keeps
    /// every rebuild on the UI thread.
    background_scene_builds: bool,
    navigation_flush_active: bool,
    navigation_burst_audit: Option<Box<flat_event_burst::Audit>>,
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
    flat_eventloop_audit: Option<Box<flat_eventloop_diagnostics::Audit>>,
    flat_eventloop_audit_started: bool,
    /// Frame times for FPS calculation
    frame_times: std::collections::VecDeque<std::time::Instant>,
    process_stats: process_stats::ProcessStats,
    thread_stats: thread_stats::ThreadStats,
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
            s100_mcp: native_mcp::Controller::default(),
            window: None,
            renderer: None,
            render_context: {
                let mut context = RenderContext::new(Viewport::new(1920.0, 1080.0));
                context.set_coverage_visibility_fusion_enabled(
                    std::env::var("FERRITE_COVERAGE_VISIBILITY_FUSION")
                        .ok()
                        .as_deref()
                        == Some("1"),
                );
                context
                    .scaler
                    .set_projection(ferrite_render::FlatProjection::EllipsoidalMercator);
                context
            },
            bounds: GeoBounds::new(-180.0, -90.0, 180.0, 90.0),
            symbol_cache,
            cell_resources: None,
            pending_portrayal_change: None,
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
            catalogue_inventory: {
                let base = get_app_base_dir();
                default_catalogue_inventory(&base)
            },
            catalogue_registry: Default::default(),
            fc_status,
            pc_status,
            s102_pc_status: CatalogueStatus::default(),
            s102_fc_status: CatalogueStatus::default(),
            s102_fc: None,
            s102_pc: None,
            cells: Vec::new(),
            loaded_source_identities: Default::default(),
            loaded_chain_paths: Vec::new(),
            loaded_discovery: Default::default(),
            cancellation_history_path: chart_publication::default_history_path(),
            coverage_inventory: None,

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
            s102_original_inputs: Default::default(),
            s102_journal_gate: None,
            require_signatures: false,
            operational_mode: false,
            verified_count: 0,
            unsigned_count: 0,
            startup_error: None,
            publication_test_fail_before_commit: false,
            s102_raster_rebuild_count: 0,
            catalogue_test_fail_history: false,
            pending_auto_s102: Vec::new(),
            s102_adjustments_path: None,
            s102_pc_path: get_app_base_dir().join("Catalogues/PC/S-102"),
            chart_loaded: false,
            loading_state: None,
            decoded_chart_cache: Arc::new(std::sync::Mutex::new(
                decoded_chart_cache::DecodedChartCache::new(),
            )),
            dataset_open_discovery: None,
            dataset_open_session: None,
            native_s421: native_s421::NativeS421::new(get_app_base_dir().join("Catalogues")),
            native_s421_ui_revision: 0,
            pending_auto_s421: Vec::new(),
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

            debug_mode,
            navigation_scene_pending: flat_event_coalescing::Pending::new(
                std::env::var_os("FERRITE_FLAT_EVENT_COALESCING").as_deref(),
            ),
            scene_build: None,
            // Background builds are only shown through the motion preview;
            // without it they would add settle work and no smoother frames.
            background_scene_builds: std::env::var_os("FERRITE_BACKGROUND_SCENE_BUILD")
                .is_none_or(|v| v != "0")
                && std::env::var_os("FERRITE_MOTION_PREVIEW").is_none_or(|v| v != "0"),
            navigation_flush_active: false,
            navigation_burst_audit: None,
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
            flat_eventloop_audit: None,
            flat_eventloop_audit_started: false,
            frame_times: std::collections::VecDeque::with_capacity(60),
            process_stats: process_stats::ProcessStats::default(),
            thread_stats: thread_stats::ThreadStats::default(),
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

    fn apply_portrayal_settings(&mut self) -> Result<()> {
        let mut candidate = self
            .renderer
            .as_ref()
            .context("Renderer not initialized")?
            .settings()
            .clone();
        // Egui edits controls before Apply. Restore live applied state before any
        // preparation; candidate values are passed explicitly, never read back.
        self.renderer
            .as_mut()
            .unwrap()
            .set_settings(self.applied_settings.clone());
        primary_viewing_layers::capture_primary(&self.pc, &mut candidate)?;
        let request = PortrayalChangeRequest {
            profile: self.current_profile_name.clone(),
            settings: candidate,
        };
        let prepared = self.prepare_portrayal_change(request)?;
        self.commit_portrayal_change(prepared)
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
        self.ensure_navigation_scene()?;
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
            primary_viewing_layers::resolve(&self.pc, settings)
                .expect("Selected S-101 layers were prevalidated"),
        );
        visible.insert(21010); // Existing plugin overlay group.
        Some(visible)
    }

    fn signature_verification_enabled(&self) -> bool {
        dataset_signature_policy::verification_enabled(
            self.renderer
                .as_ref()
                .map(|r| r.ui_state.verify_dataset_signatures),
            self.require_signatures,
            self.operational_mode,
        )
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

    fn bathymetry_viewing_groups(
        pc: &PortrayalCatalogue,
        mode: DisplayMode,
    ) -> Result<std::collections::HashSet<u32>> {
        use ferrite_s102::BathymetryDisplayPreset;
        let preset = match mode {
            DisplayMode::Base => BathymetryDisplayPreset::Base,
            DisplayMode::Standard => BathymetryDisplayPreset::Standard,
            DisplayMode::All => BathymetryDisplayPreset::Other,
        };
        ferrite_s102::bathymetry_viewing_groups_for_preset(pc, preset)
    }
    /// Load installed product resources before any datasets are opened. Errors
    /// remain visible; a failed parse is never presented as a loaded catalogue.
    fn initialize_product_catalogues(&mut self) {
        let fc_path = get_app_base_dir().join("Catalogues/FC/S-102");
        match load_feature_catalogue(&fc_path).and_then(|fc| {
            anyhow::ensure!(
                fc.product_id == "S-102",
                "Expected S-102 FC, found {}",
                fc.product_id
            );
            Ok(fc)
        }) {
            Ok(fc) => {
                self.s102_fc_status = CatalogueStatus {
                    loaded: true,
                    product_id: fc.product_id.clone(),
                    version: fc.version.clone(),
                    path: fc.source_path.display().to_string(),
                    item_count: fc.feature_types.len(),
                    validation_message: Some("Parsed installed S-102 Feature Catalogue".into()),
                };
                self.s102_fc = Some(Arc::new(fc));
            }
            Err(error) => {
                self.s102_fc_status = CatalogueStatus {
                    product_id: "S-102".into(),
                    path: fc_path.display().to_string(),
                    validation_message: Some(format!("Load failed: {error:#}")),
                    ..Default::default()
                };
                warn!("S-102 FC initialization failed: {error:#}");
            }
        }
        match PortrayalCatalogue::load_bound(&self.s102_pc_path).and_then(|pc| {
            if pc.product_id != "S-102" {
                return Err(ferrite_portrayal_catalog::PCError::InvalidValue(format!(
                    "Expected S-102 PC, found {}",
                    pc.product_id
                )));
            }
            Ok(pc)
        }) {
            Ok(pc) => {
                self.s102_pc_status = CatalogueStatus {
                    loaded: true,
                    product_id: pc.product_id.clone(),
                    version: pc.version.clone(),
                    path: pc.root_path.display().to_string(),
                    item_count: pc.symbols.symbols.len(),
                    validation_message: Some("Parsed installed S-102 Portrayal Catalogue".into()),
                };
                self.s102_pc = Some(Arc::new(pc));
            }
            Err(error) => {
                self.s102_pc_status = CatalogueStatus {
                    product_id: "S-102".into(),
                    path: self.s102_pc_path.display().to_string(),
                    validation_message: Some(format!("Load failed: {error}")),
                    ..Default::default()
                };
                warn!("S-102 PC initialization failed: {error}");
            }
        }
    }

    fn bathymetry_portrayal_with_status(
        &self,
    ) -> Result<(
        BathymetryPortrayal,
        CatalogueStatus,
        std::collections::HashSet<u32>,
    )> {
        let pc = self
            .s102_pc
            .as_ref()
            .context("S-102 PC unavailable; see Catalogue status")?
            .clone();
        let s = self
            .renderer
            .as_ref()
            .context("Renderer not initialized")?
            .settings();
        let groups = Self::bathymetry_viewing_groups(&pc, s.display_mode)?;
        BathymetryPortrayal::from_bound_catalogue(
            Arc::clone(&pc),
            &self.current_profile_name,
            DepthSettings {
                safety_contour: s.safety_contour,
                shallow_contour: s.shallow_contour,
                deep_contour: s.deep_contour,
                four_shades: !s.two_shades,
            },
        )
        .map(|portrayal| {
            (
                portrayal,
                CatalogueStatus {
                    loaded: true,
                    product_id: pc.product_id.clone(),
                    version: pc.version.clone(),
                    path: pc.root_path.display().to_string(),
                    item_count: pc.symbols.symbols.len(),
                    validation_message: Some("Active S-102 portrayal catalogue".into()),
                },
                groups,
            )
        })
    }

    fn load_bathymetry(&mut self, paths: &[PathBuf]) -> Result<()> {
        self.ensure_navigation_scene()?;
        let checkpoint = self.load_checkpoint();
        match self.load_bathymetry_transaction(paths) {
            Ok(()) => {
                self.refresh_dataset_tree();
                Ok(())
            }
            Err(error) => {
                self.restore_load_checkpoint(checkpoint);
                Err(error)
            }
        }
    }
    fn load_bathymetry_transaction(&mut self, paths: &[PathBuf]) -> Result<()> {
        self.ensure_navigation_scene()?;
        if let Some(gate) = &mut self.s102_journal_gate {
            let mut seen = std::collections::BTreeSet::new();
            for (path, _, snapshot) in &self.bathymetry {
                let record = self.s102_original_inputs.get(path);
                if !record
                    .zip(snapshot.as_ref())
                    .is_some_and(|(r, s)| Arc::ptr_eq(&r.snapshot, s))
                {
                    gate.block_for_recovery();
                    anyhow::bail!("Enforced S102 history requires every active instance's authenticated parser owner");
                }
                if seen.insert(path) {
                    if let Err(error) = gate.check_input(record, snapshot.as_ref(), path) {
                        // Existing scene authority failed, unlike a refused new import.
                        // Rolling back a load checkpoint must not resume that scene.
                        gate.block_for_recovery();
                        return Err(error);
                    }
                }
            }
        }
        if paths.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            self.loading_state.is_none(),
            "Finish or clear the current S-101 load before adding bathymetry"
        );
        let mut unique = std::collections::HashSet::new();
        let mut paths_to_load = Vec::new();
        for path in paths {
            let path = path
                .canonicalize()
                .with_context(|| format!("Cannot open {}", path.display()))?;
            if !self.bathymetry.iter().any(|(loaded, _, _)| loaded == &path)
                && unique.insert(path.clone())
            {
                paths_to_load.push(path);
            }
        }
        if paths_to_load.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            paths_to_load.len() <= 512,
            "S102 publication exceeds 512-file work budget"
        );
        // Local retained-file policy, checked before new authorization/capture.
        {
            let mut original_paths = std::collections::BTreeSet::new();
            for path in self
                .bathymetry
                .iter()
                .map(|entry| &entry.0)
                .chain(paths_to_load.iter())
            {
                original_paths.insert(path);
                anyhow::ensure!(
                    original_paths.len() <= s102_original_inputs::MAX_FILES,
                    "S102 retained input policy exceeds 512 loaded files"
                );
            }
        }
        let require_signature = self.signature_verification_enabled();
        let authorization = self.authenticate_paths(&paths_to_load)?;
        let (portrayal, pc_status, raster_groups) = self.bathymetry_portrayal_with_status()?;
        let edge = self.bathymetry_tile_edge()?;
        let active_ic = self.active_ic();
        let mut pending = Vec::new();
        let mut instance_count = 0usize;
        // Receiver snapshot admission policy, not a product-specification limit.
        let mut snapshot_budget = 512u64 * 1024 * 1024;
        for path in paths_to_load {
            let capture = s102_input_capture::CapturedInput::capture(
                &authorization,
                &path,
                require_signature,
                &mut snapshot_budget,
            )?;
            let original_input = s102_original_inputs::capture(
                &authorization,
                &path,
                capture.authenticated.as_ref(),
            )?;
            if let Some(gate) = &mut self.s102_journal_gate {
                gate.check_input(
                    original_input.as_ref(),
                    capture.authenticated.as_ref(),
                    &path,
                )?;
            }
            let snapshot = capture.authenticated.clone();
            let unsigned_input = capture.unchecked.clone();
            let data_path = capture.path();
            capture.verify()?;
            let quality_records = s102_quality_worker::decode_captured(data_path)?;
            capture.verify()?;
            let coverages =
                BathymetryCoverage::open_with_quality_records(data_path, quality_records)?;
            if coverages
                .iter()
                .any(|c| c.vertical_cs_reinterpreted_as_depth)
            {
                warn!(
                    "S-102 compatibility: {} declares verticalCS 6499 (height up) but is read as \
                     depth-down because {}=1. Verify the producer's convention before relying on depths.",
                    path.display(),
                    ferrite_s102::VERTICAL_CS_6499_AS_DEPTH_ENV
                );
            }
            let policy = DepthPolicy::load(
                self.s102_adjustments_path.as_deref(),
                data_path,
                &coverages
                    .iter()
                    .map(|c| c.vertical_datum)
                    .collect::<Vec<_>>(),
            )?;

            instance_count = instance_count
                .checked_add(coverages.len())
                .context("S102 instance count overflow")?;
            anyhow::ensure!(
                instance_count <= 4096,
                "S102 publication exceeds 4096-instance work budget"
            );
            pending.push(PreparedBathymetryInput {
                path,
                coverages,
                policy,
                snapshot,
                unsigned_input,
                capture,
                original_input,
                bounds: None,
            });
        }
        let next_original_inputs = s102_original_inputs::stage(
            &self.s102_original_inputs,
            pending
                .iter()
                .map(|input| (input.path.as_path(), input.original_input.as_ref())),
            &authorization,
        )?;
        let mut batch = self
            .renderer
            .as_mut()
            .context("Renderer not initialized")?
            .stage_s102_raster_batch(&self.render_context.scaler, |upload| {
                for input in &mut pending {
                    let coverages = &input.coverages;
                    let mosaic = ConservativeCoverage::new(
                        coverages
                            .iter()
                            .map(|c| DatumCoverage {
                                coverage: c,
                                reference: DepthReference(c.vertical_datum as u64),
                            })
                            .collect(),
                        DepthReference(input.policy.target as u64),
                        &input.policy,
                    )?;
                    let raw =
                        coverages.len() == 1 && input.policy.identity(coverages[0].vertical_datum);
                    let numeric: &dyn NumericCoverageSource =
                        if raw { &coverages[0] } else { &mosaic };
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
                    let assignment = active_ic
                        .as_ref()
                        .map(|ic| coverages[0].interoperability_assignment(&ic.catalogue))
                        .transpose()?
                        .flatten();
                    if let Some(ic) = active_ic.as_ref() {
                        for c in coverages {
                            anyhow::ensure!(
                        c.interoperability_assignment(&ic.catalogue)? == assignment,
                        "Cannot compose S102 instances with different interoperability assignments"
                    );
                        }
                    }
                    for window in numeric.numeric_geometry().windows(edge, edge)? {
                        let layer = if active_ic.is_none() {
                            ferrite_wgpu::S102RasterMaterial::Bound(portrayal.bound_raster_window(
                                numeric,
                                window,
                                format!(
                                    "{}:composed:{}:{}",
                                    input.path.display(),
                                    window.column,
                                    window.row
                                ),
                            )?)
                        } else {
                            ferrite_wgpu::S102RasterMaterial::LegacyIc(
                                interoperability::compose_raster(
                                    portrayal.raster_window(
                                        numeric,
                                        window,
                                        format!(
                                            "{}:composed:{}:{}",
                                            input.path.display(),
                                            window.column,
                                            window.row
                                        ),
                                    )?,
                                    assignment.as_ref(),
                                )?,
                            )
                        };
                        let b = layer.bounds();
                        if let Some(bounds) = &mut input.bounds {
                            bounds.min_x = bounds.min_x.min(b.min_x);
                            bounds.min_y = bounds.min_y.min(b.min_y);
                            bounds.max_x = bounds.max_x.max(b.max_x);
                            bounds.max_y = bounds.max_y.max(b.max_y);
                        } else {
                            input.bounds = Some(b);
                        }
                        upload(layer)?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            })?;
        // Derive the mixed-product camera from raw data extents, never from
        // an already padded camera. This gives the same framing when S101 or
        // S102 arrives first, and prevents margins accumulating on append.
        let mut incoming_bounds = Vec::with_capacity(pending.len());
        for input in &pending {
            incoming_bounds.push(input.bounds.context("No coverage tiles")?);
        }
        let mut next_bounds = chart_data_bounds(
            &self.cells,
            self.bathymetry_bounds
                .values()
                .copied()
                .chain(incoming_bounds),
        );
        next_bounds.expand_by_percent(0.1);
        let previous_bounds = self.bounds;
        self.bounds = next_bounds;
        let prepared = self.prepare_instructions_internal_with_raster(
            true,
            if active_ic.is_none()
                && self
                    .renderer
                    .as_ref()
                    .is_some_and(|r| r.settings() == &self.applied_settings)
                && !(ferrite_wgpu::background_test::enabled()
                    && std::env::var("FERRITE_S102_APPEND_REBUILD_REFERENCE").as_deref() == Ok("1"))
            {
                RasterPreparation::CallerStagedRaster
            } else {
                RasterPreparation::Rebuild
            },
        );
        self.bounds = previous_bounds;
        let mut prepared = prepared?;
        let mut next_pan = self.pan_offset;
        let mut next_zoom = self.zoom_level;
        if self.auto_screenshot.is_some() {
            if let Some((lat, lon)) = self.auto_center {
                if let Some(pan) = prepared
                    .context
                    .scaler
                    .projection()
                    .pan_to(next_bounds, WorldPoint::new(lon, lat))
                {
                    next_pan = (pan[0], pan[1]);
                }
            }
            if let Some(zoom) = self.auto_zoom {
                next_zoom = zoom;
            }
        }
        let wrapped = next_pan.0 - (next_pan.0 / 360.).round() * 360.;
        let view = prepared
            .context
            .scaler
            .projection()
            .view_bounds(next_bounds, next_zoom, [wrapped, next_pan.1])
            .context("Invalid proposed bathymetry camera")?;
        let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
        let (x, y, w, h) = renderer.chart_viewport_pixels();
        anyhow::ensure!(w > 0. && h > 0., "Invalid proposed chart viewport");
        prepared.context.set_viewport_rect(x, y, w, h);
        prepared.context.zoom_to_fit(view);
        prepare_flat_coverage(
            Some(prepared.coverage.as_ref()),
            &mut prepared.context,
            renderer.window().inner_size(),
            renderer.window().scale_factor(),
        )?;
        renderer.reproject_raster_material_batch(&mut batch, &prepared.context.scaler)?;
        let mut raster = renderer.prepare_raster_material_publication(batch, false, None)?;
        renderer.reproject_raster_publication(&mut raster, &prepared.context.scaler)?;

        let scene =
            renderer.prepare_raster_scene_publication_with_groups(raster, Some(raster_groups))?;
        renderer.validate_raster_scene_publication(&scene)?;
        anyhow::ensure!(
            !self.publication_test_fail_before_commit,
            "Injected bathymetry publication failure before commit"
        );
        if std::env::var("FERRITE_ROOT_S102_FAIL_SCENE").as_deref() == Ok("1") {
            anyhow::ensure!(
                ferrite_wgpu::background_test::enabled(),
                "Scene failure injection requires hidden background test"
            );
            anyhow::bail!("Injected bathymetry failure after complete material staging");
        }
        for input in &pending {
            input.capture.verify()?;
            if let Some(gate) = &mut self.s102_journal_gate {
                gate.check_input(
                    input.original_input.as_ref(),
                    input.snapshot.as_ref(),
                    &input.path,
                )?;
            }
        }
        prepared.raster_scene = Some(scene);
        // All recoverable loading, numeric, material and Flat material work is
        // complete. Publish the entire file set and its rendering state together.
        for input in pending {
            for c in &input.coverages {
                if c.observed_range_violations().iter().any(|v| *v) {
                    warn!(
                        "S-102 {} {}: decoded values outside declared extrema; original samples retained",
                        input.path.display(),
                        c.instance_name
                    );
                }
                if !c.root_enclosure.encoding_compatible() {
                    warn!(
                        "S-102 {} {}: root geographic enclosure {:?}; raw values retained",
                        input.path.display(),
                        c.instance_name,
                        c.root_enclosure
                    );
                }
                if c.observed_depth_centroids_outside_domain() {
                    warn!(
                        "S-102 {} {}: populated original depth sample positions outside instance validity; raw samples retained",
                        input.path.display(),
                        c.instance_name
                    );
                }
            }

            let path = input.path;
            self.bathymetry_bounds
                .insert(path.clone(), input.bounds.expect("validated bounds"));
            if let Some(unsigned) = input.unsigned_input {
                self.depth_inputs.insert(path.clone(), unsigned);
            }
            self.depth_policies.insert(path.clone(), input.policy);
            self.bathymetry.extend(
                input
                    .coverages
                    .into_iter()
                    .map(|c| (path.clone(), c, input.snapshot.clone())),
            );
        }
        self.s102_original_inputs = next_original_inputs;
        self.bounds = next_bounds;
        self.pan_offset = next_pan;
        self.zoom_level = next_zoom;
        self.zoom_target = next_zoom;
        self.chart_loaded = true;
        self.publish_instructions(prepared);
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.bathymetry_count = self.bathymetry.len();
            renderer.ui_state.chart_count = self.cells.len() + self.bathymetry.len();
            renderer.update_selection(&self.render_context.scaler);
        }
        if self.auto_screenshot.is_some() {
            self.frames_since_loaded = Some(0);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        self.s102_pc_status = pc_status;
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

    /// Prepare all vector/raster resources without changing active colours,
    /// settings, CPU symbol cache, texture inventory or previous target.
    /// Active affine navigation finishes before deferred UI requests are staged;
    /// existing navigation is reconciled before, never after, publication.
    fn queue_portrayal_change(&mut self, request: PortrayalChangeRequest) {
        self.pending_portrayal_change = Some(request);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_settings(self.applied_settings.clone());
            renderer.set_color_profile(&self.current_profile_name);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
    fn process_pending_portrayal_change(&mut self) {
        if self.pending_portrayal_change.is_none()
            || portrayal_navigation_pending(
                self.loading_state.is_some(),
                self.is_dragging,
                self.touch_navigation.active(),
                self.zoom_animating,
                self.pan_rebuild_phase,
                self.zoom_rebuild_phase,
                self.pan_velocity,
            )
        {
            return;
        }
        let fast = self.renderer.as_ref().is_some_and(|r| {
            let (pan, zoom, _) = r.fast_view_transform();
            pan != (0., 0.) || zoom != 1. || r.fast_view_scales().1 != 1.
        });
        if fast {
            // Reconcile existing navigation BEFORE preparing a new candidate.
            // No postcommit rebuild is used to repair a failed publication.
            self.update_view();
        }
        let request = self
            .pending_portrayal_change
            .take()
            .expect("Pending request checked");
        let result = self
            .prepare_portrayal_change(request)
            .and_then(|p| self.commit_portrayal_change(p));
        if let Err(error) = result {
            if let Some(renderer) = &mut self.renderer {
                renderer.set_settings(self.applied_settings.clone());
                renderer.set_color_profile(&self.current_profile_name);
                renderer.ui_state.notice = Some(format!(
                    "Portrayal change rejected; previous display retained: {error:#}"
                ));
            }
        }
    }

    fn prepare_portrayal_change(
        &mut self,
        request: PortrayalChangeRequest,
    ) -> Result<PreparedPortrayalChange> {
        self.poll_scene_build(true);
        self.ensure_navigation_scene()?;
        let fc = Arc::clone(&self.fc);
        let pc = Arc::clone(&self.pc);
        let symbols = self.symbol_cache.fork_empty();
        if self.cell_resources.is_some() {
            let registry = self.prepare_catalogue_registry(&fc, &pc, true)?;
            return self.prepare_portrayal_change_with_registry(
                request,
                &fc,
                &pc,
                symbols,
                CataloguePreparation::RetainedView,
                CatalogueRasterPreparation {
                    registry: Some(registry),
                    raster: RasterPreparation::Rebuild,
                },
            );
        }
        self.prepare_portrayal_change_for_catalogues(
            request,
            &fc,
            &pc,
            symbols,
            CataloguePreparation::RetainedView,
        )
    }
    /// All manual entrypoints share the same private-pair/material transaction.
    /// No renderer/UI/cache/catalogue ownership changes before successful staging.
    fn change_catalogue_pair_manually(
        &mut self,
        fc: Arc<BoundFeatureCatalogue>,
        pc: Arc<BoundPortrayalCatalogue>,
    ) -> Result<()> {
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            self.loading_state.is_none(),
            "Wait for dataset loading to finish before changing catalogues"
        );
        ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
        for cell in &self.cells {
            ferrite_s101::validate_dataset_catalogues(
                &cell.dsid,
                &fc,
                &pc.product_id,
                &pc.version,
            )?;
        }
        primary_viewing_layers::validate_new_primary(&pc, &self.applied_settings)?;
        let fc_status = validate_fc(&fc, &fc.source_path);
        let pc_status = validate_pc(&pc, &pc.root_path);
        let symbols = SymbolCache::new_with_pattern_contract(
            pc.root_path.join("Symbols"),
            pc.sources(),
            ferrite_s101::shallow_pattern_contract(&pc),
        );
        let original: Vec<_> = self
            .cells
            .iter()
            .map(CatalogueNormalization::capture)
            .collect();
        let codes = fc.feature_type_codes();
        for cell in &mut self.cells {
            cell.normalize_feature_codes(&codes);
        }
        let prepared = self.prepare_portrayal_change_for_catalogues(
            PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: self.applied_settings.clone(),
            },
            &fc,
            &pc,
            symbols,
            CataloguePreparation::Loaded {
                preserve_view: true,
            },
        );
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                for (cell, original) in self.cells.iter_mut().zip(original) {
                    original.restore(cell);
                }
                return Err(error);
            }
        };
        // Staging validated the capsule. Only infallible owner moves follow.
        self.fc = fc;
        self.pc = pc;
        self.fc_status = fc_status;
        self.pc_status = pc_status;
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.fc_status = self.fc_status.clone();
            renderer.ui_state.pc_status = self.pc_status.clone();
            renderer.ui_state.optional_viewing_layers =
                ferrite_s101::optional_viewing_layers(&self.pc);
            renderer.ui_state.notice = None;
        }
        self.install_portrayal_change(prepared);
        self.refresh_dataset_tree();
        Ok(())
    }

    fn prepare_portrayal_change_for_catalogues(
        &mut self,
        request: PortrayalChangeRequest,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        symbols: SymbolCache,
        preparation: CataloguePreparation,
    ) -> Result<PreparedPortrayalChange> {
        self.prepare_portrayal_change_with_registry(
            request,
            fc,
            pc,
            symbols,
            preparation,
            CatalogueRasterPreparation {
                registry: None,
                raster: RasterPreparation::Rebuild,
            },
        )
    }

    fn prepare_portrayal_change_with_registry(
        &mut self,
        mut request: PortrayalChangeRequest,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        mut symbols: SymbolCache,
        preparation: CataloguePreparation,
        preparation_inputs: CatalogueRasterPreparation,
    ) -> Result<PreparedPortrayalChange> {
        let CatalogueRasterPreparation {
            registry,
            raster: raster_preparation,
        } = preparation_inputs;
        if matches!(raster_preparation, RasterPreparation::CallerStagedRaster) {
            anyhow::ensure!(
                Arc::ptr_eq(fc, &self.fc) && Arc::ptr_eq(pc, &self.pc),
                "Caller-staged raster requires the current primary catalogues"
            );
            anyhow::ensure!(
                request.profile == self.current_profile_name && self.active_ic().is_none(),
                "Caller-staged raster requires unchanged regular S102 portrayal"
            );
            anyhow::ensure!(
                request.settings.safety_contour.to_bits()
                    == self.applied_settings.safety_contour.to_bits()
                    && request.settings.shallow_contour.to_bits()
                        == self.applied_settings.shallow_contour.to_bits()
                    && request.settings.deep_contour.to_bits()
                        == self.applied_settings.deep_contour.to_bits()
                    && request.settings.two_shades == self.applied_settings.two_shades
                    && request.settings.display_mode == self.applied_settings.display_mode,
                "Caller-staged raster cannot change S102 settings"
            );
            anyhow::ensure!(
                self.renderer
                    .as_ref()
                    .is_some_and(|r| r.settings() == &request.settings),
                "Caller-staged raster cannot bypass pending display settings"
            );
        }
        if pc.source_digest() != self.pc.source_digest() {
            request.settings = primary_viewing_layers::for_new_primary(pc, &request.settings);
        }
        let catalogue_registry = match registry {
            Some(registry) => self.reconcile_catalogue_registry(registry, fc, pc)?,
            None => self.prepare_catalogue_registry(fc, pc, false)?,
        };
        let owned = self.requires_cell_resources(&catalogue_registry, fc, pc)?;
        let mut cell_resources = if owned {
            Some(self.prepare_cell_resources(&catalogue_registry, &request)?)
        } else {
            None
        };
        let (preserve_view, preflight_resources) = match preparation {
            CataloguePreparation::RetainedView => (true, false),
            CataloguePreparation::Loaded { preserve_view } => (preserve_view, true),
        };
        let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
        anyhow::ensure!(
            request.settings.show_shallow_pattern || symbols.shallow_pattern_contract().is_some(),
            "Current PC has no supported independent shallow-pattern selector; previous portrayal retained"
        );
        let (pan, zoom, _) = renderer.fast_view_transform();
        anyhow::ensure!(
            pan == (0., 0.) && zoom == 1. && renderer.fast_view_scales().1 == 1.,
            "Finish current affine navigation before changing portrayal"
        );
        let profile = pc
            .color_profiles
            .profiles
            .get(&request.profile)
            .context("Requested colour profile not found")?;
        validated_lua_context(pc, Some(&request.settings))?;
        ferrite_s101::validate_catalogue_pair(fc, pc)?;
        if preflight_resources {
            for cell in &self.cells {
                let owner = catalogue_registry
                    .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                    .context("Missing validated cell catalogue owner")?;
                ferrite_s101::validate_dataset_catalogues(
                    &cell.dsid,
                    owner.fc(),
                    &owner.pc().product_id,
                    &owner.pc().version,
                )?;
                ferrite_s101::coverage_scale::dataset_reference_scale(cell)?;
            }
        }
        let mut next = self.render_context.empty_for_rebuild();
        if !self.cells.is_empty() {
            if owned {
                self.portray_cells_with_owners(&catalogue_registry, &mut next, &request)?;
            } else {
                try_lua_portrayal(
                    &self.cells,
                    fc,
                    pc,
                    &mut next,
                    &request.profile,
                    Some(&request.settings),
                )?;
            }
        }
        let active_ic = if request.settings.interoperability_enabled {
            self.ic.clone()
        } else {
            None
        };
        let ic_changed = if let Some(ic) = active_ic.as_ref() {
            if owned {
                let fcs = self
                    .cells
                    .iter()
                    .map(|cell| {
                        catalogue_registry
                            .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                            .map(|owner| owner.fc().as_ref())
                            .context("Missing IC cell FC owner")
                    })
                    .collect::<Result<Vec<_>>>()?;
                interoperability::compose_vectors_for_cells(ic, &self.cells, &fcs, &mut next)?
            } else {
                interoperability::compose_vectors(ic, &self.cells, fc, &mut next)?
            }
        } else {
            0
        };
        let base_instruction_count = next.instruction_count();
        for mut instruction in self.plugin_system.get_render_instructions() {
            instruction.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
            next.add_instruction(instruction);
        }
        if preserve_view {
            next.scaler = self.render_context.scaler.clone();
        } else {
            next.set_bounds(self.bounds);
            let (x, y, w, h) = renderer.chart_viewport_pixels();
            next.set_viewport_rect(x, y, w, h);
            next.zoom_to_fit(self.bounds);
        }
        let coverage = Arc::new(
            ferrite_s101::coverage_projection::GeographicCoverageInventory::from_cells(
                &self.cells,
            )?,
        );
        prepare_flat_coverage(
            Some(coverage.as_ref()),
            &mut next,
            renderer.window().inner_size(),
            renderer.window().scale_factor(),
        )?;
        let raster_staging = if matches!(raster_preparation, RasterPreparation::Rebuild) {
            // An ordinary S101-only installation must not depend on an S102 PC.
            let (portrayal, raster_groups) = if self.bathymetry.is_empty() {
                (None, Some(std::collections::HashSet::new()))
            } else {
                let pc = self
                    .s102_pc
                    .as_ref()
                    .context("S-102 PC unavailable; see Catalogue status")?
                    .clone();
                let groups = Self::bathymetry_viewing_groups(&pc, request.settings.display_mode)?;
                (
                    Some(BathymetryPortrayal::from_bound_catalogue(
                        Arc::clone(&pc),
                        &request.profile,
                        DepthSettings {
                            safety_contour: request.settings.safety_contour,
                            shallow_contour: request.settings.shallow_contour,
                            deep_contour: request.settings.deep_contour,
                            four_shades: !request.settings.two_shades,
                        },
                    )?),
                    Some(groups),
                )
            };
            if !self.bathymetry.is_empty() {
                self.s102_raster_rebuild_count = self.s102_raster_rebuild_count.saturating_add(1);
            }
            let edge = self.bathymetry_tile_edge()?;
            let mut batch = self
                .renderer
                .as_mut()
                .context("Renderer not initialized")?
                .stage_s102_raster_batch(&next.scaler, |upload| {
                    for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0) {
                        let (path, first, _) = &group[0];
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
                        let numeric: &dyn NumericCoverageSource =
                            if group.len() == 1 && policy.identity(first.vertical_datum) {
                                first
                            } else {
                                &mosaic
                            };
                        let assignment = active_ic
                            .as_ref()
                            .map(|ic| first.interoperability_assignment(&ic.catalogue))
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
                            let layer = if active_ic.is_none() {
                                ferrite_wgpu::S102RasterMaterial::Bound(
                                    portrayal
                                        .as_ref()
                                        .context("Missing staged S102 portrayal")?
                                        .bound_raster_window(
                                            numeric,
                                            window,
                                            format!(
                                                "{}:composed:{}:{}",
                                                path.display(),
                                                window.column,
                                                window.row
                                            ),
                                        )?,
                                )
                            } else {
                                ferrite_wgpu::S102RasterMaterial::LegacyIc(
                                    interoperability::compose_raster(
                                        portrayal
                                            .as_ref()
                                            .context("Missing staged S102 portrayal")?
                                            .raster_window(
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
                                    )?,
                                )
                            };
                            upload(layer)?;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })?;
            let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
            renderer.reproject_raster_material_batch(&mut batch, &next.scaler)?;
            let raster = renderer.prepare_raster_material_publication(batch, true, None)?;
            Some((raster, raster_groups))
        } else {
            None
        };

        let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
        let preset = match request.settings.display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        let mut groups = ferrite_s101::viewing_groups_for_preset(pc, preset)?;
        groups.extend(primary_viewing_layers::resolve(pc, &request.settings)?);
        groups.insert(21010);
        if let Some(resources) = &mut cell_resources {
            for (index, _) in self.cells.iter().enumerate() {
                let mut subset = next.empty_for_rebuild();
                subset.set_instructions_from_cache(
                    next.raw_instructions()
                        .iter()
                        .filter(|instruction| instruction.cell_index() == u32::try_from(index).ok())
                        .cloned()
                        .collect(),
                );
                let resolved = resources.resolve_mut(Some(index))?;
                preflight_candidate_symbols(
                    &subset,
                    resolved.cache,
                    resolved.profile,
                    renderer.raster_texture_limit(),
                )?;
            }
            anyhow::ensure!(
                next.raw_instructions()
                    .iter()
                    .all(|instruction| instruction.cell_index().is_some()),
                "Mixed-PC overlays require explicit host resource ownership"
            );
            renderer.preflight_owned_annotation(&next, resources)?;
            resources.seal_viewing_groups()?;
        } else if preflight_resources {
            preflight_candidate_symbols(
                &next,
                &mut symbols,
                profile,
                renderer.raster_texture_limit(),
            )?;
        }
        // Fresh private cache and actual candidate profile, never clear live GPU caches.
        let scene = if let Some((raster, raster_groups)) = raster_staging {
            let scene =
                renderer.prepare_raster_scene_publication_with_groups(raster, raster_groups)?;
            renderer.validate_raster_scene_publication(&scene)?;
            Some(scene)
        } else {
            None
        };
        anyhow::ensure!(
            !self.publication_test_fail_before_commit,
            "Injected portrayal change failure after complete scene staging"
        );
        let background = lookup_pc_color(pc, "DEPDW", &request.profile);
        Ok(PreparedPortrayalChange {
            base_instruction_count,
            portrayal: PreparedPortrayal {
                catalogue_registry,
                cell_resources,
                context: next,
                coverage,
                ic_changed,
                raster_scene: scene,
            },
            symbols,
            request,
            background,
        })
    }
    /// All Result-returning work finishes before installation. UI controls are
    /// restored while requests wait; target controls change only here. No repair
    /// update_view follows this successful commit.
    fn commit_portrayal_change(&mut self, prepared: PreparedPortrayalChange) -> Result<()> {
        anyhow::ensure!(
            !self.navigation_scene_pending.dirty(),
            "Cannot commit private portrayal capsule with pending navigation"
        );
        self.renderer
            .as_ref()
            .context("Renderer not initialized")?
            .validate_raster_scene_publication(
                prepared
                    .portrayal
                    .raster_scene
                    .as_ref()
                    .context("Missing prepared scene")?,
            )?;
        self.install_portrayal_change(prepared);
        Ok(())
    }
    /// No Result-returning operation follows durable publication checks.
    fn install_portrayal_change(&mut self, prepared: PreparedPortrayalChange) {
        let PreparedPortrayalChange {
            base_instruction_count,
            portrayal,
            symbols,
            request,
            background,
        } = prepared;
        self.current_profile_name = request.profile;
        self.applied_settings = request.settings.clone();
        self.symbol_cache = symbols;
        let renderer = self.renderer.as_mut().expect("Validated renderer");
        // Invalidate old palette resources only inside the infallible commit.
        // The staged scene owns independent new resources. This helper changes
        // neither raster epoch nor view binding, so the following capsule remains valid.
        renderer.clear_symbol_textures();
        renderer.set_settings(request.settings);
        renderer.set_color_profile(&self.current_profile_name);
        renderer.background_color = background;
        self.publish_instructions(portrayal);
        self.base_instruction_count = base_instruction_count;
        if let Some(renderer) = &mut self.renderer {
            renderer.update_selection(&self.render_context.scaler);
            renderer.precompute_triangulations(&self.render_context);
        }
        self.update_interoperability_status();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Opt-in hidden diagnostic at the settled screenshot gate; first failure
    /// preserves prior frame, then retries the same request.
    fn audit_portrayal_change_recovery(&mut self, output: &Path, profile: &str) -> Result<()> {
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused audit forbidden"
        );
        anyhow::ensure!(!output.exists(), "Audit destination must be new");
        fs::create_dir_all(output)?;
        let model = self.publication_model()?;
        let old_profile = self.current_profile_name.clone();
        let old_background = self.renderer.as_ref().unwrap().background_color;
        let old_revision = self.symbol_cache.resource_revision();
        let old_geometry_revision = self.render_context.geometry_revision();
        let old_relation_epoch = self.render_context.static_line_relation_epoch();
        let settings = self.applied_settings.clone();
        let mut target_settings = settings.clone();
        if let Ok(value) = std::env::var("FERRITE_ROOT_PORTRAYAL_CHANGE_SAFETY") {
            target_settings.safety_contour =
                value.parse().context("Invalid target safety contour")?;
        }
        if let Ok(value) = std::env::var("FERRITE_ROOT_PORTRAYAL_CHANGE_PATTERN") {
            anyhow::ensure!(
                value == "0" || value == "1",
                "Pattern audit flag must be0or1"
            );
            target_settings.show_shallow_pattern = value == "1";
        }
        let old_scaler = format!("{:?}", self.render_context.scaler);
        let old_selection = format!(
            "{:?}",
            self.renderer.as_ref().unwrap().ui_state.selected_feature
        );
        self.audit_portrayal(&output.join("before"))?;
        anyhow::ensure!(
            profile != old_profile,
            "Recovery audit requires a different candidate profile"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("before.png"))?;
        let previous_fault = self.publication_test_fail_before_commit;
        self.publication_test_fail_before_commit = true;
        let attempt = self.prepare_portrayal_change(PortrayalChangeRequest {
            profile: profile.into(),
            settings: target_settings.clone(),
        });
        self.publication_test_fail_before_commit = previous_fault;
        match attempt {
            Ok(_) => anyhow::bail!("Injected scene failure was accepted"),
            Err(error) => anyhow::ensure!(
                format!("{error:#}")
                    .contains("Injected portrayal change failure after complete scene staging"),
                "Candidate failed before the intended late stage: {error:#}"
            ),
        }
        anyhow::ensure!(
            self.publication_model()? == model
                && self.current_profile_name == old_profile
                && self.symbol_cache.resource_revision() == old_revision
                && self.render_context.geometry_revision() == old_geometry_revision
                && self.render_context.static_line_relation_epoch() == old_relation_epoch
                && self.renderer.as_ref().unwrap().background_color == old_background,
            "Rejected candidate mutated retained portrayal"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("rejected.png"))?;
        self.audit_portrayal(&output.join("rejected"))?;
        anyhow::ensure!(
            fs::read(output.join("before.png"))? == fs::read(output.join("rejected.png"))?,
            "Rejected candidate changed direct rendered chart pixels"
        );
        let retry = self.prepare_portrayal_change(PortrayalChangeRequest {
            profile: profile.into(),
            settings: target_settings.clone(),
        })?;
        self.commit_portrayal_change(retry)?;
        self.renderer.as_mut().unwrap().render()?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("retry.png"))?;
        self.audit_portrayal(&output.join("retry"))?;
        anyhow::ensure!(
            self.current_profile_name == profile
                && self.applied_settings == target_settings
                && format!("{:?}", self.render_context.scaler) == old_scaler
                && format!(
                    "{:?}",
                    self.renderer.as_ref().unwrap().ui_state.selected_feature
                ) == old_selection,
            "Committed target profile/settings changed camera or selection"
        );
        let _renderer = self.renderer.as_ref().unwrap();

        fs::write(
            output.join("recovery.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
            "hidden":true,"focused":false,"old_frame_preserved":true,"retry_committed":true,
            "old_geometry_revision":old_geometry_revision,"retry_geometry_revision":self.render_context.geometry_revision(),
            "old_relation_epoch":format!("{:?}",old_relation_epoch),"retry_relation_epoch":format!("{:?}",self.render_context.static_line_relation_epoch()),
            "relation_reuse_admitted":self.render_context.static_line_relation_epoch()==old_relation_epoch,
            "profile":self.current_profile_name,"settings":format!("{:?}",self.applied_settings),
            "scaler":format!("{:?}",self.render_context.scaler),"selection":old_selection,
            "error":"Injected portrayal change failure after complete scene staging",
            "postcommit_update_view":false,
            "scope":"opt-in hidden audit; rawbuffers are flat CPU geometry, Flat geometry only; target-palette oracle requires external control"}))?,
        )?;
        Ok(())
    }

    fn recolor_bathymetry(&mut self) -> Result<()> {
        if self.bathymetry.is_empty() {
            return Ok(());
        }
        let request = PortrayalChangeRequest {
            profile: self.current_profile_name.clone(),
            settings: self.applied_settings.clone(),
        };
        let prepared = self.prepare_portrayal_change(request)?;
        self.commit_portrayal_change(prepared)
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
                let mut info = format!(
                    "S-102 depth: {:.2} m\nCell depth uncertainty: {uncertainty}\nContainer horizontal coordinate uncertainty: {}\nContainer vertical coordinate uncertainty: {}\nVertical CRS: {} / datum code: {}\n{}",
                    selected.adjusted_depth,
                    coordinate_uncertainty(c.horizontal_position_uncertainty),
                    coordinate_uncertainty(c.vertical_position_uncertainty),
                    if c.vertical_cs_reinterpreted_as_depth {
                        format!("{} (declared; read as depth-down, compatibility mode)", c.vertical_crs)
                    } else {
                        c.vertical_crs.to_string()
                    },
                    c.vertical_datum,
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
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
                info.push('\n');
                info.push_str(&quality);
                return Ok(Some(info));
            }
        }
        Ok(missing)
    }

    /// Exercise the same coverage query used by mouse picking, without OS input.
    fn audit_interoperability(&self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            !self.navigation_scene_pending.dirty(),
            "Read-only audit requires flushed navigation scene"
        );
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
    /// Explicit audit only: request interpretation, not complete GPU permission proof.
    fn optional_layer_policy_audit(&self) -> Result<serde_json::Value> {
        anyhow::ensure!(
            self.cells.len() <= 4096,
            "Optional layer audit cell budget exceeded"
        );
        let digest = |d: &[u8; 32]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let preset = match self.applied_settings.display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        let sorted = |groups: std::collections::HashSet<u32>| {
            let mut ids = groups.into_iter().collect::<Vec<_>>();
            ids.sort_unstable();
            ids
        };
        let mut cells = Vec::with_capacity(self.cells.len());
        for (index, cell) in self.cells.iter().enumerate() {
            let dataset_key = Self::catalogue_dataset_key(&cell.dsid)?;
            let owner = self
                .catalogue_registry
                .owner_for(&dataset_key)
                .context("Optional layer audit missing actual dataset catalogue owner")?;
            let optional = primary_viewing_layers::resolve(owner.pc(), &self.applied_settings)?;
            let own_preset = ferrite_s101::viewing_groups_for_preset(owner.pc(), preset)?;
            let mut effective = own_preset.clone();
            effective.extend(optional.iter().copied());
            anyhow::ensure!(
                effective.len() <= 65536,
                "Optional layer audit group budget exceeded"
            );
            let mut foundation = owner.pc().foundation_mode.to_vec();
            foundation.sort_unstable();
            cells.push(serde_json::json!({
                "cell_index":index,"dataset_key":dataset_key,
                "product":owner.pc().product_id,"version":owner.pc().version,
                "pc_source_digest":digest(owner.pc().source_digest()),
                "optional_groups":sorted(optional),"own_preset_groups":sorted(own_preset),
                "foundation_groups":foundation,"effective_groups":sorted(effective)
            }));
        }
        Ok(serde_json::json!({
            "scope":"primary-PC-bound optional intent and per-cell resolved CPU policy; not all GPU permissions",
            "primary_pc_source_digest":digest(self.pc.source_digest()),
            "applied_owner":self.applied_settings.viewing_layer_owner.as_ref().map(digest),
            "lexical_ids":self.applied_settings.viewing_layers.iter().collect::<Vec<_>>(),
            "primary_product":self.pc.product_id,"primary_version":self.pc.version,
            "display_mode":format!("{:?}",self.applied_settings.display_mode),"cells":cells
        }))
    }

    fn audit_portrayal(&self, directory: &Path) -> Result<()> {
        anyhow::ensure!(
            !self.navigation_scene_pending.dirty(),
            "Read-only audit requires flushed navigation scene"
        );
        fs::create_dir_all(directory)?;
        let renderer = self.renderer.as_ref().context("Renderer not initialized")?;
        let instructions = bincode::serialize(self.render_context.raw_instructions())?;
        if ferrite_wgpu::audit_digest_only_enabled() {
            fs::write(
                directory.join("instructions.sha256.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "byte_len":instructions.len(),"sha256":format!("{:x}",Sha256::digest(&instructions)),"raw_buffer_retained":false
                }))?,
            )?;
        } else {
            fs::write(directory.join("instructions.bin"), &instructions)?;
        }
        let optional_layer_policy = self.optional_layer_policy_audit()?;
        fs::write(
            directory.join("snapshot.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "instruction_count": self.render_context.instruction_count(),
                "instructions_sha256": format!("{:x}",Sha256::digest(&instructions)),
                "displayed_symbols": renderer.displayed_symbols(),
                "settings": format!("{:?}",self.render_context.settings),
                "applied_settings": format!("{:?}",self.applied_settings),
                "optional_layer_policy": optional_layer_policy,
                "digital_signature_verification_enabled": renderer.ui_state.verify_dataset_signatures,
                "digital_signature_status": renderer.ui_state.security_status,
                "verified_dataset_count": self.verified_count,
                "unsigned_evaluation_dataset_count": self.unsigned_count,
                "profile": self.current_profile_name,
                "viewport": renderer.chart_viewport_pixels(),
                "camera_identity": self.render_context.scaler.flat_encoded_identity(),
                "camera_transform": self.render_context.scaler.flat_transform(),
                "camera_pan": self.pan_offset,
                "camera_data_bounds": [self.bounds.min_x, self.bounds.min_y, self.bounds.max_x, self.bounds.max_y],
                "zoom": self.zoom_level,
                "render_stats": format!("{:?}",renderer.statistics()),
                "area_triangulation_reuse": renderer.area_triangulation_reuse_statistics(),
                "overscale_program_reuse": renderer.overscale_program_statistics(),
                "flat_coverage_binding_cache": self.coverage_inventory.as_ref().map(|i|i.flat_binding_cache_statistics()),
                "flat_source_northing_cache": self.coverage_inventory.as_ref().map(|i|i.flat_northing_cache_statistics()),
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
        renderer.audit_accepted_screen_line_packet(
            directory,
            &self.render_context,
            &instructions,
        )?;
        Ok(())
    }

    fn audit_bathymetry(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
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
        // Combined publication already owns a current-view pane; no repair rebuild.
        if let Some(_r) = &self.renderer {
            anyhow::ensure!(true, "Recolor audit could not restore the Flat view");
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

    fn refresh_dataset_tree(&mut self) {
        use ferrite_wgpu::{
            DatasetCatalogueLayer, DatasetLayerEntry, DatasetLayerId, DatasetProductLayer,
        };
        let mut files = Vec::with_capacity(self.cells.len());
        let mut catalogue_bindings = Vec::with_capacity(self.cells.len());
        for (index, cell) in self.cells.iter().enumerate() {
            let Ok((product, name)) = s101_update_plan::dataset_key(&cell.dsid) else {
                continue;
            };
            let source = self
                .loaded_chain_paths
                .get(index)
                .and_then(|paths| paths.first())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            if let Some(owner) = self.catalogue_owner_for_cell(index) {
                if let Ok(cell_index) = u32::try_from(index) {
                    catalogue_bindings.push(ferrite_wgpu::DatasetCatalogueBinding {
                        cell_index: Some(cell_index),
                        id: DatasetLayerId::S101 {
                            product: product.clone(),
                            name: name.clone(),
                        },
                        source: cell.file_path.display().to_string(),
                        fc: validate_fc(owner.fc(), &owner.fc().source_path),
                        pc: validate_pc(owner.pc(), &owner.pc().root_path),
                    });
                }
            }
            files.push(DatasetLayerEntry {
                id: DatasetLayerId::S101 { product, name },
                name: cell.dsid.dataset_name.clone(),
                source,
                detail: format!(
                    "Edition {} · update {} · {} features",
                    cell.dsid.edition_number,
                    cell.dsid.update_number,
                    cell.features.len()
                ),
            });
        }
        let mut raster_files = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (path, _, _) in &self.bathymetry {
            if seen.insert(path.clone()) {
                raster_files.push(DatasetLayerEntry {
                    id: DatasetLayerId::S102(path.clone()),
                    name: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    source: path.display().to_string(),
                    detail: "S-102 bathymetry".into(),
                });
            }
        }
        let native_files = self.native_s421.dataset_rows();
        let native_catalogue = self.native_s421.catalogue_layer();
        for row in &native_files {
            catalogue_bindings.push(ferrite_wgpu::DatasetCatalogueBinding {
                cell_index: None,
                id: row.id.clone(),
                source: row.source.clone(),
                fc: native_catalogue.fc.clone(),
                pc: native_catalogue.pc.clone(),
            });
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.dataset_catalogue_bindings = catalogue_bindings;
            renderer.ui_state.catalogue_layers = vec![
                DatasetCatalogueLayer {
                    product: "S-101".into(),
                    fc: self.fc_status.clone(),
                    pc: self.pc_status.clone(),
                },
                DatasetCatalogueLayer {
                    product: "S-102".into(),
                    fc: self.s102_fc_status.clone(),
                    pc: self.s102_pc_status.clone(),
                },
            ];
            renderer.ui_state.dataset_layers = vec![
                DatasetProductLayer {
                    product: "S-101".into(),
                    fc: format!("FC {} · {}", self.fc.version, self.fc.source_path.display()),
                    pc: format!("PC {} · {}", self.pc.version, self.pc.root_path.display()),
                    files,
                },
                DatasetProductLayer {
                    product: "S-102".into(),
                    fc: format!(
                        "FC {} · {}",
                        self.s102_fc_status.version, self.s102_fc_status.path
                    ),
                    pc: format!("PC · {}", self.s102_pc_path.display()),
                    files: raster_files,
                },
            ];
            renderer
                .ui_state
                .catalogue_layers
                .push(native_catalogue.clone());
            renderer.ui_state.dataset_layers.push(DatasetProductLayer {
                product: "S-421".into(),
                fc: format!(
                    "FC {} · {}",
                    native_catalogue.fc.version, native_catalogue.fc.path
                ),
                pc: format!(
                    "PC {} · {} · host illustration",
                    native_catalogue.pc.version, native_catalogue.pc.path
                ),
                files: native_files,
            });
            if !renderer
                .ui_state
                .dataset_layers
                .iter()
                .flat_map(|p| &p.files)
                .any(|f| renderer.ui_state.selected_dataset.as_ref() == Some(&f.id))
            {
                renderer.ui_state.selected_dataset = None;
            }
        }
        self.service_mcp_ui();
    }

    fn unload_dataset(&mut self, id: ferrite_wgpu::DatasetLayerId) -> Result<()> {
        anyhow::ensure!(
            self.loading_state.is_none()
                && self.dataset_open_discovery.is_none()
                && self.dataset_open_session.is_none(),
            "Wait for dataset loading to finish before unloading"
        );
        anyhow::ensure!(
            !self.zoom_animating
                && !self.is_dragging
                && self.zoom_rebuild_phase == 0
                && self.pan_rebuild_phase == 0
                && self.pending_portrayal_change.is_none(),
            "Finish navigation or portrayal changes before unloading"
        );
        if let ferrite_wgpu::DatasetLayerId::S421 { route_id } = &id {
            self.ensure_navigation_scene()?;
            self.native_s421.unload_route(*route_id)?;
            self.native_s421_ui_revision = 0;
            if let Some(renderer) = &mut self.renderer {
                renderer.invalidate_native_route_gpu();
                renderer.ui_state.selected_dataset = None;
            }
            self.refresh_dataset_tree();
            return Ok(());
        }
        // Obtain readiness before removing an owned cell/raster or preparing its replacement.
        self.ensure_navigation_scene()?;
        let request = PortrayalChangeRequest {
            profile: self.current_profile_name.clone(),
            settings: self.applied_settings.clone(),
        };
        match id {
            ferrite_wgpu::DatasetLayerId::S101 { product, name } => {
                let key = (product, name);
                let index = self
                    .cells
                    .iter()
                    .position(|cell| {
                        s101_update_plan::dataset_key(&cell.dsid).is_ok_and(|k| k == key)
                    })
                    .context("Dataset is no longer loaded")?;
                anyhow::ensure!(
                    self.cells.len() == self.loaded_source_identities.len()
                        && self.cells.len() == self.loaded_chain_paths.len(),
                    "Loaded S-101 ownership is not aligned"
                );
                let cell = self.cells.remove(index);
                let identity = self.loaded_source_identities.remove(index);
                let paths = self.loaded_chain_paths.remove(index);
                let staged = self
                    .prepare_portrayal_change(request)
                    .and_then(|prepared| self.commit_portrayal_change(prepared));
                if let Err(error) = staged {
                    self.cells.insert(index, cell);
                    self.loaded_source_identities.insert(index, identity);
                    self.loaded_chain_paths.insert(index, paths);
                    return Err(error);
                }
                self.loaded_discovery.remove(&key);
            }
            ferrite_wgpu::DatasetLayerId::S421 { .. } => {
                anyhow::bail!("Native route dispatch state changed")
            }
            ferrite_wgpu::DatasetLayerId::S102(path) => {
                anyhow::ensure!(
                    self.bathymetry.iter().any(|(loaded, _, _)| *loaded == path),
                    "Dataset is no longer loaded"
                );
                let mut removed = Vec::new();
                let retained = std::mem::take(&mut self.bathymetry)
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, value)| {
                        if value.0 == path {
                            removed.push((index, value));
                            None
                        } else {
                            Some(value)
                        }
                    })
                    .collect();
                self.bathymetry = retained;
                let staged = self
                    .prepare_portrayal_change(request)
                    .and_then(|prepared| self.commit_portrayal_change(prepared));
                if let Err(error) = staged {
                    for (index, value) in removed {
                        self.bathymetry.insert(index, value);
                    }
                    return Err(error);
                }
                // Close removed HDF handles before releasing their private files,
                // including on Windows where open-file deletion is restricted.
                drop(removed);
                self.bathymetry_bounds.remove(&path);
                self.depth_policies.remove(&path);
                self.depth_inputs.remove(&path);
                self.s102_original_inputs.remove(&path);
            }
        }
        // Manual unloading is in-memory only: never create cancellation history.
        self.select_feature(None);
        self.pending_hit_test = None;
        self.chart_loaded = !self.cells.is_empty() || !self.bathymetry.is_empty();
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.selection_candidates.clear();
            renderer.ui_state.selection_requested = None;
            renderer.ui_state.chart_count = self.cells.len() + self.bathymetry.len();
            renderer.ui_state.bathymetry_count = self.bathymetry.len();
            renderer.ui_state.security_status = "Retained datasets keep their original verification results; dataset manually unloaded".into();
            renderer.ui_state.feature_count =
                self.cells.iter().map(|cell| cell.features.len()).sum();
            renderer.ui_state.loaded_chart = self
                .cells
                .first()
                .map(|cell| cell.dsid.dataset_name.clone());
            renderer.ui_state.selected_dataset = None;
            renderer.ui_state.notice = Some("Dataset unloaded".into());
        }
        self.refresh_dataset_tree();
        self.refresh_visible_selection();
        Ok(())
    }

    fn begin_dataset_open(&mut self, path: PathBuf, folder: bool) -> Result<()> {
        anyhow::ensure!(
            self.loading_state.is_none()
                && self.dataset_open_discovery.is_none()
                && self.dataset_open_session.is_none(),
            "Dataset loading is already in progress; wait until it finishes"
        );
        self.ensure_navigation_scene()?;
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("dataset-discovery".into())
            .spawn(move || {
                let plan = if folder {
                    dataset_open_plan::discover(&path)
                } else {
                    dataset_open_plan::single_file(&path)
                };
                let _ = sender.send(plan);
            })?;
        self.dataset_open_discovery = Some(receiver);
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.notice = Some("Discovering datasets…".into());
        }
        Ok(())
    }

    fn dataset_open_failure(&mut self, message: String) {
        if let Some(session) = &mut self.dataset_open_session {
            session.chart_failed = true;
            session.notices.push(message);
        }
    }

    fn poll_dataset_open(&mut self) {
        if let Some(receiver) = &self.dataset_open_discovery {
            let plan = match receiver.try_recv() {
                Ok(plan) => Some(plan),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!(
                    "Dataset discovery worker stopped before returning a result"
                ))),
            };
            if let Some(plan) = plan {
                self.dataset_open_discovery = None;
                match plan {
                    Ok(plan) => {
                        self.dataset_open_session = Some(DatasetOpenSession {
                            routes: plan.routes.into(),
                            rasters: plan.rasters.into(),
                            charts: Some(plan.charts),
                            selected_charts: plan.selected_charts,
                            notices: plan.notices,
                            ..Default::default()
                        });
                    }
                    Err(error) => {
                        let message = format!("Could not open datasets: {error:#}");
                        error!("{message}");
                        if let Some(renderer) = &mut self.renderer {
                            renderer.ui_state.notice = Some(message);
                        }
                    }
                }
            }
        }
        if self.loading_state.is_some() {
            return;
        }
        let Some(mut session) = self.dataset_open_session.take() else {
            return;
        };
        // Independent native route imports use captured bytes, not a path reopen.
        // Operational policy is checked before duplicate admission or parsing.
        if let Some(input) = session.routes.pop_front() {
            let before = self.native_s421.dataset_rows().len();
            match self
                .native_s421
                .import_captured(&input, self.require_signatures)
            {
                Ok(id) => {
                    if self.native_s421.dataset_rows().len() > before {
                        session.routes_loaded += 1;
                    } else {
                        session.routes_existing += 1;
                    }
                    info!(
                        "S421 route {} opened: {} · {:?} · SHA256 {:?}; unverified bare input",
                        id,
                        input.path().display(),
                        input.profile(),
                        input.sha256()
                    );
                    self.native_s421_ui_revision = 0;
                }
                Err(error) => {
                    session.routes_failed += 1;
                    let message = format!("S421 {}: {error:#}", input.path().display());
                    tracing::warn!("{message}");
                    session.notices.push(message);
                }
            }
            self.refresh_dataset_tree();
            self.dataset_open_session = Some(session);
            return;
        }
        // Each raster is transactional. A damaged/unsupported delivery cannot
        // prevent the remaining independent files from being attempted.
        if let Some(path) = session.rasters.pop_front() {
            let previous = self.bathymetry.len();
            match self.load_bathymetry(std::slice::from_ref(&path)) {
                Ok(()) if self.bathymetry.len() > previous => session.raster_loaded += 1,
                Ok(()) => session.raster_existing += 1,
                Err(error) => {
                    session.raster_failed += 1;
                    let message = format!("{}: {error:#}", path.display());
                    error!("{message}");
                    session.notices.push(message);
                }
            }
            self.refresh_dataset_tree();
            self.dataset_open_session = Some(session);
            return;
        }
        if let Some(charts) = session.charts.take() {
            if !charts.is_empty() {
                session.chart_started = true;
                self.dataset_open_session = Some(session);
                let selected = self
                    .dataset_open_session
                    .as_ref()
                    .map(|s| s.selected_charts.clone())
                    .unwrap_or_default();
                if let Err(error) = self.load_charts_with_selection(&charts, &selected) {
                    self.dataset_open_failure(format!("S-101 batch: {error:#}"));
                }
                return;
            }
        }
        for notice in &session.notices {
            tracing::warn!("Dataset open: {notice}");
        }
        let summary = if !session.chart_started
            && session.raster_loaded == 0
            && session.raster_existing == 0
            && session.raster_failed == 0
            && session.routes_loaded == 0
            && session.routes_existing == 0
            && session.routes_failed == 0
            && session.notices.len() == 1
        {
            session.notices[0].clone()
        } else {
            session.summary(self.cells.len())
        };
        if ferrite_wgpu::background_test::enabled() {
            if let Some(path) = std::env::var_os("FERRITE_OPEN_FOLDER_AUDIT_OUTPUT") {
                std::env::remove_var("FERRITE_OPEN_FOLDER_AUDIT_OUTPUT");
                let result = (|| -> Result<()> {
                    let value = serde_json::json!({
                        "rasters_loaded": session.raster_loaded,
                        "rasters_existing": session.raster_existing,
                        "rasters_failed": session.raster_failed,
                        "coverages": self.bathymetry.len(),
                        "chart_count": self.cells.len(),
                        "notices": session.notices,
                        "summary": summary,
                        "paths": self.bathymetry.iter().map(|(p, _, _)| p).collect::<Vec<_>>(),
                        "hidden": self.window.as_ref().and_then(|w| w.is_visible()) == Some(false),
                        "focused": self.window.as_ref().is_some_and(|w| w.has_focus()),
                        "ordinary_folder_queue": true,
                    });
                    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    self.startup_error =
                        Some(format!("Folder opening audit output failed: {error:#}"));
                }
                self.frames_since_loaded = Some(0);
            }
        }
        info!("{summary}");
        if let Some(renderer) = &mut self.renderer {
            if let Ok(mut log) = renderer.ui_state.diagnostics.try_lock() {
                log.push_notice(&summary);
            }
            renderer.ui_state.notice = Some(summary);
        }
    }

    /// Start loading chart files in background (non-blocking)
    fn load_charts(&mut self, paths: &[PathBuf]) -> Result<()> {
        self.load_charts_with_selection(paths, paths)
    }
    fn load_charts_with_selection(
        &mut self,
        paths: &[PathBuf],
        selected: &[PathBuf],
    ) -> Result<()> {
        self.ensure_navigation_scene()?;
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
            let (plans, cancellations, discovery_notices) = if selected.is_empty() {
                let (plans, notices) = s101_update_plan::authorized_folder_plans(
                    candidates,
                    &authorization,
                    require_signature,
                )?;
                (plans, Vec::new(), notices)
            } else {
                let (plans, cancellations) = s101_update_plan::authorized_batch(
                    candidates,
                    selected,
                    &authorization,
                    require_signature,
                    &loaded,
                    &history,
                )?;
                (plans, cancellations, Vec::new())
            };
            let editions = plans
                .iter()
                .map(|p| &p.base.id)
                .chain(self.cells.iter().map(|c| &c.dsid))
                .map(|id| {
                    id.product_edition
                        .parse::<ferrite_kernel::SpecificationVersion>()
                        .map(|v| v.edition)
                })
                .collect::<Result<std::collections::BTreeSet<_>>>()?;
            let mixed = editions.len() > 1;
            let candidate_registry = if mixed {
                use s101_catalogue_selection::{DatasetCatalogueOwner, DatasetCatalogueRequest};
                let fallback = Arc::new(DatasetCatalogueOwner::new(
                    Arc::clone(&self.fc),
                    Arc::clone(&self.pc),
                )?);
                let retained = self.prepare_catalogue_registry(&self.fc, &self.pc, true)?;
                let requests = plans
                    .iter()
                    .map(|plan| {
                        Ok(DatasetCatalogueRequest {
                            dataset_key: Self::catalogue_dataset_key(&plan.base.id)?,
                            source_identity: plan.captured_source_identity_sha(),
                            identification: plan.base.id.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let staged = retained.stage(&requests, fallback, &self.catalogue_inventory)?;
                for plan in &plans {
                    let key = Self::catalogue_dataset_key(&plan.base.id)?;
                    let owner = staged
                        .owner_for(&key)
                        .context("Missing planned dataset catalogue owner")?;
                    anyhow::ensure!(
                        owner
                            .pc()
                            .color_profiles
                            .profiles
                            .contains_key(&self.current_profile_name),
                        "Dataset PC has no active colour profile {}",
                        self.current_profile_name
                    );
                    validated_lua_context(owner.pc(), Some(&self.applied_settings))?;
                    for input in std::iter::once(&plan.base).chain(&plan.updates) {
                        ferrite_s101::validate_dataset_catalogues(
                            &input.id,
                            owner.fc(),
                            &owner.pc().product_id,
                            &owner.pc().version,
                        )?;
                    }
                }
                Some(staged)
            } else {
                None
            };
            let candidate_catalogues = if plans.is_empty() || mixed {
                None
            } else {
                let incoming: Vec<_> = plans
                    .iter()
                    .flat_map(|p| std::iter::once(&p.base).chain(&p.updates))
                    .map(|p| p.id.clone())
                    .collect();
                let retained: Vec<_> = self.cells.iter().map(|c| c.dsid.clone()).collect();
                match s101_catalogue_selection::resolve_pair_exact_first(
                    &incoming,
                    &retained,
                    &self.fc,
                    &self.pc,
                    &self.catalogue_inventory,
                )? {
                    s101_catalogue_selection::Selection::KeepCurrent => None,
                    s101_catalogue_selection::Selection::UseInstalled { fc, pc, notice } => {
                        anyhow::ensure!(
                            pc.color_profiles
                                .profiles
                                .contains_key(&self.current_profile_name),
                            "Compatible PC has no active colour profile {}; select an available profile before loading",
                            self.current_profile_name
                        );
                        validated_lua_context(&pc, Some(&self.applied_settings))?;
                        let fc_status = validate_fc(&fc, &fc.source_path);
                        let pc_status = validate_pc(&pc, &pc.root_path);
                        Some(CandidateCatalogues {
                            fc: Arc::new(*fc),
                            pc: Arc::new(*pc),
                            fc_status,
                            pc_status,
                            notice,
                        })
                    }
                }
            };
            Ok((
                authorization,
                plans,
                cancellations,
                candidate_catalogues,
                candidate_registry,
                discovery_notices,
            ))
        })();
        let (
            authorization,
            plans,
            cancellations,
            candidate_catalogues,
            candidate_registry,
            discovery_notices,
        ) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.restore_load_checkpoint(checkpoint);
                return Err(error);
            }
        };
        for notice in &discovery_notices {
            tracing::warn!("Dataset discovery: {notice}");
        }
        if let Some(session) = &mut self.dataset_open_session {
            session.notices.extend(discovery_notices);
        }
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
        let fc = Arc::clone(
            candidate_catalogues
                .as_ref()
                .map(|p| &p.fc)
                .unwrap_or(&self.fc),
        );
        let pc = candidate_catalogues
            .as_ref()
            .map(|p| &p.pc)
            .unwrap_or(&self.pc);
        let pc_product = pc.product_id.clone();
        let pc_version = pc.version.clone();

        let worker_registry = candidate_registry.clone();
        // Raw parser memo survives chart unload; it never retains resolved catalogue owners.
        if std::env::var_os("FERRITE_NO_CACHE").is_some() {
            if let Ok(mut cache) = self.decoded_chart_cache.lock() {
                cache.clear();
            }
        }
        let decoded_chart_cache = Arc::clone(&self.decoded_chart_cache);

        // Spawn background thread for loading
        std::thread::spawn(move || {
            let fc_feature_codes = fc.feature_type_codes();

            // Retain all authenticated snapshots throughout owned parsing.
            let _authorization = authorization;
            for plan in plans {
                let path = plan.base.original.clone();
                let result = (|| -> Result<_> {
                    let key = Self::catalogue_dataset_key(&plan.base.id)?;
                    let owner = worker_registry
                        .as_ref()
                        .map(|registry| {
                            registry
                                .owner_for(&key)
                                .context("Missing worker catalogue owner")
                        })
                        .transpose()?;
                    let plan_fc = owner.map(|o| o.fc().as_ref()).unwrap_or(fc.as_ref());
                    let (plan_pc_product, plan_pc_version) = owner
                        .map(|o| (o.pc().product_id.as_str(), o.pc().version.as_str()))
                        .unwrap_or((&pc_product, &pc_version));
                    let plan_feature_codes = owner.map(|o| o.fc().feature_type_codes());
                    let (mut cell, source_identity) =
                        plan.load_with_decoded_cache(&decoded_chart_cache)?;
                    if std::env::var("FERRITE_DECODED_CHART_CACHE_DIAGNOSTICS")
                        .is_ok_and(|v| v == "1")
                    {
                        if let Ok(cache) = decoded_chart_cache.lock() {
                            info!("Decoded chart cache: {:?}", cache.statistics());
                        }
                    }

                    if let Some(registry) = &worker_registry {
                        anyhow::ensure!(
                            registry
                                .binding_identity(&key)
                                .context("Missing parsed catalogue binding")?
                                .0
                                == *source_identity.sha256(),
                            "Materialized cell differs from catalogue-bound captured source"
                        );
                    }

                    ferrite_s101::validate_dataset_catalogues(
                        &cell.dsid,
                        plan_fc,
                        plan_pc_product,
                        plan_pc_version,
                    )
                    .with_context(|| format!("Incompatible S-101 dataset {}", path.display()))?;
                    ferrite_s101::validate_spatial_scale_properties(
                        &cell.dsid,
                        cell.features.values(),
                    )
                    .with_context(|| {
                        format!("Nonconformant S-101 spatial scales {}", path.display())
                    })?;
                    // Normalize feature codes
                    cell.normalize_feature_codes(
                        plan_feature_codes.as_ref().unwrap_or(&fc_feature_codes),
                    );

                    #[cfg(debug_assertions)]
                    {
                        let stats = cell.statistics();
                        info!(
                            "Loaded: {} features, {} points, {} curves, {} surfaces",
                            stats.features, stats.points, stats.curves, stats.surfaces
                        );
                    }

                    Ok(ChartPublicationResult::Load(Box::new(ChartLoadResult {
                        base_metadata: plan.base.metadata.clone(),
                        metadata: plan.ending_metadata().cloned(),
                        input_paths: plan.input_paths(),
                        cell,
                        source_identity,
                    })))
                })();

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
            allow_partial: selected.is_empty(),
            checkpoint,
            candidate_catalogues,
            candidate_registry,
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
                            if let Some(session) = &mut self.dataset_open_session {
                                session.chart_failed = true;
                                session.notices.push(message.clone());
                            }
                            if self.auto_screenshot.is_some() && !loading_state.allow_partial {
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
                        if let Some(session) = &mut self.dataset_open_session {
                            session.chart_failed = true;
                            session.notices.push(message.clone());
                        }
                        if self.auto_screenshot.is_some() && !loading_state.allow_partial {
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

        // A worker may finish before the first egui layout. Keep its owned
        // results pending until the real chart rectangle exists; never invent
        // a viewport or relax coverage validation to publish an empty ring.
        if completed {
            if std::env::var("FERRITE_ROOT_FORCE_FIRST_LOAD_LAYOUT_RACE").as_deref() == Ok("1") {
                std::env::remove_var("FERRITE_ROOT_FORCE_FIRST_LOAD_LAYOUT_RACE");
                assert!(
                    ferrite_wgpu::background_test::enabled(),
                    "Hidden race audit required"
                );
                if let Some(renderer) = &mut self.renderer {
                    renderer.ui_state.chart_area = (0., 0., 0., 0.);
                }
            }
            let publishable = self.loading_state.as_ref().is_some_and(|state| {
                chart_batch_publishable(state.allow_partial, state.failed, state.pending.len())
            });
            if publishable
                && !self
                    .renderer
                    .as_ref()
                    .is_some_and(|r| chart_viewport_ready(r.chart_viewport_pixels()))
            {
                if ferrite_wgpu::background_test::enabled() {
                    if let Some(path) = std::env::var_os("FERRITE_ROOT_LOAD_LAYOUT_DEFER_RECEIPT") {
                        std::env::remove_var("FERRITE_ROOT_LOAD_LAYOUT_DEFER_RECEIPT");
                        let pending = self.loading_state.as_ref().unwrap().pending.len();
                        if let Err(error) = fs::write(
                            path,
                            format!(
                                "{{\"pending_retained\":{pending},\"publication_deferred\":true}}"
                            ),
                        ) {
                            tracing::warn!("Could not record hidden layout defer audit: {error}");
                        }
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return false;
            }
        }

        if completed {
            let state = self
                .loading_state
                .take()
                .expect("Completed load state exists");
            if !chart_batch_publishable(state.allow_partial, state.failed, state.pending.len()) {
                self.dataset_open_failure("S-101 batch failed; previous charts retained (see application log for individual chain errors)".into());
                // Explicit-file batches are all-or-nothing. Folder publication
                // still installs only completely materialized logical datasets.
                self.restore_load_checkpoint(state.checkpoint);
                if let Some(renderer) = &mut self.renderer {
                    renderer.ui_state.loading_progress = None;
                }
            } else if state.checkpoint.signature_mode != self.signature_verification_enabled()
                || state.checkpoint.catalogue_identity != self.bound_catalogue_identity()
                || !self
                    .catalogue_registry
                    .matches_checkpoint(&state.checkpoint.registry_checkpoint)
                || state.checkpoint.catalogue_routes != self.catalogue_routes()
            {
                self.restore_load_checkpoint(state.checkpoint);
                self.load_error(anyhow::anyhow!("Verification policy or bound catalogues changed while loading; reopen the datasets"));
            } else {
                if let Some(session) = &mut self.dataset_open_session {
                    session.chart_partial = state.failed && state.allow_partial;
                }
                self.finalize_loading(
                    state.pending,
                    state.checkpoint,
                    state.candidate_catalogues,
                    state.candidate_registry,
                );
            }
        }
        if completed {
            self.refresh_dataset_tree();
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
            registry_checkpoint: self.catalogue_registry.checkpoint(),
            catalogue_routes: self.catalogue_routes(),
        }
    }
    fn catalogue_routes(&self) -> Vec<(Option<String>, Option<[u8; 32]>)> {
        self.cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                (
                    Self::catalogue_dataset_key(&cell.dsid).ok(),
                    self.loaded_source_identities
                        .get(index)
                        .map(|identity| *identity.sha256()),
                )
            })
            .collect()
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
                format!(
                    "Signature policy {}: retained results {} verified / {} unsigned evaluation; existing data was not re-verified",
                    if current_mode { "ON" } else { "OFF" },
                    self.verified_count,
                    self.unsigned_count
                )
            };
            renderer.ui_state.security_details = details;
        }
    }
    fn load_error(&mut self, error: anyhow::Error) {
        let message = format!("S-101 load rejected; previous chart retained: {error:#}");
        error!("{message}");
        self.dataset_open_failure(message.clone());
        if let Some(session) = &mut self.dataset_open_session {
            session.chart_partial = false;
        }
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
        candidate_catalogues: Option<CandidateCatalogues>,
        candidate_registry: Option<s101_catalogue_selection::DatasetCatalogueRegistry>,
    ) {
        if let Err(error) = self.ensure_navigation_scene() {
            self.restore_load_checkpoint(checkpoint);
            self.load_error(error);
            return;
        }
        let mut incoming = Vec::new();
        let mut removals = Vec::new();
        for operation in operations {
            match operation {
                ChartPublicationResult::Load(cell) => incoming.push(*cell),
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
                renderer.ui_state.notice = Some(format!(
                    "Recorded {cancelled_count} S-101 cancellation announcement(s); no stored content matched"
                ));
            }
            return;
        }
        let previous_bounds = self.bounds;
        if !self.cells.is_empty() || !self.bathymetry.is_empty() {
            self.bounds = chart_data_bounds(&self.cells, self.bathymetry_bounds.values().copied());
            self.bounds.expand_by_percent(0.1);
        }
        // All candidate FC/PC/VM/cache/material state stays private. The old live
        // pair and renderer resources still own the retained display on failure.
        let staged = if let Some(registry) = candidate_registry {
            let fc = Arc::clone(&self.fc);
            let pc = Arc::clone(&self.pc);
            let request = PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: self.applied_settings.clone(),
            };
            self.prepare_portrayal_change_with_registry(
                request,
                &fc,
                &pc,
                self.symbol_cache.fork_empty(),
                CataloguePreparation::Loaded {
                    preserve_view: cancelled_count > 0,
                },
                CatalogueRasterPreparation {
                    registry: Some(registry),
                    raster: RasterPreparation::Rebuild,
                },
            )
            .map(|change| PreparedLoadPortrayal::Current(Box::new(change.portrayal)))
        } else if let Some(owner) = candidate_catalogues {
            let symbols = SymbolCache::new_with_pattern_contract(
                owner.pc.root_path.join("Symbols"),
                owner.pc.sources(),
                ferrite_s101::shallow_pattern_contract(&owner.pc),
            );
            let request = PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: self.applied_settings.clone(),
            };
            self.prepare_portrayal_change_for_catalogues(
                request,
                &owner.fc,
                &owner.pc,
                symbols,
                CataloguePreparation::Loaded {
                    preserve_view: cancelled_count > 0,
                },
            )
            .map(|change| PreparedLoadPortrayal::Catalogue {
                owner: Box::new(owner),
                change: Box::new(change),
            })
        } else {
            self.prepare_instructions(cancelled_count > 0)
                .map(|p| PreparedLoadPortrayal::Current(Box::new(p)))
        };
        let prepared = match staged {
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
        let persist_result = if self.catalogue_test_fail_history {
            Err(anyhow::anyhow!(
                if ferrite_wgpu::background_test::enabled() {
                    "Injected catalogue history failure after complete material staging"
                } else {
                    "History fault requires hidden test"
                }
            ))
        } else {
            history.persist(transaction.cancellations())
        };
        if let Err(error) = persist_result {
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
        // The private portrayal intentionally fitted the new aggregate data bounds.
        // Commit camera policy with that scene, only AFTER history/preflight success.
        // Otherwise the actual fit viewport can be labelled with an old 200x zoom.
        if cancelled_count == 0 {
            self.pan_rebuild_phase = 0;
            self.zoom_level = 1.0;
            self.zoom_target = 1.0;
            self.pan_offset = (0.0, 0.0);
            self.pan_velocity = (0.0, 0.0);
            self.zoom_animating = false;
            self.zoom_rebuilt_level = 1.0;
            self.zoom_rebuild_phase = 0;
            if let Some(renderer) = &mut self.renderer {
                renderer.set_zoom_level(1.0);
                renderer.ui_state.zoom_level = 1.0;
            }
        }
        match prepared {
            PreparedLoadPortrayal::Current(portrayal) => self.publish_instructions(*portrayal),
            PreparedLoadPortrayal::Catalogue { owner, change } => {
                let owner = *owner;
                self.fc = owner.fc;
                self.pc = owner.pc;
                self.fc_status = owner.fc_status;
                self.pc_status = owner.pc_status;
                if let Some(renderer) = &mut self.renderer {
                    renderer.ui_state.fc_status = self.fc_status.clone();
                    renderer.ui_state.pc_status = self.pc_status.clone();
                    renderer.ui_state.optional_viewing_layers =
                        ferrite_s101::optional_viewing_layers(&self.pc);
                    renderer.ui_state.notice = Some(owner.notice);
                }
                self.install_portrayal_change(*change);
            }
        }
        drop(transaction.commit());
        self.loaded_discovery = next_metadata;
        self.chart_loaded = !self.cells.is_empty() || !self.bathymetry.is_empty();

        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.selected_feature = None;
            renderer.ui_state.selected_symbol_preview = None;
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
            self.apply_auto_screenshot_camera();
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
                        rx0,
                        ry0,
                        rx1,
                        ry1,
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
        if let Some(server) = &self.s100_mcp.server {
            server.clear();
        }
        self.navigation_scene_pending.authoritative_ready();
        // Drop the receiver so a previously started load cannot repopulate cleared charts.
        self.loading_state = None;
        self.dataset_open_discovery = None;
        self.dataset_open_session = None;
        if let Some(_r) = &mut self.renderer {}
        #[cfg(debug_assertions)]
        info!("Clearing all charts");

        self.verified_count = 0;
        self.unsigned_count = 0;
        self.cells.clear();
        self.loaded_source_identities.clear();
        self.loaded_chain_paths.clear();
        self.loaded_discovery.clear();
        self.bathymetry.clear();
        self.pending_portrayal_change = None;
        self.bathymetry_bounds.clear();
        self.depth_policies.clear();
        self.depth_inputs.clear();
        self.s102_original_inputs.clear();
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
            self.render_context.set_coverage_visibility_fusion_enabled(
                std::env::var("FERRITE_COVERAGE_VISIBILITY_FUSION")
                    .ok()
                    .as_deref()
                    == Some("1"),
            );
            self.render_context.settings = display_settings;
        }
        self.base_instruction_count = 0;
        self.coverage_inventory = None;

        if let Some(_renderer) = &mut self.renderer {}

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
            renderer.ui_state.selected_symbol_preview = None;
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
        // Cache trust comes from the app-private root, never delivered sidecars.
        let cache_dir = secure_instruction_cache::root()?;

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
            primary_viewing_layers::hash_identity(&pc_settings, &mut hasher);
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
    /// Bump when instruction fields or conversion admission semantics change.
    /// Schema 42 rejects older caches that could silently omit augmented geometry.
    const CACHE_SCHEMA_VERSION: u32 = 42;

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
        if data.len() as u64 > secure_instruction_cache::MAX_CACHE_BYTES {
            return Err("cache exceeds byte budget".into());
        }
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
        // Reject byte/declared top-level count budgets before Vec allocation.
        // Nested collection allocation is constrained by the encoded byte budget,
        // not an exact heap cap; this disposable cache is app-private, not input data.
        let count_bytes: [u8; 8] = payload
            .get(..8)
            .ok_or_else(|| "deserialization failed: missing instruction count".to_owned())?
            .try_into()
            .expect("eight-byte count");
        if u64::from_le_bytes(count_bytes) > secure_instruction_cache::MAX_INSTRUCTIONS {
            return Err("cache exceeds instruction budget".into());
        }
        use bincode::Options;
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(secure_instruction_cache::MAX_CACHE_BYTES)
            .reject_trailing_bytes()
            .deserialize(payload)
            .map_err(|e| format!("deserialization failed: {}", e))
    }

    fn prepare_instructions(&mut self, preserve_view: bool) -> Result<PreparedPortrayal> {
        self.prepare_instructions_internal(preserve_view)
    }
    fn prepare_instructions_internal(&mut self, preserve_view: bool) -> Result<PreparedPortrayal> {
        self.prepare_instructions_internal_with_raster(preserve_view, RasterPreparation::Rebuild)
    }
    fn prepare_instructions_internal_with_raster(
        &mut self,
        preserve_view: bool,
        raster_preparation: RasterPreparation,
    ) -> Result<PreparedPortrayal> {
        self.ensure_navigation_scene()?;
        let registry = self.prepare_catalogue_registry(&self.fc, &self.pc, true)?;
        if self.requires_cell_resources(&registry, &self.fc, &self.pc)? {
            let fc = Arc::clone(&self.fc);
            let pc = Arc::clone(&self.pc);
            let request = PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: self.applied_settings.clone(),
            };
            return self
                .prepare_portrayal_change_with_registry(
                    request,
                    &fc,
                    &pc,
                    self.symbol_cache.fork_empty(),
                    CataloguePreparation::Loaded { preserve_view },
                    CatalogueRasterPreparation {
                        registry: Some(registry),
                        raster: raster_preparation,
                    },
                )
                .map(|change| change.portrayal);
        }
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
                match secure_instruction_cache::read(cp) {
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
                            // A disposable rejection regenerates without deleting a live path.
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
                // Oversized scenes still portray correctly; only their disk cache is skipped.
                let bounded =
                    if instructions.len() as u64 > secure_instruction_cache::MAX_INSTRUCTIONS {
                        Err(Box::new(bincode::ErrorKind::Custom(
                            "cache instruction budget exceeded".into(),
                        )))
                    } else {
                        use bincode::Options;
                        bincode::DefaultOptions::new()
                            .with_fixint_encoding()
                            .with_limit(secure_instruction_cache::MAX_CACHE_BYTES - 40)
                            .serialize(instructions)
                    };
                match bounded {
                    Ok(payload) => {
                        let signed = Self::wrap_cache(&payload);
                        let size_kb = signed.len() / 1024;
                        match secure_instruction_cache::write_atomic(cp, &signed) {
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
            catalogue_registry: self.prepare_catalogue_registry(&self.fc, &self.pc, true)?,
            cell_resources: None,
            context: next,
            coverage: coverage_inventory,
            ic_changed,

            raster_scene: None,
        })
    }

    fn catalogue_dataset_key(id: &ferrite_s100_core::DatasetIdentification) -> Result<String> {
        Ok(serde_json::to_string(&s101_update_plan::dataset_key(id)?)?)
    }
    fn catalogue_owner_for_cell(
        &self,
        index: usize,
    ) -> Option<&Arc<s101_catalogue_selection::DatasetCatalogueOwner>> {
        let cell = self.cells.get(index)?;
        let key = Self::catalogue_dataset_key(&cell.dsid).ok()?;
        let identity = self.loaded_source_identities.get(index)?;
        if self.catalogue_registry.binding_identity(&key)?.0 != *identity.sha256() {
            return None;
        }
        self.catalogue_registry.owner_for(&key)
    }

    fn prepare_catalogue_registry(
        &self,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        retain_existing: bool,
    ) -> Result<s101_catalogue_selection::DatasetCatalogueRegistry> {
        use s101_catalogue_selection::{
            DatasetCatalogueOwner, DatasetCatalogueRegistry, DatasetCatalogueRequest,
        };
        let fallback = Arc::new(DatasetCatalogueOwner::new(Arc::clone(fc), Arc::clone(pc))?);
        let requests = self
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                Ok(DatasetCatalogueRequest {
                    dataset_key: Self::catalogue_dataset_key(&cell.dsid)?,
                    source_identity: *self
                        .loaded_source_identities
                        .get(index)
                        .context("Missing parsed cell identity for catalogue owner")?
                        .sha256(),
                    identification: cell.dsid.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let base = if retain_existing {
            self.catalogue_registry.clone()
        } else {
            DatasetCatalogueRegistry::default()
        };
        let mut next = base.stage(&requests, fallback, &self.catalogue_inventory)?;
        next.retain_keys(&requests.iter().map(|r| r.dataset_key.clone()).collect());
        for request in &requests {
            let binding = next
                .binding_identity(&request.dataset_key)
                .context("Missing staged catalogue owner")?;
            anyhow::ensure!(
                binding.0 == request.source_identity,
                "Catalogue owner belongs to a different parsed dataset"
            );
            let owner = next
                .owner_for(&request.dataset_key)
                .context("Missing staged catalogue pair")?;
            ferrite_s101::validate_dataset_catalogues(
                &request.identification,
                owner.fc(),
                &owner.pc().product_id,
                &owner.pc().version,
            )?;
        }
        Ok(next)
    }

    fn reconcile_catalogue_registry(
        &self,
        registry: s101_catalogue_selection::DatasetCatalogueRegistry,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
    ) -> Result<s101_catalogue_selection::DatasetCatalogueRegistry> {
        let requests = self
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                Ok(s101_catalogue_selection::DatasetCatalogueRequest {
                    dataset_key: Self::catalogue_dataset_key(&cell.dsid)?,
                    source_identity: *self
                        .loaded_source_identities
                        .get(index)
                        .context("Missing actual parsed source identity")?
                        .sha256(),
                    identification: cell.dsid.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let fallback = Arc::new(s101_catalogue_selection::DatasetCatalogueOwner::new(
            Arc::clone(fc),
            Arc::clone(pc),
        )?);
        let mut next = registry.stage(&requests, fallback, &self.catalogue_inventory)?;
        next.retain_keys(&requests.iter().map(|r| r.dataset_key.clone()).collect());
        for request in &requests {
            anyhow::ensure!(
                next.binding_identity(&request.dataset_key)
                    .is_some_and(|b| b.0 == request.source_identity),
                "Staged catalogue binding does not match published parsed source"
            );
        }
        Ok(next)
    }
    fn requires_cell_resources(
        &self,
        registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        fc: &BoundFeatureCatalogue,
        pc: &BoundPortrayalCatalogue,
    ) -> Result<bool> {
        for cell in &self.cells {
            let owner = registry
                .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                .context("Missing staged catalogue owner")?;
            if owner.fc().source_digest() != fc.source_digest()
                || owner.pc().source_digest() != pc.source_digest()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn prepare_cell_resources(
        &self,
        registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        request: &PortrayalChangeRequest,
    ) -> Result<ferrite_wgpu::CellPortrayalResources> {
        let bindings = self
            .cells
            .iter()
            .enumerate()
            .map(|(cell_index, cell)| {
                let owner = registry
                    .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                    .context("Missing cell PC resource owner")?;
                validated_lua_context(owner.pc(), Some(&request.settings))?;
                Ok(ferrite_wgpu::CellPortrayalResourceBinding {
                    cell_index,
                    catalogue: Arc::clone(owner.pc()),
                    shallow_pattern: ferrite_s101::shallow_pattern_contract(owner.pc()),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut resources =
            ferrite_wgpu::CellPortrayalResources::prepare(&bindings, &request.profile, 4096)?;
        let preset = match request.settings.display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        for (index, cell) in self.cells.iter().enumerate() {
            let owner = registry
                .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                .context("Missing cell viewing-group owner")?;
            let mut groups = ferrite_s101::viewing_groups_for_preset(owner.pc(), preset)?;
            groups.extend(primary_viewing_layers::resolve(
                owner.pc(),
                &request.settings,
            )?);
            resources.capture_viewing_groups_for_cell(index, Some(groups))?;
        }
        Ok(resources)
    }
    fn portray_cells_with_owners(
        &self,
        registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        next: &mut RenderContext,
        request: &PortrayalChangeRequest,
    ) -> Result<()> {
        let mut owners = std::collections::BTreeMap::new();
        for cell in &self.cells {
            let owner = registry
                .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                .context("Missing Lua cell catalogue owner")?;
            owners
                .entry((*owner.fc().source_digest(), *owner.pc().source_digest()))
                .or_insert_with(|| Arc::clone(owner));
        }
        let mut ordered = std::collections::BTreeMap::<u32, Vec<DrawingInstruction>>::new();
        for (digests, owner) in owners {
            let indexed = self.cells.iter().enumerate().filter(|(_, cell)| {
                Self::catalogue_dataset_key(&cell.dsid)
                    .ok()
                    .and_then(|key| registry.binding_identity(&key))
                    .is_some_and(|b| (b.1, b.2) == digests)
            });
            let mut staged = next.empty_for_rebuild();
            try_lua_portrayal_for_cells(
                indexed,
                owner.fc(),
                owner.pc(),
                &mut staged,
                &request.profile,
                Some(&request.settings),
            )?;
            for instruction in staged.raw_instructions() {
                let index = instruction
                    .cell_index()
                    .context("Lua instruction lacks original source cell")?;
                ordered.entry(index).or_default().push(instruction.clone());
            }
        }
        next.set_instructions_from_cache(ordered.into_values().flatten().collect());
        Ok(())
    }

    fn static_line_relation_reuse_enabled(value: Option<&str>) -> bool {
        value.is_none_or(|value| value == "1")
    }

    fn publish_instructions(&mut self, prepared: PreparedPortrayal) {
        self.poll_scene_build(true);
        let PreparedPortrayal {
            context: mut next,
            catalogue_registry,
            cell_resources,
            coverage,
            ic_changed,
            raster_scene,
        } = prepared;
        if Self::static_line_relation_reuse_enabled(
            std::env::var("FERRITE_STATIC_LINE_RELATION_REUSE")
                .ok()
                .as_deref(),
        ) {
            // Context comparison requires existing stable sorting. Do not sort or
            // mutate the retained context, especially during failed preparation.
            next.inherit_static_line_relations_from(&self.render_context);
        }
        if ferrite_render::area_triangulation_reuse_enabled() {
            next.inherit_static_area_geometry_from(&self.render_context);
        }
        self.catalogue_registry = catalogue_registry;
        self.cell_resources = cell_resources;
        self.render_context = next;
        self.coverage_inventory = Some(coverage);
        self.ic_assigned_vectors = ic_changed;
        self.base_instruction_count = self.render_context.instruction_count();
        if let Some(renderer) = &self.renderer {
            self.applied_settings = renderer.settings().clone();
        }
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();
        if let Some(renderer) = &mut self.renderer {
            renderer.begin_frame();
            if let Some(scene) = raster_scene {
                renderer.commit_raster_scene_publication(scene);
            }
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);
            let portrayal_result = if let Some(resources) = &mut self.cell_resources {
                renderer.try_add_instructions_with_resource_owners(
                    &mut self.render_context,
                    resources,
                    visible_vgs.as_ref(),
                )
            } else {
                renderer.try_add_instructions_with_symbols(
                    &mut self.render_context,
                    Some(&mut self.symbol_cache),
                    color_profile,
                    visible_vgs.as_ref(),
                )
            };
            if let Err(error) = portrayal_result {
                let message = format!("Chart portrayal preparation failed: {error}");
                tracing::error!("{message}");
                renderer.begin_frame();
                renderer.ui_state.notice = Some(message.clone());
                renderer.ui_state.clear_selection();
                renderer.set_selection_geometry(Vec::new(), &self.render_context.scaler);
                self.rendered_symbols.clear();
                self.pending_hit_test = None;
                if self.auto_screenshot.is_some() {
                    self.startup_error = Some(message);
                }
                return;
            }
            self.build_rendered_symbols();
        }
        if self.navigation_scene_pending.dirty()
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| r.navigation_scene_ready(&self.render_context.scaler))
        {
            self.navigation_scene_pending.authoritative_ready();
        }
        self.refresh_selected_symbol_preview();
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
                if let Some(wrapped_hit) = ferrite_render::hit_geometry_wrapped_visible_in_context(
                    &self.render_context,
                    index,
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
            // An index-free hit cannot identify an ENC feature or its FC owner.
            None
        };

        let feature_code = feature
            .and_then(|f| f.feature_code.as_deref())
            .unwrap_or(&sym.symbol_ref)
            .to_string();
        let source_fc = sym
            .cell_index
            .and_then(|index| self.catalogue_owner_for_cell(index as usize))
            .map(|owner| {
                let fc: &ferrite_feature_catalog::FeatureCatalogue = owner.fc();
                fc
            });
        let fields = feature
            .map(|f| dataset_pick_fields::resolve(source_fc, &feature_code, &f.attributes))
            .unwrap_or_default();
        let definition = fields.resolved.then_some(fields.definition).flatten();

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
            attributes: fields.attributes,
            world_pos: (sym.world_x, sym.world_y),
            longitude_shift: sym.longitude_shift,
            definition,
            symbol_name: (sym.kind == 0).then(|| sym.symbol_ref.clone()),
        }
    }

    /// Exercises generic single-file/recursive UI discovery and native-only
    /// removal. Original user history and all original input files stay untouched.
    fn audit_native_dataset_open(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden native dataset audit required"
        );
        let window = self.window.as_ref().context("No window")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible native audit forbidden"
        );
        anyhow::ensure!(!output.exists(), "Native dataset audit output must be new");
        std::fs::create_dir_all(output)?;
        self.cancellation_history_path = output.join("history.json");
        let input = PathBuf::from(std::env::var("FERRITE_NATIVE_DATASET_INPUT")?);
        let folder = input.is_dir();
        let expected: usize = std::env::var("FERRITE_NATIVE_DATASET_EXPECTED")?.parse()?;
        for _ in 0..5 {
            self.renderer.as_mut().context("No renderer")?.render()?;
            if self.sync_chart_layout() {
                self.update_view();
            }
        }
        let initial_routes = self.native_s421.audit_data()["routes"].clone();
        let pump = |app: &mut Self| -> Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            while app.dataset_open_discovery.is_some()
                || app.dataset_open_session.is_some()
                || app.loading_state.is_some()
            {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "Native generic opening timed out"
                );
                if app.loading_state.is_some() {
                    app.poll_loading();
                }
                app.poll_dataset_open();
                if let Some(error) = app.startup_error.take() {
                    anyhow::bail!(error);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            app.ensure_navigation_scene()?;
            Ok(())
        };
        self.begin_dataset_open(input.clone(), folder)?;
        pump(self)?;
        let loaded = self.native_s421.audit_data();
        let ids = self
            .native_s421
            .dataset_rows()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            ids.len() == expected,
            "Unexpected native route count: {} vs {expected}",
            ids.len()
        );
        let opening_notice = self.renderer.as_ref().unwrap().ui_state.notice.clone();
        if self.require_signatures {
            anyhow::ensure!(
                loaded["routes"] == initial_routes && ids.is_empty(),
                "Operational route import mutated state"
            );
        }
        self.begin_dataset_open(input, folder)?;
        pump(self)?;
        anyhow::ensure!(
            self.native_s421.audit_data()["routes"] == loaded["routes"],
            "Reopen changed native source or IDs"
        );
        for _ in 0..4 {
            self.sync_native_s421_overlay();
            self.renderer.as_mut().unwrap().render()?;
            if self.sync_chart_layout() {
                self.update_view();
            }
        }
        self.sync_native_s421_overlay();
        self.renderer.as_mut().unwrap().render()?;
        let layers=self.renderer.as_ref().unwrap().ui_state.dataset_layers.iter().map(|product| serde_json::json!({
            "product":product.product,"files":product.files.iter().map(|file|serde_json::json!({"id":format!("{:?}",file.id),"name":file.name,"source":file.source,"detail":file.detail})).collect::<Vec<_>>()
        })).collect::<Vec<_>>();
        let catalogue_bindings = self.renderer.as_ref().unwrap().ui_state.dataset_catalogue_bindings.iter().map(|binding| serde_json::json!({
            "id": format!("{:?}", binding.id), "cell_index":binding.cell_index,"source":binding.source,
            "fc":{"loaded":binding.fc.loaded,"version":binding.fc.version,"path":binding.fc.path},
            "pc":{"loaded":binding.pc.loaded,"version":binding.pc.version,"path":binding.pc.path}
        })).collect::<Vec<_>>();
        // The normal Debug OFF tracing sink stores warnings in Logs, not stdout.
        // Export that actual sink so a hidden qualification checks the same cause
        // the user can inspect, without enabling Debug or altering product UI.
        let diagnostic_sink = diagnostics_tracing::shared();
        let diagnostics = diagnostic_sink.lock().map_err(|_| anyhow::anyhow!("Diagnostic sink poisoned"))?
            .entries().map(|entry|serde_json::json!({"level":entry.level.label(),"source":entry.source.as_ref(),"message":entry.message.as_ref(),"repeats":entry.repeats})).collect::<Vec<_>>();
        let before = self.publication_model()?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot_with_ui(output.join("loaded.png"))?;
        let mut removals = Vec::new();
        for id in ids {
            self.unload_dataset(id.clone())?;
            anyhow::ensure!(
                self.publication_model()? == before,
                "Native unload changed ENC/raster publication"
            );
            self.sync_native_s421_overlay();
            self.renderer.as_mut().unwrap().render()?;
            removals.push(serde_json::json!({"id":format!("{id:?}"),"native":self.native_s421.audit_data(),"chart_model_unchanged":true}));
        }
        anyhow::ensure!(
            self.native_s421.dataset_rows().is_empty(),
            "Native unload left stale tree route"
        );
        if expected > 0 {
            anyhow::ensure!(
                !self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .native_route_encoded_in_last_pass(),
                "Last native unload retained draw"
            );
        }
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot_with_ui(output.join("unloaded.png"))?;
        let result = serde_json::json!({"expected_routes":expected,"recursive":folder,"require_signatures":self.require_signatures,"opening_notice":opening_notice,"loaded_native":loaded,"loaded_layers":layers,"catalogue_bindings":catalogue_bindings,"diagnostics":diagnostics,"publication":before,"reopen_same_sources_and_ids":true,"removals":removals,"hidden":true,"visible":self.window.as_ref().and_then(|w|w.is_visible()),"focused":self.window.as_ref().is_some_and(|w|w.has_focus())});
        std::fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&result)?,
        )?;
        Ok(())
    }

    /// Explicit hidden qualification hook, using the ordinary discovery/queue and
    /// removal paths. No official input or cancellation history is rewritten.
    fn audit_dataset_ui(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused audit forbidden"
        );
        self.ensure_navigation_scene()?;
        anyhow::ensure!(!output.exists(), "Audit output must be new");
        fs::create_dir_all(output)?;
        self.cancellation_history_path = output.join("history.json");
        let input = PathBuf::from(std::env::var("FERRITE_DATASET_UI_INPUT")?);
        let folder = std::env::var("FERRITE_DATASET_UI_KIND").is_ok_and(|s| s == "folder");
        // Match the ordinary UI's first painted frame before accepting an open
        // request; raster publication requires a measured chart viewport.
        for _ in 0..5 {
            self.renderer.as_mut().unwrap().render()?;
            self.sync_chart_layout();
        }
        self.begin_dataset_open(input, folder)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while self.dataset_open_discovery.is_some()
            || self.dataset_open_session.is_some()
            || self.loading_state.is_some()
        {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Dataset opening audit timed out"
            );
            if self.loading_state.is_some() {
                self.poll_loading();
            }
            self.poll_dataset_open();
            if let Some(error) = self.startup_error.take() {
                anyhow::bail!(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.refresh_dataset_tree();
        for _ in 0..5 {
            self.renderer.as_mut().unwrap().render()?;
            if self.sync_chart_layout() {
                self.update_view();
            }
        }
        let expected: usize = std::env::var("FERRITE_DATASET_UI_EXPECTED")?.parse()?;
        let tree_count: usize = self
            .renderer
            .as_ref()
            .unwrap()
            .ui_state
            .dataset_layers
            .iter()
            .map(|p| p.files.len())
            .sum();
        anyhow::ensure!(
            tree_count == expected && expected > 0,
            "Expected {expected} tree files, got {tree_count}"
        );
        let opening_notice = self.renderer.as_ref().unwrap().ui_state.notice.clone();
        // Qualify the actual recursive-open layout with all files and summary present.
        self.renderer.as_mut().unwrap().ui_state.debug_mode = false;
        for _ in 0..12 {
            self.renderer.as_mut().unwrap().render()?;
            if self.sync_chart_layout() {
                self.update_view();
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        fs::create_dir_all(output.join("loaded"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot_with_ui(output.join("loaded/ui.png"))?;
        self.renderer.as_mut().unwrap().ui_state.show_logs = true;
        for _ in 0..12 {
            self.renderer.as_mut().unwrap().render()?;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot_with_ui(output.join("loaded/logs.png"))?;
        self.renderer.as_mut().unwrap().ui_state.show_logs = false;
        let before = self.capture_catalogue_audit(&output.join("loaded"))?;
        let id = self
            .renderer
            .as_ref()
            .unwrap()
            .ui_state
            .dataset_layers
            .iter()
            .flat_map(|p| &p.files)
            .next()
            .context("No tree file")?
            .id
            .clone();
        let old_tree = format!(
            "{:?}",
            self.renderer.as_ref().unwrap().ui_state.dataset_layers
        );
        self.publication_test_fail_before_commit = true;
        let failure = self
            .unload_dataset(id.clone())
            .err()
            .context("Injected late unload failure accepted")?;
        self.publication_test_fail_before_commit = false;
        anyhow::ensure!(
            self.capture_catalogue_audit(&output.join("failed-unload"))? == before,
            "Failed unload changed live publication"
        );
        anyhow::ensure!(
            format!(
                "{:?}",
                self.renderer.as_ref().unwrap().ui_state.dataset_layers
            ) == old_tree,
            "Failed unload changed tree"
        );
        self.unload_dataset(id)?;
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .ui_state
                .selected_feature
                .is_none()
                && self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selection_candidates
                    .is_empty(),
            "Unloaded stale selection retained"
        );
        self.capture_catalogue_audit(&output.join("unloaded-one"))?;
        if std::env::var("FERRITE_DATASET_UI_UNLOAD_ALL").is_ok_and(|s| s == "1") {
            loop {
                let id = self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .dataset_layers
                    .iter()
                    .flat_map(|p| &p.files)
                    .next()
                    .map(|f| f.id.clone());
                let Some(id) = id else {
                    break;
                };
                self.unload_dataset(id)?;
            }
            anyhow::ensure!(
                !self.chart_loaded && self.cells.is_empty() && self.bathymetry.is_empty(),
                "Last dataset retained"
            );
            self.capture_catalogue_audit(&output.join("unloaded-all"))?;
        }
        anyhow::ensure!(
            !self.cancellation_history_path.exists(),
            "Manual unload created cancellation history"
        );
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"tree_count":tree_count,"opening_notice":opening_notice,"failed_unload":format!("{failure:#}"),"failure_binding_exact":true,"tree_before":old_tree,"remaining_cells":self.cells.len(),"remaining_raster_instances":self.bathymetry.len(),"history_written":false,"visible":self.window.as_ref().unwrap().is_visible(),"focused":self.window.as_ref().unwrap().has_focus()}),
            )?,
        )?;
        Ok(())
    }

    /// Diagnostic snapshot of the live model, excluding intentional error notices.
    /// Nondefault hidden verification hook. Owns only a new private audit/history
    /// directory; it never alters official input bytes or the ordinary startup path.
    fn catalogue_audit_binding(&self) -> Result<serde_json::Value> {
        let r = self.renderer.as_ref().context("Renderer required")?;
        let history = if self.cancellation_history_path.exists() {
            let length = fs::metadata(&self.cancellation_history_path)?.len();
            anyhow::ensure!(
                length <= 8 * 1024 * 1024,
                "Audit history exceeds bounded read"
            );
            Some(format!(
                "{:x}",
                Sha256::digest(fs::read(&self.cancellation_history_path)?)
            ))
        } else {
            None
        };
        Ok(serde_json::json!({"model":self.publication_model()?,
            "fc_digest":self.fc.source_digest(),"pc_digest":self.pc.source_digest(),
            "fc_version":self.fc.version,"pc_version":self.pc.version,
            "fc_product":self.fc.product_id,"pc_product":self.pc.product_id,
            "dataset_catalogue_owners": self.cells.iter().enumerate().map(|(index, cell)| -> Result<_> {
                let key = Self::catalogue_dataset_key(&cell.dsid)?;
                let owner = self.catalogue_registry.owner_for(&key).context("Audit catalogue owner missing")?;
                Ok(serde_json::json!({"cell_index":index,"dataset_key":key,
                    "identity":self.catalogue_registry.binding_identity(&key),
                    "fc_version":owner.fc().version,"pc_version":owner.pc().version}))
            }).collect::<Result<Vec<_>>>()?,
            "symbol_cache_revision":self.symbol_cache.resource_revision(),"symbol_cache_len":self.symbol_cache.len(),
            "fc_ui":format!("{:?}",r.ui_state.fc_status),"pc_ui":format!("{:?}",r.ui_state.pc_status),
            "optional_layers":format!("{:?}",r.ui_state.optional_viewing_layers),
            "profile":self.current_profile_name,"history_sha256":history}))
    }
    fn capture_catalogue_audit(&mut self, output: &Path) -> Result<serde_json::Value> {
        self.renderer
            .as_mut()
            .context("Renderer required")?
            .render()?;
        self.audit_portrayal(output)?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("chart.png"))?;
        let binding = self.catalogue_audit_binding()?;
        fs::write(
            output.join("binding.json"),
            serde_json::to_vec_pretty(&binding)?,
        )?;
        Ok(binding)
    }
    fn catalogue_audit_load(&mut self, paths: &[PathBuf]) -> Result<Option<String>> {
        if let Err(error) = self.load_charts(paths) {
            return Ok(Some(format!("{error:#}")));
        }
        self.wait_publication_test_load()?;
        Ok(self.startup_error.take())
    }
    fn audit_catalogue_publication(&mut self, output: &Path) -> Result<()> {
        self.poll_scene_build(true);
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let w = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            w.is_visible() == Some(false) && !w.has_focus(),
            "Visible/focused audit forbidden"
        );
        anyhow::ensure!(
            self.auto_screenshot.is_some(),
            "Audit needs --screenshot error propagation"
        );
        anyhow::ensure!(!output.exists(), "Audit output must be new");
        anyhow::ensure!(
            self.pending_auto_chart.is_empty(),
            "Pass target paths via audit JSON, not --chart"
        );
        let targets: Vec<PathBuf> =
            serde_json::from_str(&std::env::var("FERRITE_ROOT_CATALOGUE_INPUTS")?)?;
        anyhow::ensure!(
            !targets.is_empty() && targets.len() <= 256,
            "Audit input count invalid"
        );
        let expected: usize = std::env::var("FERRITE_ROOT_CATALOGUE_EXPECTED_CELLS")?.parse()?;
        let retained: Vec<PathBuf> = std::env::var("FERRITE_ROOT_CATALOGUE_RETAINED_INPUTS")
            .ok()
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default();
        anyhow::ensure!(retained.len() <= 256, "Audit retained input count invalid");
        let mut input_guards = Vec::new();
        for p in targets.iter().chain(&retained) {
            let bytes =
                ferrite_security::UnauthenticatedSnapshot::copy_bounded(p, 512 * 1024 * 1024)?;
            let captured = fs::read(bytes.path())?;
            input_guards.push((
                p.clone(),
                format!("{:x}", Sha256::digest(&captured)),
                captured.len(),
            ));
        }
        fs::create_dir_all(output)?;
        self.cancellation_history_path = output.join("history.json");
        if !retained.is_empty() {
            anyhow::ensure!(
                self.catalogue_audit_load(&retained)?.is_none(),
                "Retained original load failed"
            );
        }
        if let Ok(value) = std::env::var("FERRITE_ROOT_AUDIT_INITIAL_ZOOM") {
            self.zoom_level =
                navigation::bounded_zoom(value.parse()?).context("Invalid audit zoom")?;
            self.zoom_target = self.zoom_level;
            self.update_view();
        }
        let original = self.capture_catalogue_audit(&output.join("before"))?;
        let mode =
            std::env::var("FERRITE_ROOT_CATALOGUE_AUDIT_MODE").unwrap_or_else(|_| "success".into());
        let mut failures = Vec::new();
        if mode == "manual-reject" || mode == "manual-recovery" {
            anyhow::ensure!(
                self.catalogue_audit_load(&targets)?.is_none(),
                "Manual audit initial load failed"
            );
            anyhow::ensure!(
                self.cells.len() == expected && expected > 0,
                "Manual audit cell count mismatch"
            );
            let before = self.capture_catalogue_audit(&output.join("manual-before"))?;
            let fc = Arc::new(load_feature_catalogue(Path::new(&std::env::var(
                "FERRITE_ROOT_MANUAL_FC",
            )?))?);
            let pc = Arc::new(load_portrayal_catalogue(Path::new(&std::env::var(
                "FERRITE_ROOT_MANUAL_PC",
            )?))?);
            if mode == "manual-reject" {
                let error = self
                    .change_catalogue_pair_manually(fc, pc)
                    .err()
                    .context("Manual incompatible/resource pair accepted")?;
                anyhow::ensure!(
                    self.capture_catalogue_audit(&output.join("manual-rejected"))? == before,
                    "Manual rejection changed retained publication"
                );
                failures.push(format!("{error:#}"));
            } else {
                self.publication_test_fail_before_commit = true;
                let result = self.change_catalogue_pair_manually(fc.clone(), pc.clone());
                self.publication_test_fail_before_commit = false;
                let error = result.err().context("Manual staged failure not observed")?;
                anyhow::ensure!(
                    format!("{error:#}").contains("Injected portrayal change failure"),
                    "Unexpected manual failure: {error:#}"
                );
                anyhow::ensure!(
                    self.capture_catalogue_audit(&output.join("manual-material-failure"))?
                        == before,
                    "Failed manual staging changed publication"
                );
                failures.push(format!("{error:#}"));
                self.change_catalogue_pair_manually(fc, pc)?;
                self.capture_catalogue_audit(&output.join("manual-retry"))?;
            }
        } else if mode == "mixed-success" {
            anyhow::ensure!(
                !self.cells.is_empty(),
                "Mixed audit requires retained cells"
            );
            let retained_count = self.cells.len();
            let retained_owners: Vec<_> = self
                .cells
                .iter()
                .map(|cell| {
                    let key = Self::catalogue_dataset_key(&cell.dsid)?;
                    Ok((
                        key.clone(),
                        Arc::clone(
                            self.catalogue_registry
                                .owner_for(&key)
                                .context("Missing retained owner")?,
                        ),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            for (phase, history_fault) in [("material-failure", false), ("history-failure", true)] {
                self.publication_test_fail_before_commit = !history_fault;
                self.catalogue_test_fail_history = history_fault;
                let result = self.catalogue_audit_load(&targets);
                self.publication_test_fail_before_commit = false;
                self.catalogue_test_fail_history = false;
                let error = result?.context("Injected mixed preparation failure not observed")?;
                anyhow::ensure!(
                    error.contains(if history_fault {
                        "Injected catalogue history failure"
                    } else {
                        "Injected portrayal change failure"
                    }),
                    "Unexpected mixed preparation failure: {error}"
                );
                anyhow::ensure!(
                    self.capture_catalogue_audit(&output.join(phase))? == original,
                    "Failed mixed preparation changed retained ownership/view/history"
                );
                failures.push(error);
            }
            anyhow::ensure!(
                self.catalogue_audit_load(&targets)?.is_none(),
                "Mixed catalogue retry failed"
            );
            if std::env::var_os("FERRITE_ROOT_AUDIT_INITIAL_ZOOM").is_some() {
                let renderer = self.renderer.as_ref().context("Renderer required")?;
                anyhow::ensure!(
                    self.zoom_level == 1.
                        && self.zoom_target == 1.
                        && renderer.ui_state.zoom_level == 1.,
                    "Fit load retained stale displayed zoom"
                );
                anyhow::ensure!(
                    self.pan_offset == (0., 0.) && !self.zoom_animating,
                    "Fit load retained stale camera transform"
                );
            }
            anyhow::ensure!(
                self.cells.len() == expected && expected > retained_count,
                "Mixed publication count mismatch"
            );
            anyhow::ensure!(
                self.cell_resources
                    .as_ref()
                    .is_some_and(|r| r.cell_count() == self.cells.len()),
                "Mixed resources not installed"
            );
            for (key, owner) in retained_owners {
                anyhow::ensure!(
                    self.catalogue_registry
                        .owner_for(&key)
                        .is_some_and(|current| Arc::ptr_eq(current, &owner)),
                    "Appending another Edition rebound a retained owner"
                );
            }
            let editions = self
                .cells
                .iter()
                .map(|c| s101_catalogue_selection::dataset_version(&c.dsid).map(|v| v.edition))
                .collect::<Result<std::collections::BTreeSet<_>>>()?;
            anyhow::ensure!(
                editions.len() > 1,
                "Audit did not publish mixed product Editions"
            );
            let instructions = self.render_context.raw_instructions();
            for index in 0..self.cells.len() {
                anyhow::ensure!(
                    instructions
                        .iter()
                        .any(|i| i.cell_index() == u32::try_from(index).ok()),
                    "A mixed cell has no original indexed instructions"
                );
                anyhow::ensure!(
                    self.catalogue_owner_for_cell(index).is_some(),
                    "Mixed source identity lost its catalogue binding"
                );
            }
            self.capture_catalogue_audit(&output.join("mixed-success"))?;
            let day = self.current_profile_name.clone();
            for profile in ["Night", day.as_str()] {
                let prepared = self.prepare_portrayal_change(PortrayalChangeRequest {
                    profile: profile.into(),
                    settings: self.applied_settings.clone(),
                })?;
                self.commit_portrayal_change(prepared)?;
                self.capture_catalogue_audit(&output.join(format!("mixed-palette-{profile}")))?;
            }
        } else if mode == "mixed-reject" || mode == "signature-reject" || mode == "numeric-reject" {
            let error = self
                .catalogue_audit_load(&targets)?
                .context("Expected load rejection")?;
            if mode == "mixed-reject" {
                anyhow::ensure!(
                    error.contains("Mixed S-101"),
                    "Unexpected mixed-version rejection: {error}"
                );
            }
            if mode == "numeric-reject" {
                anyhow::ensure!(
                    error.contains("Nonconformant S-101 spatial scales"),
                    "Unexpected numeric spatial-scale rejection: {error}"
                );
            }
            let after = self.capture_catalogue_audit(&output.join("rejected"))?;
            anyhow::ensure!(
                after == original,
                "Rejected catalogue load changed retained ownership/view/history"
            );
            failures.push(error);
        } else {
            anyhow::ensure!(
                mode == "success" && self.cells.is_empty(),
                "Pair transition audit requires no retained S101 cells"
            );
            for (phase, history_fault) in [("material-failure", false), ("history-failure", true)] {
                self.publication_test_fail_before_commit = !history_fault;
                self.catalogue_test_fail_history = history_fault;
                let result = self.catalogue_audit_load(&targets);
                self.publication_test_fail_before_commit = false;
                self.catalogue_test_fail_history = false;
                let error = result?.context("Injected catalogue failure not observed")?;
                anyhow::ensure!(
                    if history_fault {
                        error.contains("Injected catalogue history failure")
                    } else {
                        error.contains("Injected portrayal change failure")
                    },
                    "Unexpected failure: {error}"
                );
                anyhow::ensure!(
                    self.capture_catalogue_audit(&output.join(phase))? == original,
                    "Failed preparation changed retained pair/cache/UI/view/history"
                );
                failures.push(error);
            }
            anyhow::ensure!(
                self.catalogue_audit_load(&targets)?.is_none(),
                "Catalogue retry failed"
            );
            anyhow::ensure!(
                self.cells.len() == expected && expected > 0,
                "Unexpected successful dataset count"
            );
            anyhow::ensure!(
                self.pc.version == "1.0.2" && self.fc.version == "1.0.2",
                "Expected actual legacy catalogue1.0.2"
            );
            anyhow::ensure!(
                self.cells.iter().all(|c| c
                    .dsid
                    .product_edition
                    .parse::<ferrite_kernel::SpecificationVersion>()
                    .is_ok_and(|v| v.edition == 1)),
                "Unexpected dataset Edition"
            );
            anyhow::ensure!(
                self.render_context.instruction_count() > 0,
                "Successful legacy portrayal is empty"
            );
            let first = self.capture_catalogue_audit(&output.join("retry"))?;
            let instruction_bytes = bincode::serialize(self.render_context.raw_instructions())?;
            let fc_digest = *self.fc.source_digest();
            let pc_digest = *self.pc.source_digest();
            let revision = self.symbol_cache.resource_revision();
            anyhow::ensure!(
                self.catalogue_audit_load(&targets)?.is_none(),
                "Repeat load failed"
            );
            anyhow::ensure!(
                *self.fc.source_digest() == fc_digest
                    && *self.pc.source_digest() == pc_digest
                    && self.symbol_cache.resource_revision() == revision,
                "Repeat load unnecessarily replaced catalogue/cache owner"
            );
            anyhow::ensure!(
                bincode::serialize(self.render_context.raw_instructions())? == instruction_bytes,
                "Repeat typed instructions differ"
            );
            let repeat = self.capture_catalogue_audit(&output.join("repeat"))?;
            fs::write(
                output.join("successful.json"),
                serde_json::to_vec_pretty(&serde_json::json!({"first":first,"repeat":repeat}))?,
            )?;
        }
        for (p, hash, length) in &input_guards {
            let snapshot =
                ferrite_security::UnauthenticatedSnapshot::copy_bounded(p, 512 * 1024 * 1024)?;
            let bytes = fs::read(snapshot.path())?;
            anyhow::ensure!(
                bytes.len() == *length && format!("{:x}", Sha256::digest(&bytes)) == *hash,
                "Audit source changed: {}",
                p.display()
            );
        }
        fs::write(
            output.join("receipt.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"mode":mode,"hidden":true,"focused":false,"inputs":input_guards,"expected_cells":expected,"failures":failures,"final":self.catalogue_audit_binding()?,"no_update_view_repair":true,"scope":"raw8 are CPU geometry exports; retained picking only if baseline has actual selection; no positive legacy pick claim"}),
            )?,
        )?;
        Ok(())
    }

    /// Explicit same-source/current-camera owner-swap audit, outside measurement.
    fn audit_ready_vector_activation(&mut self, output: &Path) -> Result<()> {
        self.poll_scene_build(true);
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden activation audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused activation audit forbidden"
        );
        anyhow::ensure!(
            self.flat_eventloop_audit.is_none() && !self.flat_eventloop_audit_started,
            "Activation/readback forbidden in measured callbacks"
        );
        anyhow::ensure!(
            self.chart_loaded
                && !self.cells.is_empty()
                && self.bathymetry.is_empty()
                && self.dataset_open_discovery.is_none()
                && self.dataset_open_session.is_none(),
            "Settled vector-only load required"
        );
        anyhow::ensure!(
            !self.zoom_animating && self.zoom_rebuild_phase == 0 && self.pan_rebuild_phase == 0,
            "Settled camera required"
        );
        anyhow::ensure!(
            self.cells.len() <= 128 && self.render_context.raw_instructions().len() <= 500_000,
            "Activation CPU inventory limit exceeded"
        );
        anyhow::ensure!(
            fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "Activation audit destination must be absent including symlink"
        );
        // Two candidates coexist for the stale-token control: <=128MiB SERIALIZED logical IR.
        // This is NOT clone allocation/RSS or a joint CPU/GPU/atlas/texture memory cap.
        const IR_BYTES: u64 = 64 * 1024 * 1024;
        let serialized_bytes = bincode::serialized_size(self.render_context.raw_instructions())?;
        anyhow::ensure!(
            serialized_bytes <= IR_BYTES,
            "IR serialized admission exceeds 64MiB per candidate"
        );
        let original_count = self.render_context.instruction_count();
        let raw_text_count = self
            .render_context
            .raw_instructions()
            .iter()
            .filter(|i| matches!(i, DrawingInstruction::Text(_)))
            .count();
        let original_hash = format!(
            "{:x}",
            Sha256::digest(bincode::serialize(self.render_context.raw_instructions())?)
        );
        let old_geometry_revision = self.render_context.geometry_revision();
        let old_coverage_revision = self.render_context.coverage_view_revision();
        let before = self.catalogue_audit_binding()?;
        let camera = self.navigation_camera_bits();
        let registry = self.catalogue_registry.checkpoint();
        let expected_text_draws = std::env::var("FERRITE_READY_VECTOR_ACTIVATION_TEXT_DRAWS")
            .ok()
            .map(|v| {
                v.parse::<usize>()
                    .context("Invalid expected text draw count")
            })
            .transpose()?;
        let symbol_snapshot = |renderer: &ferrite_wgpu::WgpuRenderer| -> Result<Vec<u8>> {
            let rows = renderer.displayed_symbols_with_sources();
            anyhow::ensure!(
                rows.len() <= 500_000,
                "Displayed symbol snapshot row cap exceeded"
            );
            anyhow::ensure!(
                bincode::serialized_size(&rows)? <= 32 * 1024 * 1024,
                "Displayed symbol snapshot exceeds 32MiB logical serialized payload"
            );
            Ok(bincode::serialize(&rows)?)
        };
        let symbol_before = symbol_snapshot(self.renderer.as_ref().context("Renderer required")?)?;
        let reference = self
            .renderer
            .as_ref()
            .context("Renderer required")?
            .audit_displayed_vector_frame()?;
        anyhow::ensure!(
            reference.extent == [window.inner_size().width, window.inner_size().height],
            "Window/renderer extent mismatch"
        );
        let (ready, stale) = {
            let renderer = self.renderer.as_ref().context("Renderer required")?;
            let (pan, zoom, pivot) = renderer.fast_view_transform();
            anyhow::ensure!(
                pan == (0., 0.)
                    && zoom == 1.
                    && pivot == (0., 0.)
                    && renderer.geometry_matches_view(&self.render_context.scaler),
                "Affine/stale camera forbidden"
            );
            anyhow::ensure!(
                renderer.settings() == &self.applied_settings
                    && renderer.ui_state.color_profile == self.current_profile_name,
                "Applied policy/palette mismatch"
            );
            let settings = renderer.vector_emission_settings();
            let groups = self.get_visible_viewing_groups();
            let boxes = renderer.vector_world_map_chart_boxes();
            anyhow::ensure!(boxes.len() <= 4096, "World map clone cap exceeded");
            let request = PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: self.applied_settings.clone(),
            };
            let prepare = || -> Result<ferrite_wgpu::ReadyVectorGpuFrame> {
                let mut next = self.render_context.empty_for_rebuild();
                next.set_instructions_from_cache(self.render_context.raw_instructions().to_vec());
                prepare_flat_coverage(
                    self.coverage_inventory.as_deref(),
                    &mut next,
                    window.inner_size(),
                    window.scale_factor(),
                )?;
                anyhow::ensure!(
                    format!(
                        "{:x}",
                        Sha256::digest(bincode::serialize(next.raw_instructions())?)
                    ) == original_hash,
                    "Fresh coverage altered original ordered IR"
                );
                let resources = self.prepare_cell_resources(&self.catalogue_registry, &request)?;
                let wrap = 360.0 * next.scaler.scale_x() as f32;
                let emission = renderer.prepare_private_vector_emission(
                    next,
                    resources,
                    groups.as_ref(),
                    settings,
                    wrap,
                    boxes.to_vec(),
                )?;
                let ready = renderer.prepare_private_vector_gpu(emission)?;
                renderer.validate_private_vector_gpu(&ready)?;
                let expected_key = [
                    (reference.extent[0] as f32).to_bits(),
                    (reference.extent[1] as f32).to_bits(),
                    0f32.to_bits(),
                    0f32.to_bits(),
                    1f32.to_bits(),
                    1f32.to_bits(),
                    0f32.to_bits(),
                    0f32.to_bits(),
                    wrap.to_bits(),
                ];
                anyhow::ensure!(
                    ready.target_continuous_transform_key() == expected_key,
                    "Private target phase/extent/wrap mismatch"
                );
                Ok(ready)
            };
            (prepare()?, prepare()?)
        };
        let text_draw_count = ready.text_draw_count();
        if let Some(expected) = expected_text_draws {
            anyhow::ensure!(
                text_draw_count == expected,
                "Expected text-positive draw count differs: {text_draw_count} != {expected}"
            );
            if expected != 0 {
                anyhow::ensure!(
                    raw_text_count != 0,
                    "Text fixture must have genuine original Text IR"
                );
            }
        }
        anyhow::ensure!(
            stale.text_draw_count() == text_draw_count
                && stale.geometry_index_counts() == ready.geometry_index_counts(),
            "Two original candidate outputs disagree"
        );
        let target_geometry_revision = ready.context().geometry_revision();
        let target_coverage_revision = ready.context().coverage_view_revision();
        let target_key = ready.target_continuous_transform_key();
        let new_gpu_payload = ready.buffer_payload_bytes();
        let index_counts = ready.geometry_index_counts();
        let glyph_positive = text_draw_count > 0 && raw_text_count > 0;
        // Independent App checks occur AFTER complete candidate preparation, BEFORE Renderer activation.
        anyhow::ensure!(
            self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera
                && self.catalogue_audit_binding()? == before,
            "Candidate preparation changed current App/source/model binding"
        );
        // No allocation, fallible calls or setters between successful Renderer swap and App owner moves.
        let installed = self
            .renderer
            .as_mut()
            .context("Renderer required")?
            .activate_ready_vector_frame(ready)?;
        let (context, resources) = installed.into_parts();
        self.render_context = context;
        self.cell_resources = Some(resources);
        anyhow::ensure!(
            self.render_context.geometry_revision() == target_geometry_revision
                && self.render_context.coverage_view_revision() == target_coverage_revision
                && target_geometry_revision != old_geometry_revision,
            "New context revision was not adopted"
        );
        anyhow::ensure!(
            self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera
                && self.catalogue_audit_binding()? == before,
            "Same-source activation changed App/model/camera/registry snapshot"
        );
        let rejection = match self
            .renderer
            .as_mut()
            .unwrap()
            .activate_ready_vector_frame(stale)
        {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Pre-swap Ready was admitted after scene epoch replacement"),
        };
        anyhow::ensure!(
            rejection.contains("publication binding changed"),
            "Stale Ready rejected for an unexpected cause: {rejection}"
        );
        let renderer = self.renderer.as_ref().unwrap();
        let after = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            after.rgba8 == reference.rgba8
                && after.extent == reference.extent
                && after.texture_format == reference.texture_format
                && after.samples == reference.samples,
            "Activated display/stale rejection changed chart RGBA"
        );
        drop(after);
        anyhow::ensure!(
            symbol_snapshot(self.renderer.as_ref().unwrap())? == symbol_before,
            "Activation/stale rejection changed ordered displayed symbol source metadata"
        );
        let mut normal_frame_hashes = Vec::with_capacity(5);
        for _ in 0..5 {
            let renderer = self.renderer.as_mut().unwrap();
            anyhow::ensure!(
                renderer.window().is_visible() == Some(false) && !renderer.window().has_focus(),
                "Normal activation audit frame visible/focused"
            );
            renderer.render()?; // Actual overlay/layout/private atlas path; no update_view/repair.
            let image = renderer.audit_displayed_vector_frame()?;
            anyhow::ensure!(
                image.rgba8 == reference.rgba8
                    && image.extent == reference.extent
                    && image.texture_format == reference.texture_format
                    && image.samples == reference.samples,
                "Normal render after activation changed chart RGBA"
            );
            normal_frame_hashes.push(format!("{:x}", Sha256::digest(&image.rgba8)));
        }
        anyhow::ensure!(
            self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera
                && self.catalogue_audit_binding()? == before,
            "Normal activation audit frames changed App/model/registry/camera snapshot"
        );
        anyhow::ensure!(
            symbol_snapshot(self.renderer.as_ref().unwrap())? == symbol_before,
            "Normal frames changed ordered displayed symbol source metadata"
        );
        let mut report = serde_json::Map::new();
        report.insert(
            "displayed_symbol_metadata_sha256".into(),
            serde_json::json!(format!("{:x}", Sha256::digest(&symbol_before))),
        );
        report.insert(
            "displayed_symbol_metadata_bytes".into(),
            serde_json::json!(symbol_before.len()),
        );
        report.insert(
            "displayed_symbol_metadata_exact".into(),
            serde_json::json!(true),
        );
        report.insert("schema".into(), serde_json::json!(1));
        report.insert("passed".into(), serde_json::json!(true));
        report.insert("scope".into(), serde_json::json!("hidden same-original-source/current-camera vector activation; not general App/raster/journal transaction"));
        report.insert(
            "rgba_sha256".into(),
            serde_json::json!(format!("{:x}", Sha256::digest(&reference.rgba8))),
        );
        report.insert(
            "rgba_bytes".into(),
            serde_json::json!(reference.rgba8.len()),
        );
        report.insert("extent".into(), serde_json::json!(reference.extent));
        report.insert(
            "original_ir_sha256".into(),
            serde_json::json!(original_hash),
        );
        report.insert(
            "original_ir_count".into(),
            serde_json::json!(original_count),
        );
        report.insert(
            "raw_text_ir_count".into(),
            serde_json::json!(raw_text_count),
        );
        report.insert("text_draw_count".into(), serde_json::json!(text_draw_count));
        report.insert(
            "text_positive_original".into(),
            serde_json::json!(glyph_positive),
        );
        report.insert(
            "expected_text_draws".into(),
            serde_json::json!(expected_text_draws),
        );
        report.insert(
            "old_geometry_revision".into(),
            serde_json::json!(old_geometry_revision),
        );
        report.insert(
            "target_geometry_revision".into(),
            serde_json::json!(target_geometry_revision),
        );
        report.insert(
            "old_coverage_revision".into(),
            serde_json::json!(old_coverage_revision),
        );
        report.insert(
            "target_coverage_revision".into(),
            serde_json::json!(target_coverage_revision),
        );
        report.insert("target_transform_key".into(), serde_json::json!(target_key));
        report.insert(
            "new_gpu_buffer_payload_bytes".into(),
            serde_json::json!(new_gpu_payload),
        );
        report.insert("gpu_index_counts".into(), serde_json::json!(index_counts));
        report.insert(
            "serialized_ir_bytes_per_candidate".into(),
            serde_json::json!(serialized_bytes),
        );
        report.insert(
            "serialized_ir_admission_limit_per_candidate".into(),
            serde_json::json!(IR_BYTES),
        );
        report.insert("simultaneous_candidate_count".into(), serde_json::json!(2));
        report.insert("stale_epoch_rejection".into(), serde_json::json!(rejection));
        report.insert(
            "normal_frame_hashes".into(),
            serde_json::json!(normal_frame_hashes),
        );
        report.insert(
            "chart_text_buffer_statistics".into(),
            self.renderer
                .as_ref()
                .unwrap()
                .chart_text_buffer_statistics(),
        );
        report.insert(
            "model_registry_camera_exact".into(),
            serde_json::json!(true),
        );
        report.insert("new_revision_adopted".into(), serde_json::json!(true));
        report.insert(
            "positive_pick_query_proven".into(),
            serde_json::json!(false),
        );
        report.insert(
            "egui_ui_atlas_readback_proven".into(),
            serde_json::json!(false),
        );
        report.insert(
            "individual_gpu_buffer_mask_bytes_readback".into(),
            serde_json::json!(false),
        );
        fs::create_dir(output)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join("report.json"))?;
        serde_json::to_writer_pretty(&mut file, &report)?;
        Ok(())
    }

    /// Hidden same-original mixed Renderer publication proof; no journal/model replacement claim.
    fn prepare_joint_audit_vector(
        &self,
        profile: &str,
    ) -> Result<ferrite_wgpu::ReadyVectorGpuFrame> {
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let window = self.window.as_ref().context("Window required")?;
        let mut next = self.render_context.empty_for_rebuild();
        next.set_instructions_from_cache(self.render_context.raw_instructions().to_vec());
        prepare_flat_coverage(
            self.coverage_inventory.as_deref(),
            &mut next,
            window.inner_size(),
            window.scale_factor(),
        )?;
        anyhow::ensure!(
            bincode::serialize(next.raw_instructions())?
                == bincode::serialize(self.render_context.raw_instructions())?,
            "Joint audit fresh context changed original ordered IR"
        );
        let request = PortrayalChangeRequest {
            profile: profile.into(),
            settings: self.applied_settings.clone(),
        };
        let resources = self.prepare_cell_resources(&self.catalogue_registry, &request)?;
        let groups = self.get_visible_viewing_groups();
        let boxes = renderer.vector_world_map_chart_boxes();
        anyhow::ensure!(boxes.len() <= 4096, "Joint audit world map cap exceeded");
        let wrap = 360.0 * next.scaler.scale_x() as f32;
        let emission = renderer.prepare_private_vector_emission(
            next,
            resources,
            groups.as_ref(),
            renderer.vector_emission_settings(),
            wrap,
            boxes.to_vec(),
        )?;
        Ok(renderer.prepare_private_vector_gpu(emission)?)
    }
    fn prepare_joint_audit_raster(
        &mut self,
        wrong_camera: bool,
    ) -> Result<ferrite_wgpu::PreparedRasterScenePublication> {
        let (_, _, groups) = self.bathymetry_portrayal_with_status()?;
        let renderer = self.renderer.as_mut().context("Renderer required")?;
        // Reuse the ACTUAL displayed immutable regular materials; never invent a replacement texture.
        let batch = renderer.stage_raster_material_batch::<ferrite_wgpu::WgpuError>(
            &self.render_context.scaler,
            |_| Ok(()),
        )?;
        let mut raster = renderer.prepare_raster_material_publication(batch, false, None)?;
        if wrong_camera {
            let mut wrong = self.render_context.scaler.clone();
            wrong.pan(17., 0.);
            anyhow::ensure!(
                wrong.flat_encoded_identity() != self.render_context.scaler.flat_encoded_identity(),
                "Wrong-camera control did not change actual scaler"
            );
            renderer.reproject_raster_publication(&mut raster, &wrong)?;
        }
        Ok(renderer.prepare_raster_scene_publication_with_groups(raster, Some(groups))?)
    }
    fn audit_joint_vector_raster_activation(&mut self, output: &Path) -> Result<()> {
        self.poll_scene_build(true);
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled()
                && window.is_visible() == Some(false)
                && !window.has_focus(),
            "Hidden unfocused joint audit required"
        );
        anyhow::ensure!(
            self.flat_eventloop_audit.is_none() && !self.flat_eventloop_audit_started,
            "Joint readback forbidden in measured callbacks"
        );
        anyhow::ensure!(
            self.chart_loaded
                && !self.cells.is_empty()
                && !self.bathymetry.is_empty()
                && self.dataset_open_discovery.is_none()
                && self.dataset_open_session.is_none(),
            "Settled genuine S101 and S102 required"
        );
        anyhow::ensure!(
            !self.zoom_animating && self.zoom_rebuild_phase == 0 && self.pan_rebuild_phase == 0,
            "Settled joint camera required"
        );
        anyhow::ensure!(
            self.cells.len() <= 128
                && self.bathymetry.len() <= 32
                && self.render_context.raw_instructions().len() <= 500_000
                && bincode::serialized_size(self.render_context.raw_instructions())?
                    <= 64 * 1024 * 1024,
            "Joint audit input admission exceeds logical cap; not an RSS cap"
        );
        anyhow::ensure!(
            fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "Joint audit destination must be absent"
        );
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let (pan, zoom, pivot) = renderer.fast_view_transform();
        anyhow::ensure!(
            pan == (0., 0.)
                && zoom == 1.
                && pivot == (0., 0.)
                && renderer.geometry_matches_view(&self.render_context.scaler)
                && renderer.settings() == &self.applied_settings
                && renderer.ui_state.color_profile == self.current_profile_name,
            "Joint audit stale affine or policy"
        );
        let before = self.bathymetry_publication_model()?;
        let catalogue = self.catalogue_audit_binding()?;
        let registry = self.catalogue_registry.checkpoint();
        let camera = self.navigation_camera_bits();
        let original_revision = self.render_context.geometry_revision();
        let profile = self.current_profile_name.clone();
        let s102_pc = self
            .s102_pc
            .as_ref()
            .context("Retained S102 PC missing")?
            .clone();
        let policy = renderer.settings();
        let depth = DepthSettings {
            safety_contour: policy.safety_contour,
            shallow_contour: policy.shallow_contour,
            deep_contour: policy.deep_contour,
            four_shades: !policy.two_shades,
        };
        let symbols = bincode::serialize(&renderer.displayed_symbols_with_sources())?;
        anyhow::ensure!(
            symbols.len() <= 32 * 1024 * 1024,
            "Joint symbol serialized cap exceeded"
        );
        fs::create_dir(output)?;
        fn capture(app: &mut ChartApp, path: &Path) -> Result<(u32, u32, Vec<u8>)> {
            app.renderer
                .as_mut()
                .context("Renderer required")?
                .save_screenshot(path)?;
            let pixels = image::open(path)?.into_rgba8();
            let (w, h) = pixels.dimensions();
            anyhow::ensure!(
                u64::from(w) * u64::from(h) * 4 <= 64 * 1024 * 1024,
                "Joint RGBA cap exceeded"
            );
            Ok((w, h, pixels.into_raw()))
        }
        let reference = capture(self, &output.join("before.png"))?;
        let direct_reference = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_regular_mixed_frame(true)?;
        let vector_only = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_regular_mixed_frame(false)?;
        anyhow::ensure!(
            direct_reference.extent == vector_only.extent
                && direct_reference.rgba8 != vector_only.rgba8,
            "Original S102 has no visible pixel contribution in this fixture"
        );
        drop(vector_only);
        let raster_visible_count = self
            .renderer
            .as_ref()
            .unwrap()
            .raster_composition_metadata()
            .filter(|row| row.3)
            .count();
        anyhow::ensure!(
            raster_visible_count > 0,
            "No visible original raster material"
        );
        let direct_exact = |renderer: &ferrite_wgpu::WgpuRenderer| -> Result<()> {
            let actual = renderer.audit_displayed_regular_mixed_frame(true)?;
            anyhow::ensure!(
                actual.rgba8 == direct_reference.rgba8
                    && actual.extent == direct_reference.extent
                    && actual.texture_format == direct_reference.texture_format
                    && actual.samples == direct_reference.samples,
                "Direct mixed pass differs before any preparation/upload repair"
            );
            Ok(())
        };
        let wrong_vector = self.prepare_joint_audit_vector(&profile)?;
        let wrong_raster = self.prepare_joint_audit_raster(true)?;
        let camera_rejection = match self
            .renderer
            .as_ref()
            .unwrap()
            .prepare_ready_vector_bound_s102_publication(
                wrong_vector,
                wrong_raster,
                s102_pc.clone(),
                profile.clone(),
                depth,
            ) {
            Err(e) => e.to_string(),
            Ok(_) => anyhow::bail!("Joint accepted wrong actual raster camera"),
        };
        anyhow::ensure!(
            camera_rejection.contains("Raster geometry was not prepared"),
            "Wrong-camera rejection cause differs: {camera_rejection}"
        );
        direct_exact(self.renderer.as_ref().unwrap())?;
        anyhow::ensure!(
            capture(self, &output.join("after-camera-rejection.png"))? == reference
                && self.bathymetry_publication_model()? == before
                && self.catalogue_audit_binding()? == catalogue
                && self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera,
            "Wrong-camera rejection changed old mixed display/model/registry/camera"
        );
        let foreign_profile = if profile == "Dusk" { "Day" } else { "Dusk" };
        let foreign_vector = self.prepare_joint_audit_vector(foreign_profile)?;
        let foreign_raster = self.prepare_joint_audit_raster(false)?;
        let palette_rejection = match self
            .renderer
            .as_ref()
            .unwrap()
            .prepare_ready_vector_bound_s102_publication(
                foreign_vector,
                foreign_raster,
                s102_pc.clone(),
                profile.clone(),
                depth,
            ) {
            Err(e) => e.to_string(),
            Ok(_) => anyhow::bail!("Joint accepted foreign vector target palette"),
        };
        anyhow::ensure!(
            palette_rejection.contains("Vector target palette differs"),
            "Foreign-palette rejection cause differs: {palette_rejection}"
        );
        direct_exact(self.renderer.as_ref().unwrap())?;
        anyhow::ensure!(
            capture(self, &output.join("after-palette-rejection.png"))? == reference
                && self.bathymetry_publication_model()? == before
                && self.catalogue_audit_binding()? == catalogue
                && self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera,
            "Foreign-palette rejection changed old mixed display/model/registry/camera"
        );
        // Same catalogue bytes at the same path do not mint the actual producer Arc.
        let fresh_pc = Arc::new(PortrayalCatalogue::load_bound(&self.s102_pc_path)?);
        anyhow::ensure!(
            fresh_pc.source_digest() == s102_pc.source_digest()
                && !Arc::ptr_eq(&fresh_pc, &s102_pc),
            "PC ownership negative is not same-byte independent capture"
        );
        let foreign_owner_vector = self.prepare_joint_audit_vector(&profile)?;
        let foreign_owner_raster = self.prepare_joint_audit_raster(false)?;
        let pc_rejection = match self
            .renderer
            .as_ref()
            .unwrap()
            .prepare_ready_vector_bound_s102_publication(
                foreign_owner_vector,
                foreign_owner_raster,
                fresh_pc,
                profile.clone(),
                depth,
            ) {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Same-byte foreign S102 owner accepted"),
        };
        anyhow::ensure!(
            pc_rejection.contains("producer differs"),
            "Foreign owner refusal reason: {pc_rejection}"
        );
        direct_exact(self.renderer.as_ref().unwrap())?;
        anyhow::ensure!(
            self.bathymetry_publication_model()? == before
                && self.catalogue_audit_binding()? == catalogue
                && self.navigation_camera_bits() == camera,
            "Foreign owner refusal changed original state"
        );
        let vector = self.prepare_joint_audit_vector(&profile)?;
        let vector_index_counts = vector.geometry_index_counts();
        anyhow::ensure!(
            vector_index_counts.iter().any(|count| *count > 0),
            "No actual original vector geometry in mixed fixture"
        );
        let text_draw_count = vector.text_draw_count();
        let target_revision = vector.context().geometry_revision();
        let raster = self.prepare_joint_audit_raster(false)?;
        let ready = self
            .renderer
            .as_ref()
            .unwrap()
            .prepare_ready_vector_bound_s102_publication(
                vector,
                raster,
                s102_pc.clone(),
                profile.clone(),
                depth,
            )?;
        let stale_vector = self.prepare_joint_audit_vector(&profile)?;
        let stale_raster = self.prepare_joint_audit_raster(false)?;
        let stale = self
            .renderer
            .as_ref()
            .unwrap()
            .prepare_ready_vector_bound_s102_publication(
                stale_vector,
                stale_raster,
                s102_pc.clone(),
                profile.clone(),
                depth,
            )?;
        anyhow::ensure!(
            self.bathymetry_publication_model()? == before
                && self.catalogue_audit_binding()? == catalogue
                && self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera,
            "Joint preparation changed App owners"
        );
        let installed = self
            .renderer
            .as_mut()
            .unwrap()
            .activate_ready_vector_raster_publication(ready)?;
        let (context, resources) = installed.into_parts();
        self.render_context = context;
        self.cell_resources = Some(resources);
        direct_exact(self.renderer.as_ref().unwrap())?;
        anyhow::ensure!(
            self.render_context.geometry_revision() == target_revision
                && target_revision != original_revision,
            "Joint did not adopt actual new context"
        );
        let stale_rejection = match self
            .renderer
            .as_mut()
            .unwrap()
            .activate_ready_vector_raster_publication(stale)
        {
            Err(e) => e.to_string(),
            Ok(_) => anyhow::bail!("Joint accepted pre-swap stale capsule"),
        };
        anyhow::ensure!(
            stale_rejection.contains("publication binding changed"),
            "Stale joint rejection cause differs: {stale_rejection}"
        );
        direct_exact(self.renderer.as_ref().unwrap())?;
        anyhow::ensure!(
            capture(self, &output.join("after-joint.png"))? == reference,
            "Joint swap or stale rejection changed whole mixed RGBA"
        );
        let mut frame_hashes = Vec::with_capacity(5);
        for i in 0..5 {
            self.renderer.as_mut().unwrap().render()?;
            direct_exact(self.renderer.as_ref().unwrap())?;
            let actual = capture(self, &output.join(format!("normal-{i}.png")))?;
            anyhow::ensure!(
                actual == reference,
                "Normal mixed frame differs after joint activation"
            );
            frame_hashes.push(format!("{:x}", Sha256::digest(&actual.2)));
        }
        anyhow::ensure!(
            self.bathymetry_publication_model()? == before
                && self.catalogue_audit_binding()? == catalogue
                && self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera
                && bincode::serialize(
                    &self
                        .renderer
                        .as_ref()
                        .unwrap()
                        .displayed_symbols_with_sources()
                )? == symbols,
            "Joint activation changed original model/registry/camera/symbol source metadata"
        );
        let report = serde_json::json!({"passed":true,"scope":"same-original/current-camera genuine S101 and regular S102 Renderer joint activation; no full App/journal/continuous authorization or FPS claim",
            "hidden":true,"focused":false,"charts":self.cells.len(),"bathymetry_instances":self.bathymetry.len(),
            "original_ir_count":self.render_context.instruction_count(),"text_draw_count":text_draw_count,
            "original_ir_sha256":format!("{:x}",Sha256::digest(bincode::serialize(self.render_context.raw_instructions())?)),
            "rgba_sha256":format!("{:x}",Sha256::digest(&reference.2)),"rgba_bytes":reference.2.len(),"extent":[reference.0,reference.1],
            "frame_hashes":frame_hashes,"camera_rejection":camera_rejection,"palette_rejection":palette_rejection,"stale_rejection":stale_rejection,
            "direct_mixed_rgba_sha256":format!("{:x}",Sha256::digest(&direct_reference.rgba8)),
            "direct_immediate_gpu_exact":true,"raster_actual_pixel_contribution":true,
            "visible_raster_count":raster_visible_count,"vector_index_counts":vector_index_counts,
            "model_registry_camera_exact":true,"displayed_symbols_exact":true,"new_context_revision_adopted":true,
            "no_update_view_repair":true,"raster_pc_digest":s102_pc.source_digest(),"profile":profile,"raster_evaluation_owner_checked":true,"same_byte_foreign_pc_owner_rejection":pc_rejection,
            "raster_semantic_producer_binding_proven":false,"continuous_positive_proven":false,"positive_pick_query_proven":false});
        fs::write(
            output.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        Ok(())
    }

    /// Same-original single-PC private preparation only; not an App/raster publication.
    fn audit_private_single_pc_vector_gpu(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Hidden unfocused window required"
        );
        anyhow::ensure!(
            self.flat_eventloop_audit.is_none() && !self.flat_eventloop_audit_started,
            "Private readback forbidden in measured callbacks"
        );
        anyhow::ensure!(
            self.chart_loaded && self.cells.len() == 1 && self.bathymetry.is_empty(),
            "Single original chain and vector-only scene required"
        );
        anyhow::ensure!(
            !self.zoom_animating && self.zoom_rebuild_phase == 0 && self.pan_rebuild_phase == 0,
            "Settled camera required"
        );
        anyhow::ensure!(
            self.catalogue_owner_for_cell(0)
                .is_some_and(|owner| owner.pc().source_digest() == self.pc.source_digest()),
            "Single PC audit cannot force global resources onto another cell PC"
        );
        anyhow::ensure!(
            fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "Audit destination must be absent including symlink"
        );
        let original = self.render_context.raw_instructions();
        anyhow::ensure!(
            original.len() <= 500_000 && bincode::serialized_size(original)? <= 64 * 1024 * 1024,
            "Private clone logical IR admission exceeded"
        );
        let original_hash = format!("{:x}", Sha256::digest(bincode::serialize(original)?));
        let before = self.catalogue_audit_binding()?;
        let camera = self.navigation_camera_bits();
        let registry = self.catalogue_registry.checkpoint();
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let reference = renderer.audit_displayed_vector_frame()?;
        let settings = renderer.vector_emission_settings();
        let groups = self.get_visible_viewing_groups();
        let boxes = renderer.vector_world_map_chart_boxes();
        anyhow::ensure!(boxes.len() <= 4096, "World-map copy admission exceeded");
        let prepare = |settings| -> Result<ferrite_wgpu::PreparedSinglePcVectorEmission> {
            let origin = renderer.capture_publication_origin();
            let mut next = self.render_context.empty_for_rebuild();
            next.set_instructions_from_cache(original.to_vec());
            prepare_flat_coverage(
                self.coverage_inventory.as_deref(),
                &mut next,
                window.inner_size(),
                window.scale_factor(),
            )?;
            anyhow::ensure!(
                format!(
                    "{:x}",
                    Sha256::digest(bincode::serialize(next.raw_instructions())?)
                ) == original_hash,
                "Private coverage altered original ordered IR"
            );
            let wrap = 360.0 * next.scaler.scale_x() as f32;
            let cache = SymbolCache::new_with_pattern_contract(
                self.pc.root_path.join("Symbols"),
                self.pc.sources(),
                ferrite_s101::shallow_pattern_contract(&self.pc),
            );
            let resources = ferrite_wgpu::SinglePcVectorResources::new(
                cache,
                Arc::clone(&self.pc),
                &self.current_profile_name,
                groups.clone(),
            )?;
            Ok(
                renderer.prepare_private_single_pc_vector_emission_from_origin(
                    origin,
                    next,
                    resources,
                    settings,
                    wrap,
                    boxes.to_vec(),
                )?,
            )
        };
        let emission = prepare(settings)?;
        let ready = renderer.prepare_private_single_pc_vector_gpu(emission)?;
        renderer.validate_private_single_pc_vector_gpu(&ready)?;
        let candidate = renderer.audit_private_single_pc_vector_frame(&ready)?;
        anyhow::ensure!(
            reference.extent == candidate.extent
                && reference.texture_format == candidate.texture_format
                && reference.samples == candidate.samples
                && reference.rgba8 == candidate.rgba8,
            "Single PC private GPU RGBA differs from original"
        );
        let payload = ready.buffer_payload_bytes();
        let indices = ready.geometry_index_counts();
        let text_draws = ready.text_draw_count();
        let target = ready.target_continuous_transform_key();
        drop(candidate);
        drop(ready);
        let after = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            reference.rgba8 == after.rgba8,
            "Private Single PC preparation changed original GPU scene"
        );
        drop(after);
        let mut invalid = settings;
        invalid.symbol_scale = 0.;
        let rejection = match prepare(invalid) {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Invalid finite private Single PC settings admitted"),
        };
        let after_failure = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            reference.rgba8 == after_failure.rgba8,
            "Private Single PC refusal changed original GPU scene"
        );
        anyhow::ensure!(
            self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera
                && self.catalogue_audit_binding()? == before,
            "Private Single PC stage changed App/registry/source/pick snapshot"
        );
        // Drift only the hidden audit UI policy AFTER CPU preparation. The old
        // snapshot must span the CPU/GPU gap, rather than be recaptured at GPU start.
        let stale_emission = prepare(settings)?;
        let early_origin = renderer.capture_publication_origin();
        let mut early_context = self.render_context.empty_for_rebuild();
        early_context.set_instructions_from_cache(original.to_vec());
        let early_resources = ferrite_wgpu::SinglePcVectorResources::new(
            SymbolCache::new_with_pattern_contract(
                self.pc.root_path.join("Symbols"),
                self.pc.sources(),
                ferrite_s101::shallow_pattern_contract(&self.pc),
            ),
            Arc::clone(&self.pc),
            &self.current_profile_name,
            groups.clone(),
        )?;
        let early_boxes = boxes.to_vec();
        let renderer = self.renderer.as_mut().context("Renderer required")?;
        let early_old_profile = std::mem::replace(
            &mut renderer.ui_state.color_profile,
            "audit-early-policy-drift".to_owned(),
        );
        let early_result = renderer.prepare_private_single_pc_vector_emission_from_origin(
            early_origin,
            early_context,
            early_resources,
            settings,
            360. * self.render_context.scaler.scale_x() as f32,
            early_boxes,
        );
        renderer.ui_state.color_profile = early_old_profile;
        let early_rejection = match early_result {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Early SinglePc policy drift admitted"),
        };
        anyhow::ensure!(
            early_rejection.contains("Early App publication origin changed"),
            "Unexpected early SinglePc refusal: {early_rejection}"
        );
        anyhow::ensure!(
            renderer.audit_displayed_vector_frame()?.rgba8 == reference.rgba8,
            "Early SinglePc refusal changed original GPU scene"
        );
        let old_profile = std::mem::replace(
            &mut renderer.ui_state.color_profile,
            "audit-policy-drift".to_owned(),
        );
        let stale_result = renderer.prepare_private_single_pc_vector_gpu(stale_emission);
        renderer.ui_state.color_profile = old_profile; // restore before any audit Result
        let stale_rejection = match stale_result {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("CPU/GPU policy drift admitted by private preparation"),
        };
        anyhow::ensure!(
            stale_rejection.contains("environment is stale"),
            "Unexpected drift refusal: {stale_rejection}"
        );
        anyhow::ensure!(
            renderer.audit_displayed_vector_frame()?.rgba8 == reference.rgba8,
            "CPU/GPU drift refusal changed original GPU scene"
        );
        fs::create_dir(output)?;
        fs::write(
            output.join("report.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "passed":true,"hidden":true,"focused":false,"resource_mode":"SinglePc",
                "early_policy_drift_rejection":early_rejection,"early_policy_drift_old_gpu_exact":true,
                "cpu_gpu_policy_drift_rejection":stale_rejection,
                "cpu_gpu_policy_drift_old_gpu_exact":true,
                "original_ir_sha256":original_hash,"original_ir_count":original.len(),
                "rgba_sha256":format!("{:x}",Sha256::digest(&reference.rgba8)),"rgba_bytes":reference.rgba8.len(),
                "extent":reference.extent,"gpu_index_counts":indices,"new_gpu_payload_bytes":payload,
                "text_draw_count":text_draws,"target_transform_key":target,"rejection":rejection,
                "displayed_after_candidate_exact":true,"displayed_after_rejection_exact":true,
                "registry_checkpoint_exact":true,"publication_snapshot_exact":true,
                "scope":"Single same-original PC vector-only private GPU preparation; no activation, mixed raster/App atomic commit, official cancellation or FPS proof"
            }))?,
        )?;
        Ok(())
    }

    /// Explicit hidden, untimed vector-only GPU preparation proof; never publishes a scene.
    ///
    /// Real changed-profile Lua preparation plus target-camera staging, no activation.
    fn prepare_target_audit_capsule(
        &mut self,
        single: bool,
        wrong_word: Option<usize>,
        wrong_profile: bool,
    ) -> Result<(TargetAuditCapsule, serde_json::Value)> {
        self.ensure_navigation_scene()?;
        let origin = self
            .renderer
            .as_ref()
            .context("Renderer required")?
            .capture_publication_origin();
        let request = PortrayalChangeRequest {
            profile: "Dusk".to_owned(),
            settings: self.applied_settings.clone(),
        };
        anyhow::ensure!(
            self.current_profile_name != request.profile,
            "TARGET audit needs non-Dusk OLD policy"
        );
        let change = self.prepare_portrayal_change(request)?;
        let PreparedPortrayalChange {
            portrayal,
            symbols,
            request,
            background,
            ..
        } = change;
        let PreparedPortrayal {
            context: mut next,
            catalogue_registry,
            cell_resources,
            coverage,
            raster_scene,
            ..
        } = portrayal;
        anyhow::ensure!(
            next.raw_instructions().len() <= 500_000
                && bincode::serialized_size(next.raw_instructions())? <= 64 * 1024 * 1024,
            "TARGET candidate IR exceeds logical budget"
        );
        let old_camera = self.render_context.scaler.flat_encoded_identity();
        next.scaler.pan(17., 0.);
        let target_camera = next.scaler.flat_encoded_identity();
        anyhow::ensure!(
            target_camera.is_some() && target_camera != old_camera,
            "TARGET camera control did not change actual camera"
        );
        let window = self.window.as_ref().context("Window required")?;
        prepare_flat_coverage(
            Some(coverage.as_ref()),
            &mut next,
            window.inner_size(),
            window.scale_factor(),
        )?;
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let mut scene = raster_scene
            .context("TARGET requires explicitly staged full empty raster inventory")?;
        renderer.reproject_target_raster_scene(&mut scene, &next.scaler)?;
        let preset = match request.settings.display_mode {
            DisplayMode::Base => ferrite_s101::DisplayPreset::Base,
            DisplayMode::Standard => ferrite_s101::DisplayPreset::Standard,
            DisplayMode::All => ferrite_s101::DisplayPreset::Other,
        };
        let mut groups = ferrite_s101::viewing_groups_for_preset(&self.pc, preset)?;
        groups.extend(primary_viewing_layers::resolve(
            &self.pc,
            &request.settings,
        )?);
        groups.insert(21010);
        let mut emitted = renderer.vector_emission_settings();
        emitted.show_shallow_pattern = request.settings.show_shallow_pattern;
        emitted.background_color = background;
        let mut expected = emitted;
        if let Some(word) = wrong_word {
            match word {
                0 => expected.symbol_scale = f32::from_bits(expected.symbol_scale.to_bits() ^ 1),
                1 => expected.show_soundings = !expected.show_soundings,
                2 => expected.animation_mode = !expected.animation_mode,
                3 => expected.show_shallow_pattern = !expected.show_shallow_pattern,
                4..=7 => {
                    let mut c = expected.background_color.to_array();
                    c[word - 4] = f32::from_bits(c[word - 4].to_bits() ^ 1);
                    expected.background_color = ferrite_render::Color::rgba(c[0], c[1], c[2], c[3]);
                }
                _ => anyhow::bail!("Invalid emission word control"),
            }
        }
        let colors = emitted.background_color.to_array();
        let bits = [
            emitted.symbol_scale.to_bits(),
            u32::from(emitted.show_soundings),
            u32::from(emitted.animation_mode),
            u32::from(emitted.show_shallow_pattern),
            colors[0].to_bits(),
            colors[1].to_bits(),
            colors[2].to_bits(),
            colors[3].to_bits(),
        ];
        let wrap = 360. * next.scaler.scale_x() as f32;
        let boxes = renderer.vector_world_map_chart_boxes();
        anyhow::ensure!(boxes.len() <= 4096, "TARGET world-map receiver cap");
        let target_profile = if wrong_profile {
            self.current_profile_name.clone()
        } else {
            request.profile.clone()
        };
        let (capsule, counts) = if single {
            anyhow::ensure!(
                self.cells.iter().enumerate().all(|(i, _)| self
                    .catalogue_owner_for_cell(i)
                    .is_some_and(|o| Arc::ptr_eq(o.pc(), &self.pc))),
                "SinglePc TARGET fixture must have actual identical PC owners"
            );
            let resources = ferrite_wgpu::SinglePcVectorResources::new(
                symbols,
                self.pc.clone(),
                &request.profile,
                Some(groups),
            )?;
            let emitted_cpu = renderer.prepare_private_single_pc_vector_emission_from_origin(
                origin,
                next,
                resources,
                emitted,
                wrap,
                boxes.to_vec(),
            )?;
            let ready = renderer.prepare_private_single_pc_vector_gpu(emitted_cpu)?;
            let counts = ready.geometry_index_counts();
            let capsule = renderer.prepare_target_single_pc_vector_raster_publication(
                ready,
                scene,
                request.settings,
                target_profile,
                expected,
            )?;
            (TargetAuditCapsule::TargetSingle(capsule), counts)
        } else {
            let resources = match cell_resources {
                Some(resources) => resources,
                None => self.prepare_cell_resources(&catalogue_registry, &request)?,
            };
            let emitted_cpu = renderer.prepare_private_vector_emission_from_origin(
                origin,
                next,
                resources,
                Some(&groups),
                emitted,
                wrap,
                boxes.to_vec(),
            )?;
            let ready = renderer.prepare_private_vector_gpu(emitted_cpu)?;
            let counts = ready.geometry_index_counts();
            let capsule = renderer.prepare_target_vector_raster_publication(
                ready,
                scene,
                request.settings,
                target_profile,
                expected,
            )?;
            (TargetAuditCapsule::TargetOwned(capsule), counts)
        };
        anyhow::ensure!(
            counts.iter().any(|n| *n != 0),
            "TARGET has no actual geometry"
        );
        Ok((
            capsule,
            serde_json::json!({"target_profile":"Dusk", "old_camera":old_camera,
            "target_camera":target_camera,"target_emission_words":bits,"index_counts":counts}),
        ))
    }

    /// Dedicated hidden renderer test only. The caller exits before App interaction resumes.
    fn activate_target_audit_capsule(
        &mut self,
        capsule: TargetAuditCapsule,
        zoom: f64,
    ) -> Result<ferrite_render::RenderContext> {
        let compilation_scale = self
            .cells
            .iter()
            .map(ferrite_s101::coverage_scale::dataset_reference_scale)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(0);
        let renderer = self.renderer.as_mut().context("Renderer required")?;
        let context = match capsule {
            TargetAuditCapsule::TargetOwned(c) => {
                let bindings = renderer.activate_target_vector_raster_publication(
                    c,
                    zoom,
                    compilation_scale,
                )?;
                let (context, resources) = bindings.into_parts();
                anyhow::ensure!(
                    resources.cell_count() == self.cells.len()
                        && resources.cell_profiles().all(|(_, p)| p.id == "Dusk"),
                    "Activated OwnedCells resources differ from actual target"
                );
                context
            }
            TargetAuditCapsule::TargetSingle(c) => {
                let bindings = renderer.activate_target_single_pc_vector_raster_publication(
                    c,
                    zoom,
                    compilation_scale,
                )?;
                let (context, resources) = bindings.into_parts();
                anyhow::ensure!(
                    Arc::ptr_eq(resources.catalogue(), &self.pc)
                        && resources.profile().id == "Dusk",
                    "Activated SinglePc resources lost original PC owner"
                );
                context
            }
        };
        Ok(context)
    }

    /// Opt-in, hidden and untimed qualification; optional renderer activation exits immediately.
    fn audit_target_vector_raster_preparation(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden TARGET audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Hidden unfocused TARGET audit required"
        );
        anyhow::ensure!(
            self.flat_eventloop_audit.is_none() && !self.flat_eventloop_audit_started,
            "TARGET readback forbidden in measured callbacks"
        );
        anyhow::ensure!(
            self.chart_loaded
                && !self.cells.is_empty()
                && self.bathymetry.is_empty()
                && self.cells.len() <= 128
                && self.render_context.raw_instructions().len() <= 500_000
                && bincode::serialized_size(self.render_context.raw_instructions())?
                    <= 64 * 1024 * 1024,
            "TARGET audit needs bounded vector-only original fixture"
        );
        anyhow::ensure!(
            !self.zoom_animating && self.zoom_rebuild_phase == 0 && self.pan_rebuild_phase == 0,
            "TARGET audit needs settled navigation"
        );
        anyhow::ensure!(
            fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "TARGET output must be absent including symlink"
        );
        let single = std::env::var("FERRITE_PRIVATE_SINGLE_PC_GPU_AUDIT").as_deref() == Ok("1");
        let before = self.publication_model()?;
        let catalogues = self.catalogue_audit_binding()?;
        let registry = self.catalogue_registry.checkpoint();
        let camera = self.navigation_camera_bits();
        let raw = bincode::serialize(self.render_context.raw_instructions())?;
        let order = self.render_context.static_instruction_order_identity();
        let revision = self.render_context.geometry_revision();
        let symbols_revision = self.symbol_cache.resource_revision();
        let old_pc = self.pc.clone();
        let old_fc = self.fc.clone();
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        anyhow::ensure!(
            renderer.settings() == &self.applied_settings
                && renderer.ui_state.color_profile == self.current_profile_name
                && renderer.geometry_matches_view(&self.render_context.scaler),
            "OLD policy/view mismatch"
        );
        let (pan, zoom, pivot) = renderer.fast_view_transform();
        anyhow::ensure!(
            pan == (0., 0.)
                && zoom == 1.
                && pivot == (0., 0.)
                && renderer.fast_view_scales().1 == 1.,
            "OLD affine preview must be settled"
        );
        let old_origin = renderer.capture_publication_origin();
        let old_pixels = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            old_pixels.rgba8.len() <= 64 * 1024 * 1024,
            "TARGET readback budget exceeded"
        );
        // Preparing and rejecting candidates must preserve the early origin.
        // A normal render intentionally advances the GPU resource generation;
        // compare its displayed pixels and App model separately below.
        let check_old_pixels_and_model = |app: &ChartApp| -> Result<()> {
            let renderer = app.renderer.as_ref().context("Renderer required")?;
            let actual = renderer.audit_displayed_vector_frame()?;
            anyhow::ensure!(
                actual.rgba8 == old_pixels.rgba8
                    && actual.extent == old_pixels.extent
                    && actual.texture_format == old_pixels.texture_format
                    && actual.samples == old_pixels.samples,
                "TARGET staging/rejection changed OLD decoded chart pixels"
            );
            anyhow::ensure!(
                app.publication_model()? == before
                    && app.catalogue_audit_binding()? == catalogues
                    && app.catalogue_registry.matches_checkpoint(&registry)
                    && app.navigation_camera_bits() == camera
                    && bincode::serialize(app.render_context.raw_instructions())? == raw
                    && app.render_context.geometry_revision() == revision
                    && Arc::ptr_eq(
                        &order,
                        &app.render_context.static_instruction_order_identity()
                    )
                    && app.symbol_cache.resource_revision() == symbols_revision
                    && Arc::ptr_eq(&old_pc, &app.pc)
                    && Arc::ptr_eq(&old_fc, &app.fc),
                "TARGET staging/rejection changed OLD App/source/registry/resources/picks"
            );
            Ok(())
        };
        let check_old = |app: &ChartApp| -> Result<()> {
            app.renderer
                .as_ref()
                .context("Renderer required")?
                .validate_publication_origin(&old_origin)?;
            check_old_pixels_and_model(app)
        };
        let mut target_pixels: Option<ferrite_wgpu::PrivateVectorReadback> = None;
        let mut target_metadata = serde_json::Value::Null;
        for _ in 0..2 {
            let (capsule, metadata) = self.prepare_target_audit_capsule(single, None, false)?;
            let renderer = self.renderer.as_ref().context("Renderer required")?;
            let pixels = match &capsule {
                TargetAuditCapsule::TargetOwned(c) => renderer.audit_target_vector_frame(c)?,
                TargetAuditCapsule::TargetSingle(c) => {
                    renderer.audit_target_single_pc_vector_frame(c)?
                }
            };
            anyhow::ensure!(
                pixels.extent == old_pixels.extent
                    && pixels.texture_format == old_pixels.texture_format
                    && pixels.samples == old_pixels.samples
                    && pixels.rgba8.len() <= 64 * 1024 * 1024
                    && pixels.rgba8 != old_pixels.rgba8,
                "Changed TARGET has wrong dimensions/budget or unchanged pixels"
            );
            if let Some(previous) = &target_pixels {
                anyhow::ensure!(
                    previous.rgba8 == pixels.rgba8 && target_metadata == metadata,
                    "Repeated actual TARGET preparation differs"
                );
            }
            target_metadata = metadata;
            target_pixels = Some(pixels);
            drop(capsule);
            check_old(self)?;
        }
        let mut rejections = Vec::new();
        for word in 0..8 {
            let error = match self.prepare_target_audit_capsule(single, Some(word), false) {
                Err(error) => format!("{error:#}"),
                Ok(_) => anyhow::bail!("TARGET emission word {word} mismatch admitted"),
            };
            anyhow::ensure!(
                error.contains("TARGET policy differs"),
                "Unexpected word {word} rejection: {error}"
            );
            rejections
                .push(serde_json::json!({"control":"emission_word","word":word,"error":error}));
            check_old(self)?;
        }
        let profile_error = match self.prepare_target_audit_capsule(single, None, true) {
            Err(error) => format!("{error:#}"),
            Ok(_) => anyhow::bail!("TARGET wrong profile admitted"),
        };
        anyhow::ensure!(
            profile_error.contains("Vector target palette differs"),
            "Unexpected target profile rejection: {profile_error}"
        );
        rejections.push(serde_json::json!({"control":"target_profile","error":profile_error}));
        check_old(self)?;
        let (capsule, _) = self.prepare_target_audit_capsule(single, None, false)?;
        let renderer = self.renderer.as_mut().context("Renderer required")?;
        let old_profile = std::mem::replace(
            &mut renderer.ui_state.color_profile,
            "audit-late-target-old-drift".to_owned(),
        );
        let drift_result = match &capsule {
            TargetAuditCapsule::TargetOwned(c) => {
                renderer.validate_target_vector_raster_publication(c)
            }
            TargetAuditCapsule::TargetSingle(c) => {
                renderer.validate_target_single_pc_vector_raster_publication(c)
            }
        };
        renderer.ui_state.color_profile = old_profile;
        let drift_error = match drift_result {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Late OLD drift admitted"),
        };
        anyhow::ensure!(
            drift_error.contains("publication binding changed"),
            "Unexpected OLD drift rejection: {drift_error}"
        );
        drop(capsule);
        rejections.push(serde_json::json!({"control":"late_old_value_drift","error":drift_error,"aba_epoch_proven":false}));
        check_old(self)?;
        self.renderer
            .as_mut()
            .context("Renderer required")?
            .render()?;
        check_old_pixels_and_model(self)?;
        let final_origin_error = self
            .renderer
            .as_ref()
            .context("Renderer required")?
            .validate_publication_origin(&old_origin)
            .expect_err("A normal render must invalidate the old GPU generation");
        anyhow::ensure!(
            final_origin_error
                .to_string()
                .contains("Early App publication origin changed"),
            "Unexpected final render generation rejection: {final_origin_error}"
        );
        let target_pixels = target_pixels.context("Missing actual TARGET readback")?;
        let activation_requested =
            std::env::var("FERRITE_TARGET_ACTIVATION_AUDIT").as_deref() == Ok("1");
        let emission_bits = |v: ferrite_wgpu::VectorEmissionSettings| {
            let c = v.background_color.to_array();
            [
                v.symbol_scale.to_bits(),
                u32::from(v.show_soundings),
                u32::from(v.animation_mode),
                u32::from(v.show_shallow_pattern),
                c[0].to_bits(),
                c[1].to_bits(),
                c[2].to_bits(),
                c[3].to_bits(),
            ]
        };
        let mut activation = serde_json::Value::Null;
        if activation_requested {
            // All earlier OLD checks are complete. Never resume the App after this renderer-only test.
            let (invalid, _) = self.prepare_target_audit_capsule(single, None, false)?;
            let invalid_error = match self.activate_target_audit_capsule(invalid, f64::NAN) {
                Err(e) => format!("{e:#}"),
                Ok(_) => anyhow::bail!("Nonfinite TARGET zoom admitted"),
            };
            anyhow::ensure!(
                invalid_error.contains("Invalid TARGET navigation labels"),
                "Unexpected invalid zoom rejection: {invalid_error}"
            );
            check_old_pixels_and_model(self)?;
            let (candidate, _) = self.prepare_target_audit_capsule(single, None, false)?;
            let (sibling, _) = self.prepare_target_audit_capsule(single, None, false)?;
            let (target_settings, target_emission, target_camera) = match &candidate {
                TargetAuditCapsule::TargetOwned(c) => (
                    c.target_settings().clone(),
                    c.target_emission_settings(),
                    c.context().scaler.flat_encoded_identity(),
                ),
                TargetAuditCapsule::TargetSingle(c) => (
                    c.target_settings().clone(),
                    c.target_emission_settings(),
                    c.context().scaler.flat_encoded_identity(),
                ),
            };
            let before_activation = self
                .renderer
                .as_ref()
                .context("Renderer required")?
                .capture_publication_origin();
            let context = self.activate_target_audit_capsule(candidate, self.zoom_level)?;
            anyhow::ensure!(
                context.scaler.flat_encoded_identity() == target_camera,
                "Activated context camera differs from staged target"
            );
            let renderer = self.renderer.as_ref().context("Renderer required")?;
            anyhow::ensure!(
                renderer.settings() == &target_settings
                    && renderer.ui_state.color_profile == "Dusk"
                    && renderer.ui_state.zoom_level.to_bits() == self.zoom_level.to_bits()
                    && emission_bits(renderer.vector_emission_settings())
                        == emission_bits(target_emission),
                "Activated renderer policy differs from staged target"
            );
            anyhow::ensure!(
                renderer
                    .validate_publication_origin(&before_activation)
                    .is_err(),
                "TARGET activation did not retire OLD resource generation"
            );
            let live_target = renderer.audit_displayed_vector_frame()?;
            anyhow::ensure!(
                live_target.rgba8 == target_pixels.rgba8
                    && live_target.extent == target_pixels.extent
                    && live_target.texture_format == target_pixels.texture_format
                    && live_target.samples == target_pixels.samples,
                "Activated vector pixels differ from private TARGET readback"
            );
            let activated_origin = renderer.capture_publication_origin();
            let stale_error = match self.activate_target_audit_capsule(sibling, self.zoom_level) {
                Err(e) => format!("{e:#}"),
                Ok(_) => anyhow::bail!("Stale sibling TARGET activated twice"),
            };
            anyhow::ensure!(
                stale_error.contains("publication binding changed"),
                "Unexpected stale sibling rejection: {stale_error}"
            );
            let renderer = self.renderer.as_ref().context("Renderer required")?;
            renderer.validate_publication_origin(&activated_origin)?;
            anyhow::ensure!(
                renderer.audit_displayed_vector_frame()?.rgba8 == target_pixels.rgba8
                    && renderer.settings() == &target_settings
                    && renderer.ui_state.color_profile == "Dusk"
                    && emission_bits(renderer.vector_emission_settings())
                        == emission_bits(target_emission),
                "Stale activation rejection changed TARGET"
            );
            self.renderer
                .as_mut()
                .context("Renderer required")?
                .render()?;
            let renderer = self.renderer.as_ref().context("Renderer required")?;
            anyhow::ensure!(
                renderer.audit_displayed_vector_frame()?.rgba8 == target_pixels.rgba8,
                "Normal render after TARGET activation changed vector pixels"
            );
            anyhow::ensure!(
                self.publication_model()? == before
                    && self.catalogue_registry.matches_checkpoint(&registry)
                    && self.navigation_camera_bits() == camera,
                "Renderer-only audit unexpectedly changed App model"
            );
            activation = serde_json::json!({"passed":true,"renderer_only":true,
                "invalid_zoom_rejected":invalid_error,"stale_sibling_rejected":stale_error,
                "returned_target_camera_exact":true,"actual_resources_checked":true,
                "private_target_equals_displayed_before_and_after_normal_render":true,
                "rejection_preserves_activated_origin":true,
                "app_model_adopted":false,"app_exits_immediately":true,
                "genuine_s102_native":"unperformed","live_overlay_rejection_native":"unperformed"});
        }
        let report = serde_json::json!({"schema":1,"passed":true,
            "scope":"hidden untimed TARGET preparation and optional renderer-only activation; vector readback excludes raster composition, egui, selection overlays and native routes",
            "mode":if single {"SinglePc"} else {"OwnedCells"},"target":target_metadata,
            "old_rgba_sha256":format!("{:x}",Sha256::digest(&old_pixels.rgba8)),
            "target_rgba_sha256":format!("{:x}",Sha256::digest(&target_pixels.rgba8)),
            "target_repeated_exact":true,"target_differs_from_old":true,
            "old_after_all_controls_and_final_render_exact":true,
            "old_origin_preserved_after_preparation_and_rejections":true,
            "normal_render_invalidates_old_gpu_generation":true,
            "final_render_preserves_gpu_owner_identity":false,"rejections":rejections,
            "actual_s102_bound_owner_native_test":"unperformed; requires genuine mixed fixture",
            "raster_bearing_camera_negative":"not applicable to explicit empty inventory",
            "current_only_activation_negative":"unperformed in this audit",
            "full_app_atomic":false,"target_activated":activation_requested,
            "activation":activation,"independent_target_composition_oracle":false,
            "fps_measured":false});
        fs::create_dir(output)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join("report.json"))?;
        serde_json::to_writer_pretty(&mut file, &report)?;
        Ok(())
    }

    fn audit_private_vector_gpu(&mut self, output: &Path) -> Result<()> {
        if std::env::var("FERRITE_TARGET_VECTOR_RASTER_AUDIT").as_deref() == Ok("1") {
            return self.audit_target_vector_raster_preparation(output);
        }
        if std::env::var("FERRITE_PRIVATE_SINGLE_PC_GPU_AUDIT").as_deref() == Ok("1") {
            return self.audit_private_single_pc_vector_gpu(output);
        }
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden private vector audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused private audit forbidden"
        );
        anyhow::ensure!(
            self.flat_eventloop_audit.is_none() && !self.flat_eventloop_audit_started,
            "Private readback is forbidden in measured callbacks"
        );
        anyhow::ensure!(
            self.chart_loaded && !self.cells.is_empty() && self.bathymetry.is_empty(),
            "Loaded vector-only scene required"
        );
        anyhow::ensure!(
            self.cells.len() <= 128 && self.render_context.raw_instructions().len() <= 500_000,
            "Private audit CPU inventory limit exceeded"
        );
        anyhow::ensure!(
            !self.zoom_animating && self.zoom_rebuild_phase == 0 && self.pan_rebuild_phase == 0,
            "Settled camera required"
        );
        anyhow::ensure!(
            fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "Audit destination must be absent (including symlink)"
        );
        // Admission is serialized logical payload, NOT allocator capacity, joint scene memory or RSS.
        const IR_BYTES: u64 = 64 * 1024 * 1024;
        let original = self.render_context.raw_instructions();
        let serialized_bytes = bincode::serialized_size(original)?;
        anyhow::ensure!(
            serialized_bytes <= IR_BYTES,
            "Private IR clone admission exceeds 64MiB serialized payload"
        );
        let original_hash = format!("{:x}", Sha256::digest(bincode::serialize(original)?));
        let before = self.catalogue_audit_binding()?;
        let camera = self.navigation_camera_bits();
        let registry = self.catalogue_registry.checkpoint();
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let (pan, zoom, pivot) = renderer.fast_view_transform();
        anyhow::ensure!(
            pan == (0., 0.)
                && zoom == 1.
                && pivot == (0., 0.)
                && renderer.geometry_matches_view(&self.render_context.scaler),
            "Affine preview/stale geometry forbidden"
        );
        let settings = renderer.vector_emission_settings();
        let request = PortrayalChangeRequest {
            profile: self.current_profile_name.clone(),
            settings: self.applied_settings.clone(),
        };
        anyhow::ensure!(
            renderer.settings() == &self.applied_settings,
            "Applied/displayed settings mismatch"
        );
        anyhow::ensure!(
            renderer.ui_state.color_profile == self.current_profile_name,
            "Displayed palette mismatch"
        );
        let groups = self.get_visible_viewing_groups();
        let boxes = renderer.vector_world_map_chart_boxes();
        anyhow::ensure!(boxes.len() <= 4096, "World-map box clone cap exceeded");
        let reference = renderer.audit_displayed_vector_frame()?; // Also rejects native overlay/raster/dirty buffers.
        let origin = renderer.capture_publication_origin();
        let mut next = self.render_context.empty_for_rebuild();
        next.set_instructions_from_cache(original.to_vec());
        prepare_flat_coverage(
            self.coverage_inventory.as_deref(),
            &mut next,
            window.inner_size(),
            window.scale_factor(),
        )?;
        anyhow::ensure!(
            format!(
                "{:x}",
                Sha256::digest(bincode::serialize(next.raw_instructions())?)
            ) == original_hash,
            "Fresh coverage altered ordered original IR"
        );
        let resources = self.prepare_cell_resources(&self.catalogue_registry, &request)?;
        let wrap = 360.0 * next.scaler.scale_x() as f32; // Identical normal publish expression.
        let emission = renderer.prepare_private_vector_emission_from_origin(
            origin,
            next,
            resources,
            groups.as_ref(),
            settings,
            wrap,
            boxes.to_vec(),
        )?;
        let cpu_counts = emission.output_counts();
        let ready = renderer.prepare_private_vector_gpu(emission)?;
        renderer.validate_private_vector_gpu(&ready)?;
        let target_key = ready.target_continuous_transform_key();
        let expected_target_key = [
            (reference.extent[0] as f32).to_bits(),
            (reference.extent[1] as f32).to_bits(),
            0f32.to_bits(),
            0f32.to_bits(),
            1f32.to_bits(),
            1f32.to_bits(),
            0f32.to_bits(),
            0f32.to_bits(),
            wrap.to_bits(),
        ];
        anyhow::ensure!(
            target_key == expected_target_key,
            "Private target phase/extent/wrap binding differs"
        );
        let gpu_payload = ready.buffer_payload_bytes();
        let index_counts = ready.geometry_index_counts();
        let text_draws = ready.text_draw_count();
        let symbol_batches = ready.symbol_batch_count();
        let candidate = renderer.audit_private_vector_frame(&ready)?;
        anyhow::ensure!(
            reference.extent == candidate.extent
                && reference.texture_format == candidate.texture_format
                && reference.samples == candidate.samples
                && reference.rgba8 == candidate.rgba8,
            "Private candidate GPU RGBA differs"
        );
        let rgba_hash = format!("{:x}", Sha256::digest(&reference.rgba8));
        let rgba_len = reference.rgba8.len();
        drop(candidate);
        drop(ready);
        let old_again = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            reference.rgba8 == old_again.rgba8
                && reference.extent == old_again.extent
                && reference.texture_format == old_again.texture_format
                && reference.samples == old_again.samples,
            "Private preparation changed displayed GPU scene"
        );
        drop(old_again);
        // Finite, explicitly invalid policy; rejected at API entry before resources are sealed/emitted.
        // This deliberately does NOT invalidate live viewport/source or clear a published frame.
        let mut invalid = settings;
        invalid.symbol_scale = 0.;
        let mut rejected_context = self.render_context.empty_for_rebuild();
        rejected_context.set_instructions_from_cache(original.to_vec());
        let rejected_resources = self.prepare_cell_resources(&self.catalogue_registry, &request)?;
        let rejection = match renderer.prepare_private_vector_emission(
            rejected_context,
            rejected_resources,
            groups.as_ref(),
            invalid,
            wrap,
            boxes.to_vec(),
        ) {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Invalid finite private settings unexpectedly admitted"),
        };
        let after_failure = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            reference.rgba8 == after_failure.rgba8
                && reference.extent == after_failure.extent
                && reference.texture_format == after_failure.texture_format
                && reference.samples == after_failure.samples,
            "Private rejection changed displayed GPU scene"
        );
        anyhow::ensure!(
            self.catalogue_registry.matches_checkpoint(&registry)
                && self.navigation_camera_bits() == camera
                && self.catalogue_audit_binding()? == before,
            "Private preparation changed App/model/catalogue/pick snapshot"
        );
        let early_origin = renderer.capture_publication_origin();
        let mut early_context = self.render_context.empty_for_rebuild();
        early_context.set_instructions_from_cache(original.to_vec());
        let early_resources = self.prepare_cell_resources(&self.catalogue_registry, &request)?;
        let early_boxes = boxes.to_vec();
        let renderer = self.renderer.as_mut().context("Renderer required")?;
        let old_profile = std::mem::replace(
            &mut renderer.ui_state.color_profile,
            "audit-early-policy-drift".to_owned(),
        );
        let early_result = renderer.prepare_private_vector_emission_from_origin(
            early_origin,
            early_context,
            early_resources,
            groups.as_ref(),
            settings,
            wrap,
            early_boxes,
        );
        renderer.ui_state.color_profile = old_profile;
        let early_rejection = match early_result {
            Err(error) => error.to_string(),
            Ok(_) => anyhow::bail!("Early OwnedCells policy drift admitted"),
        };
        anyhow::ensure!(
            early_rejection.contains("Early App publication origin changed"),
            "Unexpected early OwnedCells refusal: {early_rejection}"
        );
        anyhow::ensure!(
            renderer.audit_displayed_vector_frame()?.rgba8 == reference.rgba8,
            "Early OwnedCells refusal changed original GPU scene"
        );
        let report = serde_json::json!({"schema":1,"passed":true,"scope":"hidden untimed vector chart GPU RGBA; excludes egui, selection overlay, raster and native routes; no App atomic commit claim",
            "rgba_sha256":rgba_hash,"rgba_bytes":rgba_len,"extent":reference.extent,"format":format!("{:?}",reference.texture_format),"samples":reference.samples,
            "original_ir_sha256":original_hash,"original_ir_count":original.len(),"serialized_ir_bytes":serialized_bytes,"serialized_ir_admission_limit":IR_BYTES,
            "cpu_output_counts":cpu_counts,"gpu_index_counts":index_counts,"new_gpu_buffer_payload_bytes":gpu_payload,"text_draw_count":text_draws,"symbol_batch_count":symbol_batches,
            "early_policy_drift_rejection":early_rejection,"early_policy_drift_old_gpu_exact":true,
            "target_transform_key":target_key,"camera_bits":camera,"rejection":rejection,"displayed_after_candidate_exact":true,"displayed_after_rejection_exact":true,
            "registry_checkpoint_exact":true,"publication_snapshot_exact":true,"positive_pick_query_proven":false,"individual_gpu_buffer_or_mask_readback":false});
        fs::create_dir(output)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join("report.json"))?;
        serde_json::to_writer_pretty(&mut file, &report)?;
        Ok(())
    }

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
    fn audit_decoded_chart_reopen(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let w = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            w.is_visible() == Some(false) && !w.has_focus(),
            "Visible audit forbidden"
        );
        anyhow::ensure!(
            self.cells.len() == 1 && self.bathymetry.is_empty(),
            "One S101 chain required"
        );
        anyhow::ensure!(
            std::env::var_os("FERRITE_NO_CACHE").is_none(),
            "Cache audit requires enabled cache"
        );
        anyhow::ensure!(!output.exists(), "Audit destination must be new");
        fs::create_dir_all(output)?;
        let paths = self.loaded_chain_paths[0].clone();
        let mut phases = Vec::new();
        self.decoded_chart_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Cache poisoned"))?
            .clear();
        let mut reference = None;
        for label in ["cold", "warm"] {
            self.ensure_navigation_scene()?;
            let (product, name) = s101_update_plan::dataset_key(&self.cells[0].dsid)?;
            self.unload_dataset(ferrite_wgpu::DatasetLayerId::S101 { product, name })?;
            anyhow::ensure!(self.cells.is_empty(), "Unload did not remove cell");
            let before = self.decoded_chart_cache.lock().unwrap().statistics();
            let started = std::time::Instant::now();
            let error = self.catalogue_audit_load(&paths)?;
            anyhow::ensure!(error.is_none(), "Reopen rejected: {error:?}");
            let elapsed = started.elapsed().as_secs_f64();
            self.ensure_navigation_scene()?;
            for _ in 0..5 {
                self.renderer.as_mut().unwrap().render()?;
                if self.sync_chart_layout() {
                    self.update_view();
                }
            }
            let after = self.decoded_chart_cache.lock().unwrap().statistics();
            if label == "cold" {
                anyhow::ensure!(
                    after.misses == before.misses + 1 && after.hits == before.hits,
                    "Cold did not parse"
                );
            } else {
                anyhow::ensure!(
                    after.hits == before.hits + 1 && after.misses == before.misses,
                    "Warm did not hit cache"
                );
            }
            let binding = self.capture_catalogue_audit(&output.join(label))?;
            let model = self.publication_model()?;
            let stable = serde_json::json!({"cells":model["cells"], "chains":model["chain_paths"], "instructions":model["instructions_sha256"], "owners":binding["dataset_catalogue_owners"], "zoom_bits":self.zoom_level.to_bits(), "pan_bits":[self.pan_offset.0.to_bits(),self.pan_offset.1.to_bits()]});
            if let Some(ref previous) = reference {
                anyhow::ensure!(
                    previous == &stable,
                    "Warm changed decoded cell/owner/IR/camera"
                );
            } else {
                reference = Some(stable);
            }
            phases.push(serde_json::json!({"phase":label,"reopen_authorization_through_publication_seconds":elapsed,"cache_before":before,"cache_after":after}));
        }
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"passed":true,"phases":phases,"scope":"Actual same-App unload/reopen; total wall includes authentication, catalogue validation and publication. Retained payload budget excludes process RSS. GPU/RGBA comparison independent."}),
            )?,
        )?;
        Ok(())
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
        self.ensure_navigation_scene()?;
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
            .save_screenshot(output.join("before.png"))?;
        // Exercise the ordinary interactive branch, not the auto-screenshot
        // ordinary interactive publication branch.
        self.publication_test_fail_before_commit = true;
        self.load_charts(std::slice::from_ref(&selected))?;
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
            .save_screenshot(output.join("after-late-failure.png"))?;
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
            .save_screenshot(output.join("after-success.png"))?;
        let mut direct_pick_probes = Vec::new();
        for y in 1..=4 {
            for x in 1..=6 {
                let point = ferrite_render::ScreenPoint::new(
                    viewport.0 + viewport.2 * x as f32 / 7.,
                    viewport.1 + viewport.3 * y as f32 / 5.,
                );
                let renderer = self.renderer.as_mut().unwrap();

                let candidates = renderer
                    .selection_candidates_in_context(&self.render_context, point, 4.)
                    .len();
                anyhow::ensure!(
                    candidates == 0,
                    "Cancelled object remained in renderer picking"
                );
                direct_pick_probes.push(serde_json::json!({"point":[point.x,point.y],"backend":"flat selection index","candidates":candidates}));
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

    fn capture_coverage_lifecycle(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden lifecycle audit required"
        );
        fs::create_dir_all(output)?;
        self.select_feature(None);
        self.renderer
            .as_mut()
            .context("Renderer missing")?
            .ui_state
            .selection_candidates
            .clear();
        std::env::set_var("FERRITE_ROOT_KEY_PROOF_COVERAGE", output.join("coverage"));
        self.update_view();
        std::env::remove_var("FERRITE_ROOT_KEY_PROOF_COVERAGE");
        anyhow::ensure!(
            self.startup_error.is_none(),
            "Lifecycle update failed {:?}",
            self.startup_error
        );
        let r = self.renderer.as_mut().unwrap();
        r.render()?;
        r.wait_hidden_key_frame()?;
        anyhow::ensure!(
            r.window().is_visible() == Some(false) && !r.window().has_focus(),
            "Visible/focused lifecycle forbidden"
        );
        let viewport = r.chart_viewport_pixels();
        r.save_screenshot(output.join("chart.png"))?;
        self.audit_portrayal(&output.join("audit"))?;
        self.renderer
            .as_ref()
            .unwrap()
            .export_hidden_key_flat_coverage(&output.join("coverage"), &self.render_context)?;
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
        fs::write(
            output.join("semantic.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"picks":picks,"profile":self.current_profile_name,"source_cells":self.cells.iter().map(|c|&c.file_path).collect::<Vec<_>>(),"window_visible":false,"window_focus":false}),
            )?,
        )?;
        fs::write(output.join("coverage-scope.json"), b"{\"mode\":\"2D\",\"scope\":\"actual Flat coverage and picking; no removed 3D geometry cache claim\"}")?;
        Ok(())
    }
    fn regenerate_coverage_lifecycle(&mut self) -> Result<()> {
        let prepared = self.prepare_instructions(true)?;
        self.publish_instructions(prepared);
        Ok(())
    }

    fn resume_coverage_lifecycle_resize(&mut self) -> Result<bool> {
        let mut pending = self
            .coverage_lifecycle_resize
            .take()
            .context("Resize state missing")?;
        let window = self.window.as_ref().context("Window missing")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Resize exposed/focused window"
        );
        let actual = window.inner_size();
        let ready = pending.ready(actual);
        if !ready {
            anyhow::ensure!(
                pending.started.elapsed() < std::time::Duration::from_secs(10),
                "Native Resized→Redraw timeout: requested {:?}, actual {:?}, observed {:?}",
                pending.requested,
                actual,
                pending.observed
            );
            self.coverage_lifecycle_resize = Some(pending);
            return Ok(false);
        }
        // The normal Resized handler has resized GPU/context and rebuilt coverage.
        let i = usize::from(pending.restore);
        self.capture_coverage_lifecycle(&pending.output.join(format!("10-viewport-{i}")))?;
        fs::write(
            pending.output.join(format!("10-viewport-{i}-event.json")),
            serde_json::to_vec_pretty(
                &serde_json::json!({"requested":[pending.requested.width,pending.requested.height],"actual_window":[actual.width,actual.height],"observed_resized":[actual.width,actual.height],"after_redraw":true,"window_visible":false,"window_focus":false}),
            )?,
        )?;
        if !pending.restore {
            pending.restore = true;
            pending.requested = pending.original;
            pending.observed = None;
            pending.started = std::time::Instant::now();
            pending.next_poll = pending.started;
            let original = pending.original;
            self.coverage_lifecycle_resize = Some(pending);
            let _ = self.window.as_ref().unwrap().request_inner_size(original);
            self.window.as_ref().unwrap().request_redraw();
            return Ok(false);
        }
        self.complete_coverage_cache_lifecycle(&pending.output)?;
        Ok(true)
    }
    fn complete_coverage_cache_lifecycle(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        let old_danger = self.renderer.as_ref().unwrap().settings().isolated_dangers;
        for (i, enabled) in [true, false, true].into_iter().enumerate() {
            self.renderer
                .as_mut()
                .unwrap()
                .ui_state
                .settings
                .isolated_dangers = enabled;
            self.regenerate_coverage_lifecycle()?;
            self.capture_coverage_lifecycle(&output.join(format!("09-shallow-water-dangers-{i}")))?;
        }
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .settings
            .isolated_dangers = old_danger;
        self.regenerate_coverage_lifecycle()?;
        fs::write(
            output.join("scope.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"source_reorder":true,"source_remove_restore":true,"fcpc_actual_reload":true,"profile_actual_change":true,"publication_rejection_cache_identity":true,"shallow_water_dangers_regenerated":[true,false,true],"hardware_dpi_changed":false,"viewport_actual_mutated":true,"camera_zoom_path":[1,2,20,200,20,2,1],"official_update_source_replacement":"separate Root publication audit required","window_visible":false,"window_focus":false}),
            )?,
        )?;
        Ok(())
    }

    fn bathymetry_publication_model(&self) -> Result<serde_json::Value> {
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let mut policies = self
            .depth_policies
            .iter()
            .map(|(path, policy)| (path.clone(), policy.target))
            .collect::<Vec<_>>();
        policies.sort();
        let mut inputs = self.depth_inputs.keys().cloned().collect::<Vec<_>>();
        inputs.sort();
        let mut bounds = self
            .bathymetry_bounds
            .iter()
            .map(|(path, b)| {
                (
                    path.clone(),
                    [
                        b.min_x.to_bits(),
                        b.min_y.to_bits(),
                        b.max_x.to_bits(),
                        b.max_y.to_bits(),
                    ],
                )
            })
            .collect::<Vec<_>>();
        bounds.sort();
        let sources=self.bathymetry.iter().map(|(path,c,snapshot)|serde_json::json!({"path":path,"instance":c.instance_name,"geometry":format!("{:?}",c.numeric_geometry()),"datum":c.vertical_datum,"snapshot":snapshot.as_ref().map(|s|s.path())})).collect::<Vec<_>>();
        Ok(
            serde_json::json!({"vectors":self.publication_model()?,"sources":sources,"policies":policies,"private_inputs":inputs,"bounds":bounds,"pan_bits":[self.pan_offset.0.to_bits(),self.pan_offset.1.to_bits()],"zoom_bits":self.zoom_level.to_bits(),"zoom_target_bits":self.zoom_target.to_bits(),"raster_composition":renderer.raster_composition_metadata().map(|(p,o,g,v)|(format!("{p:?}"),o,g.to_vec(),v)).collect::<Vec<_>>(),"ui_bathymetry_count":renderer.ui_state.bathymetry_count,"ui_chart_count":renderer.ui_state.chart_count}),
        )
    }
    /// A genuine application transaction regression using private immutable
    /// copies of public HDF5 files, never source dataset modifications.
    fn audit_gis_program_reuse(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled()
                && window.is_visible() == Some(false)
                && !window.has_focus(),
            "Hidden audit required"
        );
        fs::create_dir_all(output)?;
        let settings = self.applied_settings.clone();
        let mut timings = Vec::new();
        for profile in ["Day", "Dusk", "Night"] {
            let prepared = self.prepare_portrayal_change(PortrayalChangeRequest {
                profile: profile.into(),
                settings: settings.clone(),
            })?;
            self.commit_portrayal_change(prepared)?;
            for index in 0..24 {
                self.zoom_level = 200. + f64::from(index % 3);
                self.zoom_target = self.zoom_level;
                let start = std::time::Instant::now();
                self.update_view();
                self.ensure_navigation_scene()?;
                timings.push(serde_json::json!({"profile":profile,"index":index,
                    "prepare_ns":start.elapsed().as_nanos()}));
            }
            self.renderer.as_mut().unwrap().render()?;
            let dir = output.join(profile);
            self.audit_portrayal(&dir)?;
            self.renderer
                .as_mut()
                .unwrap()
                .save_screenshot(dir.join("frame.png"))?;
        }
        fs::write(
            output.join("receipt.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
            "timings":timings,"program":self.renderer.as_ref().unwrap().overscale_program_statistics(),
            "scope":"Hidden actual view preparation HOST time; excludes render/readback/UI and does not establish FPS"}))?,
        )?;
        Ok(())
    }

    fn audit_raster_viewing_groups(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled()
                && window.is_visible() == Some(false)
                && !window.has_focus(),
            "Hidden audit required"
        );
        anyhow::ensure!(!self.bathymetry.is_empty(), "S102 raster required");
        fs::create_dir_all(output)?;
        let pc = PortrayalCatalogue::load(&self.s102_pc_path)?;
        let groups = Self::bathymetry_viewing_groups(&pc, self.applied_settings.display_mode)?;
        let composition = |r: &WgpuRenderer| {
            r.raster_composition_metadata()
                .map(|(plane, priority, g, v)| (format!("{plane:?}"), priority, g.to_vec(), v))
                .collect::<Vec<_>>()
        };
        let baseline = composition(self.renderer.as_ref().unwrap());
        anyhow::ensure!(
            !baseline.is_empty() && baseline.iter().all(|row| row.3),
            "Own PC enabled rasters are hidden"
        );
        for zoom in [1., 2., 1.] {
            self.zoom_level = zoom;
            self.zoom_target = zoom;
            self.update_view();
            anyhow::ensure!(
                composition(self.renderer.as_ref().unwrap()) == baseline,
                "Vector camera rebuild changed raster visibility"
            );
        }
        self.renderer
            .as_mut()
            .unwrap()
            .set_raster_enabled_groups(Some(&std::collections::HashSet::new()));
        anyhow::ensure!(
            composition(self.renderer.as_ref().unwrap())
                .iter()
                .all(|row| !row.3),
            "Empty selection failed to hide raster"
        );
        self.update_view();
        anyhow::ensure!(
            composition(self.renderer.as_ref().unwrap())
                .iter()
                .all(|row| !row.3),
            "Vector rebuild overwrote raster selection"
        );
        self.renderer
            .as_mut()
            .unwrap()
            .set_raster_enabled_groups(Some(&groups));
        let mut modes = Vec::new();
        for mode in [DisplayMode::Base, DisplayMode::Standard, DisplayMode::All] {
            let request = PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: SettingsState {
                    display_mode: mode,
                    ..self.applied_settings.clone()
                },
            };
            let old = composition(self.renderer.as_ref().unwrap());
            self.publication_test_fail_before_commit = true;
            let failed = self.prepare_portrayal_change(request.clone());
            self.publication_test_fail_before_commit = false;
            anyhow::ensure!(failed.is_err(), "Failure injection not reached");
            anyhow::ensure!(
                composition(self.renderer.as_ref().unwrap()) == old,
                "Failed staging changed raster visibility"
            );
            let prepared = self.prepare_portrayal_change(request)?;
            anyhow::ensure!(
                composition(self.renderer.as_ref().unwrap()) == old,
                "Private preparation changed raster visibility"
            );
            self.commit_portrayal_change(prepared)?;
            let now = composition(self.renderer.as_ref().unwrap());
            anyhow::ensure!(now == baseline, "Display preset uses a foreign PC group");
            self.renderer.as_mut().unwrap().render()?;
            self.audit_portrayal(&output.join(mode.as_str()))?;
            modes.push(serde_json::json!({"mode":mode.as_str(),"raster_composition":now}));
        }
        fs::write(
            output.join("receipt.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"hidden":true,"pc_path":self.s102_pc_path,"own_groups":groups,"vector_camera_preserves_groups":true,"empty_groups_hide_and_stay_hidden_on_vector_rebuild":true,"failed_staging_retains_groups":true,"modes":modes}),
            )?,
        )?;
        Ok(())
    }

    /// Hidden lifecycle qualification with public signed originals; no removal authority.
    fn audit_s102_original_retention(&mut self, output: &Path) -> Result<()> {
        use sha2::Digest;
        self.ensure_navigation_scene()?;
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled()
                && window.is_visible() == Some(false)
                && !window.has_focus(),
            "Hidden audit required"
        );
        anyhow::ensure!(
            self.bathymetry.is_empty() && self.s102_original_inputs.is_empty(),
            "Original retention audit needs no initial rasters"
        );
        let paths: Vec<PathBuf> =
            serde_json::from_str(&std::env::var("FERRITE_S102_ORIGINAL_RETENTION_INPUTS")?)?;
        let paths = paths
            .iter()
            .map(|p| p.canonicalize())
            .collect::<std::io::Result<Vec<_>>>()?;
        anyhow::ensure!(
            paths.len() >= 3,
            "Original retention needs three distinct inputs"
        );
        fs::create_dir_all(output)?;
        if std::env::var("FERRITE_S102_POPULATED_HISTORY_AUDIT").as_deref() == Ok("1") {
            anyhow::ensure!(
                self.s102_journal_gate.is_some(),
                "Populated history audit requires explicit history"
            );
            let before = self.bathymetry_publication_model()?;
            self.renderer
                .as_mut()
                .unwrap()
                .ui_state
                .verify_dataset_signatures = false;
            let unchecked = self
                .load_bathymetry(&paths[..1])
                .expect_err("OFF must not bypass history");
            anyhow::ensure!(
                unchecked.to_string().contains("authenticated original"),
                "Unexpected OFF refusal: {unchecked:#}"
            );
            self.renderer
                .as_mut()
                .unwrap()
                .ui_state
                .verify_dataset_signatures = true;
            let cancelled = self
                .load_bathymetry(&paths[..1])
                .expect_err("Cancelled original must not load");
            anyhow::ensure!(
                cancelled
                    .to_string()
                    .contains("Cancelled S102 original cannot be displayed or reused"),
                "Tombstone rejection occurred at wrong boundary: {cancelled:#}"
            );
            anyhow::ensure!(
                self.bathymetry_publication_model()? == before
                    && self.bathymetry.is_empty()
                    && self.s102_original_inputs.is_empty(),
                "Rejected original changed existing publication"
            );
            self.ensure_navigation_scene()?;
            self.load_bathymetry(&paths[1..])?;
            anyhow::ensure!(
                self.s102_original_inputs.len() == paths.len() - 1,
                "Tombstone refusal blocked unrelated authenticated originals"
            );
            let admitted = self.s102_journal_gate.as_ref().unwrap().successful_checks();
            self.clear_charts();
            self.ensure_s102_journal_ready()?;
            anyhow::ensure!(
                self.bathymetry.is_empty() && self.s102_original_inputs.is_empty(),
                "Clear retained unrelated original owners"
            );
            let again = self
                .load_bathymetry(&paths[..1])
                .expect_err("Clear must retain tombstone");
            anyhow::ensure!(
                again
                    .to_string()
                    .contains("Cancelled S102 original cannot be displayed or reused"),
                "Clear reset history: {again:#}"
            );
            self.ensure_s102_journal_ready()?;
            fs::write(
                output.join("receipt.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                "hidden":true,"focused":false,"cancelled_path":paths[0],
                "signature_off_rejected":true,"signed_cancelled_original_rejected":true,
                "rejected_import_preserves_model":true,"unrelated_originals_loaded":paths.len()-1,
                "successful_admission_checks":admitted,"clear_preserves_tombstone":true,
                "scope":"Pre-populated synthetic-authorized test tombstone with actual signed originals; no App cancellation receipt ingestion/removal or official producer authority"}))?,
            )?;
            return Ok(());
        }
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .verify_dataset_signatures = true;
        self.load_bathymetry(&paths[..1])?;
        let original = self.bathymetry_publication_model()?;
        let old = self.s102_original_inputs.clone();
        anyhow::ensure!(old.len() == 1, "Original proof not retained");
        std::env::set_var("FERRITE_ROOT_S102_FAIL_SCENE", "1");
        let failed = self.load_bathymetry(&paths[1..]);
        std::env::remove_var("FERRITE_ROOT_S102_FAIL_SCENE");
        let late_error = failed.expect_err("Injected staging must fail");
        anyhow::ensure!(
            late_error
                .to_string()
                .contains("after complete material staging"),
            "Failure occurred before desired publication boundary: {late_error:#}"
        );
        anyhow::ensure!(
            self.bathymetry_publication_model()? == original
                && self.s102_original_inputs.len() == old.len()
                && old.iter().all(|(p, v)| self
                    .s102_original_inputs
                    .get(p)
                    .is_some_and(|n| Arc::ptr_eq(n, v))),
            "Failed staging changed retained originals/model"
        );
        drop(old);
        self.load_bathymetry(&paths[1..])?;
        anyhow::ensure!(
            self.s102_original_inputs.len() == paths.len(),
            "Missing retained originals"
        );
        let mut evidence = Vec::new();
        for (path, record) in &self.s102_original_inputs {
            let bytes = fs::read(record.snapshot.path())?;
            anyhow::ensure!(
                record.snapshot.source == *path
                    && record.original.resource_path() == path
                    && record.original.resource_size() == bytes.len() as u64
                    && record.original.resource_sha384()
                        == format!("{:x}", sha2::Sha384::digest(&bytes))
                    && record.original.catalogue_sha384()
                        == format!(
                            "{:x}",
                            sha2::Sha384::digest(record.original.catalogue_bytes())
                        )
                    && !record.original.signatures().is_empty()
                    && !record.original.catalogue_signatures().is_empty(),
                "Original proof content mismatch"
            );
            anyhow::ensure!(
                self.bathymetry
                    .iter()
                    .filter(|(p, _, _)| p == path)
                    .all(|(_, _, s)| s.as_ref().is_some_and(|v| Arc::ptr_eq(v, &record.snapshot))),
                "Reader uses foreign original snapshot"
            );
            evidence.push(serde_json::json!({"path":path,"resource_sha384":record.original.resource_sha384(),
                "catalogue_sha384":record.original.catalogue_sha384(),"size":record.original.resource_size(),
                "resource_signatures":record.original.signatures().len(),"reader_arc_exact":true}));
        }
        let fresh = self.authenticate_paths(&paths[..1])?;
        let old_snapshot = &self.s102_original_inputs[&paths[0]].snapshot;
        anyhow::ensure!(
            s102_original_inputs::capture(&fresh, &paths[0], Some(old_snapshot)).is_err(),
            "Foreign parser snapshot accepted"
        );
        let foreign_path = paths[1].as_path();
        anyhow::ensure!(
            s102_original_inputs::stage(
                &Default::default(),
                [(foreign_path, Some(&self.s102_original_inputs[&paths[0]]))],
                &fresh
            )
            .is_err(),
            "Foreign original path accepted"
        );
        anyhow::ensure!(
            s102_original_inputs::stage(
                &self.s102_original_inputs,
                [(
                    paths[0].as_path(),
                    Some(&self.s102_original_inputs[&paths[0]])
                )],
                &fresh
            )
            .is_err(),
            "Duplicate original publication accepted"
        );
        drop(fresh);
        if let Some(gate) = &mut self.s102_journal_gate {
            let record = self.s102_original_inputs[&paths[0]].clone();
            let foreign = Arc::new(record.original.capture_resource()?);
            anyhow::ensure!(
                gate.check_input(Some(&record), Some(&foreign), &paths[0])
                    .is_err(),
                "S102 replay gate accepted independent same-byte parser owner"
            );
            anyhow::ensure!(
                gate.successful_checks() >= paths.len() as u64 * 2,
                "S102 replay gate did not check both capture and publication"
            );
        }
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("loaded"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("loaded.png"))?;
        let middle = paths[paths.len() / 2].clone();
        let private = self.s102_original_inputs[&middle]
            .snapshot
            .path()
            .to_path_buf();
        let weak = Arc::downgrade(&self.s102_original_inputs[&middle].snapshot);
        self.unload_dataset(ferrite_wgpu::DatasetLayerId::S102(middle.clone()))?;
        anyhow::ensure!(
            !self.s102_original_inputs.contains_key(&middle)
                && weak.upgrade().is_none()
                && !private.exists(),
            "Manual unload retained original reader/file"
        );
        let remaining = self
            .s102_original_inputs
            .values()
            .map(|v| (Arc::downgrade(&v.snapshot), v.snapshot.path().to_path_buf()))
            .collect::<Vec<_>>();
        self.clear_charts();
        anyhow::ensure!(
            self.s102_original_inputs.is_empty()
                && self.bathymetry.is_empty()
                && remaining
                    .iter()
                    .all(|(w, p)| w.upgrade().is_none() && !p.exists()),
            "Clear retained original reader/file"
        );
        fs::write(
            output.join("receipt.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
            "hidden":true,"focused":false,"signed_originals":evidence,
            "late_failure_preserves_registry_and_model":true,"foreign_snapshot_rejected":true,
            "foreign_path_rejected":true,"duplicate_publication_rejected":true,
            "middle_unload_releases_reader_and_private_file":true,"clear_releases_remaining":true,
            "journal_admission_checks":self.s102_journal_gate.as_ref().map(|g| g.successful_checks()),
            "journal_preserved_after_unload_clear":self.s102_journal_gate.is_some(),
            "scope":"Current signed original loading and manual lifecycle only; no cancellation permission/replay or cross-platform proof"}))?,
        )?;
        Ok(())
    }

    /// Synthetic standard commands through the actual S101 adapter and shared GPU.
    /// No claim that a delivered PC rule naturally emits these fixtures.
    fn audit_geographic_annulus(&self, output: &Path) -> Result<()> {
        use ferrite_kernel::geodesy::{direct, inverse, GeographicPosition};
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Hidden/unfocused audit required"
        );
        anyhow::ensure!(
            !output.exists()
                && self.loading_state.is_none()
                && !self.cells.is_empty()
                && self.bathymetry.is_empty(),
            "Fresh output and settled vector-only scene required"
        );
        let renderer = self.renderer.as_ref().context("Renderer required")?;
        let old_gpu = renderer.audit_displayed_vector_frame()?;
        let old_model = self.publication_model()?;
        let camera = self.navigation_camera_bits();
        let (x, y, w, h) = renderer.chart_viewport_pixels();
        let origin = self
            .render_context
            .scaler
            .screen_to_world(ferrite_render::ScreenPoint::new(x + w * 0.5, y + h * 0.5));
        let east = self
            .render_context
            .scaler
            .screen_to_world(ferrite_render::ScreenPoint::new(
                x + w * 0.5 + 100.,
                y + h * 0.5,
            ));
        let center = GeographicPosition::new(origin.y, (origin.x + 180.).rem_euclid(360.) - 180.)?;
        let east = GeographicPosition::new(east.y, (east.x + 180.).rem_euclid(360.) - 180.)?;
        let radius = inverse(center, east)?.distance_m;
        let cell = &self.cells[0];
        let key = Self::catalogue_dataset_key(&cell.dsid)?;
        let pc = self
            .catalogue_registry
            .owner_for(&key)
            .context("Source PC required")?
            .pc();
        let source_feature_id = cell
            .features
            .iter()
            .filter(|(_, feature)| {
                feature.primitive_type != ferrite_s100_core::SpatialPrimitiveType::Point
            })
            .map(|(id, _)| *id)
            .min()
            .context("Real nonpoint feature required")?;
        let unknown_feature_id = cell
            .features
            .keys()
            .copied()
            .max()
            .context("Source features required")?
            .checked_add(1)
            .context("Missing-feature fixture ID overflow")?;
        anyhow::ensure!(
            !cell.features.contains_key(&unknown_feature_id),
            "Unknown-feature negative must be absent"
        );
        let prepare = |sweep: Option<f64>,
                       feature_id: i64|
         -> Result<ferrite_wgpu::ReadyVectorGpuFrame> {
            let mut next = self.render_context.empty_for_rebuild();
            if let Some(sweep) = sweep {
                let script=format!("DrawingPriority:7;Annulus:{},{},{},{},35,{sweep};AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;LineStyle:L,,0.64,CHBLK;LineInstructionUnsuppressed:L",origin.x,origin.y,radius,radius*0.5);
                let command =
                    ferrite_lua::PortrayalResult::parse(&feature_id.to_string(), &script, "")?;
                convert_lua_results_for_cell(
                    &[command],
                    cell,
                    pc,
                    &mut next,
                    0,
                    &self.current_profile_name,
                )?;
                anyhow::ensure!(
                    next.raw_instructions().len() == 1,
                    "Expected one retained standard-command fixture"
                );
            }
            prepare_flat_coverage(
                self.coverage_inventory.as_deref(),
                &mut next,
                window.inner_size(),
                window.scale_factor(),
            )?;
            let request = PortrayalChangeRequest {
                profile: self.current_profile_name.clone(),
                settings: self.applied_settings.clone(),
            };
            let resources = self.prepare_cell_resources(&self.catalogue_registry, &request)?;
            let groups = self.get_visible_viewing_groups();
            let wrap = 360. * next.scaler.scale_x() as f32;
            let emission = renderer.prepare_private_vector_emission(
                next,
                resources,
                groups.as_ref(),
                renderer.vector_emission_settings(),
                wrap,
                Vec::new(),
            )?;
            Ok(renderer.prepare_private_vector_gpu(emission)?)
        };
        let missing_feature_error = match prepare(Some(360.), unknown_feature_id) {
            Ok(_) => anyhow::bail!("Missing feature unexpectedly accepted for coverage"),
            Err(error) => format!("{error:#}"),
        };
        anyhow::ensure!(
            missing_feature_error.contains("Missing portrayal origin"),
            "Missing feature must fail source binding: {missing_feature_error}"
        );
        let baseline = renderer.audit_private_vector_frame(&prepare(None, source_feature_id)?)?;
        let mid = direct(center, 35., radius * 0.75)?;
        let query = self.render_context.scaler.world_to_screen(WorldPoint::new(
            mid.longitude_near(origin.x)?,
            mid.latitude(),
        ));
        fs::create_dir(output)?;
        let mut rows = Vec::new();
        for sweep in [360., -360., 270., -270.] {
            let candidate = prepare(Some(sweep), source_feature_id)?;
            let context = candidate.context();
            let cache_before = context.retained_path_cache_stats();
            let hit_bits = |hit: Option<ferrite_render::WrappedGeometryHit>| {
                hit.map(|hit| {
                    [
                        hit.hit.distance.to_bits(),
                        u64::from(hit.hit.nearest.x.to_bits()),
                        u64::from(hit.hit.nearest.y.to_bits()),
                        hit.longitude_shift.to_bits(),
                    ]
                })
            };
            let original_hit = hit_bits(ferrite_render::hit_geometry_wrapped_visible(
                &context.raw_instructions()[0],
                &context.scaler,
                query,
                3.,
                None,
                true,
            ));
            let shared_hit = hit_bits(ferrite_render::hit_geometry_wrapped_visible_in_context(
                context, 0, query, 3., None, true,
            ));
            anyhow::ensure!(
                original_hit == shared_hit,
                "Shared annulus pick differs from original"
            );
            let cache_after = context.retained_path_cache_stats();
            let cache_enabled = std::env::var("FERRITE_RETAINED_PATH_CACHE").as_deref() != Ok("0");
            if cache_enabled {
                anyhow::ensure!(
                    cache_before.entries == 1
                        && cache_after.hits > cache_before.hits
                        && cache_after.retained_payload_bytes <= 8 * 1024 * 1024,
                    "Expected bounded actual render/pick geographic cache reuse"
                );
            }
            let actual = renderer.audit_private_vector_frame(&candidate)?;
            anyhow::ensure!(
                actual.extent == baseline.extent,
                "Actual GPU extent differs"
            );
            let changed = actual
                .rgba8
                .as_chunks::<4>()
                .0
                .iter()
                .zip(baseline.rgba8.as_chunks::<4>().0.iter())
                .filter(|(a, b)| a != b)
                .count();
            anyhow::ensure!(
                changed > 100,
                "Annulus fixture has no meaningful GPU pixel contribution"
            );
            let mut gap_changed = 0;
            for dy in -2..=2 {
                for dx in -2..=2 {
                    let px = query.x.round() as i64 + i64::from(dx);
                    let py = query.y.round() as i64 + i64::from(dy);
                    anyhow::ensure!(
                        px >= 0
                            && py >= 0
                            && px < i64::from(actual.extent[0])
                            && py < i64::from(actual.extent[1]),
                        "Radial GPU query outside extent"
                    );
                    let i = ((py as usize) * actual.extent[0] as usize + px as usize) * 4;
                    gap_changed += usize::from(actual.rgba8[i..i + 4] != baseline.rgba8[i..i + 4]);
                }
            }
            anyhow::ensure!(
                (sweep.abs() == 360. && gap_changed == 0)
                    || (sweep.abs() != 360. && gap_changed > 0),
                "Actual GPU invented/omitted radial annulus boundary"
            );
            rows.push(serde_json::json!({"sweep":sweep,"cache_enabled":cache_enabled,"cache_hits":cache_after.hits,"cache_misses":cache_after.misses,"cache_entries":cache_after.entries,"cache_retained_bytes":cache_after.retained_payload_bytes,"wrapped_pick_exact":true,"wrapped_pick_bits":shared_hit,"changed_pixels":changed,"radial_query_changed_pixels":gap_changed,"rgba_sha256":format!("{:x}",sha2::Sha256::digest(&actual.rgba8))}));
        }
        let after = renderer.audit_displayed_vector_frame()?;
        anyhow::ensure!(
            after.rgba8 == old_gpu.rgba8
                && self.publication_model()? == old_model
                && self.navigation_camera_bits() == camera,
            "Private fixture changed displayed App/GPU/model/camera"
        );
        fs::write(
            output.join("report.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"passed":true,"rows":rows,"actual_source_feature_id":source_feature_id,"missing_feature_rejected":missing_feature_error,"actual_adapter_gpu":true,"displayed_gpu_model_camera_unchanged":true,"hidden":true,"focused":false,"scope":"synthetic standard Annulus commands through actual S101 adapter/private shared GPU at current camera; not naturally emitted official rule/full standard/FPS proof"}),
            )?,
        )?;
        Ok(())
    }

    fn audit_s102_append_reuse(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden audit required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Hidden/unfocused audit required"
        );
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            self.loading_state.is_none() && self.bathymetry.is_empty(),
            "Completed vector-only load required"
        );
        anyhow::ensure!(
            self.requires_cell_resources(&self.catalogue_registry, &self.fc, &self.pc)?,
            "Actual mixed PC resources required"
        );
        anyhow::ensure!(
            self.active_ic().is_none(),
            "Regular portrayal requires no active interoperability catalogue"
        );
        let paths: Vec<PathBuf> =
            serde_json::from_str(&std::env::var("FERRITE_S102_APPEND_INPUTS")?)?;
        anyhow::ensure!(
            paths.len() == 2 && paths[0] != paths[1],
            "Two distinct private HDF inputs required"
        );
        let private = output
            .parent()
            .context("Output parent")?
            .join("input")
            .canonicalize()?;
        for p in &paths {
            anyhow::ensure!(
                p.canonicalize()?.parent() == Some(private.as_path()),
                "Private audit inputs required"
            );
        }
        fs::create_dir(output)?;
        let reference =
            std::env::var("FERRITE_S102_APPEND_REBUILD_REFERENCE").as_deref() == Ok("1");
        let mut rows = Vec::new();
        let count = self.s102_raster_rebuild_count;
        let start = std::time::Instant::now();
        self.load_bathymetry(&paths[..1])?;
        rows.push(serde_json::json!({"stage":"first","elapsed_ns":start.elapsed().as_nanos(),"discarded_rebuilds":self.s102_raster_rebuild_count-count}));
        anyhow::ensure!(
            self.s102_raster_rebuild_count == count,
            "First append rebuilt an empty old raster set"
        );
        // Successful ordinary App publication uploads vector buffers lazily on
        // its first normal frame. Failure controls below never call render.
        self.renderer.as_mut().unwrap().render()?;
        let old = self.bathymetry_publication_model()?;
        let camera = self.navigation_camera_bits();
        let old_gpu = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_regular_mixed_frame(true)?;
        let count = self.s102_raster_rebuild_count;
        std::env::set_var("FERRITE_ROOT_S102_FAIL_SCENE", "1");
        let failed = self.load_bathymetry(&paths[1..]);
        std::env::remove_var("FERRITE_ROOT_S102_FAIL_SCENE");
        let error = failed.unwrap_err();
        anyhow::ensure!(
            error
                .to_string()
                .contains("after complete material staging"),
            "Expected outer late failure: {error:#}"
        );
        let actual = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_regular_mixed_frame(true)?;
        anyhow::ensure!(
            actual.rgba8 == old_gpu.rgba8
                && actual.extent == old_gpu.extent
                && self.bathymetry_publication_model()? == old
                && self.navigation_camera_bits() == camera,
            "Failed append changed displayed GPU/model/camera"
        );
        anyhow::ensure!(
            self.s102_raster_rebuild_count - count == u64::from(reference),
            "Unexpected discarded rebuild count after late failure"
        );
        rows.push(serde_json::json!({"stage":"late-failure","discarded_rebuilds":self.s102_raster_rebuild_count-count}));
        let count = self.s102_raster_rebuild_count;
        let start = std::time::Instant::now();
        self.load_bathymetry(&paths[1..])?;
        rows.push(serde_json::json!({"stage":"second","elapsed_ns":start.elapsed().as_nanos(),"discarded_rebuilds":self.s102_raster_rebuild_count-count}));
        anyhow::ensure!(
            self.s102_raster_rebuild_count - count == u64::from(reference),
            "Unexpected discarded rebuild count on actual append"
        );
        anyhow::ensure!(
            self.bathymetry.iter().any(|(p, _, _)| p == &paths[0])
                && self.bathymetry.iter().any(|(p, _, _)| p == &paths[1]),
            "Both original parsed sources required"
        );
        self.renderer.as_mut().unwrap().render()?;
        let mixed = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_regular_mixed_frame(true)?;
        let vector = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_regular_mixed_frame(false)?;
        anyhow::ensure!(
            mixed.rgba8 != vector.rgba8,
            "Actual HDF pixel contribution required"
        );
        fs::write(output.join("mixed.rgba"), &mixed.rgba8)?;
        fs::write(
            output.join("report.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"passed":true,"reference_rebuild":reference,"rows":rows,"model":self.bathymetry_publication_model()?,"camera":self.navigation_camera_bits(),"extent":mixed.extent,"late_failure_gpu_model_camera_exact":true,"raster_contribution":true,"scope":"actual mixed-PC regular S102 append; no full App atomicity/FPS claim"}),
            )?,
        )?;
        Ok(())
    }

    fn audit_root_bathymetry_publication(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden background test required"
        );
        let window = self.window.as_ref().context("Window required")?;
        anyhow::ensure!(
            window.is_visible() == Some(false) && !window.has_focus(),
            "Visible/focused audit forbidden"
        );
        anyhow::ensure!(
            self.loading_state.is_none() && self.bathymetry.is_empty(),
            "Audit requires completed original vector load and no existing bathymetry"
        );
        let paths: Vec<PathBuf> =
            serde_json::from_str(&std::env::var("FERRITE_ROOT_S102_PUBLICATION_INPUTS")?)?;
        anyhow::ensure!(
            paths.len() >= 2,
            "Audit needs at least two public HDF5 copies"
        );
        let private = output
            .parent()
            .context("Output parent")?
            .join("input")
            .canonicalize()?;
        for p in &paths {
            anyhow::ensure!(
                p.canonicalize()?.parent() == Some(private.as_path()),
                "Audit requires private input copies"
            );
        }
        fs::create_dir_all(output)?;
        let original = self.bathymetry_publication_model()?;
        let original_inventory = self.coverage_inventory.clone();
        self.audit_portrayal(&output.join("before"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("before.png"))?;
        let bad = private.join("invalid-last.H5");
        anyhow::ensure!(!bad.exists(), "New invalid fixture required");
        fs::write(&bad, b"explicitly invalid HDF5 transaction fixture")?;
        let partial_error = self.load_bathymetry(&[paths[0].clone(), bad]).unwrap_err();
        anyhow::ensure!(
            self.bathymetry_publication_model()? == original,
            "Partial file set altered model"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-partial"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("after-partial.png"))?;

        std::env::set_var("FERRITE_ROOT_S102_FAIL_SCENE", "1");
        let failed = self.load_bathymetry(&paths);
        std::env::remove_var("FERRITE_ROOT_S102_FAIL_SCENE");
        let late_error = failed.unwrap_err();
        {
            anyhow::ensure!(
                late_error
                    .to_string()
                    .contains("after complete material staging"),
                "Wrong late-stage diagnostic: {late_error:#}"
            );
        }
        anyhow::ensure!(
            self.bathymetry_publication_model()? == original,
            "Late staging failure altered model"
        );
        anyhow::ensure!(
            match (&original_inventory, &self.coverage_inventory) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            },
            "Failure replaced coverage inventory"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-late"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("after-late.png"))?;
        self.load_bathymetry(&paths)?;
        anyhow::ensure!(
            self.bathymetry_bounds.len() == paths.len() && self.depth_policies.len() == paths.len(),
            "Successful publication did not publish all file metadata"
        );
        anyhow::ensure!(
            self.renderer.as_ref().unwrap().ui_state.bathymetry_count == self.bathymetry.len(),
            "UI count out of sync"
        );

        // Immediate direct frame, never update_view repair after commit.
        self.renderer.as_mut().unwrap().render()?;
        self.audit_portrayal(&output.join("after-success"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("after-success.png"))?;
        let successful = true;
        fs::write(
            output.join("publication.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"original":original,"after":self.bathymetry_publication_model()?,"partial_error":format!("{partial_error:#}"),"late_error":format!("{late_error:#}"),"successful_retry":successful,"all_files":paths.len(),"hidden":true,"focused":false,"no_update_view_repair":true,"signature_policy":"default OFF; no signed producer-cancellation or general continuous mapping claim"}),
            )?,
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
        self.ensure_navigation_scene()?;
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

        self.audit_portrayal(&output.join("before"))?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("before.png"))?;
        // Force the ordinary interactive finalize branch for both receipts.
        let saved_screenshot = self.auto_screenshot.take();
        let mut phases = Vec::new();
        for phase in ["first", "replay"] {
            self.load_charts(std::slice::from_ref(&selected))?;
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
                .save_screenshot(output.join(format!("{phase}.png")))?;
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
    fn audit_root_publication(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
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
            .save_screenshot(output.join("before.png"))?;
        self.publication_test_fail_before_commit = true;
        self.load_charts(std::slice::from_ref(&selected))?;
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
            .save_screenshot(output.join("after-late-failure.png"))?;
        anyhow::ensure!(
            self.publication_model()? == original,
            "Rendering after rejection changed model"
        );

        self.load_charts(std::slice::from_ref(&selected))?;
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
            .save_screenshot(output.join("after-batch-failure.png"))?;
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
            .save_screenshot(output.join("after-success.png"))?;
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

    fn audit_animation(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;
        // Reconcile the settled UI viewport before comparing the same camera.
        // Object selection can resize a panel during an explicit UI capture.
        for _ in 0..4 {
            self.renderer.as_mut().context("No renderer")?.render()?;
            if self.sync_chart_layout() {
                self.update_view();
            }
        }
        let mut rows = Vec::new();
        let mut expected = None;
        for (index, animation) in [false, true, false].into_iter().enumerate() {
            self.renderer
                .as_mut()
                .context("No renderer")?
                .set_animation_mode(animation);
            let start = std::time::Instant::now();
            // Exercise the production preserved-declutter animation path,
            // with the exact same camera as stationary/final frames.
            let before_camera = self.render_context.scaler.flat_encoded_identity();
            self.update_view_ex(false, animation);
            anyhow::ensure!(
                before_camera == self.render_context.scaler.flat_encoded_identity(),
                "Animation audit changed camera"
            );
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
            rows.push(serde_json::json!({"animation":animation,"preserve_declutter":animation,"camera_identity":before_camera,"update_seconds":start.elapsed().as_secs_f64(),"area_triangles":stats.area_triangles,
                "line_triangles":stats.line_triangles,"line_visibility_counts":renderer.line_visibility_counts(),"temporal_visibility_counts":renderer.temporal_visibility_counts(),"symbols":stats.symbol_instances,"text_labels":stats.text_labels,"displayed_geometries":visible.len()}));
            // Explicit audit captures actual GPU buffers and rendered frames, not only counts.
            let directory = output
                .with_extension("frames")
                .join(format!("{index}-{animation}"));
            self.renderer.as_mut().unwrap().render()?;
            self.audit_portrayal(&directory)?;
            self.renderer
                .as_mut()
                .unwrap()
                .save_screenshot(directory.join("frame.png"))?;
        }
        std::fs::write(output, serde_json::to_vec_pretty(&rows)?)?;
        info!(
            "Animation audit: stationary/animated/stationary geometry, symbols and text counts match"
        );
        Ok(())
    }

    fn audit_object_selection_mode(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Hidden test required"
        );
        self.ensure_navigation_scene()?;
        self.build_rendered_symbols();
        let renderer = self.renderer.as_ref().context("No renderer")?;
        anyhow::ensure!(
            !renderer.ui_state.object_selection_mode,
            "Selection must default OFF"
        );
        anyhow::ensure!(
            !self.plugin_system.has_loaded_plugins(),
            "Audit requires no plugins"
        );
        let (x, y, w, h) = renderer.chart_viewport_pixels();
        let point = self
            .rendered_symbols
            .iter()
            .find(|symbol| {
                symbol.kind == 0
                    && symbol.cell_index.is_some()
                    && symbol.screen_x > x + 20.
                    && symbol.screen_x < x + w - 20.
                    && symbol.screen_y > y + 20.
                    && symbol.screen_y < y + h - 20.
            })
            .map(|symbol| (f64::from(symbol.screen_x), f64::from(symbol.screen_y)))
            .context("No visible source object for actual click audit")?;
        let before = format!("{:?}", renderer.ui_state.selected_feature);
        let camera = (
            self.zoom_level.to_bits(),
            format!("{:?}", self.render_context.scaler),
        );
        self.chart_click(point);
        anyhow::ensure!(
            format!(
                "{:?}",
                self.renderer.as_ref().unwrap().ui_state.selected_feature
            ) == before,
            "OFF changed selection"
        );
        anyhow::ensure!(
            (
                self.zoom_level.to_bits(),
                format!("{:?}", self.render_context.scaler)
            ) == camera,
            "OFF changed camera"
        );
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .object_selection_mode = true;
        self.chart_click(point);
        let renderer = self.renderer.as_ref().unwrap();
        let feature = renderer
            .ui_state
            .selected_feature
            .as_ref()
            .context("ON failed to select visible object")?;
        anyhow::ensure!(
            !renderer.ui_state.object_details_hidden,
            "ON did not reveal detail panel"
        );
        let cell = feature
            .cell_index
            .context("Selected object missing cell owner")? as usize;
        anyhow::ensure!(
            feature.source.as_deref()
                == Some(self.cells[cell].file_path.to_string_lossy().as_ref()),
            "Wrong selected source"
        );
        let owner = self
            .catalogue_owner_for_cell(cell)
            .context("Selected catalogue owner missing")?;
        let preview = renderer
            .ui_state
            .selected_symbol_preview
            .as_ref()
            .context("Actual selected symbol preview missing")?;
        anyhow::ensure!(
            preview.matches_bound(
                feature,
                &self.current_profile_name,
                owner.pc().source_digest()
            ),
            "Preview uses wrong source/PC/palette"
        );

        let identity = serde_json::json!({"cell":cell,"feature_id":feature.feature_id,"source":feature.source,"symbol":feature.symbol_name,"preview_pc_digest":preview.pc_digest(),"preview_palette":self.current_profile_name});
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .set_object_selection_mode(false);
        self.clear_selection();
        self.chart_click((point.0 + 40., point.1 + 40.));
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .ui_state
                .selected_feature
                .is_none()
                && self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selected_symbol_preview
                    .is_none()
                && self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selection_candidates
                    .is_empty()
                && self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .coverage_info
                    .is_none(),
            "Mode OFF retained selection"
        );
        self.renderer
            .as_mut()
            .unwrap()
            .ui_state
            .set_object_selection_mode(true);
        self.chart_click(point);
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .ui_state
                .selected_feature
                .is_some(),
            "Cannot reselect after OFF"
        );
        self.clear_selection(); // Same path used by Escape.
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .ui_state
                .selected_feature
                .is_none()
                && self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .selection_candidates
                    .is_empty()
                && self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .ui_state
                    .object_selection_mode,
            "Escape failed or disabled selection mode"
        );
        self.chart_click(point);
        fs::create_dir_all(output)?;
        self.renderer.as_mut().unwrap().render()?;
        self.update_view(); // Reconcile the chart viewport after the detail panel opened.
        let renderer = self.renderer.as_mut().unwrap();
        for _ in 0..12 {
            renderer.render()?;
        }
        renderer.save_screenshot_with_ui(output.join("selected.png"))?;
        renderer.ui_state.show_logs = true;
        renderer.ui_state.show_load_status = true;
        for _ in 0..3 {
            renderer.render()?;
        }
        renderer.save_screenshot_with_ui(output.join("separate-windows.png"))?;
        renderer.ui_state.show_logs = false;
        anyhow::ensure!(
            renderer.ui_state.show_load_status,
            "Closing Logs closed Load status"
        );
        renderer.render()?;
        renderer.save_screenshot_with_ui(output.join("status-only.png"))?;
        renderer.ui_state.show_load_status = false;
        let day = self.current_profile_name.clone();
        for profile in ["Night", day.as_str()] {
            let prepared = self.prepare_portrayal_change(PortrayalChangeRequest {
                profile: profile.into(),
                settings: self.applied_settings.clone(),
            })?;
            self.commit_portrayal_change(prepared)?;
            let renderer = self.renderer.as_mut().unwrap();
            let feature = renderer
                .ui_state
                .selected_feature
                .as_ref()
                .context("Palette change lost selection")?;
            anyhow::ensure!(
                renderer
                    .ui_state
                    .selected_symbol_preview
                    .as_ref()
                    .is_some_and(|p| p.matches(feature, profile)),
                "Palette change retained stale preview"
            );
            renderer.render()?;
            renderer.save_screenshot_with_ui(output.join(format!("selected-{profile}.png")))?;
        }
        fs::write(
            output.join("receipt.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"default_off":true,"off_preserves_camera_selection":true,"on_actual_click_selects_source":identity,"mode_off_clears_selection":true,"escape_shared_path_clears_selection":true,"hidden":true,"point":point}),
            )?,
        )?;
        Ok(())
    }

    fn audit_selection(&mut self, output: &Path) -> Result<()> {
        self.ensure_navigation_scene()?;

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
                    let p = self
                        .render_context
                        .resolved_line_paths(index, scaler)
                        .context("Displayed line missing")?
                        .find_map(|points| {
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
                anyhow::ensure!(
                    self.find_features_at_impl(x, point.y as f64, radius, false)
                        == self.find_features_at_impl(x, point.y as f64, radius, true),
                    "Indexed selection changed candidates, order, nearest position or longitude copy"
                );
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
        self.navigation_scene_pending.explicit_retry();
        if !self.flush_navigation_scene() {
            return;
        }
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

    // Add inside App impl. No global PC fallback for a foreign/missing cell owner.
    fn prepare_selected_symbol_preview(
        &mut self,
        feature: &SelectedFeature,
    ) -> Result<Option<ferrite_wgpu::SelectedSymbolPreview>> {
        self.poll_scene_build(true);
        if feature.symbol_name.is_none() {
            return Ok(None);
        }
        let index = feature
            .cell_index
            .context("Selected symbol lacks source cell")? as usize;
        let cell = self
            .cells
            .get(index)
            .context("Selected symbol cell no longer loaded")?;
        anyhow::ensure!(
            feature.source.as_deref() == Some(cell.file_path.to_string_lossy().as_ref()),
            "Selected symbol source does not match retained cell"
        );
        let owner = Arc::clone(
            self.catalogue_owner_for_cell(index)
                .context("Selected symbol exact retained source/PC binding unavailable")?,
        );
        if let Some(resources) = &mut self.cell_resources {
            return ferrite_wgpu::prepare_selected_symbol_preview(feature, resources)
                .map_err(Into::into);
        }
        // Single-owner rendering may not own a per-cell registry. Prepare ONE exact
        // retained cell PC here, never the application's unverified global name cache.
        let binding = ferrite_wgpu::CellPortrayalResourceBinding {
            cell_index: index,
            catalogue: Arc::clone(owner.pc()),
            shallow_pattern: None,
        };
        let mut resources = ferrite_wgpu::CellPortrayalResources::prepare(
            &[binding],
            &self.current_profile_name,
            1,
        )?;
        ferrite_wgpu::prepare_selected_symbol_preview(feature, &mut resources).map_err(Into::into)
    }
    fn refresh_selected_symbol_preview(&mut self) {
        let selected = self
            .renderer
            .as_ref()
            .and_then(|r| r.ui_state.selected_feature.clone());
        if let Some(feature) = &selected {
            // Check actual retained source first; numeric cell ID is not identity.
            let cell = feature
                .cell_index
                .and_then(|index| self.cells.get(index as usize));
            let source_matches = cell.is_some_and(|cell| {
                feature.source.as_deref() == Some(cell.file_path.to_string_lossy().as_ref())
            });
            let owner = feature
                .cell_index
                .and_then(|index| self.catalogue_owner_for_cell(index as usize));
            if source_matches
                && owner.is_some_and(|owner| {
                    self.renderer
                        .as_ref()
                        .and_then(|r| r.ui_state.selected_symbol_preview.as_ref())
                        .is_some_and(|p| {
                            p.matches_bound(
                                feature,
                                &self.current_profile_name,
                                owner.pc().source_digest(),
                            )
                        })
                })
            {
                return; // no SVG parse/copy/texture upload on identical repeated selection
            }
        }
        let preview = selected.as_ref().and_then(|feature| {
            match self.prepare_selected_symbol_preview(feature) {
                Ok(preview) => preview,
                Err(error) => {
                    tracing::warn!("Selected symbol preview unavailable: {error:#}");
                    None
                }
            }
        });
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.selected_symbol_preview = preview;
        }
    }

    /// Deselection is infallible and never flushes/retries a portrayal rebuild.
    fn clear_selection(&mut self) {
        if let Some(renderer) = &mut self.renderer {
            renderer.ui_state.clear_selection();
            renderer.ui_state.clear_selection_requested = false;
            renderer.set_selection_geometry(Vec::new(), &self.render_context.scaler);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn select_feature(&mut self, selected: Option<SelectedFeature>) {
        self.navigation_scene_pending.explicit_retry();
        if !self.flush_navigation_scene() {
            return;
        }
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
                                self.render_context
                                    .resolved_line_paths(index, &self.render_context.scaler)
                                    .into_iter()
                                    .flatten()
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
            if selected.is_some() {
                renderer.ui_state.object_details_hidden = false;
            }
            renderer.ui_state.selected_feature = selected;
            renderer.set_selection_geometry(geometry, &self.render_context.scaler);
        }
        self.refresh_selected_symbol_preview();
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

    fn handle_native_s421_ui(&mut self, event_json: &str) -> Result<()> {
        anyhow::ensure!(
            event_json.len() <= 64 * 1024,
            "S-421 command exceeds receiver byte limit"
        );
        let event: ferrite_s421::ui::UiEvent = serde_json::from_str(event_json)?;
        if matches!(
            event,
            ferrite_s421::ui::UiEvent::New
                | ferrite_s421::ui::UiEvent::Finish
                | ferrite_s421::ui::UiEvent::SetLegGeometry { .. }
        ) {
            anyhow::ensure!(
                self.chart_loaded,
                "Open chart data before authoring a route"
            );
            self.ensure_navigation_scene()?;
            let renderer = self.renderer.as_mut().context("Renderer unavailable")?;
            match event {
                ferrite_s421::ui::UiEvent::New => self.native_s421.begin_local_edit(
                    renderer,
                    &self.render_context.scaler,
                    &self.current_profile_name,
                )?,
                ferrite_s421::ui::UiEvent::Finish => self.native_s421.finish_local_edit(
                    renderer,
                    &self.render_context.scaler,
                    &self.current_profile_name,
                )?,
                ferrite_s421::ui::UiEvent::SetLegGeometry { id, geometry } => {
                    self.native_s421.set_local_leg_geometry(
                        id,
                        geometry,
                        renderer,
                        &self.render_context.scaler,
                        &self.current_profile_name,
                    )?
                }
                _ => unreachable!(),
            }
            renderer.ui_state.object_selection_mode = false;
            self.clear_selection();
            self.is_dragging = false;
            self.pan_velocity = (0., 0.);
            self.recent_positions.clear();
            return Ok(());
        }
        self.native_s421
            .handle_event(event_json, self.require_signatures)
    }

    fn chart_click(&mut self, position: (f64, f64)) {
        let select_objects = self
            .renderer
            .as_ref()
            .is_some_and(|r| r.ui_state.object_selection_mode);
        if !self.native_s421.editing()
            && !select_objects
            && !self.plugin_system.has_loaded_plugins()
        {
            return;
        }
        self.navigation_scene_pending.explicit_retry();
        if !self.flush_navigation_scene() {
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
        if self.native_s421.editing() {
            let result =
                self.ensure_navigation_scene().and_then(|_| {
                    let world = self.render_context.scaler.screen_to_world(
                        ferrite_render::ScreenPoint::new(position.0 as f32, position.1 as f32),
                    );
                    let renderer = self.renderer.as_mut().context("Renderer unavailable")?;
                    self.native_s421.add_local_waypoint(
                        world.x,
                        world.y,
                        renderer,
                        &self.render_context.scaler,
                        &self.current_profile_name,
                    )
                });
            if let Err(error) = result {
                if let Some(renderer) = &mut self.renderer {
                    renderer.ui_state.notice = Some(format!("S-421 waypoint rejected: {error:#}"));
                }
            } else {
                self.refresh_dataset_tree();
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return;
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
        } else if select_objects {
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
                if coverage_info.is_some() {
                    renderer.ui_state.object_details_hidden = false;
                }
                renderer.ui_state.coverage_info = coverage_info;
            }

            self.select_feature(selected);
            info!(
                "Click at ({:.4}, {:.4}): {} unique objects found",
                world.x, world.y, nearby_count
            );
        }
    }

    fn apply_gesture_motion(&mut self, motion: navigation::GestureMotion) {
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
        let bounds = match self
            .render_context
            .scaler
            .projection()
            .pan_between(self.bounds, bounds)
        {
            Some(pan) => {
                self.pan_offset = (pan[0], pan[1]);
                self.rebuild_camera_bounds().unwrap_or(bounds)
            }
            None => bounds,
        };
        self.render_context.zoom_to_fit(bounds);
        if self.renderer.is_some() {
            if self.navigation_scene_pending.dirty() || !self.preview_or_background_build() {
                if self
                    .navigation_scene_pending
                    .may_defer(self.plugin_system.has_loaded_plugins())
                {
                    self.defer_navigation_scene(true, false);
                } else {
                    self.update_view();
                }
            } else if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.zoom_level = zoom;
            }
        }
        self.zoom_last_scroll = std::time::Instant::now();
        self.zoom_rebuild_phase = 2;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Start emitting the current camera's scene on a worker. The visible frame
    /// keeps the affine preview until `poll_scene_build` installs the result.
    fn try_start_scene_build(&mut self) -> bool {
        if !self.background_scene_builds
            || self.scene_build.is_some()
            || !self.chart_loaded
            || self.loading_state.is_some()
            || self.navigation_flush_active
            || self.plugin_system.has_loaded_plugins()
            || self.cell_resources.is_none()
            || !self
                .renderer
                .as_ref()
                .is_some_and(|r| r.background_scene_build_supported())
        {
            return false;
        }
        let Some(bounds) = self.rebuild_camera_bounds() else {
            return false;
        };
        let Some(window) = self.window.clone() else {
            return false;
        };
        if !self
            .renderer
            .as_mut()
            .is_some_and(|r| r.prepare_background_scene_build())
        {
            return false;
        }
        let groups = self.get_visible_viewing_groups();
        let Some(resources) = self.cell_resources.take() else {
            return false;
        };
        let renderer = self.renderer.as_mut().expect("checked above");
        // Same camera prefix as rebuild_view_ex, on a source fork.
        let mut context = self.render_context.fork_for_emission();
        let (x, y, w, h) = renderer.chart_viewport_pixels();
        if w > 0.0 && h > 0.0 {
            context.set_viewport_rect(x, y, w, h);
        }
        context.zoom_to_fit(bounds);
        let mut job = renderer.begin_scene_build(context, resources, groups);
        let inventory = self.coverage_inventory.clone();
        let (extent, density) = (window.inner_size(), window.scale_factor());
        self.scene_build = Some(std::thread::spawn(move || {
            match prepare_flat_coverage(inventory.as_deref(), job.context_mut(), extent, density) {
                Ok(()) => job.run(),
                Err(error) => job.fail(format!("coverage preparation failed: {error:#}")),
            }
        }));
        true
    }

    /// Install a finished background scene; with `wait`, block until it is.
    /// Always returns `cell_resources` to the App.
    fn poll_scene_build(&mut self, wait: bool) {
        if !self
            .scene_build
            .as_ref()
            .is_some_and(|handle| wait || handle.is_finished())
        {
            return;
        }
        let handle = self.scene_build.take().expect("checked above");
        let built = match handle.join() {
            Ok(built) => built,
            Err(_) => {
                // The worker owned the resources; rebuild them synchronously.
                tracing::error!("Background scene build panicked");
                self.startup_error = Some("Background scene build panicked".into());
                return;
            }
        };
        let build_time = built.build_time;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let installed = renderer.install_built_scene(built, &self.render_context);
        self.cell_resources = Some(installed.resources);
        if !installed.installed {
            tracing::debug!(
                "Background scene discarded: {}",
                installed.reason.as_deref().unwrap_or("unknown")
            );
            return;
        }
        tracing::debug!("Background scene installed after {build_time:?}");
        // Show the newer scene through the current camera, starting the next
        // build if the camera already left its drift budget. Only an exact
        // blocker leaves a UI-thread rebuild.
        if !wait && !self.preview_or_background_build() {
            self.update_view_ex(false, false);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Reconcile the visible scene with the stationary camera. True once that
    /// is done or a background build for this camera is running; false means
    /// retry next frame (another build is still in flight).
    fn settle_scene(&mut self) -> bool {
        let exact = self.renderer.as_ref().is_some_and(|r| {
            let (pan, zoom, _) = r.fast_view_transform();
            pan == (0., 0.)
                && zoom == 1.
                && !r.motion_preview_active()
                && r.geometry_matches_view(&self.render_context.scaler)
        });
        if exact {
            return true;
        }
        if self.scene_build.is_some() {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return false;
        }
        if self.try_start_scene_build() {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return true;
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.reset_pan_offset();
        }
        self.update_view_ex(false, false);
        true
    }

    /// Affine preview, else a background build plus preview. False means the
    /// caller must rebuild on the UI thread.
    fn preview_or_background_build(&mut self) -> bool {
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };
        if renderer.set_gpu_view_scaler(&self.render_context.scaler) {
            // Build ahead of the drift budget so the next scene usually lands
            // before the preview's stale border shows.
            if renderer.preview_refresh_due() && renderer.background_build_previewable() {
                self.try_start_scene_build();
            }
            return true;
        }
        if renderer.scene_build_pending()
            || !renderer.background_build_previewable()
            || !self.try_start_scene_build()
        {
            return false;
        }
        self.renderer
            .as_mut()
            .is_some_and(|r| r.set_gpu_view_scaler(&self.render_context.scaler))
    }

    /// The bounds `rebuild_view_ex` derives from (pan_offset, zoom_level).
    /// Navigation fits through the same values so whether or when a rebuild
    /// runs (motion preview drift, settle) never perturbs the camera.
    fn rebuild_camera_bounds(&self) -> Option<ferrite_render::GeoBounds> {
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        self.render_context.scaler.projection().view_bounds(
            self.bounds,
            self.zoom_level,
            [wrapped_pan_x, self.pan_offset.1],
        )
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
        let bounds = self.rebuild_camera_bounds().unwrap_or(bounds);
        self.render_context.zoom_to_fit(bounds);
        if self.renderer.is_some()
            && (self.navigation_scene_pending.dirty() || !self.preview_or_background_build())
        {
            if self
                .navigation_scene_pending
                .may_defer(self.plugin_system.has_loaded_plugins())
            {
                self.defer_navigation_scene(false, false);
            } else {
                self.update_view_ex(false, false);
            }
        }
    }

    /// Update the view based on current zoom and pan
    /// - `rebuild_hit_test`: if false, skip rebuilding the hit-test symbol list
    /// - `preserve_declutter`: if true, preserve symbol declutter grids to avoid flickering
    fn start_flat_eventloop_diagnostics(&mut self, path: PathBuf) -> anyhow::Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Eventloop diagnostic requires BG=1"
        );
        anyhow::ensure!(
            std::env::var_os("FERRITE_FLAT_SERVICE_AUDIT").is_none(),
            "Do not combine synchronous and eventloop diagnostics"
        );
        anyhow::ensure!(
            !path.join("flat-eventloop.json").exists(),
            "Refuse to overwrite existing eventloop proof"
        );
        let r = self
            .renderer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No renderer"))?;
        anyhow::ensure!(
            !r.window().is_visible().unwrap_or(true) && !r.window().has_focus(),
            "Eventloop diagnostic cannot activate a window"
        );
        // Explicit timestamp qualification uses identical legacy-profiler OFF
        // policy in both probe OFF/ON runs. Screenshot startup otherwise enables
        // debug profiling. This is hidden-audit-only, never a user mode change.
        if std::env::var_os("FERRITE_GPU_TIMESTAMP_AUDIT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            self.debug_mode = false;
            let renderer = self.renderer.as_mut().unwrap();
            renderer.ui_state.debug_mode = false;
            renderer.set_profiling_enabled(false);
        }
        // Qualify the real palette transaction before timing any callbacks.
        // This opt-in is reachable only after the hidden-window checks above.
        if let Some(profile) = std::env::var_os("FERRITE_FLAT_EVENTLOOP_PROFILE") {
            let profile = profile.to_str().context("Non-UTF8 diagnostic palette")?;
            anyhow::ensure!(
                ["Day", "Dusk", "Night"].contains(&profile),
                "Unknown diagnostic palette"
            );
            let prepared = self.prepare_portrayal_change(PortrayalChangeRequest {
                profile: profile.into(),
                settings: self.applied_settings.clone(),
            })?;
            self.commit_portrayal_change(prepared)?;
            self.ensure_navigation_scene()?;
        }
        let bounds = [
            self.bounds.min_x,
            self.bounds.min_y,
            self.bounds.max_x,
            self.bounds.max_y,
        ];
        anyhow::ensure!(
            flat_service_trajectory::valid_bounds(bounds),
            "Invalid diagnostic bounds"
        );
        self.select_feature(None);
        self.pan_velocity = (0., 0.);
        self.is_dragging = false;
        self.flat_eventloop_audit = Some(Box::new(flat_eventloop_diagnostics::Audit::new(
            path,
            bounds,
            self.zoom_level,
            self.pan_offset,
        )));
        if let Some(value) = std::env::var_os("FERRITE_FLAT_EVENTLOOP_BURST") {
            anyhow::ensure!(
                !self.plugin_system.has_loaded_plugins(),
                "Burst coalescing qualification requires no loaded plugins"
            );
            self.navigation_burst_audit = Some(Box::new(flat_event_burst::Audit::new(
                &value,
                std::env::var_os("FERRITE_FLAT_EVENTLOOP_BURST_CAPTURE").as_deref()
                    == Some(std::ffi::OsStr::new("1")),
            )?));
        }
        // Opt-in GPU timestamps collect the first 64 actual chart passes.
        // Allocation precedes the measured callbacks. Unsupported devices are
        // reported explicitly instead of substituting a zero duration.
        if std::env::var_os("FERRITE_GPU_FRAME_TIMESTAMPS").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            let result = self
                .renderer
                .as_mut()
                .unwrap()
                .begin_gpu_frame_timestamp_batch();
            if let Err(error) = result {
                let path = &self.flat_eventloop_audit.as_ref().unwrap().path;
                std::fs::create_dir_all(path)?;
                std::fs::write(
                    path.join("gpu-timestamp-unavailable.json"),
                    serde_json::to_vec_pretty(
                        &serde_json::json!({"gpu_chart_ms":null,"reason":error.to_string()}),
                    )?,
                )?;
            }
        }
        self.flat_eventloop_audit_started = true;
        Ok(())
    }
    fn begin_flat_eventloop_diagnostic_frame(
        &mut self,
        callback_entry: Option<std::time::Instant>,
    ) -> anyhow::Result<()> {
        let Some(a) = self.flat_eventloop_audit.as_ref() else {
            return Ok(());
        };
        let replay = self
            .navigation_burst_audit
            .as_ref()
            .and_then(|b| b.replay_index);
        let index = replay.unwrap_or(a.next);
        if replay.is_some() {
            let r = self
                .renderer
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No replay renderer"))?;
            anyhow::ensure!(
                ferrite_wgpu::background_test::enabled()
                    && r.window().is_visible() == Some(false)
                    && !r.window().has_focus(),
                "Replay window visible/focused"
            );
        }
        let (target_zoom, target_pan) = if replay.is_some() {
            a.pose_at(index)
        } else {
            a.pose()
        };
        if replay.is_none() {
            let source = self.render_context.geometry_revision();
            self.renderer
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("No renderer"))?
                .begin_flat_diagnostics(index as u64, source, index as u64)?;
            self.flat_eventloop_audit.as_mut().unwrap().begin(
                callback_entry.ok_or_else(|| anyhow::anyhow!("Missing callback entry clock"))?,
            );
            let line_before = self.renderer.as_ref().unwrap().line_preparation_work();
            self.flat_eventloop_audit
                .as_mut()
                .unwrap()
                .begin_line_work(line_before);
        }
        let camera = std::time::Instant::now();
        let viewport = self.render_context.scaler.viewport;
        let center = (
            viewport.width as f64 * 0.5 + viewport.x as f64,
            viewport.height as f64 * 0.5 + viewport.y as f64,
        );
        if self.navigation_burst_audit.is_some() {
            let burst = self.navigation_burst_audit.as_ref().unwrap().burst;
            self.navigation_burst_audit.as_mut().unwrap().begin();
            let initial_zoom = self.zoom_level;
            let initial_pan = self.pan_offset;
            for step in 1..=burst {
                // Fixed diagnostic target interpolation; OFF and ON use the
                // same ordered helper calls, never queued/reordered camera deltas.
                let t = step as f64 / burst as f64;
                let zoom = if step == burst {
                    target_zoom
                } else {
                    initial_zoom * (target_zoom / initial_zoom).powf(t)
                };
                let pan = if step == burst {
                    target_pan
                } else {
                    (
                        initial_pan.0 + (target_pan.0 - initial_pan.0) * t,
                        initial_pan.1 + (target_pan.1 - initial_pan.1) * t,
                    )
                };
                let wrapped = pan.0 - (pan.0 / 360.0).round() * 360.0;
                let bounds = self
                    .render_context
                    .scaler
                    .projection()
                    .view_bounds(self.bounds, zoom, [wrapped, pan.1])
                    .ok_or_else(|| anyhow::anyhow!("Invalid burst target"))?;
                let anchor =
                    self.render_context
                        .scaler
                        .screen_to_world(ferrite_render::ScreenPoint::new(
                            center.0 as f32,
                            center.1 as f32,
                        ));
                let mut desired = self.render_context.scaler.clone();
                desired.zoom_to_fit(bounds);
                let to = desired.world_to_screen(anchor);
                if let Some(replay_pose) = replay.filter(|_| step == burst) {
                    self.renderer
                        .as_mut()
                        .unwrap()
                        .arm_accepted_screen_line_packet(replay_pose);
                }
                self.apply_gesture_motion(navigation::GestureMotion {
                    from: center,
                    to: (to.x as f64, to.y as f64),
                    ratio: zoom / self.zoom_level,
                });
                let bits = self.navigation_camera_bits();
                self.navigation_burst_audit.as_mut().unwrap().motion(bits)?;
            }
        } else {
            // Use the actual touch/pinch camera route, not direct geometry mutation.
            let wrapped = target_pan.0 - (target_pan.0 / 360.0).round() * 360.0;
            let target_bounds = self
                .render_context
                .scaler
                .projection()
                .view_bounds(self.bounds, target_zoom, [wrapped, target_pan.1])
                .ok_or_else(|| anyhow::anyhow!("Invalid gesture target bounds"))?;
            let anchor =
                self.render_context
                    .scaler
                    .screen_to_world(ferrite_render::ScreenPoint::new(
                        center.0 as f32,
                        center.1 as f32,
                    ));
            let mut desired = self.render_context.scaler.clone();
            desired.zoom_to_fit(target_bounds);
            let to = desired.world_to_screen(anchor);
            self.apply_gesture_motion(navigation::GestureMotion {
                from: center,
                to: (to.x as f64, to.y as f64),
                ratio: target_zoom / self.zoom_level,
            });
        }
        self.renderer.as_ref().unwrap().record_flat_stage(
            ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Camera,
            camera.elapsed(),
        );
        let view = self.render_context.scaler.geo_bounds;
        let overlap = flat_service_trajectory::overlap_fraction(
            self.flat_eventloop_audit.as_ref().unwrap().bounds,
            [view.min_x, view.min_y, view.max_x, view.max_y],
        );
        anyhow::ensure!(
            index >= 400 || overlap > 0.,
            "Primary diagnostic pose outside loaded AABB"
        );
        self.flat_eventloop_audit.as_mut().unwrap().overlap[index] = overlap;
        Ok(())
    }
    fn navigation_camera_bits(&self) -> [u64; 12] {
        let scaler = &self.render_context.scaler;
        let b = scaler.geo_bounds;
        let v = scaler.viewport;
        [
            self.zoom_level.to_bits(),
            self.pan_offset.0.to_bits(),
            self.pan_offset.1.to_bits(),
            b.min_x.to_bits(),
            b.min_y.to_bits(),
            b.max_x.to_bits(),
            b.max_y.to_bits(),
            scaler.scale_x().to_bits(),
            scaler.scale_y().to_bits(),
            v.x.to_bits() as u64,
            v.y.to_bits() as u64,
            ((v.width.to_bits() as u64) << 32) | v.height.to_bits() as u64,
        ]
    }
    fn end_flat_eventloop_diagnostic_frame(&mut self) -> anyhow::Result<()> {
        if let Some(index) = self
            .navigation_burst_audit
            .as_ref()
            .and_then(|b| b.replay_index)
        {
            let r = self.renderer.as_ref().unwrap();
            anyhow::ensure!(
                r.window().is_visible() == Some(false) && !r.window().has_focus(),
                "Replay rendered window visible/focused"
            );
            let bits = self.navigation_camera_bits();
            self.navigation_burst_audit
                .as_ref()
                .unwrap()
                .verify_replay(index, bits)?;
            if flat_event_burst::Audit::selected(index) {
                self.ensure_navigation_scene()?;
                let folder = self
                    .flat_eventloop_audit
                    .as_ref()
                    .unwrap()
                    .path
                    .join("burst-selected-poses")
                    .join(format!("pose-{index:04}"));
                anyhow::ensure!(!folder.exists(), "Refuse overwrite selected pose");
                std::fs::create_dir_all(&folder)?;
                let r = self.renderer.as_ref().unwrap();
                anyhow::ensure!(
                    r.window().is_visible() == Some(false) && !r.window().has_focus(),
                    "Selected pose visible/focused"
                );
                self.audit_portrayal(&folder)?;
                self.renderer
                    .as_mut()
                    .unwrap()
                    .save_screenshot(folder.join("frame.png"))?;
                let r = self.renderer.as_ref().unwrap();
                anyhow::ensure!(
                    r.window().is_visible() == Some(false) && !r.window().has_focus(),
                    "Readback window visible/focused"
                );
                self.navigation_burst_audit
                    .as_mut()
                    .unwrap()
                    .captured(index, &folder)?;
            }
            self.navigation_burst_audit.as_mut().unwrap().replay_index = Some(index + 1);
            if index + 1 == 500 {
                let a = self.flat_eventloop_audit.take().unwrap();
                let burst = self.navigation_burst_audit.take().unwrap();
                burst.export_capture(&a.path)?;
                self.zoom_level = a.saved_zoom;
                self.zoom_target = a.saved_zoom;
                self.pan_offset = a.saved_pan;
                self.zoom_animating = false;
                self.pan_velocity = (0., 0.);
                self.pan_rebuild_phase = 0;
                self.zoom_rebuild_phase = 0;
                self.update_view();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            return Ok(());
        }
        let camera_bits = self
            .navigation_burst_audit
            .as_ref()
            .map(|_| self.navigation_camera_bits());
        let Some(a) = self.flat_eventloop_audit.as_mut() else {
            return Ok(());
        };
        if let Some(burst) = self.navigation_burst_audit.as_mut() {
            burst.complete(a.next, camera_bits.expect("Active burst camera bits"))?;
        }
        let service = a
            .start
            .ok_or_else(|| anyhow::anyhow!("Missing eventloop frame start"))?
            .elapsed();
        a.gpu_coverage_host[a.next] = self.renderer.as_ref().unwrap().flat_gpu_coverage_host_ns();
        let row = self
            .renderer
            .as_mut()
            .unwrap()
            .finish_flat_diagnostics(service.as_nanos().min(u64::MAX as u128) as u64)?;
        a.complete(row)?;
        if a.next == flat_eventloop_diagnostics::FRAME_COUNT {
            a.export()?;
            if std::env::var_os("FERRITE_SURFACE_PACING_DIAGNOSTICS").as_deref()
                == Some(std::ffi::OsStr::new("1"))
            {
                let pacing = self
                    .renderer
                    .as_ref()
                    .unwrap()
                    .surface_pacing_snapshot()
                    .ok_or_else(|| anyhow::anyhow!("Surface pacing collector unavailable"))?;
                std::fs::write(
                    a.path.join("surface-pacing.json"),
                    serde_json::to_vec_pretty(&pacing)?,
                )?;
                anyhow::ensure!(
                    pacing["complete_500_normal_surface_frames"] == true,
                    "Surface pacing ledger incomplete; no latency result admitted"
                );
            }
            // All CPU timing samples are closed before the single GPU readback.
            // Readback also precedes any untimed correctness replay.
            if std::env::var_os("FERRITE_GPU_FRAME_TIMESTAMPS").as_deref()
                == Some(std::ffi::OsStr::new("1"))
            {
                let gpu = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .finish_gpu_frame_timestamp_batch()?;
                std::fs::write(
                    a.path.join("gpu-timestamp-batch.json"),
                    serde_json::to_vec_pretty(&gpu)?,
                )?;
            }
            if let Some(burst) = self.navigation_burst_audit.as_ref() {
                burst.export(&a.path)?;
            }
            if self
                .navigation_burst_audit
                .as_ref()
                .is_some_and(|b| b.capture_enabled)
            {
                let (zoom, pan) = (a.saved_zoom, a.saved_pan);
                self.navigation_burst_audit.as_mut().unwrap().replay_index = Some(0);
                self.zoom_level = zoom;
                self.zoom_target = zoom;
                self.pan_offset = pan;
                self.zoom_animating = false;
                self.pan_velocity = (0., 0.);
                self.pan_rebuild_phase = 0;
                self.zoom_rebuild_phase = 0;
                self.update_view();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                return Ok(());
            }
            self.navigation_burst_audit = None;
            let a = self.flat_eventloop_audit.take().unwrap();
            self.zoom_level = a.saved_zoom;
            self.zoom_target = a.saved_zoom;
            self.pan_offset = a.saved_pan;
            self.zoom_animating = false;
            self.pan_velocity = (0., 0.);
            self.pan_rebuild_phase = 0;
            self.zoom_rebuild_phase = 0;
            self.update_view();
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        Ok(())
    }

    fn audit_flat_service(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        self.ensure_navigation_scene()?;
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Flat service requires background mode"
        );
        let mut ledger = ferrite_render::flat_reuse_diagnostics::FlatFrameLedger::<512>::default();
        anyhow::ensure!(
            ferrite_render::flat_reuse_diagnostics::FlatFrameLedger::<512>::capacity_bytes()
                .unwrap_or(usize::MAX)
                <= 1024 * 1024,
            "Diagnostic budget"
        );
        let mut prepare_wall = [0u64; 500];
        let mut render_wall = [0u64; 500];
        let mut bounds_overlap = [0.0f64; 500];
        let mut gpu_coverage_host_wall = [0u64; 500];
        let loaded = [
            self.bounds.min_x,
            self.bounds.min_y,
            self.bounds.max_x,
            self.bounds.max_y,
        ];
        anyhow::ensure!(
            flat_service_trajectory::valid_bounds(loaded),
            "Invalid loaded chart bounds"
        );
        let saved_zoom = self.zoom_level;
        let saved_pan = self.pan_offset;
        self.select_feature(None);
        for cycle in 0..5u64 {
            for pose in 0..100u64 {
                let r = self
                    .renderer
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No renderer"))?;
                r.begin_flat_diagnostics(
                    cycle * 100 + pose,
                    self.render_context.geometry_revision(),
                    cycle * 100 + pose,
                )?;
                let service = std::time::Instant::now();
                let camera = std::time::Instant::now();
                let t = pose as f64 / 99.0;
                self.zoom_level =
                    navigation::MAX_ZOOM.powf(if cycle.is_multiple_of(2) { t } else { 1.0 - t });
                // Primary: chart-relative +/-20% extent, outside sweep separate.
                self.pan_offset = flat_service_trajectory::pan(loaded, t, cycle == 4);
                self.render_context
                    .scaler
                    .projection()
                    .view_bounds(
                        self.bounds,
                        self.zoom_level,
                        [
                            self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0,
                            self.pan_offset.1,
                        ],
                    )
                    .map(|b| self.render_context.zoom_to_fit(b))
                    .ok_or_else(|| anyhow::anyhow!("Invalid diagnostic camera"))?;
                let view = self.render_context.scaler.geo_bounds;
                let overlap = flat_service_trajectory::overlap_fraction(
                    loaded,
                    [view.min_x, view.min_y, view.max_x, view.max_y],
                );
                bounds_overlap[(cycle * 100 + pose) as usize] = overlap;
                anyhow::ensure!(
                    cycle == 4 || overlap > 0.0,
                    "Primary pose does not intersect loaded chart AABB"
                );
                let r = self.renderer.as_mut().unwrap();
                r.record_flat_stage(
                    ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Camera,
                    camera.elapsed(),
                );
                if !r.set_gpu_view_scaler(&self.render_context.scaler) {
                    self.update_view_ex(false, true);
                }
                prepare_wall[(cycle * 100 + pose) as usize] =
                    service.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                let render_start = std::time::Instant::now();
                let r = self.renderer.as_mut().unwrap();
                r.render()?;
                render_wall[(cycle * 100 + pose) as usize] =
                    render_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                r.wait_hidden_key_frame()?;
                gpu_coverage_host_wall[(cycle * 100 + pose) as usize] =
                    r.flat_gpu_coverage_host_ns();
                ledger.record(r.finish_flat_diagnostics(
                    service.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                )?);
            }
        }
        self.zoom_level = saved_zoom;
        self.pan_offset = saved_pan;
        self.update_view();
        let rows:Vec<_>=ledger.rows().map(|r|serde_json::json!({"frame":r.frame,"trajectory":if r.frame<400{"chart_relative"}else{"outside_wide"},"phase":if r.frame<100 || r.frame>=400{"first_traversal"}else{"warm_revisit"},"chart_aabb_overlap_fraction":bounds_overlap[r.frame as usize],"chart_aabb_intersects":bounds_overlap[r.frame as usize]>0.0,"source_epoch":r.source_epoch,"view_epoch":r.view_epoch,"service_ns":r.service_ns,"prepare_wall_ns":prepare_wall[r.frame as usize],"render_through_present_wall_ns":render_wall[r.frame as usize],"hidden":r.hidden,"focused":r.focused,"internal_stage_timing_available":r.internal_stage_timing_available,"spans_ns":r.available_spans(),"coverage_gpu_binding_plan_upload_host_wall_ns":gpu_coverage_host_wall[r.frame as usize],"reuse_attempts":r.reuse_attempts,"reuse_accepted":r.reuse_accepted,"rejected_by_bit":r.rejected_by_bit,"source_iterations":r.work.source_commands,"executed_commands":r.work.executed_commands,"dependency_iterations":r.work.dependency_iterations,"buffer_upload_calls":r.work.buffer_upload_calls,"buffer_upload_bytes":r.work.buffer_upload_bytes,"triangulation_hits":r.work.triangulation_hits,"triangulation_cold":r.work.triangulation_cold,"area_pattern_projected_vertices":r.work.projected_vertices,"area_pattern_input_triangles":r.work.triangles})).collect();
        let mut service: Vec<_> = ledger
            .rows()
            .filter(|r| r.frame >= 100 && r.frame < 400)
            .map(|r| r.service_ns)
            .collect();
        service.sort_unstable();
        let quantile =
            |q: f64| service[((service.len() as f64 * q).ceil() as usize).saturating_sub(1)];
        let summarize = |primary: bool, inside: bool, warm: bool| {
            let mut ns: Vec<_> = ledger
                .rows()
                .filter(|r| {
                    (r.frame < 400) == primary
                        && (bounds_overlap[r.frame as usize] > 0.0) == inside
                        && (r.frame >= 100 && r.frame < 400) == warm
                })
                .map(|r| r.service_ns)
                .collect();
            ns.sort_unstable();
            let q = |f: f64| {
                if ns.is_empty() {
                    None
                } else {
                    Some(ns[((ns.len() as f64 * f).ceil() as usize).saturating_sub(1)])
                }
            };
            serde_json::json!({"trajectory":if primary{"chart_relative"}else{"outside_wide"},"chart_aabb_intersects":inside,"warm_revisit":warm,"samples":ns.len(),"p95_ns":q(0.95),"p99_ns":q(0.99),"over_16_7_ms":ns.iter().filter(|&&n|n>16_700_000).count()})
        };
        let categories: Vec<_> = [true, false]
            .into_iter()
            .flat_map(|primary| {
                [true, false].into_iter().flat_map(move |inside| {
                    [true, false]
                        .into_iter()
                        .map(move |warm| (primary, inside, warm))
                })
            })
            .map(|(p, i, w)| summarize(p, i, w))
            .collect();
        std::fs::create_dir_all(path)?;
        std::fs::write(
            path.join("flat-service.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"loaded_chart_aabb":loaded,"categories":categories,"aabb_scope":"geographic loaded bounding rectangle only, not exact DataCoverage polygon/visibility/pixel area","rows":rows,"dropped":ledger.dropped(),"warm_nearest_rank_p95_ns":quantile(0.95),"warm_nearest_rank_p99_ns":quantile(0.99),"warm_over_16_7_ms":service.iter().filter(|&&n|n>16_700_000).count(),"scope":"serialized service includes prepare+async submit+present+residual completion; NOT GPU duration or foreground FPS; readback excluded; overlapping stage walls nonadditive","source_iteration_scope":"per emission including dependency retry","upload_scope":"only instrumented create_vertex/index_buffer payloads; excludes symbol mapped buffers/text/texture/uniform uploads"}),
            )?,
        )?;
        Ok(())
    }

    fn update_view_ex(&mut self, rebuild_hit_test: bool, preserve_declutter: bool) {
        let request = self
            .navigation_scene_pending
            .requirements()
            .map(|old| flat_event_coalescing::Request {
                hit_test: old.hit_test || rebuild_hit_test,
                preserve_declutter: old.preserve_declutter && preserve_declutter,
            })
            .unwrap_or(flat_event_coalescing::Request {
                hit_test: rebuild_hit_test,
                preserve_declutter,
            });
        let ready = self.rebuild_view_ex(request.hit_test, request.preserve_declutter);
        if self.navigation_scene_pending.dirty() {
            self.navigation_scene_pending.finish(ready);
        }
    }
    fn flush_navigation_scene(&mut self) -> bool {
        flat_event_coalescing::flush_runtime(self)
    }
    fn ensure_s102_journal_ready(&mut self) -> Result<()> {
        if let Some(gate) = &mut self.s102_journal_gate {
            if let Err(error) = gate.require_ready() {
                gate.block_for_recovery();
                return Err(error);
            }
        }
        Ok(())
    }
    fn ensure_navigation_scene(&mut self) -> Result<()> {
        self.ensure_s102_journal_ready()?;
        self.poll_scene_build(true);
        // An approximate motion preview is never a consumer scene.
        if let Some(renderer) = self.renderer.as_mut().filter(|r| r.motion_preview_active()) {
            renderer.reset_pan_offset();
            self.update_view_ex(true, false);
        }
        anyhow::ensure!(
            flat_event_coalescing::ensure_runtime(self),
            "Deferred navigation scene is not ready; consumer/publication rejected"
        );
        Ok(())
    }
    fn defer_navigation_scene(&mut self, hit_test: bool, preserve_declutter: bool) {
        if let Some(a) = self.navigation_burst_audit.as_mut() {
            a.counts.deferred += 1;
        }
        self.navigation_scene_pending
            .request(hit_test, preserve_declutter);
        // Exact original update_view_ex camera-prefix order. This normalization
        // still occurs per event before the next gesture anchor is read.
        self.next_temporal_wake = None;
        if let Some(renderer) = &mut self.renderer {
            renderer.reset_pan_offset();
        }
        self.zoom_rebuilt_level = self.zoom_level;
        if let Some(renderer) = &self.renderer {
            let (x, y, w, h) = renderer.chart_viewport_pixels();
            if w > 0.0 && h > 0.0 {
                self.render_context.set_viewport_rect(x, y, w, h);
            }
        }
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        let Some(new_bounds) = self.render_context.scaler.projection().view_bounds(
            self.bounds,
            self.zoom_level,
            [wrapped_pan_x, self.pan_offset.1],
        ) else {
            self.navigation_scene_pending.finish(false);
            return;
        };
        self.render_context.zoom_to_fit(new_bounds);
        // Preserve arbitrary plugin callback count and final owned instructions
        // per rejected event. Do not assume plugin &self methods are pure.
        self.render_context.remove_coverage_exempt_instructions();
        for mut instr in self.plugin_system.get_render_instructions() {
            instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
            self.render_context.add_instruction(instr);
        }
    }
    fn rebuild_view_ex(&mut self, rebuild_hit_test: bool, preserve_declutter: bool) -> bool {
        self.poll_scene_build(true);
        if let Err(error) = self.ensure_s102_journal_ready() {
            self.startup_error = Some(format!("S102 history recovery required: {error:#}"));
            return false;
        }
        if let Some(a) = self.navigation_burst_audit.as_mut() {
            a.counts.rebuild_attempts += 1;
        }
        let flat_camera_timer = self
            .renderer
            .as_ref()
            .filter(|r| r.flat_diagnostics_active())
            .map(|_| std::time::Instant::now());
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
            return false;
        };

        self.render_context.zoom_to_fit(new_bounds);

        if let (Some(t), Some(r)) = (flat_camera_timer, self.renderer.as_ref()) {
            r.record_flat_stage(
                ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Camera,
                t.elapsed(),
            );
        }
        // Pre-compute color profile and viewing groups before mutable borrow of renderer
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();

        // Prepare plugin instructions before renderer borrow
        // Always add plugin instructions (route overlays should render even without charts)
        let flat_overlay_timer = self
            .renderer
            .as_ref()
            .filter(|r| r.flat_diagnostics_active())
            .map(|_| std::time::Instant::now());
        if !self.navigation_flush_active {
            self.render_context.remove_coverage_exempt_instructions();
            for mut instr in self.plugin_system.get_render_instructions() {
                instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
                self.render_context.add_instruction(instr);
            }
        }
        if let (Some(t), Some(r)) = (flat_overlay_timer, self.renderer.as_ref()) {
            r.record_flat_stage(
                ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Overlay,
                t.elapsed(),
            );
        }
        let flat_coverage_timer = self
            .renderer
            .as_ref()
            .filter(|r| r.flat_diagnostics_active())
            .map(|_| std::time::Instant::now());
        if let Some(renderer) = &self.renderer {
            if let Err(error) = prepare_flat_coverage(
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
                return false;
            }
        }

        if let (Some(t), Some(r)) = (flat_coverage_timer, self.renderer.as_ref()) {
            r.record_flat_stage(
                ferrite_render::flat_reuse_diagnostics::FlatFrameStage::Coverage,
                t.elapsed(),
            );
        }
        if let Some(renderer) = &mut self.renderer {
            // Update zoom level for symbol decluttering and UI
            renderer.set_zoom_level(self.zoom_level);
            renderer.ui_state.zoom_level = self.zoom_level;
            renderer.ui_state.signature_counts = Some((self.verified_count, self.unsigned_count));
            // During animation, preserve declutter state to avoid flickering
            renderer.begin_frame_ex(preserve_declutter);

            renderer.update_raster_view(&self.render_context.scaler);
            renderer.update_selection(&self.render_context.scaler);

            // Draw world map coastlines as the lowest layer (before chart data)
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);

            // Chart data + plugin overlay rendering
            let portrayal_result = if let Some(resources) = &mut self.cell_resources {
                renderer.try_add_instructions_with_resource_owners(
                    &mut self.render_context,
                    resources,
                    visible_vgs.as_ref(),
                )
            } else {
                renderer.try_add_instructions_with_symbols(
                    &mut self.render_context,
                    Some(&mut self.symbol_cache),
                    color_profile,
                    visible_vgs.as_ref(),
                )
            };
            if let Err(error) = portrayal_result {
                let message = format!("Chart portrayal preparation failed: {error}");
                tracing::error!("{message}");
                renderer.begin_frame();
                renderer.ui_state.notice = Some(message.clone());
                renderer.ui_state.clear_selection();
                renderer.set_selection_geometry(Vec::new(), &self.render_context.scaler);
                self.rendered_symbols.clear();
                self.pending_hit_test = None;
                if self.auto_screenshot.is_some() {
                    self.startup_error = Some(message);
                }
                return false;
            }
        }

        // Rebuild symbols for hit testing (skip during animation for performance)
        if rebuild_hit_test && self.chart_loaded {
            let hit_test_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let flat_pick_timer = self
                .renderer
                .as_ref()
                .filter(|r| r.flat_diagnostics_active())
                .map(|_| std::time::Instant::now());
            self.build_rendered_symbols();
            if let (Some(t), Some(r)) = (flat_pick_timer, self.renderer.as_ref()) {
                r.record_flat_stage(
                    ferrite_render::flat_reuse_diagnostics::FlatFrameStage::PickIndex,
                    t.elapsed(),
                );
            }
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
        if self.navigation_scene_pending.enabled() {
            self.renderer
                .as_ref()
                .is_some_and(|r| r.navigation_scene_ready(&self.render_context.scaler))
        } else {
            true
        }
    }

    /// Panel layout is measured during rendering. Rebuild on any changed axis,
    /// not only the left panel origin, before accepting an automated pick/export.
    fn sync_chart_layout(&mut self) -> bool {
        if !self.flush_navigation_scene() {
            return false;
        }
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

    fn sync_native_s421_overlay(&mut self) {
        let native_packet = self
            .native_s421
            .prepare_overlay(&self.render_context.scaler, &self.current_profile_name);
        if let Some(renderer) = &mut self.renderer {
            let supported = self
                .native_s421
                .resources_support_palette(&self.current_profile_name);
            let result = match native_packet {
                Ok(packet) => renderer.sync_native_route_gpu(packet, &self.render_context.scaler),
                Err(error) => {
                    renderer.invalidate_native_route_gpu();
                    Err(format!("{error:#}"))
                }
            };
            self.native_s421
                .set_gpu_capability(supported && result.is_ok(), result.err());
        }
    }

    fn apply_auto_screenshot_camera(&mut self) {
        if self.auto_screenshot.is_none() {
            return;
        }
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
        self.update_view();
        self.frames_since_loaded = Some(0);
    }

    /// Actual normal camera rebuild and route publication, plus deliberately stale
    /// consumer checks. Explicit hidden diagnostics are excluded from frame timing.
    fn audit_native_s421_editor(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Editor audit requires hidden mode"
        );
        anyhow::ensure!(
            self.window
                .as_ref()
                .is_some_and(|w| w.is_visible() == Some(false) && !w.has_focus()),
            "Editor audit window must be hidden and unfocused"
        );
        anyhow::ensure!(
            self.native_s421.audit_data()["routes"]
                .as_array()
                .is_some_and(|a| a.is_empty()),
            "Editor audit requires no imported routes"
        );
        anyhow::ensure!(
            self.s102_fc_status.loaded && self.s102_pc_status.loaded,
            "Default S-102 catalogues not initialized"
        );
        anyhow::ensure!(!output.exists(), "Editor audit output already exists");
        std::fs::create_dir_all(output)?;
        self.ensure_navigation_scene()?;
        let model = serde_json::to_vec(&self.publication_model()?)?;
        let camera = self.render_context.scaler.flat_encoded_identity();
        self.renderer
            .as_mut()
            .context("No renderer")?
            .ui_state
            .object_selection_mode = false;
        // Actual App command route; no direct controller-only shortcut.
        self.handle_native_s421_ui(r#"{"type":"New"}"#)?;
        anyhow::ensure!(self.native_s421.editing(), "New did not start editing");
        let empty = self.native_s421.audit_data();
        anyhow::ensure!(
            self.handle_native_s421_ui(r#"{"type":"Finish"}"#).is_err(),
            "Empty route Finish accepted"
        );
        anyhow::ensure!(
            self.native_s421.audit_data() == empty,
            "Failed empty Finish changed host"
        );
        let (x, y, w, h) = self.renderer.as_ref().unwrap().chart_viewport_pixels();
        let points = [
            (
                f64::from(x) + f64::from(w) * 0.45,
                f64::from(y) + f64::from(h) * 0.45,
            ),
            (
                f64::from(x) + f64::from(w) * 0.55,
                f64::from(y) + f64::from(h) * 0.55,
            ),
        ];
        for (index, point) in points.into_iter().enumerate() {
            anyhow::ensure!(self.chart_contains(point), "Audit click outside chart");
            self.chart_click(point);
            let state = self.native_s421.audit_data();
            anyhow::ensure!(
                state["routes"][0]["waypoints"].as_u64() == Some((index + 1) as u64),
                "Native map click not appended"
            );
            self.renderer.as_mut().unwrap().render()?;
            anyhow::ensure!(
                self.renderer
                    .as_ref()
                    .unwrap()
                    .native_route_encoded_in_last_pass(),
                "Route not encoded after click"
            );
        }
        // Exercise the actual App dispatch and CPU-to-GPU publication for both models.
        let mut geometry_choice_rows = Vec::new();
        let before_choice = self.native_s421.audit_data();
        let waypoint_id = before_choice["ui"]["waypoints"][1]["id"]
            .as_u64()
            .context("Missing incoming waypoint")?;
        let first_id = before_choice["ui"]["waypoints"][0]["id"]
            .as_u64()
            .context("Missing first waypoint")?;
        for geometry in ["Orthodrome", "Loxodrome"] {
            self.handle_native_s421_ui(&serde_json::json!({"type":"SetLegGeometry", "id":waypoint_id, "geometry":geometry}).to_string())?;
            let state = self.native_s421.audit_data();
            anyhow::ensure!(
                state["ui"]["waypoints"][1]["leg_geometry"].as_str() == Some(geometry),
                "UI curve declaration stale"
            );
            anyhow::ensure!(
                state["ui"]["waypoints"][1]["initial_bearing_deg"]
                    .as_f64()
                    .is_some_and(f64::is_finite),
                "WGS84 bearing unavailable"
            );
            anyhow::ensure!(
                state["world_cache"]["solver_legs_in_cache"].as_u64() == Some(1),
                "Declared local curve did not reach sampler"
            );
            anyhow::ensure!(
                state["prepared_overlay"]["curve_points"]
                    .as_u64()
                    .is_some_and(|n| n >= 2),
                "No curve geometry prepared"
            );
            self.renderer.as_mut().unwrap().render()?;
            anyhow::ensure!(
                self.renderer
                    .as_ref()
                    .unwrap()
                    .native_route_encoded_in_last_pass(),
                "Declared curve not encoded"
            );
            geometry_choice_rows.push(serde_json::json!({"geometry":geometry, "waypoint":state["ui"]["waypoints"][1], "world_cache":state["world_cache"], "overlay":state["prepared_overlay"]}));
        }
        let retained_choice = self.native_s421.audit_data();
        let retained_choice_pixels = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_native_frame(true)?;
        anyhow::ensure!(self.handle_native_s421_ui(&serde_json::json!({"type":"SetLegGeometry", "id":first_id, "geometry":"Orthodrome"}).to_string()).is_err(), "First waypoint accepted incoming geometry");
        anyhow::ensure!(
            self.native_s421.audit_data() == retained_choice,
            "Failed geometry choice mutated host"
        );
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .audit_displayed_native_frame(true)?
                .rgba8
                == retained_choice_pixels.rgba8,
            "Failed geometry choice mutated GPU frame"
        );
        self.handle_native_s421_ui(r#"{"type":"Finish"}"#)?;
        anyhow::ensure!(!self.native_s421.editing(), "Finish left editor active");
        self.renderer.as_mut().unwrap().render()?;
        let shown = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_native_frame(true)?;
        let component = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_native_frame(false)?;
        anyhow::ensure!(
            shown.rgba8 != component.rgba8,
            "Route made no pixel contribution"
        );
        // A prepared wrong-camera packet is refused at the actual GPU prepare boundary.
        let mut wrong_camera = self.render_context.scaler.clone();
        wrong_camera.pan(17., 0.);
        let wrong = self
            .native_s421
            .prepare_overlay(&wrong_camera, &self.current_profile_name)?;
        let retained_host = self.native_s421.audit_data();
        let refusal = self
            .renderer
            .as_mut()
            .unwrap()
            .sync_native_route_gpu(wrong, &self.render_context.scaler);
        anyhow::ensure!(refusal.is_err(), "Wrong-camera GPU packet accepted");
        let after = self
            .renderer
            .as_ref()
            .unwrap()
            .audit_displayed_native_frame(true)?;
        anyhow::ensure!(
            shown.rgba8 == after.rgba8,
            "GPU refusal changed displayed pixels"
        );
        anyhow::ensure!(
            self.native_s421.audit_data() == retained_host,
            "GPU refusal changed host"
        );
        self.native_s421
            .prepare_overlay(&self.render_context.scaler, &self.current_profile_name)?;
        self.handle_native_s421_ui(r#"{"type":"New"}"#)?;
        anyhow::ensure!(self.native_s421.editing(), "Second New failed");
        self.native_s421.stop_local_edit(); // Same stop path dispatched by Escape.
        let stopped = self.native_s421.audit_data()["routes"].clone();
        self.chart_click(points[0]);
        anyhow::ensure!(
            self.native_s421.audit_data()["routes"] == stopped,
            "Stopped editor consumed click"
        );
        anyhow::ensure!(
            serde_json::to_vec(&self.publication_model()?)? == model,
            "Editor changed chart model"
        );
        anyhow::ensure!(
            self.render_context.scaler.flat_encoded_identity() == camera,
            "Editor changed camera"
        );
        self.renderer.as_mut().unwrap().render()?;
        self.renderer
            .as_mut()
            .unwrap()
            .save_screenshot(output.join("editor.png"))?;
        self.renderer
            .as_ref()
            .unwrap()
            .audit_native_route_gpu_buffers(output)?;
        std::fs::write(
            output.join("editor.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "hidden":true,"selection_mode_off":true,"app_new_click_finish":true,"actual_route_pixels":true,
                "app_geometry_choice":true,"invalid_geometry_choice_preserved_host_and_pixels":true,"geometry_choice_rows":geometry_choice_rows,
                "wrong_camera_gpu_refused":format!("{:?}",refusal.err()),"failed_gpu_whole_rgba_unchanged":true,
                "stopped_editor_click_ignored":true,"chart_model_unchanged":true,"camera_unchanged":true,
                "s102_fc":self.s102_fc_status.version,"s102_pc":self.s102_pc_status.version,
                "native":self.native_s421.audit_data(),"full_s421_conformance":false
            }))?,
        )?;
        Ok(())
    }

    fn audit_native_s421_lifecycle(&mut self, output: &Path) -> Result<()> {
        anyhow::ensure!(
            ferrite_wgpu::background_test::enabled(),
            "Native lifecycle audit requires background mode"
        );
        std::fs::create_dir_all(output)?;
        let initial_pan = self.pan_offset;
        let initial_zoom = self.zoom_level;
        let mut rows = Vec::new();
        for (name, pan, zoom) in [
            ("initial", initial_pan, initial_zoom),
            ("pan", (initial_pan.0 + 1., initial_pan.1), initial_zoom),
            ("zoom", initial_pan, (initial_zoom * 1.25).min(500.)),
            ("returned", initial_pan, initial_zoom),
        ] {
            self.pan_offset = pan;
            self.zoom_level = zoom;
            self.zoom_target = zoom;
            self.update_view();
            self.ensure_navigation_scene()?;
            // Settle measured egui chart viewport, then rebuild through the same
            // production boundary used for live frames.
            for _ in 0..3 {
                self.sync_native_s421_overlay();
                self.renderer.as_mut().context("No renderer")?.render()?;
                if self.sync_chart_layout() {
                    self.update_view();
                }
            }
            self.sync_native_s421_overlay();
            let renderer = self.renderer.as_mut().context("No renderer")?;
            renderer.render()?;
            anyhow::ensure!(
                renderer.native_route_encoded_in_last_pass(),
                "Native frame {name} was not encoded"
            );
            let case = output.join(name);
            std::fs::create_dir_all(&case)?;
            renderer.audit_native_route_gpu_buffers(&case)?;
            renderer.save_screenshot(case.join("chart.png"))?;
            rows.push(serde_json::json!({"case":name,"camera":self.render_context.scaler.flat_encoded_identity(),"native":self.native_s421.audit_data(),"encoded":true}));
        }
        // Change the actual renderer's camera consumer without a new packet.
        let mut stale = self.render_context.scaler.clone();
        stale.pan(1., 0.);
        let renderer = self.renderer.as_mut().context("No renderer")?;
        renderer.update_viewport_bounds(&stale);
        renderer.render()?;
        anyhow::ensure!(
            !renderer.native_route_encoded_in_last_pass(),
            "Stale route packet accepted after camera change"
        );
        rows.push(serde_json::json!({"case":"stale-camera","encoded":false}));
        self.update_view();
        self.sync_native_s421_overlay();
        self.renderer.as_mut().unwrap().render()?;
        anyhow::ensure!(
            self.renderer
                .as_ref()
                .unwrap()
                .native_route_encoded_in_last_pass(),
            "Native camera recovery failed"
        );
        // PC currently has only Day. Failed preparation must not paint a Day
        // substitute in a Night session or reuse an old frame publication.
        let original_palette = std::mem::replace(&mut self.current_profile_name, "Night".into());
        self.sync_native_s421_overlay();
        self.renderer.as_mut().unwrap().render()?;
        anyhow::ensure!(
            !self
                .renderer
                .as_ref()
                .unwrap()
                .native_route_encoded_in_last_pass(),
            "Unsupported palette painted old route"
        );
        rows.push(serde_json::json!({"case":"unsupported-palette","encoded":false,"native":self.native_s421.audit_data()}));
        self.current_profile_name = original_palette;
        self.sync_native_s421_overlay();
        for (name, enabled) in [("hidden", false), ("shown", true)] {
            self.native_s421
                .handle_event(r#"{"type":"ToggleRendering"}"#, false)?;
            self.sync_native_s421_overlay();
            self.renderer.as_mut().unwrap().render()?;
            anyhow::ensure!(
                self.renderer
                    .as_ref()
                    .unwrap()
                    .native_route_encoded_in_last_pass()
                    == enabled,
                "Route toggle mismatch"
            );
            self.renderer
                .as_mut()
                .unwrap()
                .save_screenshot(output.join(format!("{name}.png")))?;
            rows.push(serde_json::json!({"case":name,"encoded":enabled,"native":self.native_s421.audit_data()}));
        }
        self.native_s421.clear();
        self.sync_native_s421_overlay();
        self.renderer.as_mut().unwrap().render()?;
        anyhow::ensure!(
            !self
                .renderer
                .as_ref()
                .unwrap()
                .native_route_encoded_in_last_pass(),
            "Cleared routes retained GPU draw"
        );
        rows.push(serde_json::json!({"case":"clear","encoded":false,"native":self.native_s421.audit_data()}));
        std::fs::write(
            output.join("lifecycle.json"),
            serde_json::to_vec_pretty(&rows)?,
        )?;
        Ok(())
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
                            // Update render context viewport
                            let size = window.inner_size();
                            self.render_context
                                .set_viewport(size.width as f32, size.height as f32);

                            // Startup and worker-thread warning/error events share this bounded owner.
                            renderer.ui_state.diagnostics = diagnostics_tracing::shared();
                            renderer.ui_state.diagnostic_sink_dropped =
                                diagnostics_tracing::shared_dropped();
                            // Initialize UI state
                            renderer.ui_state.verify_dataset_signatures =
                                self.require_signatures || self.operational_mode;
                            renderer.ui_state.signature_verification_locked = self.operational_mode;
                            renderer.ui_state.settings.viewing_layers =
                                self.applied_settings.viewing_layers.clone();
                            renderer.ui_state.settings.viewing_layer_owner =
                                self.applied_settings.viewing_layer_owner;
                            renderer.ui_state.optional_viewing_layers =
                                ferrite_s101::optional_viewing_layers(&self.pc);
                            renderer.ui_state.settings.display_mode =
                                self.applied_settings.display_mode;
                            renderer.ui_state.version = VERSION.to_string();
                            renderer.ui_state.zoom_level = self.zoom_level;
                            renderer.ui_state.signature_counts =
                                Some((self.verified_count, self.unsigned_count));
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
                                let portrayal_result =
                                    if let Some(resources) = &mut self.cell_resources {
                                        renderer.try_add_instructions_with_resource_owners(
                                            &mut self.render_context,
                                            resources,
                                            visible_vgs.as_ref(),
                                        )
                                    } else {
                                        renderer.try_add_instructions_with_symbols(
                                            &mut self.render_context,
                                            Some(&mut self.symbol_cache),
                                            color_profile,
                                            visible_vgs.as_ref(),
                                        )
                                    };
                                if let Err(error) = portrayal_result {
                                    let message =
                                        format!("Chart portrayal preparation failed: {error}");
                                    tracing::error!("{message}");
                                    renderer.begin_frame();
                                    renderer.ui_state.notice = Some(message.clone());
                                    renderer.ui_state.clear_selection();
                                    renderer.set_selection_geometry(
                                        Vec::new(),
                                        &self.render_context.scaler,
                                    );
                                    self.rendered_symbols.clear();
                                    self.pending_hit_test = None;
                                    self.startup_error = Some(message);
                                } else {
                                    self.build_rendered_symbols();
                                }
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

                            let _args: Vec<_> = std::env::args().collect();

                            self.renderer = Some(renderer);
                            self.native_s421_ui_revision = 0;
                            let native_routes = std::mem::take(&mut self.pending_auto_s421);
                            for path in native_routes {
                                if let Err(error) =
                                    self.native_s421.import_path(&path, self.require_signatures)
                                {
                                    self.startup_error =
                                        Some(format!("S-421 startup import failed: {error:#}"));
                                    event_loop.exit();
                                    return;
                                }
                                if self.auto_screenshot.is_some() {
                                    self.frames_since_loaded = Some(0);
                                }
                            }
                            if !self.chart_loaded && self.auto_screenshot.is_some() {
                                self.apply_auto_screenshot_camera();
                            }
                            self.refresh_dataset_tree();

                            if ferrite_wgpu::background_test::enabled() {
                                if let Some(path) = std::env::var_os("FERRITE_NATIVE_DATASET_AUDIT")
                                {
                                    std::env::remove_var("FERRITE_NATIVE_DATASET_AUDIT");
                                    if let Err(error) =
                                        self.audit_native_dataset_open(&PathBuf::from(path))
                                    {
                                        self.startup_error = Some(format!(
                                            "Native dataset opening audit failed: {error:#}"
                                        ));
                                    }
                                    event_loop.exit();
                                    return;
                                }
                            }
                            if let Some(path) = std::env::var_os("FERRITE_DATASET_UI_AUDIT") {
                                std::env::remove_var("FERRITE_DATASET_UI_AUDIT");
                                if let Err(error) = self.audit_dataset_ui(&PathBuf::from(path)) {
                                    self.startup_error =
                                        Some(format!("Dataset UI audit failed: {error:#}"));
                                }
                                event_loop.exit();
                                return;
                            }
                            if let Some(output) =
                                std::env::var_os("FERRITE_ROOT_CATALOGUE_PUBLICATION_AUDIT")
                            {
                                std::env::remove_var("FERRITE_ROOT_CATALOGUE_PUBLICATION_AUDIT");
                                if let Err(error) =
                                    self.audit_catalogue_publication(&PathBuf::from(output))
                                {
                                    self.startup_error = Some(format!(
                                        "Catalogue publication audit failed: {error:#}"
                                    ));
                                }
                                event_loop.exit();
                                return;
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
            self.thread_stats.reset();
            self.frame_times.clear();
            if let Some(renderer) = &mut self.renderer {
                renderer.ui_state.debug_mode = self.debug_mode;
                renderer.set_profiling_enabled(self.debug_mode);
                renderer.ui_state.debug_cpu_usage = None;
                renderer.ui_state.debug_main_thread_cpu = None;
                renderer.ui_state.debug_memory_mb = None;
                renderer.ui_state.debug_gpu.utilization_percent = None;
                renderer.ui_state.debug_history = Default::default();
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return;
        }

        if let WindowEvent::KeyboardInput { event, .. } = &event {
            if event.state == winit::event::ElementState::Pressed
                && !event.repeat
                && event.logical_key
                    == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)
            {
                self.clear_selection();
                if self.native_s421.editing() {
                    self.native_s421.stop_local_edit();
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
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
                    if self.background_scene_builds {
                        self.settle_scene();
                    } else {
                        self.update_view();
                    }
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
                    if !self.background_scene_builds {
                        self.update_view();
                    }
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
                // Attempt old-scene barrier first; resize is an explicit recovery
                // event. Dirty remains if it fails; only the new ready rebuild clears.
                self.navigation_scene_pending.explicit_retry();
                let _ = self.flush_navigation_scene();
                // Native DPI changes affect fixed-size strokes/symbols and SCAMIN
                // even when the physical window dimensions stay the same.
                self.update_view();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Resized(physical_size) => {
                // Attempt old-scene barrier first; resize is an explicit recovery
                // event. Dirty remains if it fails; only the new ready rebuild clears.
                self.navigation_scene_pending.explicit_retry();
                let _ = self.flush_navigation_scene();
                if let Some(pending) = self.coverage_lifecycle_resize.as_mut() {
                    if physical_size.width > 0 && physical_size.height > 0 {
                        pending.observed = Some(physical_size);
                    }
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
                self.service_mcp_ui();
                if let Err(error) = self.ensure_s102_journal_ready() {
                    self.startup_error = Some(format!("S102 history recovery required: {error:#}"));
                    event_loop.exit();
                    return;
                }
                let diagnostic_callback_entry = self
                    .flat_eventloop_audit
                    .as_ref()
                    .map(|_| std::time::Instant::now());
                if self.coverage_lifecycle_resize.is_some() {
                    match self.resume_coverage_lifecycle_resize() {
                        Ok(true) => {
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            self.startup_error =
                                Some(format!("Coverage resize lifecycle failed: {e:#}"));
                            event_loop.exit();
                        }
                    }
                    return;
                }
                if let Err(e) =
                    self.begin_flat_eventloop_diagnostic_frame(diagnostic_callback_entry)
                {
                    self.startup_error = Some(format!("Eventloop diagnostic start failed: {e:#}"));
                    event_loop.exit();
                    return;
                }
                // Poll for completed async hit-test build
                self.poll_hit_test();
                self.poll_scene_build(false);

                // Process inertia/momentum
                let now = std::time::Instant::now();
                let dt = now.duration_since(self.last_frame_time).as_secs_f64();
                self.last_frame_time = now;

                // Frame profiling: begin frame
                let profiling_enabled = ferrite_wgpu::profiler::is_profiling_enabled();
                if profiling_enabled {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.cpu_profiler.begin_navigation_frame(
                            self.is_dragging
                                || self.zoom_animating
                                || self.pan_velocity.0.abs() > 0.0001
                                || self.pan_velocity.1.abs() > 0.0001
                                || self.pan_rebuild_phase != 0
                                || self.zoom_rebuild_phase != 0
                                || now.duration_since(self.zoom_last_scroll)
                                    < std::time::Duration::from_millis(150)
                                || self.flat_eventloop_audit.is_some(),
                        );
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
                    if let Some(bounds) = ferrite_render::anchored_zoom_bounds_projected(
                        self.render_context.scaler.projection(),
                        self.bounds,
                        viewport,
                        self.zoom_level,
                        WorldPoint::new(self.zoom_anchor_world.0, self.zoom_anchor_world.1),
                        ferrite_render::ScreenPoint::new(cursor_sx, cursor_sy),
                        self.render_context.scaler.geo_bounds.center().y,
                    ) {
                        let bounds = match self
                            .render_context
                            .scaler
                            .projection()
                            .pan_between(self.bounds, bounds)
                        {
                            Some(pan) => {
                                self.pan_offset = (pan[0], pan[1]);
                                self.rebuild_camera_bounds().unwrap_or(bounds)
                            }
                            None => bounds,
                        };
                        self.render_context.zoom_to_fit(bounds);
                        if self.renderer.is_some() {
                            if !self.preview_or_background_build() {
                                // View-dependent placement/parent visibility cannot
                                // be obtained by scaling old screen anchors.
                                self.update_view_ex(false, true);
                            } else if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.zoom_level = self.zoom_level;
                                renderer.ui_state.signature_counts =
                                    Some((self.verified_count, self.unsigned_count));
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
                    // A UI-thread settle is debounced against further wheel
                    // input; a background settle can start immediately.
                    let debounce_ms = if self.background_scene_builds { 0 } else { 80 };
                    if elapsed.as_millis() >= debounce_ms
                        && !self.zoom_animating
                        && self.settle_scene()
                    {
                        self.zoom_rebuilt_level = self.zoom_level;
                        self.zoom_rebuild_phase = 1;
                    }
                } else if self.zoom_rebuild_phase == 1 {
                    let elapsed = now.duration_since(self.zoom_last_scroll);
                    // Hit-test reads the installed scene; wait for a settle build.
                    let delay_ms = if self.background_scene_builds { 0 } else { 300 };
                    if elapsed.as_millis() >= delay_ms && self.scene_build.is_none() {
                        // Hit-test only (geometry unchanged since Phase 1)
                        self.build_rendered_symbols();
                        self.zoom_rebuild_phase = 0;
                    }
                }

                // Deferred pan rebuild after inertia/drag stops
                // Phase 2→1: Rebuild geometry + declutter, skip hit-test
                // Phase 1→0 (150ms): Rebuild hit-test only
                if self.pan_rebuild_phase == 2 {
                    if self.settle_scene() {
                        self.pan_rebuild_phase = 1;
                    }
                } else if self.pan_rebuild_phase == 1 {
                    let elapsed = now.duration_since(self.pan_rebuild_time);
                    let delay_ms = if self.background_scene_builds { 0 } else { 150 };
                    if elapsed.as_millis() >= delay_ms && self.scene_build.is_none() {
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

                // Hidden diagnostics exercise the same asynchronous folder queue
                // as the menu, rather than bypassing it with --s102 startup loads.
                if ferrite_wgpu::background_test::enabled() {
                    if let Some(path) = std::env::var_os("FERRITE_OPEN_FOLDER_AUDIT_INPUT") {
                        std::env::remove_var("FERRITE_OPEN_FOLDER_AUDIT_INPUT");
                        if let Err(error) = self.begin_dataset_open(PathBuf::from(path), true) {
                            self.startup_error =
                                Some(format!("Folder opening audit failed: {error:#}"));
                            event_loop.exit();
                            return;
                        }
                    }
                }
                self.poll_dataset_open();
                let dataset_busy = self.loading_state.is_some()
                    || self.dataset_open_discovery.is_some()
                    || self.dataset_open_session.is_some();
                let (select_dataset, unload_dataset) = self
                    .renderer
                    .as_mut()
                    .map(|renderer| {
                        renderer.ui_state.dataset_loading = dataset_busy;
                        (
                            renderer.ui_state.dataset_selection_requested.take(),
                            renderer.ui_state.dataset_unload_requested.take(),
                        )
                    })
                    .unwrap_or((None, None));
                if let Some(id) = select_dataset {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.ui_state.selected_dataset = Some(id);
                    }
                }
                if let Some(id) = unload_dataset {
                    if let Err(error) = self.unload_dataset(id) {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.ui_state.notice = Some(format!(
                                "Could not unload dataset; previous display retained: {error:#}"
                            ));
                        }
                    }
                }
                self.process_pending_portrayal_change();

                if self.renderer.as_mut().is_some_and(|renderer| {
                    std::mem::take(&mut renderer.ui_state.clear_selection_requested)
                }) {
                    self.clear_selection();
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
                if let Some(_r) = &mut self.renderer {}
                let view_changed = false;
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
                    if let Err(error) = self.ensure_navigation_scene() {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.ui_state.notice =
                                Some(format!("Could not open dataset folder: {error:#}"));
                        }
                    } else if let Some(folder) = rfd::FileDialog::new()
                        .set_title("Open Dataset Folder")
                        .pick_folder()
                    {
                        if let Err(error) = self.begin_dataset_open(folder, true) {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.notice =
                                    Some(format!("Could not open dataset folder: {error:#}"));
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
                            self.ensure_navigation_scene()?;
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
                            primary_viewing_layers::validate_new_primary(
                                &pc,
                                &self.applied_settings,
                            )?;
                            Ok((fc, pc))
                        })();
                        match candidate {
                            Ok((fc, pc)) => {
                                if let Err(error) =
                                    self.change_catalogue_pair_manually(Arc::new(fc), Arc::new(pc))
                                {
                                    if let Some(r) = &mut self.renderer {
                                        r.ui_state.notice = Some(format!(
                                            "Catalogue set portrayal failed: {error:#}"
                                        ));
                                    }
                                }
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
                        self.thread_stats.reset();
                        self.frame_times.clear();
                        renderer.set_profiling_enabled(self.debug_mode);
                        renderer.ui_state.debug_cpu_usage = None;
                        renderer.ui_state.debug_main_thread_cpu = None;
                        renderer.ui_state.debug_memory_mb = None;
                        renderer.ui_state.debug_gpu.utilization_percent = None;
                        renderer.ui_state.debug_history = Default::default();
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
                    if let Err(error) = self.ensure_navigation_scene() {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.ui_state.notice =
                                Some(format!("Could not open dataset: {error:#}"));
                        }
                    } else if let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "S-100 datasets / exchange catalogue",
                            &["000", "h5", "H5", "xml"],
                        )
                        .add_filter("All files (including numeric updates)", &["*"])
                        .set_title("Open Dataset")
                        .pick_file()
                    {
                        if let Err(error) = self.begin_dataset_open(path, false) {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.ui_state.notice =
                                    Some(format!("Could not open dataset: {error:#}"));
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
                            self.ensure_navigation_scene()?;
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
                                if let Err(error) = self.change_catalogue_pair_manually(
                                    Arc::new(new_fc),
                                    self.pc.clone(),
                                ) {
                                    if let Some(r) = &mut self.renderer {
                                        r.ui_state.notice = Some(format!(
                                            "Feature catalogue portrayal failed: {error:#}"
                                        ));
                                    }
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
                            self.ensure_navigation_scene()?;
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
                            primary_viewing_layers::validate_new_primary(
                                &new_pc,
                                &self.applied_settings,
                            )?;
                            Ok(new_pc)
                        });
                        match candidate {
                            Ok(new_pc) => {
                                if let Err(error) = self.change_catalogue_pair_manually(
                                    self.fc.clone(),
                                    Arc::new(new_pc),
                                ) {
                                    if let Some(r) = &mut self.renderer {
                                        r.ui_state.notice = Some(format!(
                                            "Portrayal catalogue rebuild failed: {error:#}"
                                        ));
                                    }
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
                    if let Err(error) = self.ensure_navigation_scene() {
                        if let Some(r) = self.renderer.as_mut() {
                            r.ui_state.notice = Some(format!("Screenshot rejected: {error:#}"));
                        }
                        if self.auto_screenshot.is_some() || self.flat_eventloop_audit.is_some() {
                            self.startup_error =
                                Some(format!("Screenshot navigation barrier failed: {error:#}"));
                            event_loop.exit();
                        }
                        return;
                    }
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
                    if self.background_scene_builds {
                        // Same animated, previewed path as the wheel, about the
                        // chart centre; the settle build runs off the UI thread.
                        self.pan_velocity = (0., 0.);
                        self.pan_rebuild_phase = 0;
                        self.zoom_cursor_screen = (centre.x, centre.y);
                        let anchor = self.render_context.scaler.screen_to_world(centre);
                        self.zoom_anchor_world = (anchor.x, anchor.y);
                        self.zoom_target = target;
                        self.zoom_animating = true;
                        self.zoom_last_scroll = std::time::Instant::now();
                        self.zoom_rebuild_phase = 2;
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                        continue;
                    }

                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.zoom_level = target;
                    self.zoom_target = target;
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
                    self.refresh_dataset_tree();
                    // Deactivate all plugins (close panels) and clear plugin data
                    self.plugin_system.deactivate_all_plugins();
                    self.plugin_system.clear_all_data();
                    self.native_s421.clear();
                }

                // Egui candidate controls are not live portrayal. Coalesce the
                // latest settings/profile request and restore applied controls now.
                if color_change.is_some() || settings_change.is_some() {
                    let candidate_settings = settings_change
                        .map(|mut settings| {
                            primary_viewing_layers::capture_primary(&self.pc, &mut settings)?;
                            Ok::<_, anyhow::Error>(settings)
                        })
                        .transpose();
                    match candidate_settings {
                        Ok(settings_change) => {
                            let request = coalesce_portrayal_request(
                                self.pending_portrayal_change.take(),
                                &self.current_profile_name,
                                &self.applied_settings,
                                color_change,
                                settings_change,
                            );
                            self.queue_portrayal_change(request);
                        }
                        Err(error) => {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.set_settings(self.applied_settings.clone());
                                renderer.ui_state.notice = Some(format!("Primary PC layer selection rejected; previous display retained: {error:#}"));
                            }
                        }
                    }
                }

                // Handle plugin toggle request (only when chart is loaded)
                if let Some(plugin_id) = plugin_toggle {
                    if plugin_id == native_s421::PROVIDER_ID {
                        self.native_s421.toggle_panel();
                        if !self.native_s421.panel_visible() {
                            self.native_s421.stop_local_edit();
                        }
                    } else if self.chart_loaded {
                        if let Err(error) = self.ensure_navigation_scene() {
                            if let Some(r) = self.renderer.as_mut() {
                                r.ui_state.notice =
                                    Some(format!("Plugin change rejected: {error:#}"));
                            }
                            if self.auto_screenshot.is_some() || self.flat_eventloop_audit.is_some()
                            {
                                self.startup_error = Some(format!(
                                    "Plugin change navigation barrier failed: {error:#}"
                                ));
                                event_loop.exit();
                            }
                            return;
                        }
                        self.plugin_system.toggle_plugin(&plugin_id);
                    }
                }

                // Layout changes are not navigation inputs. In particular, a
                // loading-status/tree width change must not alter the geographic
                // centre supplied by --center or the last user gesture. The
                // post-render sync_chart_layout path fits the same camera bounds
                // to the measured chart viewport without accumulating world pan.
                if let Some(renderer) = &mut self.renderer {
                    let _ = renderer.take_pan_adjust_pixels();
                }

                // Plain redraws must not re-arm failed deferred work. Inspect
                // queued events without consuming them before a successful barrier.
                let queued_plugin_ui = self
                    .renderer
                    .as_ref()
                    .is_some_and(|r| !r.ui_state.plugin_ui_events.is_empty());
                let loaded_plugins = self.plugin_system.has_loaded_plugins();
                if !flat_event_coalescing::plugin_ui_runtime(self, loaded_plugins, queued_plugin_ui)
                {
                    let error = anyhow::anyhow!(
                        "Deferred navigation scene is not ready; consumer/publication rejected"
                    );
                    {
                        if let Some(r) = self.renderer.as_mut() {
                            r.ui_state.notice = Some(format!("Plugin UI rejected: {error:#}"));
                        }
                        if self.auto_screenshot.is_some() || self.flat_eventloop_audit.is_some() {
                            self.startup_error =
                                Some(format!("Plugin UI navigation barrier failed: {error:#}"));
                            event_loop.exit();
                        }
                        return;
                    }
                }
                if self.native_s421.editing()
                    && self
                        .renderer
                        .as_ref()
                        .is_some_and(|renderer| renderer.ui_state.object_selection_mode)
                {
                    self.native_s421.stop_local_edit();
                }
                // Update plugin toolbar buttons in UI
                if let Some(renderer) = &mut self.renderer {
                    let mut buttons: Vec<_> = self
                        .plugin_system
                        .get_toolbar_buttons()
                        .into_iter()
                        .filter(|btn| btn.plugin_id != native_s421::PROVIDER_ID)
                        .map(|btn| ferrite_wgpu::PluginButton {
                            plugin_id: btn.plugin_id,
                            label: btn.label,
                            tooltip: btn.tooltip,
                            active: btn.active,
                        })
                        .collect();
                    buttons.push(ferrite_wgpu::PluginButton {
                        plugin_id: native_s421::PROVIDER_ID.into(),
                        label: "S-421 Routes".into(),
                        tooltip: Some("Import/export and inspect route data".into()),
                        active: self.native_s421.panel_visible(),
                    });
                    renderer.set_plugin_buttons(buttons);

                    // Cached native UI is rebuilt only after route/panel mutations;
                    // its presence does not disable ordinary navigation coalescing.
                    if loaded_plugins || self.native_s421_ui_revision != self.native_s421.revision()
                    {
                        let mut ui_data = self.plugin_system.get_active_plugin_ui_data();
                        ui_data.retain(|(id, _)| id != native_s421::PROVIDER_ID);
                        ui_data.push((
                            native_s421::PROVIDER_ID.into(),
                            self.native_s421.ui_json().into(),
                        ));
                        renderer.set_plugin_ui_data(ui_data);
                        self.native_s421_ui_revision = self.native_s421.revision();
                    }
                }
                let native_plugin_events = self
                    .renderer
                    .as_mut()
                    .map(|renderer| renderer.take_plugin_ui_events())
                    .unwrap_or_default();
                // Process plugin UI events
                for (plugin_id, event_json) in native_plugin_events {
                    if plugin_id == native_s421::PROVIDER_ID {
                        if let Err(error) = self.handle_native_s421_ui(&event_json) {
                            if let Some(r) = self.renderer.as_mut() {
                                r.ui_state.notice = Some(format!("S-421: {error:#}"));
                            }
                        }
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                        self.refresh_dataset_tree();
                        continue; // Native metadata commands never reportray the ENC scene.
                    }
                    self.plugin_system.send_ui_event(&plugin_id, &event_json);
                    // Update view after UI event
                    if self.chart_loaded {
                        self.update_view();
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
                            renderer.ui_state.debug_main_thread_cpu = self.thread_stats.sample();
                            renderer.ui_state.debug_memory_mb = sample.resident_mib;
                            renderer.ui_state.debug_gpu.utilization_percent =
                                gpu_usage::sample(&renderer.ui_state.debug_gpu.adapter);
                            renderer.ui_state.debug_history.record(
                                renderer.ui_state.debug_fps,
                                sample.cpu_percent,
                                sample.resident_mib,
                            );
                            if let Some(path) = &self.debug_stats_audit {
                                self.debug_stats_samples.push(serde_json::json!({
                                    "cpu_percent": sample.cpu_percent,
                                    "main_thread_cpu_percent": renderer.ui_state.debug_main_thread_cpu,
                                    "resident_mib": sample.resident_mib,
                                    "gpu_device_percent": renderer.ui_state.debug_gpu.utilization_percent,
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
                    renderer.set_cursor_world(p.x, p.y);
                }
                if !self.flush_navigation_scene() {
                    if self.auto_screenshot.is_some() || self.flat_eventloop_audit.is_some() {
                        self.startup_error = Some("Deferred navigation scene flush failed".into());
                        event_loop.exit();
                    }
                    return;
                }
                if self
                    .navigation_burst_audit
                    .as_ref()
                    .and_then(|b| b.replay_index)
                    .is_none()
                {
                    if let Some(a) = self.flat_eventloop_audit.as_mut() {
                        let after = self
                            .renderer
                            .as_ref()
                            .and_then(|r| r.line_preparation_work());
                        if let Err(error) = a.finish_line_work(after) {
                            self.startup_error =
                                Some(format!("Line preparation diagnostic failed: {error:#}"));
                            event_loop.exit();
                            return;
                        }
                        a.before_render();
                    }
                }
                // Publish only the exact current-camera, own-PC route packet.
                self.sync_native_s421_overlay();
                // Render
                if let Some(renderer) = &mut self.renderer {
                    renderer.ui_state.coordinate_ruler_scaler = (self.chart_loaded
                        && renderer.ui_state.coordinate_rulers)
                        .then(|| self.render_context.scaler.clone());
                    if let Err(e) = renderer.render() {
                        error!("Render error: {}", e);
                        if self.flat_eventloop_audit.is_some() {
                            self.startup_error =
                                Some(format!("Eventloop diagnostic render failed: {e}"));
                            event_loop.exit();
                            return;
                        }
                    }

                    // Frame profiling: end frame (logs periodic report)
                    if profiling_enabled {
                        renderer.cpu_profiler.end_frame();
                    }
                }

                if self
                    .navigation_burst_audit
                    .as_ref()
                    .and_then(|b| b.replay_index)
                    .is_none()
                {
                    if let Some(a) = self.flat_eventloop_audit.as_mut() {
                        a.after_render();
                    }
                }
                if self.startup_error.is_some() {
                    event_loop.exit();
                    return;
                }
                if self.sync_chart_layout() {
                    if self.flat_eventloop_audit.is_some() {
                        self.startup_error =
                            Some("Diagnostic layout changed during measured frame".into());
                        event_loop.exit();
                        return;
                    }
                    if self.frames_since_loaded.is_some() {
                        self.frames_since_loaded = Some(0);
                    }
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return;
                }

                if self.loading_state.is_none() && !self.pending_auto_s102.is_empty() {
                    let paths = std::mem::take(&mut self.pending_auto_s102);
                    if let Err(error) = self.load_bathymetry(&paths) {
                        let message = format!("S-102 startup load failed: {error:#}");
                        error!("{message}");
                        if self.auto_screenshot.is_some() {
                            self.startup_error = Some(message);
                            event_loop.exit();
                            return;
                        }
                        if let Some(renderer) = &mut self.renderer {
                            renderer.ui_state.notice = Some(message);
                        }
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
                // A preview or background-built scene with no settle scheduled
                // (programmatic camera, ended burst) is reconciled here once.
                if !view_settled
                    && self.flat_eventloop_audit.is_none()
                    && !self.zoom_animating
                    && self.zoom_rebuild_phase == 0
                    && self.pan_rebuild_phase == 0
                    && !self.is_dragging
                    && self.pan_velocity == (0., 0.)
                {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.reset_pan_offset();
                    }
                    self.update_view_ex(false, false);
                }
                if self.flat_eventloop_audit.is_none()
                    && self.frames_since_loaded.is_some()
                    && (!view_settled
                        || self.dataset_open_discovery.is_some()
                        || self.dataset_open_session.is_some())
                {
                    self.frames_since_loaded = Some(0);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                // Auto-screenshot: wait a few frames after load for rendering to stabilize
                let flat_audit_active = self.flat_eventloop_audit.is_some();
                if let Some(count) = self
                    .frames_since_loaded
                    .as_mut()
                    .filter(|_| !flat_audit_active)
                {
                    *count += 1;
                    if *count >= 5 {
                        if let Some(path) =
                            std::env::var_os("FERRITE_JOINT_VECTOR_RASTER_ACTIVATION_AUDIT")
                        {
                            std::env::remove_var("FERRITE_JOINT_VECTOR_RASTER_ACTIVATION_AUDIT");
                            if let Err(error) =
                                self.audit_joint_vector_raster_activation(&PathBuf::from(path))
                            {
                                self.startup_error = Some(format!(
                                    "Joint vector/raster activation audit failed: {error:#}"
                                ));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) =
                            std::env::var_os("FERRITE_READY_VECTOR_ACTIVATION_AUDIT")
                        {
                            std::env::remove_var("FERRITE_READY_VECTOR_ACTIVATION_AUDIT");
                            if let Err(error) =
                                self.audit_ready_vector_activation(&PathBuf::from(path))
                            {
                                self.startup_error = Some(format!(
                                    "Ready vector activation audit failed: {error:#}"
                                ));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_PRIVATE_VECTOR_GPU_AUDIT") {
                            std::env::remove_var("FERRITE_PRIVATE_VECTOR_GPU_AUDIT");
                            if let Err(error) = self.audit_private_vector_gpu(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Private vector GPU audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                            if std::env::var("FERRITE_TARGET_ACTIVATION_AUDIT").as_deref()
                                == Ok("1")
                            {
                                // Renderer-only audit deliberately leaves App model OLD. Do not dispatch
                                // another interaction, screenshot or update_view under mismatched owners.
                                event_loop.exit();
                                return;
                            }
                        }
                        if !self.flat_eventloop_audit_started {
                            if let Some(path) = std::env::var_os("FERRITE_FLAT_EVENTLOOP_AUDIT") {
                                if let Err(e) =
                                    self.start_flat_eventloop_diagnostics(PathBuf::from(path))
                                {
                                    self.startup_error = Some(format!(
                                        "Eventloop diagnostic activation failed: {e:#}"
                                    ));
                                    event_loop.exit();
                                    return;
                                }
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_FLAT_SERVICE_AUDIT") {
                            if let Err(e) = self.audit_flat_service(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("Flat service diagnostic failed: {e:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_CANCELLATION_AUDIT") {
                            if let Err(error) = self.audit_root_cancellation(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("Cancellation audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }

                        if let Some(path) = std::env::var_os("FERRITE_ROOT_PORTRAYAL_CHANGE_AUDIT")
                        {
                            std::env::remove_var("FERRITE_ROOT_PORTRAYAL_CHANGE_AUDIT");
                            let target = std::env::var("FERRITE_ROOT_PORTRAYAL_CHANGE_PROFILE")
                                .unwrap_or_else(|_| "Night".into());
                            if let Err(error) =
                                self.audit_portrayal_change_recovery(&PathBuf::from(path), &target)
                            {
                                self.startup_error = Some(format!(
                                    "Atomic portrayal change audit failed: {error:#}"
                                ));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) =
                            std::env::var_os("FERRITE_S102_ORIGINAL_RETENTION_AUDIT")
                        {
                            std::env::remove_var("FERRITE_S102_ORIGINAL_RETENTION_AUDIT");
                            if let Err(error) =
                                self.audit_s102_original_retention(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Original retention audit failed: {error:#}"));
                            }
                            event_loop.exit();
                            return;
                        }
                        if let Some(path) = std::env::var_os("FERRITE_GEOGRAPHIC_ANNULUS_AUDIT") {
                            std::env::remove_var("FERRITE_GEOGRAPHIC_ANNULUS_AUDIT");
                            if let Err(error) = self.audit_geographic_annulus(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Geographic Annulus audit failed: {error:#}"));
                            }
                            event_loop.exit();
                            return;
                        }
                        if let Some(path) = std::env::var_os("FERRITE_S102_APPEND_AUDIT") {
                            std::env::remove_var("FERRITE_S102_APPEND_AUDIT");
                            if let Err(error) = self.audit_s102_append_reuse(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("S102 append audit failed: {error:#}"));
                            }
                            event_loop.exit();
                            return;
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_S102_PUBLICATION_AUDIT")
                        {
                            std::env::remove_var("FERRITE_ROOT_S102_PUBLICATION_AUDIT");
                            if let Err(error) =
                                self.audit_root_bathymetry_publication(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Bathymetry publication audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_PUBLICATION_AUDIT") {
                            if let Err(error) = self.audit_root_publication(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("Publication audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }

                        if let Some(path) = std::env::var_os("FERRITE_ROOT_GIS_PROGRAM_AUDIT") {
                            std::env::remove_var("FERRITE_ROOT_GIS_PROGRAM_AUDIT");
                            if let Err(error) = self.audit_gis_program_reuse(&PathBuf::from(path)) {
                                self.startup_error =
                                    Some(format!("GIS program audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_RASTER_GROUPS_AUDIT") {
                            std::env::remove_var("FERRITE_ROOT_RASTER_GROUPS_AUDIT");
                            if let Err(error) =
                                self.audit_raster_viewing_groups(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Raster viewing group audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_OBJECT_MODE_AUDIT") {
                            std::env::remove_var("FERRITE_ROOT_OBJECT_MODE_AUDIT");
                            if let Err(error) =
                                self.audit_object_selection_mode(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Object selection mode audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_DECODED_CACHE_AUDIT") {
                            std::env::remove_var("FERRITE_ROOT_DECODED_CACHE_AUDIT");
                            if let Err(error) =
                                self.audit_decoded_chart_reopen(&PathBuf::from(path))
                            {
                                self.startup_error =
                                    Some(format!("Decoded cache audit failed: {error:#}"));
                                event_loop.exit();
                                return;
                            }
                        }
                        if let Some(path) = std::env::var_os("FERRITE_ROOT_UI_PRESENTATION_AUDIT") {
                            std::env::remove_var("FERRITE_ROOT_UI_PRESENTATION_AUDIT");
                            if ferrite_wgpu::background_test::enabled() {
                                self.build_rendered_symbols();
                                let selected = self
                                    .rendered_symbols
                                    .first()
                                    .map(|symbol| self.describe_hit(symbol));
                                self.select_feature(selected);
                                if let Some(renderer) = &mut self.renderer {
                                    renderer.ui_state.show_logs = true;
                                    renderer.ui_state.debug_mode = false;
                                    // Allow UI layout and its opening transition to settle before visual QA.
                                    for _ in 0..12 {
                                        if let Err(error) = renderer.render() {
                                            self.startup_error = Some(format!(
                                                "UI presentation settle failed: {error:#}"
                                            ));
                                            event_loop.exit();
                                            return;
                                        }
                                        std::thread::sleep(std::time::Duration::from_millis(30));
                                    }
                                    if let Err(error) = renderer.render().and_then(|_| {
                                        renderer.save_screenshot_with_ui(PathBuf::from(&path))
                                    }) {
                                        self.startup_error = Some(format!(
                                            "UI presentation audit failed: {error:#}"
                                        ));
                                        event_loop.exit();
                                        return;
                                    }
                                    renderer.ui_state.show_logs = false;
                                    if let Err(error) = renderer.render().and_then(|_| {
                                        renderer.save_screenshot_with_ui(
                                            PathBuf::from(&path).with_extension("object.png"),
                                        )
                                    }) {
                                        self.startup_error = Some(format!(
                                            "Object details presentation audit failed: {error:#}"
                                        ));
                                        event_loop.exit();
                                        return;
                                    }
                                }
                            }
                        }
                        if ferrite_wgpu::background_test::enabled() {
                            if let Some(path) = std::env::var_os("FERRITE_POINT_RETENTION_AUDIT") {
                                std::env::remove_var("FERRITE_POINT_RETENTION_AUDIT");
                                if let Err(error) = self.audit_point_retention(&PathBuf::from(path))
                                {
                                    self.startup_error =
                                        Some(format!("Point retention audit failed: {error:#}"));
                                    event_loop.exit();
                                    return;
                                }
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
                        if ferrite_wgpu::background_test::enabled() {
                            if let Some(path) = std::env::var_os("FERRITE_NATIVE_S421_EDITOR_AUDIT")
                            {
                                std::env::remove_var("FERRITE_NATIVE_S421_EDITOR_AUDIT");
                                if let Err(error) =
                                    self.audit_native_s421_editor(&PathBuf::from(path))
                                {
                                    self.startup_error =
                                        Some(format!("S-421 editor audit failed: {error:#}"));
                                    event_loop.exit();
                                    return;
                                }
                            }
                        }
                        if ferrite_wgpu::background_test::enabled() {
                            if let Some(path) =
                                std::env::var_os("FERRITE_NATIVE_S421_LIFECYCLE_AUDIT")
                            {
                                std::env::remove_var("FERRITE_NATIVE_S421_LIFECYCLE_AUDIT");
                                if let Err(error) =
                                    self.audit_native_s421_lifecycle(&PathBuf::from(path))
                                {
                                    self.startup_error = Some(format!(
                                        "Native S-421 lifecycle audit failed: {error:#}"
                                    ));
                                    event_loop.exit();
                                    return;
                                }
                            }
                        }
                        if ferrite_wgpu::background_test::enabled() {
                            if let Some(path) = std::env::var_os("FERRITE_NATIVE_S421_AUDIT") {
                                std::env::remove_var("FERRITE_NATIVE_S421_AUDIT");
                                if let Some(renderer) = &self.renderer {
                                    let _ = renderer.audit_native_route_cpu_inputs(
                                        &PathBuf::from(&path).with_extension("gpu"),
                                    );
                                }
                                let value = serde_json::json!({
                                    "native": self.native_s421.audit_data(),
                                    "native_gpu_encoded": self.renderer.as_ref().is_some_and(|r|r.native_route_encoded_in_last_pass()),
                                    "external_plugins": self.plugin_system.get_loaded_plugins().len(),
                                    "charts": self.cells.len(),
                                    "background_test": true,
                                    "native_window_visible": self.window.as_ref().and_then(|w| w.is_visible()),
                                    "native_window_has_focus": self.window.as_ref().is_some_and(|w| w.has_focus()),
                                    "require_signatures": self.require_signatures,
                                });
                                if let Err(error) = std::fs::write(
                                    PathBuf::from(path),
                                    serde_json::to_vec_pretty(&value).expect("native audit"),
                                ) {
                                    self.startup_error =
                                        Some(format!("Native S-421 audit failed: {error}"));
                                    event_loop.exit();
                                    return;
                                }
                            }
                        }
                        if let Some(path) = self.auto_screenshot.take() {
                            info!("Auto-screenshot: saving to {}", path.display());
                            if let Some(renderer) = &mut self.renderer {
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
                if let Err(e) = self.end_flat_eventloop_diagnostic_frame() {
                    self.startup_error =
                        Some(format!("Eventloop diagnostic completion failed: {e:#}"));
                    event_loop.exit();
                    return;
                }
                let needs_redraw = {
                    let has_inertia =
                        self.pan_velocity.0.abs() > 0.00001 || self.pan_velocity.1.abs() > 0.00001;
                    let has_loading = self.loading_state.is_some()
                        || !self.pending_auto_s102.is_empty()
                        || self.dataset_open_discovery.is_some()
                        || self.dataset_open_session.is_some();
                    let has_screenshot_pending = self.frames_since_loaded.is_some();
                    let has_zoom_pending = self.zoom_rebuild_phase > 0;
                    let egui_needs = self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.egui_needs_repaint());
                    let has_pan_rebuild = self.pan_rebuild_phase > 0;
                    self.flat_eventloop_audit.is_some()
                        || self.is_dragging
                        || self.zoom_animating
                        || has_inertia
                        || has_loading
                        || has_screenshot_pending
                        || has_zoom_pending
                        || has_pan_rebuild
                        || self.pending_portrayal_change.is_some()
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

                    self.pan_camera_by([world_dx, world_dy]);

                    // Track recent positions for velocity calculation (keep last 100ms worth)

                    self.recent_positions.push((new_pos, now));

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
                    // Navigation keeps the CPU scaler current; only the UI-thread
                    // fallback must rebuild before the anchor is read.
                    if !self.background_scene_builds {
                        self.update_view();
                    }
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
                            // The release settles the scene; with background
                            // builds the preview continues under the new drag.
                            if !self.background_scene_builds {
                                if let Some(renderer) = &mut self.renderer {
                                    renderer.reset_pan_offset();
                                }
                                self.update_view();
                            }
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
                if self.native_s421.editing()
                    && self.chart_contains(self.mouse_pos)
                    && !self
                        .renderer
                        .as_ref()
                        .is_some_and(|r| r.egui_wants_pointer())
                {
                    self.native_s421.stop_local_edit();
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return;
                }
                if let Err(error) = self.ensure_navigation_scene() {
                    if let Some(r) = self.renderer.as_mut() {
                        r.ui_state.notice = Some(format!("Plugin click rejected: {error:#}"));
                    }
                    if self.auto_screenshot.is_some() || self.flat_eventloop_audit.is_some() {
                        self.startup_error =
                            Some(format!("Plugin click navigation barrier failed: {error:#}"));
                        event_loop.exit();
                    }
                    return;
                }
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
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(pending) = self.coverage_lifecycle_resize.as_mut() {
            let now = std::time::Instant::now();
            if now >= pending.next_poll {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                pending.next_poll = now + std::time::Duration::from_millis(100);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(pending.next_poll));
            return;
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
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(s102_quality_worker::FLAG))
    {
        if let Err(error) = s102_quality_worker::run_child() {
            eprintln!("S-102 quality decoder: {error:#}");
            std::process::exit(2);
        }
        return;
    }
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
    let history_path = s102_journal_admission::history_path(std::env::args().skip(1))?;
    let s102_journal_gate = history_path
        .as_deref()
        .map(s102_journal_admission::Gate::open_existing)
        .transpose()?;
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

    // Warnings/errors remain available in the UI with DebugOFF; disk/console
    // diagnostics retain their existing Debug-only policy.
    init_logging(&config.log_path, config.debug_mode)?;

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
    let symbol_cache = SymbolCache::new_with_pattern_contract(
        &symbols_path,
        pc.sources(),
        ferrite_s101::shallow_pattern_contract(&pc),
    );
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

    app.pending_auto_s421 = config.auto_s421;
    app.applied_settings.display_mode = initial_display_mode;
    app.applied_settings.viewing_layers = config.initial_viewing_layers.into_iter().collect();
    primary_viewing_layers::capture_primary(&app.pc, &mut app.applied_settings)?;
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
    app.operational_mode = config.operational_mode;
    app.pending_auto_s102 = config.auto_s102;
    app.s102_pc_path = config.s102_pc_path;
    app.initialize_product_catalogues();
    app.catalogue_inventory = config.catalogue_inventory;
    app.s102_adjustments_path = config.s102_adjustments_path;
    app.s102_journal_gate = s102_journal_gate;
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
    pc: &BoundPortrayalCatalogue,
    settings: Option<&SettingsState>,
) -> Result<LuaContextParameters> {
    if let Some(settings) = settings {
        primary_viewing_layers::resolve(pc, settings)?;
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
    try_lua_portrayal_for_cells(
        cells.iter().enumerate(),
        fc,
        pc,
        render_context,
        profile_name,
        settings,
    )
}

/// A catalogue owner may process a subset of the scene without renumbering
/// source cells. Original global indices keep coverage, picking and dependencies
/// bound to the same chart when several owner engines feed one display list.
fn try_lua_portrayal_for_cells<'a>(
    cells: impl IntoIterator<Item = (usize, &'a S101Cell)>,
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
    for (cell_index, cell) in cells {
        debug!(
            "Processing cell {}: {}",
            cell_index,
            cell.file_path.display()
        );

        // Build the owned host input directly; fresh private context metadata
        // was immediately discarded here and cannot affect Lua initialization.
        let cell_data = ferrite_lua::CellData::from_cell(cell);

        let results = engine
            .process_owned_cell(cell_data, context.clone())
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
    let icon_path = get_app_base_dir().join("icon.ico");

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
fn init_logging(log_path: &Path, debug_mode: bool) -> Result<()> {
    if !debug_mode {
        tracing_subscriber::registry()
            .with(diagnostics_tracing::DiagnosticLayer::default())
            .init();
        return Ok(());
    }
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
        .with(diagnostics_tracing::DiagnosticLayer::default())
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
            format!(
                "<S100_FC_FeatureCatalogue><name>{name}</name><versionNumber>2.0</versionNumber><productId>S-101</productId></S100_FC_FeatureCatalogue>"
            )
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
    fn cache_rejects_forged_sequence_count_before_deserialize() {
        let file = ChartApp::wrap_cache(&u64::MAX.to_le_bytes());
        assert_eq!(
            ChartApp::verify_and_deserialize_cache(&file).unwrap_err(),
            "cache exceeds instruction budget"
        );
        let mut payload = bincode::serialize(&Vec::<DrawingInstruction>::new()).unwrap();
        payload.push(0);
        assert!(
            ChartApp::verify_and_deserialize_cache(&ChartApp::wrap_cache(&payload))
                .unwrap_err()
                .contains("deserialization failed")
        );
    }

    #[test]
    fn schema42_rejects_geometry_omission_cache_before_hash_or_decode() {
        let mut wrapped = ChartApp::wrap_cache(&[]);
        wrapped[4..8].copy_from_slice(&41u32.to_le_bytes());
        wrapped[8] ^= 1;
        assert!(ChartApp::verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=41, expected=42"));
    }

    #[test]
    fn schema42_retains_geographic_annulus_and_rejects_incomplete_old_portrayal_cache() {
        let mut line = ferrite_render::LineInstruction::new(vec![
            WorldPoint::new(179.9, 50.),
            WorldPoint::new(179.9, 50.),
        ]);
        line.portrayal_path = Some(ferrite_render::PortrayalPath::GeographicAnnulus {
            center: (179.9, 50.),
            outer: 50_000.,
            inner: 25_000.,
            start: 35.,
            sweep: -270.,
        });
        let payload = bincode::serialize(&vec![DrawingInstruction::Line(line)]).unwrap();
        let wrapped = ChartApp::wrap_cache(&payload);
        let restored = ChartApp::verify_and_deserialize_cache(&wrapped).unwrap();
        assert_eq!(bincode::serialize(&restored).unwrap(), payload);
        let mut old = wrapped;
        old[4..8].copy_from_slice(&40u32.to_le_bytes());
        assert!(ChartApp::verify_and_deserialize_cache(&old)
            .unwrap_err()
            .contains("file=40, expected=42"));
    }
    #[test]
    fn schema42_rejects_prior_unscoped_optional_policy_cache_before_decode() {
        let mut wrapped =
            ChartApp::wrap_cache(&bincode::serialize(&Vec::<DrawingInstruction>::new()).unwrap());
        wrapped[4..8].copy_from_slice(&39u32.to_le_bytes());
        assert!(ChartApp::verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=39, expected=42"));
    }

    #[test]
    fn schema42_keeps_full_text_font_style_and_rejects_schema38_before_decode() {
        let mut text =
            ferrite_render::TextInstruction::new("depth".into(), WorldPoint::new(1., 2.));
        text.font_style = ferrite_render::TextFontStyle {
            weight: ferrite_render::TextFontWeight::Light,
            proportion: ferrite_render::TextFontProportion::MonoSpaced,
            serifs: true,
            underline: true,
            strikethrough: true,
            upperline: true,
            reference: Some("Font-A".into()),
        };
        let style = text.font_style.clone();
        let mut wrapped = ChartApp::wrap_cache(
            &bincode::serialize(&vec![DrawingInstruction::Text(text)]).unwrap(),
        );
        let restored = ChartApp::verify_and_deserialize_cache(&wrapped).unwrap();
        let DrawingInstruction::Text(text) = &restored[0] else {
            panic!()
        };
        assert_eq!(text.font_style, style);
        wrapped[4..8].copy_from_slice(&38u32.to_le_bytes());
        assert!(ChartApp::verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("schema version mismatch: file=38, expected=42"));
    }
    #[test]
    fn schema42_retains_authored_fill_reference_and_rejects_37() {
        let mut area = AreaInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 0.),
            WorldPoint::new(0., 1.),
        ])
        .with_pattern_fill("DIAMOND1P".into(), (22.5, 0.), (0., 43.13));
        area.fill_ref = Some("DIAMOND1".into());
        let mut wrapped = ChartApp::wrap_cache(
            &bincode::serialize(&vec![DrawingInstruction::Area(area)]).unwrap(),
        );
        let decoded = ChartApp::verify_and_deserialize_cache(&wrapped).unwrap();
        let DrawingInstruction::Area(area) = &decoded[0] else {
            panic!("not area")
        };
        assert_eq!(area.fill_ref.as_deref(), Some("DIAMOND1"));
        wrapped[4..8].copy_from_slice(&37u32.to_le_bytes());
        assert!(ChartApp::verify_and_deserialize_cache(&wrapped)
            .unwrap_err()
            .contains("file=37, expected=42"));
    }

    #[test]
    fn schema42_round_trip_preserves_both_clip_modes_and_source() {
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
        assert_eq!(&file[4..8], &ChartApp::CACHE_SCHEMA_VERSION.to_le_bytes());
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
            "schema version mismatch: file=36, expected=42"
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
            "schema version mismatch: file=36, expected=42"
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
mod coverage_resize_event_tests {
    use super::CoverageLifecycleResize;
    use winit::dpi::PhysicalSize;
    fn state() -> CoverageLifecycleResize {
        let now = std::time::Instant::now();
        CoverageLifecycleResize {
            output: std::path::PathBuf::new(),
            original: PhysicalSize::new(1280, 950),
            requested: PhysicalSize::new(960, 712),
            restore: false,
            observed: None,
            started: now,
            next_poll: now,
        }
    }
    #[test]
    fn requested_extent_without_resized_event_is_not_a_proof() {
        let s = state();
        assert!(!s.ready(s.requested));
    }
    #[test]
    fn resized_and_actual_must_match_and_really_change_viewport() {
        let mut s = state();
        s.observed = Some(s.original);
        assert!(!s.ready(s.original));
        s.observed = Some(s.requested);
        assert!(!s.ready(s.original));
        assert!(s.ready(s.requested));
        s.observed = Some(PhysicalSize::new(0, 0));
        assert!(!s.ready(PhysicalSize::new(0, 0)));
    }
    #[test]
    fn restoration_requires_its_own_native_event_and_original_extent() {
        let mut s = state();
        s.restore = true;
        s.observed = Some(s.requested);
        assert!(!s.ready(s.requested));
        s.observed = Some(s.original);
        assert!(s.ready(s.original));
    }
}

#[cfg(test)]
mod portrayal_request_tests {
    use super::*;
    #[test]
    fn deferred_requests_keep_latest_colour_and_latest_settings_together() {
        let applied = SettingsState::default();
        let first = coalesce_portrayal_request(None, "Day", &applied, Some("Dusk".into()), None);
        let mut changed = applied.clone();
        changed.safety_contour = 23.;
        let second =
            coalesce_portrayal_request(Some(first), "Day", &applied, None, Some(changed.clone()));
        let third =
            coalesce_portrayal_request(Some(second), "Day", &applied, Some("Night".into()), None);
        assert_eq!(third.profile, "Night");
        assert_eq!(third.settings, changed);
        assert_eq!(applied, SettingsState::default());
    }
    #[test]
    fn requests_wait_until_all_navigation_and_loading_phases_finish() {
        assert!(!portrayal_navigation_pending(
            false,
            false,
            false,
            false,
            0,
            0,
            (0., 0.)
        ));
        for flags in [
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            assert!(portrayal_navigation_pending(
                flags.0,
                flags.1,
                flags.2,
                flags.3,
                0,
                0,
                (0., 0.)
            ));
        }
        assert!(portrayal_navigation_pending(
            false,
            false,
            false,
            false,
            1,
            0,
            (0., 0.)
        ));
        assert!(portrayal_navigation_pending(
            false,
            false,
            false,
            false,
            0,
            1,
            (0., 0.)
        ));
        assert!(portrayal_navigation_pending(
            false,
            false,
            false,
            false,
            0,
            0,
            (0.001, 0.)
        ));
    }
}

#[cfg(test)]
mod static_line_relation_policy_tests {
    #[test]
    fn default_enables_exact_context_comparison_and_explicit_controls_override() {
        assert!(super::ChartApp::static_line_relation_reuse_enabled(None));
        assert!(super::ChartApp::static_line_relation_reuse_enabled(Some(
            "1"
        )));
        for value in ["0", "", "true", "unknown"] {
            assert!(!super::ChartApp::static_line_relation_reuse_enabled(Some(
                value
            )));
        }
    }
}

#[cfg(test)]
mod candidate_catalogue_material_tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            // Concurrent tests must never share a directory even when the OS
            // clock returns the same timestamp to two fixture constructors.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "s101-candidate-material-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(root.join("Symbols")).unwrap();
            std::fs::write(root.join("Symbols/A.svg"),"<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'><rect width='10' height='10' fill='red'/></svg>").unwrap();
            Self { root }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn context(symbol: &str) -> RenderContext {
        let mut context = RenderContext::new(Viewport::new(100., 100.));
        context.add_instruction(DrawingInstruction::Point(
            ferrite_render::PointInstruction::new(symbol.into(), WorldPoint::new(0., 0.)),
        ));
        context
    }
    #[test]
    fn candidate_missing_svg_and_gpu_limit_leave_live_cache_revision_and_payload_unchanged() {
        let f = Fixture::new();
        let profile = ferrite_portrayal_catalog::ColorProfile::new("Day".into(), "Day".into());
        let mut live = SymbolCache::new(f.root.join("Symbols"));
        assert!(live.get_symbol("A", &profile).is_some());
        let old_revision = live.resource_revision();
        let old_len = live.len();
        let mut private = live.fork_empty();
        assert!(
            preflight_candidate_symbols(&context("MISSING"), &mut private, &profile, 4096).is_err()
        );
        assert!(preflight_candidate_symbols(&context("A"), &mut private, &profile, 1).is_err());
        drop(private);
        assert_eq!(live.resource_revision(), old_revision);
        assert_eq!(live.len(), old_len);
        assert!(live.get_symbol("A", &profile).is_some());
    }
    #[test]
    fn successful_private_materials_keep_live_cache_until_commit_owner_move() {
        let f = Fixture::new();
        let profile = ferrite_portrayal_catalog::ColorProfile::new("Day".into(), "Day".into());
        let live = SymbolCache::new(f.root.join("Symbols"));
        let old_revision = live.resource_revision();
        let mut private = live.fork_empty();
        preflight_candidate_symbols(&context("A"), &mut private, &profile, 4096).unwrap();
        assert_eq!(live.len(), 0);
        assert_eq!(live.resource_revision(), old_revision);
        assert_eq!(private.len(), 1);
        let installed = private;
        assert_eq!(installed.len(), 1);
        assert_ne!(installed.resource_revision(), old_revision);
    }
}

#[cfg(test)]
mod manual_catalogue_normalization_tests {
    use super::*;
    fn cell() -> S101Cell {
        S101Cell {
            file_path: PathBuf::from("original.000"),
            dsid: Default::default(),
            code_mappings: Default::default(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 1.,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: Default::default(),
            multi_points: Default::default(),
            curves: Default::default(),
            composite_curves: Default::default(),
            surfaces: Default::default(),
            features: Default::default(),
            information: Default::default(),
            spatial_information_associations: Default::default(),
        }
    }
    #[test]
    fn normalization_rollback_restores_both_mapping_directions_and_source() {
        let mut cell = cell();
        cell.code_mappings
            .feature_types
            .insert(7, "BuoyCardinal".into());
        // Keep an unrelated reverse entry: normalization is not a canonical-map rebuild.
        cell.code_mappings
            .feature_types
            .str_to_num
            .insert("OldAlias".into(), 9);
        let original = CatalogueNormalization::capture(&cell);
        cell.normalize_feature_codes(&["CardinalBuoy".into()]);
        assert_eq!(
            cell.code_mappings.feature_types.get_string(7).unwrap(),
            "CardinalBuoy"
        );
        original.restore(&mut cell);
        assert_eq!(
            cell.code_mappings.feature_types.get_string(7).unwrap(),
            "BuoyCardinal"
        );
        assert_eq!(
            cell.code_mappings.feature_types.get_numeric("OldAlias"),
            Some(9)
        );
        assert_eq!(cell.file_path, PathBuf::from("original.000"));
    }
    #[test]
    fn repeated_failed_preparations_restore_original_mapping_exactly() {
        let mut cell = cell();
        cell.code_mappings
            .feature_types
            .insert(7, "buoycardinal".into());
        let before = cell.code_mappings.feature_types.clone();
        for _ in 0..2 {
            let original = CatalogueNormalization::capture(&cell);
            cell.normalize_feature_codes(&["BuoyCardinal".into()]);
            original.restore(&mut cell);
            assert_eq!(
                cell.code_mappings.feature_types.num_to_str,
                before.num_to_str
            );
            assert_eq!(
                cell.code_mappings.feature_types.str_to_num,
                before.str_to_num
            );
        }
    }
    #[test]
    fn rollback_restores_none_and_alias_feature_codes_without_replacing_graph() {
        let mut cell = cell();
        for (id, code) in [(1, Some("BuoyCardinal")), (2, None)] {
            cell.features.insert(
                id,
                ferrite_s100_core::FeatureRecord {
                    frid: ferrite_s100_core::FRID {
                        rcid: id as u32,
                        nftc: 7,
                        rver: 3,
                        ruin: 1,
                    },
                    foid: None,
                    attributes: Vec::new(),
                    spatial_associations: Vec::new(),
                    information_associations: Vec::new(),
                    feature_associations: Vec::new(),
                    masks: Vec::new(),
                    feature_code: code.map(str::to_owned),
                    primitive_type: ferrite_s100_core::SpatialPrimitiveType::NoGeometry,
                },
            );
        }
        let original = CatalogueNormalization::capture(&cell);
        cell.normalize_feature_codes(&["CardinalBuoy".into()]);
        assert_eq!(
            cell.features[&1].feature_code.as_deref(),
            Some("CardinalBuoy")
        );
        original.restore(&mut cell);
        assert_eq!(
            cell.features[&1].feature_code.as_deref(),
            Some("BuoyCardinal")
        );
        assert_eq!(cell.features[&2].feature_code, None);
        assert_eq!(cell.features[&1].frid.rver, 3);
        assert_eq!(cell.features.len(), 2);
    }
}

#[cfg(test)]
mod dataset_open_session_tests {
    use super::*;
    #[test]
    fn publication_waits_for_a_noncollapsed_physical_chart_rectangle() {
        assert!(chart_viewport_ready((250., 44., 1670., 940.)));
        for rect in [
            (0., 0., 0., 0.),
            (0., 0., 1., 0.),
            (0., 0., -1., 1.),
            (f32::NAN, 0., 1., 1.),
            (0., 0., f32::INFINITY, 1.),
            (1e30, 0., 1., 1.),
        ] {
            assert!(!chart_viewport_ready(rect));
        }
    }
    #[test]
    fn failed_folder_chain_does_not_block_other_complete_datasets_but_file_batch_is_atomic() {
        assert!(chart_batch_publishable(true, true, 2));
        assert!(!chart_batch_publishable(false, true, 2));
        assert!(!chart_batch_publishable(true, true, 0));
        assert!(chart_batch_publishable(false, false, 1));
        let session = DatasetOpenSession {
            chart_started: true,
            chart_failed: true,
            chart_partial: true,
            ..Default::default()
        };
        assert!(session
            .summary(2)
            .contains("completed with failed datasets"));
    }
    #[test]
    fn partial_success_summary_exposes_failure_and_caps_display_without_losing_log_details() {
        let session = DatasetOpenSession {
            raster_loaded: 2,
            raster_existing: 1,
            raster_failed: 1,
            chart_started: true,
            chart_failed: true,
            notices: (0..40).map(|n| format!("failure-{n}")).collect(),
            ..Default::default()
        };
        let text = session.summary(17);
        assert!(text.contains("failed; previous charts retained"));
        assert!(text.contains("2 loaded, 1 already open, 1 failed"));
        assert!(text.contains("40 skipped or failed entries"));
        assert!(!text.contains("failure-0"));
        assert!(!text.contains('\n'));
        assert!(text.contains("See Logs"));
        assert_eq!(session.notices.len(), 40);
        assert_eq!(session.notices.last().unwrap(), "failure-39");
    }
}

impl flat_event_coalescing::Runtime for ChartApp {
    fn scene_pending(&self) -> &flat_event_coalescing::Pending {
        &self.navigation_scene_pending
    }
    fn scene_pending_mut(&mut self) -> &mut flat_event_coalescing::Pending {
        &mut self.navigation_scene_pending
    }
    fn begin_scene_flush(&mut self) {
        if let Some(a) = self.navigation_burst_audit.as_mut() {
            a.counts.flush_attempts += 1;
        }
        self.navigation_flush_active = true;
    }
    fn rebuild_scene(&mut self, request: flat_event_coalescing::Request) {
        self.update_view_ex(request.hit_test, request.preserve_declutter);
    }
    fn end_scene_flush(&mut self) {
        self.navigation_flush_active = false;
        if self.navigation_scene_pending.dirty() {
            if let Some(a) = self.navigation_burst_audit.as_mut() {
                a.counts.failed_flushes += 1;
            }
        }
    }
}

#[cfg(test)]
mod navigation_scene_runtime_controls {
    use super::flat_event_coalescing::{self, Pending, Request, Runtime};
    use std::collections::VecDeque;

    // App's production dispatcher with injected readiness outcomes. No GPU,
    // actual plugin library, physical OS event, or native failure claim.
    struct InjectedAppRuntime {
        pending: Pending,
        outcomes: VecDeque<bool>,
        calls: Vec<Request>,
        active: bool,
        flushes: usize,
        failures: usize,
    }
    impl InjectedAppRuntime {
        fn new(outcomes: impl IntoIterator<Item = bool>) -> Self {
            Self {
                pending: Pending::new(Some("1".as_ref())),
                outcomes: outcomes.into_iter().collect(),
                calls: Vec::new(),
                active: false,
                flushes: 0,
                failures: 0,
            }
        }
        // Identical real App PluginUI predicate and pre-render dispatcher.
        fn ordinary_redraw(&mut self, loaded: bool, queued: bool) -> bool {
            if !flat_event_coalescing::plugin_ui_runtime(self, loaded, queued) {
                return false;
            }
            flat_event_coalescing::flush_runtime(self)
        }
    }
    impl Runtime for InjectedAppRuntime {
        fn scene_pending(&self) -> &Pending {
            &self.pending
        }
        fn scene_pending_mut(&mut self) -> &mut Pending {
            &mut self.pending
        }
        fn begin_scene_flush(&mut self) {
            assert!(!self.active);
            self.active = true;
            self.flushes += 1;
        }
        fn rebuild_scene(&mut self, request: Request) {
            assert!(self.active);
            self.calls.push(request);
            let ready = self.outcomes.pop_front().expect("Unexpected heavy retry");
            self.pending.finish(ready);
        }
        fn end_scene_flush(&mut self) {
            assert!(self.active);
            self.active = false;
            if self.pending.dirty() {
                self.failures += 1;
            }
        }
    }
    #[test]
    fn injected_failed_flush_500_plain_redraws_never_retry_or_render() {
        let mut app = InjectedAppRuntime::new([false]);
        app.pending.request(true, false);
        assert!(!flat_event_coalescing::flush_runtime(&mut app));
        for _ in 0..500 {
            assert!(!app.ordinary_redraw(false, false));
        }
        assert_eq!((app.flushes, app.calls.len(), app.failures), (1, 1, 1));
        assert!(!app.active);
        assert!(app.pending.dirty());
        assert_eq!(app.pending.runnable(), None);
        assert!(app.outcomes.is_empty());
    }
    #[test]
    fn chooser_explicit_consumer_retry_fail_closed_then_authoritative_ready() {
        let mut app = InjectedAppRuntime::new([false, false, true]);
        app.pending.request(true, false);
        assert!(!flat_event_coalescing::flush_runtime(&mut app));
        assert!(!flat_event_coalescing::ensure_runtime(&mut app));
        assert!(app.pending.dirty());
        for _ in 0..500 {
            assert!(!app.ordinary_redraw(false, false));
        }
        assert_eq!(app.calls.len(), 2);
        assert!(flat_event_coalescing::ensure_runtime(&mut app));
        assert_eq!(app.calls.len(), 3);
        assert!(!app.pending.dirty());
        assert!(flat_event_coalescing::flush_runtime(&mut app));
        assert_eq!(app.calls.len(), 3);
    }
    #[test]
    fn loaded_plugin_falls_back_and_never_admits_deferred_route() {
        let enabled = Pending::new(Some("1".as_ref()));
        let disabled = Pending::new(Some("0".as_ref()));
        assert!(enabled.may_defer(false));
        assert!(!enabled.may_defer(true));
        assert!(!disabled.may_defer(false));
        assert!(!disabled.may_defer(true));
        // Defensive transition of existing pending state: an actual loaded
        // plugin consumer must obtain readiness before callbacks can proceed.
        let mut app = InjectedAppRuntime::new([false, true]);
        app.pending.request(true, false);
        assert!(!flat_event_coalescing::flush_runtime(&mut app));
        assert!(app.ordinary_redraw(true, false));
        assert_eq!(app.calls.len(), 2);
        assert!(!app.pending.dirty());
    }
    #[test]
    fn queued_plugin_ui_retries_without_consuming_on_failed_barrier() {
        let mut app = InjectedAppRuntime::new([false, false, true]);
        app.pending.request(false, true);
        assert!(!flat_event_coalescing::flush_runtime(&mut app));
        assert!(!app.ordinary_redraw(false, true));
        assert!(app.pending.dirty());
        assert!(app.ordinary_redraw(false, true));
        assert!(!app.pending.dirty());
        assert_eq!(app.calls.len(), 3);
    }
    #[test]
    fn new_motion_retries_union_of_flags_exactly_once() {
        let mut app = InjectedAppRuntime::new([false, true]);
        app.pending.request(true, false);
        assert!(!flat_event_coalescing::flush_runtime(&mut app));
        app.pending.request(false, true);
        assert!(app.ordinary_redraw(false, false));
        assert_eq!(
            app.calls,
            vec![
                Request {
                    hit_test: true,
                    preserve_declutter: false
                };
                2
            ]
        );
        assert!(!app.pending.dirty());
        assert_eq!(app.flushes, 2);
    }
    #[test]
    fn dataset_open_and_unload_barriers_do_not_mutate_after_failed_flush() {
        for operation in ["single-file", "recursive-folder", "unload", "publication"] {
            let mut app = InjectedAppRuntime::new([false, false, true]);
            app.pending.request(true, false);
            assert!(!flat_event_coalescing::flush_runtime(&mut app));
            let mut source_mutations = Vec::new();
            if flat_event_coalescing::ensure_runtime(&mut app) {
                source_mutations.push(operation);
            }
            assert!(source_mutations.is_empty());
            for _ in 0..500 {
                assert!(!app.ordinary_redraw(false, false));
            }
            assert_eq!(app.calls.len(), 2);
            if flat_event_coalescing::ensure_runtime(&mut app) {
                source_mutations.push(operation);
            }
            assert_eq!(source_mutations, [operation]);
            assert_eq!(app.calls.len(), 3);
            assert!(!app.pending.dirty());
        }
    }
    #[test]
    fn stopped_navigation_flushes_final_requirements_once_then_idle_is_cheap() {
        let mut app = InjectedAppRuntime::new([true]);
        for _ in 0..8 {
            app.pending.request(false, true);
        }
        // End-of-gesture requires an authoritative hit-test and declutter rebuild.
        app.pending.request(true, false);
        assert!(app.ordinary_redraw(false, false));
        for _ in 0..500 {
            assert!(app.ordinary_redraw(false, false));
        }
        assert_eq!(
            app.calls,
            [Request {
                hit_test: true,
                preserve_declutter: false
            }]
        );
        assert_eq!(app.flushes, 1);
        assert!(app.outcomes.is_empty());
    }
}

#[cfg(test)]
mod installed_product_catalogue_tests {
    use super::*;

    #[test]
    fn s102_catalogues_are_loaded_before_datasets_and_reused_for_evaluation() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fc = Arc::new(load_feature_catalogue(&base.join("Catalogues/FC/S-101")).unwrap());
        let pc = Arc::new(load_portrayal_catalogue(&base.join("Catalogues/PC/S-101")).unwrap());
        let cache = SymbolCache::new_with_pattern_contract(
            &pc.root_path.join("Symbols"),
            pc.sources(),
            ferrite_s101::shallow_pattern_contract(&pc),
        );
        let mut app = ChartApp::new(
            cache,
            "Day".into(),
            fc,
            pc,
            CatalogueStatus::default(),
            CatalogueStatus::default(),
            false,
            vec![],
            None,
            false,
            None,
            None,
        );
        app.s102_pc_path = base.join("Catalogues/PC/S-102");
        app.initialize_product_catalogues();
        assert!(app.cells.is_empty() && app.bathymetry.is_empty());
        assert!(
            app.s102_fc_status.loaded,
            "{:?}",
            app.s102_fc_status.validation_message
        );
        assert!(
            app.s102_pc_status.loaded,
            "{:?}",
            app.s102_pc_status.validation_message
        );
        assert_eq!(app.s102_fc_status.product_id, "S-102");
        assert_eq!(app.s102_pc_status.product_id, "S-102");
        let held = app.s102_pc.as_ref().unwrap();
        let portrayal = BathymetryPortrayal::from_bound_catalogue(
            held.clone(),
            "Day",
            DepthSettings::default(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(
            held,
            portrayal.bound_evaluation().unwrap().catalogue()
        ));
        assert_eq!(app.s102_fc.as_ref().unwrap().version, "3.0.0");
    }
}
