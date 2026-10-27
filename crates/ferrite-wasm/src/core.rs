//! The host-neutral part of the web bindings: an in-memory [`ChartEngine`]
//! fed with bytes, with plain JSON results. Builds natively too (tests).
use anyhow::{bail, ensure, Context, Result};
use ferrite_cli::{engine_config, feature_json, inspection_json, issue_json, palettes};
use ferrite_engine::chart_engine::{
    Camera, ChartEngine, DatasetSource, FeatureId, OpenReport, View, Zoom,
};
use ferrite_engine::platform::{
    CacheEntry, CacheStore, Clock, MemoryCacheStore, MemoryDataSource, Platform, SequentialExecutor,
};
use ferrite_engine::scene::PreparedScene;
use ferrite_engine::{PortrayalSource, ViewportPixels};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

// In-memory paths are dataset and authorization keys and must be absolute:
// `/x` is absolute in the browser (Unix-style paths) but has no drive on
// Windows, where native builds (tests) use the virtual drive `C:`
// (`ferrite_engine::platform::memory_path`).
/// Where catalogue files live in the in-memory data source.
#[cfg(not(windows))]
pub const CATALOGUE_ROOT: &str = "/catalogues";
#[cfg(windows)]
pub const CATALOGUE_ROOT: &str = "C:/catalogues";
/// Where added dataset files are named (their identity only).
#[cfg(not(windows))]
pub const DATA_ROOT: &str = "/data";
#[cfg(windows)]
pub const DATA_ROOT: &str = "C:/data";

/// Magic of a catalogue bundle (see [`parse_bundle`]).
pub const BUNDLE_MAGIC: &[u8; 4] = b"FSB1";

/// Files of a bundle: `FSB1`, then per file a little-endian `u32` name
/// length, the UTF-8 relative name, a `u32` data length and the data
/// (written by `tools/build_wasm.py`).
pub fn parse_bundle(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    ensure!(
        bytes.len() >= 4 && &bytes[..4] == BUNDLE_MAGIC,
        "Not a catalogue bundle"
    );
    let mut rest = &bytes[4..];
    let mut take = |len: usize| -> Result<&[u8]> {
        ensure!(rest.len() >= len, "Truncated catalogue bundle");
        let (head, tail) = rest.split_at(len);
        rest = tail;
        Ok(head)
    };
    let mut files = Vec::new();
    while let Ok(len) = take(4) {
        let len = u32::from_le_bytes(len.try_into().expect("4 bytes")) as usize;
        let name = std::str::from_utf8(take(len)?)
            .context("Bundle name is not UTF-8")?
            .to_owned();
        let len = u32::from_le_bytes(take(4)?.try_into().expect("4 bytes")) as usize;
        files.push((name, take(len)?.to_vec()));
    }
    Ok(files)
}

/// A relative catalogue or dataset name without root, `..` or empty parts.
fn relative_name(name: &str) -> Result<PathBuf> {
    let name = name.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("Invalid file name {name}"),
            part => out.push(part),
        }
    }
    ensure!(!out.as_os_str().is_empty(), "Empty file name");
    Ok(out)
}

/// The page's cache store: entries in memory ([`MemoryCacheStore`]) that
/// also records which entries the engine wrote, so the page can persist
/// them in an asynchronous store (IndexedDB) after a preparation
/// ([`Self::take_writes`]) and load them back before the next page's
/// preparation ([`Self::preload`]). The engine's cache API stays
/// synchronous; persistence happens between engine calls.
#[derive(Debug, Default)]
pub struct PageCacheStore {
    memory: MemoryCacheStore,
    written: Mutex<BTreeSet<PathBuf>>,
    /// Entries read since [`Self::take_reads`] (cache hits).
    reads: std::sync::atomic::AtomicUsize,
}

impl PageCacheStore {
    /// The entry path of cache file `name` (`.ferrite_*`, no separators).
    pub fn entry(&self, name: &str) -> Result<PathBuf> {
        ensure!(
            name.starts_with(".ferrite_") && !name.contains(['/', '\\']) && !name.contains(".."),
            "Not a cache entry name: {name}"
        );
        Ok(self.memory.root().context("No memory cache")?.join(name))
    }

    /// Store an entry loaded from elsewhere (not reported as written).
    pub fn preload(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.preload_checked(name, bytes, false)
    }

    /// [`Self::preload`]; `verified`: the host checked the payload's
    /// SHA-256 against the entry's stored hash, so the engine's first read
    /// does not hash it again ([`MemoryCacheStore::write_verified`]).
    pub fn preload_checked(&self, name: &str, bytes: &[u8], verified: bool) -> Result<()> {
        let path = self.entry(name)?;
        if verified {
            self.memory.write_verified(&path, bytes)
        } else {
            self.memory.write(&path, bytes)
        }
        .with_context(|| format!("Cannot store {name}"))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entry(name)
            .is_ok_and(|path| self.memory.modified(&path).is_some())
    }

    /// Entries written by the engine since the last call (name and bytes;
    /// entries pruned meanwhile are skipped).
    pub fn take_writes(&self) -> Vec<(String, Vec<u8>)> {
        let written = std::mem::take(&mut *self.written.lock().expect("cache lock"));
        written
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_owned();
                Some((name, self.memory.read(&path).ok()?))
            })
            .collect()
    }

    /// Number of entries read by the engine since the last call.
    pub fn take_reads(&self) -> usize {
        self.reads.swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.memory.len()
    }

    pub fn is_empty(&self) -> bool {
        self.memory.is_empty()
    }
}

impl CacheStore for PageCacheStore {
    fn root(&self) -> Option<PathBuf> {
        self.memory.root()
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let bytes = self.memory.read(path)?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(bytes)
    }
    /// Remembers integrity checks (entries change only through writes).
    fn read_entry(&self, path: &Path) -> std::io::Result<CacheEntry> {
        let entry = self.memory.read_entry(path)?;
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(entry)
    }
    fn mark_verified(&self, path: &Path, version: u64) {
        self.memory.mark_verified(path, version)
    }
    fn modified(&self, path: &Path) -> Option<SystemTime> {
        self.memory.modified(path)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.memory.write(path, bytes)?;
        if let Ok(mut written) = self.written.lock() {
            written.insert(path.to_path_buf());
        }
        Ok(())
    }
    fn mark_used(&self, path: &Path) {
        self.memory.mark_used(path)
    }
    fn prune(&self) {
        self.memory.prune()
    }
}

/// The engine of a web page: catalogues and datasets from bytes, the
/// page's cache store (precomputed S-101 payloads, per-cell portrayal
/// caches), a sequential executor and the host's clock.
pub struct WebEngineCore {
    engine: ChartEngine,
    cache: Arc<PageCacheStore>,
    pending: Vec<(PathBuf, Arc<[u8]>)>,
    opened: OpenReport,
    scene: Option<PreparedScene>,
    /// The scene was prepared with the current datasets, display and
    /// portrayal choice, so view changes may re-project it.
    scene_current: bool,
    zoom: f64,
    size: [u32; 2],
    portrayal: PortrayalChoice,
    /// Precomputed payloads stored so far.
    payloads: usize,
    /// Where the S-101 instructions of the current scene came from.
    used_source: Option<PortrayalSource>,
    /// Payload and S-102 entry names already offered to the page by
    /// [`Self::missing_cache_entries`] (looked up once per page).
    offered: BTreeSet<String>,
}

/// Whether this build runs the portrayal catalogue's Lua rules (feature
/// `lua-wasm`; natively the engine's `lua-native`).
pub const LUA_AVAILABLE: bool = cfg!(feature = "lua-wasm");

/// How S-101 cells are portrayed on the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortrayalChoice {
    /// Lua when it is built and no precomputed payloads were loaded;
    /// otherwise payloads, falling back to Lua when one is missing.
    Auto,
    Lua,
    Precomputed,
}

impl PortrayalChoice {
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "auto" => Self::Auto,
            "lua" => Self::Lua,
            "precomputed" => Self::Precomputed,
            other => bail!("Unknown portrayal {other:?} (auto, lua, precomputed)"),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Lua => "lua",
            Self::Precomputed => "precomputed",
        }
    }
}

fn source_name(source: PortrayalSource) -> &'static str {
    match source {
        PortrayalSource::Lua => "lua",
        PortrayalSource::Precomputed => "precomputed",
    }
}

impl WebEngineCore {
    /// An engine over catalogue files named relative to the catalogue root
    /// (`FC/S-101/...`, `PC/S-101/...`, optionally S-124 and S-129).
    pub fn new(catalogue_files: Vec<(String, Vec<u8>)>, clock: Arc<dyn Clock>) -> Result<Self> {
        let mut data = MemoryDataSource::new();
        for (name, bytes) in catalogue_files {
            data.insert(Path::new(CATALOGUE_ROOT).join(relative_name(&name)?), bytes);
        }
        let cache = Arc::new(PageCacheStore::default());
        let platform = Platform::in_memory(data)
            .with_cache(Arc::clone(&cache) as _)
            .with_clock(clock)
            .with_executor(Arc::new(SequentialExecutor));
        let mut engine = ChartEngine::new(engine_config(platform, CATALOGUE_ROOT))?;
        // Chosen per preparation (`PortrayalChoice`); without Lua always payloads.
        engine.set_portrayal_source(PortrayalSource::Precomputed);
        // S-102: with Lua, the rule's instruction string goes through the
        // cache store, so a page with the stored entry skips Lua for S-102
        // (without Lua the entry is required anyway).
        #[cfg(feature = "s102-pure")]
        engine.set_s102_options(ferrite_engine::products::s102::S102Options {
            reuse_rule_instructions: LUA_AVAILABLE,
            ..Default::default()
        });
        Ok(Self {
            engine,
            cache,
            pending: Vec::new(),
            opened: OpenReport::default(),
            scene: None,
            scene_current: false,
            zoom: 1.0,
            size: [0, 0],
            portrayal: PortrayalChoice::Auto,
            payloads: 0,
            used_source: None,
            offered: BTreeSet::new(),
        })
    }

    pub fn engine(&self) -> &ChartEngine {
        &self.engine
    }

    #[cfg(all(test, feature = "lua-wasm", feature = "s102-pure"))]
    fn engine_mut(&mut self) -> &mut ChartEngine {
        &mut self.engine
    }

    /// Queue a dataset file for the next [`Self::open`]; `path` is its
    /// identity (file name, exchange-set layout).
    pub fn add_file(&mut self, path: &str, bytes: Vec<u8>) -> Result<()> {
        let path = Path::new(DATA_ROOT).join(relative_name(path)?);
        self.pending.retain(|(p, _)| *p != path);
        self.pending.push((path, Arc::from(bytes)));
        Ok(())
    }

    /// Number of queued dataset files.
    pub fn pending_files(&self) -> usize {
        self.pending.len()
    }

    /// Store a precomputed S-101 cell payload (`.ferrite_pre_<key>` written
    /// by `ferrite precompute`) in the memory cache store.
    pub fn load_precomputed(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.load_precomputed_checked(name, bytes, false)
    }

    fn load_precomputed_checked(&mut self, name: &str, bytes: &[u8], verified: bool) -> Result<()> {
        let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
        // Web servers often hide dot files; the leading dot is optional.
        let file = match file.strip_prefix('.') {
            Some(_) => file.to_owned(),
            None => format!(".{file}"),
        };
        ensure!(
            file.starts_with(".ferrite_pre_") && !file.contains(".."),
            "Not a precomputed payload name: {name}"
        );
        self.cache.preload_checked(&file, bytes, verified)?;
        self.payloads += 1;
        self.scene_current = false;
        Ok(())
    }

    /// Names of the cache entries the next portrayal reads and the page's
    /// cache store does not hold yet, for the page to load from its
    /// persistent store ([`Self::load_cache_entry`]) before preparing:
    ///
    /// - the S-102 rule instructions of the loaded bathymetry
    ///   ([`ChartEngine::s102_cache_entry`]);
    /// - unless Lua is chosen, the precomputed payloads of the opened cells
    ///   ([`ChartEngine::precomputed_cell_entries`]); when any are listed,
    ///   only these two kinds are, since whether Lua runs depends on the
    ///   payloads found: call again after loading them;
    /// - when the next preparation portrays with Lua ([`Self::lua_needed`]),
    ///   the per-cell Lua portrayal caches of the opened cells for the
    ///   current display ([`ChartEngine::cell_cache_entries`]).
    ///
    /// Payload and S-102 names are listed once per page (a page store that
    /// lacks them keeps lacking them).
    pub fn missing_cache_entries(&mut self) -> Vec<String> {
        let file_name = |path: &PathBuf| path.file_name()?.to_str().map(str::to_owned);
        let mut once = Vec::new();
        if let Some(name) = self.engine.s102_cache_entry().as_ref().and_then(file_name) {
            once.push(name);
        }
        if self.portrayal != PortrayalChoice::Lua {
            once.extend(
                self.engine
                    .precomputed_cell_entries()
                    .iter()
                    .filter_map(file_name),
            );
        }
        once.retain(|name| !self.cache.contains(name) && !self.offered.contains(name));
        self.offered.extend(once.iter().cloned());
        if once.iter().any(|name| name.starts_with(".ferrite_pre_")) {
            return once;
        }
        if self.lua_needed() {
            once.extend(
                self.engine
                    .cell_cache_entries()
                    .iter()
                    .filter_map(file_name)
                    .filter(|name| !self.cache.contains(name)),
            );
        }
        once
    }

    /// Whether the next preparation portrays S-101 cells with Lua: Lua is
    /// built and chosen, or `auto` lacks a payload of an opened cell (then
    /// the preparation falls back to Lua for every cell).
    pub fn lua_needed(&mut self) -> bool {
        if !LUA_AVAILABLE || self.engine.state().layers.s101().cells.is_empty() {
            return false;
        }
        match self.portrayal {
            PortrayalChoice::Lua => true,
            PortrayalChoice::Precomputed => false,
            PortrayalChoice::Auto => {
                self.payloads == 0
                    || !self.engine.precomputed_cell_entries().iter().all(|path| {
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|name| self.cache.contains(name))
                    })
            }
        }
    }

    /// Store a cache entry the page kept from an earlier session (not
    /// reported again by [`Self::take_cache_writes`]); a precomputed payload
    /// (`.ferrite_pre_*`) counts as loaded like [`Self::load_precomputed`].
    pub fn load_cache_entry(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.load_cache_entry_checked(name, bytes, None)
    }

    /// [`Self::load_cache_entry`] with the SHA-256 of the entry's payload
    /// (`bytes[40..]`) computed by the host (e.g. Web Crypto, much faster
    /// than hashing in WebAssembly). When it equals the hash stored in the
    /// entry, the engine's read skips its own SHA-256 check (magic, schema
    /// version and decoding are still checked); otherwise the entry is
    /// stored unchecked and the engine verifies it as usual.
    pub fn load_cache_entry_checked(
        &mut self,
        name: &str,
        bytes: &[u8],
        payload_sha256: Option<&[u8]>,
    ) -> Result<()> {
        let verified = payload_sha256.is_some_and(|digest| {
            ferrite_s101::layer::portrayal_cache::stored_digest(bytes) == Some(digest)
        });
        if name.starts_with(".ferrite_pre_") {
            return self.load_precomputed_checked(name, bytes, verified);
        }
        self.cache.preload_checked(name, bytes, verified)
    }

    /// Set the residency threshold (estimated instructions from which only
    /// cells near the view are portrayed; None: the engine default). The
    /// next view change portrays again.
    pub fn set_residency_threshold(&mut self, threshold: Option<usize>) {
        self.engine.set_residency_threshold(threshold);
        self.scene_current = false;
    }

    /// Cache entries written since the last call, for the page to persist.
    pub fn take_cache_writes(&self) -> Vec<(String, Vec<u8>)> {
        self.cache.take_writes()
    }

    /// Whether the current scene can follow a view change by re-projection
    /// (then [`Self::set_view`] usually portrays nothing).
    pub fn scene_current(&self) -> bool {
        self.scene_current && self.scene.is_some()
    }

    /// Select how S-101 cells are portrayed (`auto`, `lua`, `precomputed`);
    /// applies from the next preparation.
    pub fn set_portrayal(&mut self, name: &str) -> Result<()> {
        let choice = PortrayalChoice::parse(name)?;
        ensure!(
            LUA_AVAILABLE || choice != PortrayalChoice::Lua,
            "This build has no Lua portrayal (feature lua-wasm)"
        );
        self.portrayal = choice;
        self.scene_current = false;
        Ok(())
    }

    /// The portrayal choice, whether Lua is built and the source of the
    /// current scene's S-101 instructions (JSON).
    pub fn portrayal_json(&self) -> String {
        json!({
            "choice": self.portrayal.name(),
            "lua_available": LUA_AVAILABLE,
            "payloads": self.payloads,
            "used": self.used_source.map(source_name),
        })
        .to_string()
    }

    /// Open the queued files; the report of this open as JSON.
    pub fn open(&mut self) -> Result<String> {
        ensure!(!self.pending.is_empty(), "No files added");
        let files = std::mem::take(&mut self.pending);
        let report = self.engine.open_blocking(DatasetSource::Bytes(files))?;
        self.scene = None;
        self.scene_current = false;
        let json = inspection_json(&self.engine, &report);
        self.opened.datasets.extend(report.datasets);
        self.opened.issues.extend(report.issues);
        self.opened.notices.extend(report.notices);
        Ok(json.to_string())
    }

    /// Every opened dataset with the issues and notices of all opens so far
    /// and the exchange-set checks.
    pub fn report_json(&self) -> String {
        let mut report = inspection_json(&self.engine, &self.opened);
        let (issues, notices) = self.engine.exchange_set_checks();
        report["exchange_set"] = json!({
            "issues": issues.iter().map(issue_json).collect::<Vec<_>>(),
            "notices": notices,
        });
        report.to_string()
    }

    pub fn palettes_json(&self) -> String {
        json!(palettes(&self.engine)).to_string()
    }

    pub fn set_palette(&mut self, name: &str) -> Result<()> {
        let mut display = self.engine.display().clone();
        display.profile = name.to_owned();
        self.engine.set_display(display)?;
        self.scene_current = false;
        Ok(())
    }

    /// Portray the opened datasets for a `width` x `height` physical-pixel
    /// view at `density` physical pixels per CSS pixel. The camera centre is
    /// (`center_lat`, `center_lon`) (NaN: centre of the data); `zoom` is the
    /// factor over the all-data view, or with `is_scale` the display scale
    /// denominator 1:`zoom`. Returns the scene summary as JSON.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        width: u32,
        height: u32,
        density: f64,
        center_lat: f64,
        center_lon: f64,
        zoom: f64,
        is_scale: bool,
    ) -> Result<String> {
        let (view, bounds, factor) = self.view_of(
            width, height, density, center_lat, center_lon, zoom, is_scale,
        )?;
        self.scene = None;
        self.scene_current = false;
        let scene = self.portray(&view)?;
        self.finish(scene, bounds, factor, false, [width, height])
    }

    /// Move to a view like [`Self::prepare`], re-projecting the current
    /// scene when only the camera or size changed and the view keeps the
    /// same resident cells ([`ChartEngine::reproject`]); otherwise (first
    /// view, other datasets, display or portrayal, or another resident cell
    /// set) the cells are portrayed again. The summary's `reprojected` tells
    /// which happened.
    #[allow(clippy::too_many_arguments)]
    pub fn set_view(
        &mut self,
        width: u32,
        height: u32,
        density: f64,
        center_lat: f64,
        center_lon: f64,
        zoom: f64,
        is_scale: bool,
    ) -> Result<String> {
        let (view, bounds, factor) = self.view_of(
            width, height, density, center_lat, center_lon, zoom, is_scale,
        )?;
        if self.scene_current {
            if let Some(mut scene) = self.scene.take() {
                if self.engine.reproject(&mut scene, &view)? {
                    return self.finish(scene, bounds, factor, true, [width, height]);
                }
            }
        }
        self.scene = None;
        self.scene_current = false;
        let scene = self.portray(&view)?;
        self.finish(scene, bounds, factor, false, [width, height])
    }

    #[allow(clippy::too_many_arguments)]
    fn view_of(
        &self,
        width: u32,
        height: u32,
        density: f64,
        center_lat: f64,
        center_lon: f64,
        zoom: f64,
        is_scale: bool,
    ) -> Result<(View, ferrite_render::GeoBounds, f64)> {
        ensure!(zoom.is_finite() && zoom > 0., "Invalid zoom");
        let center =
            (center_lat.is_finite() && center_lon.is_finite()).then_some([center_lat, center_lon]);
        let camera = Camera {
            center,
            zoom: if is_scale {
                Zoom::Scale(zoom)
            } else {
                Zoom::Factor(zoom)
            },
        };
        let viewport = ViewportPixels {
            width,
            height,
            scale_factor: density,
        };
        let view = View {
            fit: None,
            viewport,
            camera: Some(camera),
        };
        let (bounds, factor) = self.engine.camera_view(camera, viewport)?;
        Ok((view, bounds, factor))
    }

    /// Prepare `view` with the chosen portrayal source (see
    /// [`PortrayalChoice::Auto`]).
    fn portray(&mut self, view: &View) -> Result<PreparedScene> {
        let lua_first = match self.portrayal {
            PortrayalChoice::Lua => true,
            PortrayalChoice::Precomputed => false,
            PortrayalChoice::Auto => LUA_AVAILABLE && self.payloads == 0,
        };
        let first = if lua_first {
            PortrayalSource::Lua
        } else {
            PortrayalSource::Precomputed
        };
        self.engine.set_portrayal_source(first);
        let scene = match self.engine.prepare(view) {
            Ok(scene) => scene,
            Err(error)
                if self.portrayal == PortrayalChoice::Auto
                    && LUA_AVAILABLE
                    && first == PortrayalSource::Precomputed =>
            {
                tracing::info!("Precomputed portrayal unavailable ({error:#}); using Lua");
                self.engine.set_portrayal_source(PortrayalSource::Lua);
                self.engine.prepare(view)?
            }
            Err(error) => return Err(error),
        };
        self.used_source = (!self.engine.state().layers.s101().cells.is_empty())
            .then(|| self.engine.portrayal_source());
        Arc::try_unwrap(scene).map_err(|_| anyhow::anyhow!("Prepared scene is shared"))
    }

    fn finish(
        &mut self,
        scene: PreparedScene,
        bounds: ferrite_render::GeoBounds,
        factor: f64,
        reprojected: bool,
        size: [u32; 2],
    ) -> Result<String> {
        let summary = json!({
            "instructions": scene.instruction_count(),
            "zoom": factor,
            "display_scale": scene.context.scaler.display_scale,
            "bounds": {
                "west": bounds.min_x, "south": bounds.min_y,
                "east": bounds.max_x, "north": bounds.max_y,
            },
            "data_bounds": {
                "west": self.engine.state().bounds.min_x,
                "south": self.engine.state().bounds.min_y,
                "east": self.engine.state().bounds.max_x,
                "north": self.engine.state().bounds.max_y,
            },
            "palette": self.engine.display().profile,
            "portrayal": self.used_source.map(source_name),
            "reprojected": reprojected,
            "resident_cells": self.engine.state().layers.s101().resident_cells.as_ref()
                .map(|r| r.iter().filter(|r| **r).count()),
            "cells": self.engine.state().layers.s101().cells.len(),
            "bathymetry_files": self.engine.state().bathymetry_extents().len(),
            // Cache entries read by this preparation (per-cell portrayal
            // caches and precomputed payloads): hits skip Lua.
            "cache_reads": self.cache.take_reads(),
        });
        self.scene = Some(scene);
        self.scene_current = true;
        self.zoom = factor;
        self.size = size;
        Ok(summary.to_string())
    }

    /// The prepared scene, its zoom factor and pixel size.
    pub fn scene_mut(&mut self) -> Option<(&ChartEngine, &mut PreparedScene, f64, [u32; 2])> {
        let (zoom, size) = (self.zoom, self.size);
        let engine = &self.engine;
        self.scene.as_mut().map(|scene| (engine, scene, zoom, size))
    }

    /// The raw Part 9a output of the portrayal catalogue's Lua rules for
    /// every opened S-101 cell with the current display settings: per cell a
    /// `cell\t<index>\t<dataset>\t<emissions>` line, then one
    /// `<feature>\t<instructions>\t<observed parameters, sorted>` line per
    /// `HostPortrayalEmit` call in emission order. Native and wasm builds
    /// must produce identical text (`tools/lua_wasm_parity.py`).
    #[cfg(feature = "lua-wasm")]
    pub fn part9a_dump(&self) -> Result<String> {
        use ferrite_lua::{CellData, PortrayalEngine, TypeCatalogue};
        use std::fmt::Write as _;
        let state = self.engine.state();
        let context = ferrite_s101::layer::portrayal::validated_lua_context(
            &state.s101.pc,
            Some(&self.engine.display().settings),
        )?;
        let mut engine = PortrayalEngine::new_with_sources(state.s101.pc.sources())?;
        engine.set_shared_type_catalogue(Arc::new(TypeCatalogue::from_feature_catalogue(
            &state.s101.fc,
        )));
        engine.initialize()?;
        let mut out = String::new();
        for (index, cell) in state.s101.cells.iter().enumerate() {
            let (_, emitted) = engine
                .process_owned_cell_recording(CellData::from_cell(cell), context.clone())
                .with_context(|| format!("Portrayal of {} failed", cell.dsid.dataset_name))?;
            let _ = writeln!(
                out,
                "cell\t{index}\t{}\t{}",
                cell.dsid.dataset_name,
                emitted.len()
            );
            for [feature, instructions, observed] in emitted {
                // The rules list observed context parameters in Lua table
                // traversal order, which follows the per-state string hash
                // seed (address and time based in native builds too); the
                // engine sorts these records, so the dump does as well.
                let mut observed: Vec<&str> = observed.split(';').collect();
                observed.sort_unstable();
                let _ = writeln!(out, "{feature}\t{instructions}\t{}", observed.join(";"));
            }
        }
        Ok(out)
    }

    /// Features under physical pixel (`x`, `y`) of the prepared scene within
    /// `radius` pixels, best first, as a JSON array of feature reports.
    pub fn pick_json(&mut self, x: f64, y: f64, radius: f32) -> Result<String> {
        ensure!(
            x.is_finite() && y.is_finite() && radius.is_finite() && radius >= 0.,
            "Invalid pick position or radius"
        );
        let scene = self.scene.as_ref().context("No prepared scene")?;
        let hits = self.engine.pick(scene, [x, y], radius, None);
        let mut seen = std::collections::HashSet::new();
        let mut reports = Vec::new();
        for hit in &hits {
            let id: FeatureId = hit.into();
            if !seen.insert((id.cell_index, id.feature_id)) {
                continue;
            }
            if let Some(report) = self.engine.feature(id) {
                reports.push(feature_json(&report));
            }
        }
        Ok(serde_json::Value::Array(reports).to_string())
    }

    /// Report of one feature (`cell_index` None for S-124/S-129 objects).
    pub fn feature_json(&mut self, cell_index: Option<u32>, feature_id: i64) -> Result<String> {
        let report = self
            .engine
            .feature(FeatureId {
                cell_index,
                feature_id,
            })
            .context("No such feature")?;
        Ok(feature_json(&report).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_engine::platform::SystemClock;

    fn repository() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn catalogue_files(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for product in [
            "FC/S-101", "PC/S-101", "FC/S-102", "PC/S-102", "FC/S-124", "PC/S-124",
        ] {
            if !root.join("Catalogues").join(product).is_dir() {
                continue;
            }
            let base = root.join("Catalogues");
            let mut stack = vec![base.join(product)];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        let name = path.strip_prefix(&base).unwrap().to_string_lossy();
                        files.push((name.into_owned(), std::fs::read(&path).unwrap()));
                    }
                }
            }
        }
        files
    }

    #[test]
    fn bundle_round_trip_and_rejections() {
        let mut bytes = BUNDLE_MAGIC.to_vec();
        for (name, data) in [("FC/S-101/a.xml", &b"one"[..]), ("PC/b", b"")] {
            bytes.extend((name.len() as u32).to_le_bytes());
            bytes.extend(name.as_bytes());
            bytes.extend((data.len() as u32).to_le_bytes());
            bytes.extend(data);
        }
        assert_eq!(
            parse_bundle(&bytes).unwrap(),
            vec![
                ("FC/S-101/a.xml".to_string(), b"one".to_vec()),
                ("PC/b".to_string(), vec![])
            ]
        );
        assert!(parse_bundle(&bytes[..bytes.len() - 1]).is_err());
        assert!(parse_bundle(b"ZIP!").is_err());
        assert!(relative_name("../x").is_err());
        assert_eq!(relative_name("/a\\b/./c").unwrap(), PathBuf::from("a/b/c"));
    }

    #[test]
    fn page_cache_reports_engine_writes_only() {
        let cache = PageCacheStore::default();
        assert!(cache.preload("../x", b"").is_err());
        assert!(cache.preload(".ferrite_a/b", b"").is_err());
        assert!(cache.preload("plain.bin", b"").is_err());
        cache.preload(".ferrite_cell_1.bin", b"kept").unwrap();
        assert!(cache.contains(".ferrite_cell_1.bin"));
        assert!(cache.take_writes().is_empty());
        let path = cache.entry(".ferrite_cell_2.bin").unwrap();
        cache.write(&path, b"new").unwrap();
        assert_eq!(cache.read(&path).unwrap(), b"new");
        assert_eq!(cache.take_reads(), 1);
        assert_eq!(
            cache.take_writes(),
            vec![(".ferrite_cell_2.bin".to_string(), b"new".to_vec())]
        );
        assert!(cache.take_writes().is_empty());
        assert_eq!(cache.len(), 2);
    }

    /// A host-computed payload SHA-256 equal to the stored hash marks the
    /// entry checked (the engine's read then skips hashing); a wrong or
    /// missing digest leaves the check to the engine.
    #[test]
    fn host_digest_marks_matching_entries_checked() {
        use ferrite_s101::layer::portrayal_cache::{
            encode_cache_payload, stored_digest, wrap_cache,
        };
        let root = repository();
        let mut core = WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
        let file = wrap_cache(&encode_cache_payload(&[]).unwrap());
        // wrap_cache stores the payload's SHA-256.
        let digest: [u8; 32] = stored_digest(&file).unwrap().try_into().unwrap();
        let checked = |core: &WebEngineCore, name: &str| {
            let path = core.cache.entry(name).unwrap();
            core.cache.memory.read_entry(&path).unwrap().verified
        };
        core.load_cache_entry_checked(".ferrite_cell_a.bin", &file, Some(&digest))
            .unwrap();
        assert!(checked(&core, ".ferrite_cell_a.bin"));
        let mut wrong = digest;
        wrong[0] ^= 1;
        core.load_cache_entry_checked(".ferrite_cell_b.bin", &file, Some(&wrong))
            .unwrap();
        assert!(!checked(&core, ".ferrite_cell_b.bin"));
        core.load_cache_entry(".ferrite_cell_c.bin", &file).unwrap();
        assert!(!checked(&core, ".ferrite_cell_c.bin"));
        core.load_cache_entry_checked(".ferrite_pre_d.bin", &file, Some(&digest))
            .unwrap();
        assert!(checked(&core, ".ferrite_pre_d.bin"));
        assert_eq!(core.payloads, 1);
        // Not a cache file: never marked checked.
        core.load_cache_entry_checked(".ferrite_s102_e.bin", b"text", Some(&digest))
            .unwrap();
        assert!(!checked(&core, ".ferrite_s102_e.bin"));
        assert!(core.take_cache_writes().is_empty());
    }

    #[test]
    fn opens_s124_from_bytes_prepares_and_picks() {
        let root = repository();
        let gml = root.join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML",
        );
        if !gml.is_file() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let mut core = WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
        assert!(core.load_precomputed("x.bin", b"").is_err());
        core.add_file("124GB00S1Y2026N36.GML", std::fs::read(&gml).unwrap())
            .unwrap();
        let report: serde_json::Value = serde_json::from_str(&core.open().unwrap()).unwrap();
        assert_eq!(report["datasets"][0]["product"], "S-124", "{report}");
        let summary: serde_json::Value = serde_json::from_str(
            &core
                .prepare(640, 480, 1.0, f64::NAN, f64::NAN, 1.0, false)
                .unwrap(),
        )
        .unwrap();
        assert!(summary["instructions"].as_u64().unwrap() > 0, "{summary}");
        let hits: serde_json::Value =
            serde_json::from_str(&core.pick_json(320., 240., 400.).unwrap()).unwrap();
        let first = &hits.as_array().expect("array")[0];
        assert!(first["feature_type"].is_string(), "{hits}");
        let id = first["feature_id"].as_i64().unwrap();
        assert!(core.feature_json(None, id).is_ok());
        core.set_palette("Night").unwrap();
        assert!(core.set_palette("Nope").is_err());
        assert!(core.palettes_json().contains("Day"));
    }

    /// Render the current scene of `core` offscreen.
    fn draw(core: &mut WebEngineCore) -> Vec<u8> {
        let (engine, scene, zoom, [w, h]) = core.scene_mut().expect("scene");
        let mut gpu = ferrite_cli::OffscreenChartRenderer::new(w, h, 1.0).unwrap();
        gpu.render(engine, scene, zoom).unwrap().into_raw()
    }

    /// set_view re-projects (no portrayal) and draws exactly what a fresh
    /// preparation of the same view draws.
    fn assert_reprojection_matches(
        files: &[(String, Vec<u8>)],
        first: (f64, f64, f64),
        second: (f64, f64, f64),
        expect_reprojected: bool,
    ) {
        let root = repository();
        let open = || {
            let mut core =
                WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
            for (name, bytes) in files {
                core.add_file(name, bytes.clone()).unwrap();
            }
            core.open().unwrap();
            core
        };
        let mut moved = open();
        let summary = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let a = summary(
            moved
                .set_view(320, 240, 1.0, first.0, first.1, first.2, false)
                .unwrap(),
        );
        assert_eq!(a["reprojected"], false);
        let b = summary(
            moved
                .set_view(320, 240, 1.0, second.0, second.1, second.2, false)
                .unwrap(),
        );
        assert_eq!(b["reprojected"], expect_reprojected, "{b}");
        let mut fresh = open();
        let c = summary(
            fresh
                .prepare(320, 240, 1.0, second.0, second.1, second.2, false)
                .unwrap(),
        );
        assert_eq!(b["instructions"], c["instructions"]);
        assert_eq!(b["display_scale"], c["display_scale"]);
        assert!(
            draw(&mut moved) == draw(&mut fresh),
            "re-projected frame differs"
        );
        // A display change portrays again.
        moved.set_palette("Night").unwrap();
        let d = summary(
            moved
                .set_view(320, 240, 1.0, second.0, second.1, second.2, false)
                .unwrap(),
        );
        assert_eq!(d["reprojected"], false);
    }

    #[test]
    fn set_view_reprojects_overlays_like_a_fresh_prepare() {
        let root = repository();
        let gml = root.join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML",
        );
        if !gml.is_file() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let files = vec![(
            "124GB00S1Y2026N36.GML".to_string(),
            std::fs::read(&gml).unwrap(),
        )];
        assert_reprojection_matches(
            &files,
            (f64::NAN, f64::NAN, 1.0),
            (f64::NAN, f64::NAN, 2.5),
            true,
        );
    }

    /// Lua portrayal through the web core, then pan and zoom by re-projection.
    #[cfg(feature = "lua-wasm")]
    #[test]
    fn set_view_reprojects_lua_cells_like_a_fresh_prepare() {
        let root = repository();
        let cell = root.join(
            "TestData/SHOM-S101/NormalizedExchangeSets/S100_ENC_ROOT_AcrossTheCHannel_product_Edition1_2026-03-25T06_53_12/S100_ROOT/S-101/DATASET_FILES/101FR00471610/101FR00471610.000",
        );
        if !cell.is_file() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let files = vec![(
            "101FR00471610.000".to_string(),
            std::fs::read(&cell).unwrap(),
        )];
        assert_reprojection_matches(
            &files,
            (f64::NAN, f64::NAN, 1.0),
            (f64::NAN, f64::NAN, 3.0),
            true,
        );
        let mut core = WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
        core.add_file("101FR00471610.000", std::fs::read(&cell).unwrap())
            .unwrap();
        core.open().unwrap();
        let s: serde_json::Value = serde_json::from_str(
            &core
                .set_view(320, 240, 1.0, f64::NAN, f64::NAN, 1.0, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(s["portrayal"], "lua", "{s}");
        assert!(core.part9a_dump().unwrap().starts_with("cell\t0\t"));
    }

    /// The persistent cache protocol of the page: a second page preloads the
    /// entries the first one wrote and portrays from them (no Lua run), with
    /// the same result.
    #[cfg(feature = "lua-wasm")]
    #[test]
    fn cell_caches_written_by_one_page_serve_the_next() {
        let root = repository();
        let cell = root.join(
            "TestData/SHOM-S101/NormalizedExchangeSets/S100_ENC_ROOT_AcrossTheCHannel_product_Edition1_2026-03-25T06_53_12/S100_ROOT/S-101/DATASET_FILES/101FR00471610/101FR00471610.000",
        );
        if !cell.is_file() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let open = || {
            let mut core =
                WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
            core.add_file("101FR00471610.000", std::fs::read(&cell).unwrap())
                .unwrap();
            core.open().unwrap();
            core
        };
        let summary = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let mut first = open();
        // The cell's precomputed payload is looked up first (once); without
        // one the cell is portrayed with Lua and its Lua cache is wanted.
        let payload = first.missing_cache_entries();
        assert_eq!(payload.len(), 1, "{payload:?}");
        assert!(payload[0].starts_with(".ferrite_pre_"), "{payload:?}");
        let wanted = first.missing_cache_entries();
        assert_eq!(wanted.len(), 1, "{wanted:?}");
        assert!(wanted[0].starts_with(".ferrite_cell_"), "{wanted:?}");
        let a = summary(
            first
                .set_view(320, 240, 1.0, f64::NAN, f64::NAN, 2.0, false)
                .unwrap(),
        );
        assert_eq!(a["cache_reads"], 0, "{a}");
        let saved = first.take_cache_writes();
        assert_eq!(
            saved.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            wanted
        );
        assert!(first.missing_cache_entries().is_empty());

        let mut second = open();
        assert_eq!(second.missing_cache_entries(), payload);
        assert_eq!(second.missing_cache_entries(), wanted);
        for (name, bytes) in &saved {
            second.load_cache_entry(name, bytes).unwrap();
        }
        assert!(second.missing_cache_entries().is_empty());
        let b = summary(
            second
                .set_view(320, 240, 1.0, f64::NAN, f64::NAN, 2.0, false)
                .unwrap(),
        );
        assert_eq!(b["cache_reads"], 1, "{b}");
        assert_eq!(a["instructions"], b["instructions"]);
        assert!(second.take_cache_writes().is_empty());
        assert!(
            draw(&mut first) == draw(&mut second),
            "cached frame differs"
        );
        // Another palette needs other entries.
        second.set_palette("Night").unwrap();
        assert_eq!(second.missing_cache_entries().len(), 1);
    }

    /// S-102 from bytes through the web core: opened, portrayed with the
    /// rule in Lua and drawn by the shared emitter; re-projection keeps the
    /// uploaded rasters and draws what a fresh preparation draws.
    #[cfg(all(feature = "lua-wasm", feature = "s102-pure"))]
    #[test]
    fn s102_from_bytes_is_drawn() {
        let root = repository();
        let dir = root.join(
            "TestData/SHOM-S102/ExchangeSets/S100_ENC_ROOT_SAINTMALO_5m_2026-05-26/S100_ROOT/S-102/DATASET_FILES",
        );
        if !dir.is_dir() || !root.join("Catalogues/PC/S-102").is_dir() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            for file in std::fs::read_dir(entry.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                files.push((
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
        files.sort();
        let open = |files: &[(String, Vec<u8>)]| {
            let mut core =
                WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
            for (name, bytes) in files {
                core.add_file(name, bytes.clone()).unwrap();
            }
            let report: serde_json::Value = serde_json::from_str(&core.open().unwrap()).unwrap();
            (core, report)
        };
        let (mut core, report) = open(&files);
        assert_eq!(
            report["bathymetry"].as_array().unwrap().len(),
            7,
            "{report}"
        );
        let summary = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let a = summary(
            core.set_view(320, 240, 1.0, 48.645, -2.03, 1.0, false)
                .unwrap(),
        );
        assert_eq!(a["bathymetry_files"], 7, "{a}");
        let mut gpu = ferrite_cli::OffscreenChartRenderer::new(320, 240, 1.0).unwrap();
        let mut frame = |core: &mut WebEngineCore| {
            let (engine, scene, zoom, _) = core.scene_mut().unwrap();
            gpu.render(engine, scene, zoom)
                .map(|image| image.into_raw())
        };
        let drawn = frame(&mut core).unwrap();
        // Only S-102 is open: the depth shades fill the view.
        let colours: std::collections::HashSet<&[u8]> = drawn.chunks(4).collect();
        assert!(colours.len() > 3, "{} colours", colours.len());
        // The tiles went to `gpu`: another renderer refuses the scene.
        let (engine, scene, zoom, [w, h]) = core.scene_mut().unwrap();
        let mut other = ferrite_cli::OffscreenChartRenderer::new(w, h, 1.0).unwrap();
        assert!(other.render(engine, scene, zoom).is_err());
        // Re-projected frames keep the rasters on the GPU and draw what a
        // fresh preparation of the moved view draws.
        let b = summary(
            core.set_view(320, 240, 1.0, 48.64, -2.02, 2.0, false)
                .unwrap(),
        );
        assert_eq!(b["reprojected"], true, "{b}");
        let once = frame(&mut core).unwrap();
        let twice = frame(&mut core).unwrap();
        assert!(once == twice, "second frame of the same scene differs");
        let (mut fresh, _) = open(&files);
        fresh
            .prepare(320, 240, 1.0, 48.64, -2.02, 2.0, false)
            .unwrap();
        assert!(draw(&mut fresh) == once, "re-projected S-102 frame differs");
    }

    /// Dropped precomputed payloads and the S-102 rule instructions persist
    /// like Lua caches: a page that loads them back portrays neither S-101
    /// nor S-102 with Lua and draws what the page with the dropped payload
    /// drew.
    #[cfg(all(feature = "lua-wasm", feature = "s102-pure"))]
    #[test]
    fn payloads_and_s102_instructions_serve_the_next_page() {
        let root = repository();
        let cell = root.join(
            "TestData/SHOM-S101/NormalizedExchangeSets/S100_ENC_ROOT_AcrossTheCHannel_product_Edition1_2026-03-25T06_53_12/S100_ROOT/S-101/DATASET_FILES/101FR00471610/101FR00471610.000",
        );
        let h5 = root.join(
            "TestData/SHOM-S102/ExchangeSets/S100_ENC_ROOT_SAINTMALO_5m_2026-05-26/S100_ROOT/S-102/DATASET_FILES/102FR00SMALOG000001/102FR00SMALOG000001.H5",
        );
        if !cell.is_file() || !h5.is_file() || !root.join("Catalogues/PC/S-102").is_dir() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let files = [
            ("101FR00471610.000", std::fs::read(&cell).unwrap()),
            ("102FR00SMALOG000001.H5", std::fs::read(&h5).unwrap()),
        ];
        let open = || {
            let mut core =
                WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
            for (name, bytes) in &files {
                core.add_file(name, bytes.clone()).unwrap();
            }
            core.open().unwrap();
            core
        };
        let summary = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let view = |core: &mut WebEngineCore| {
            summary(
                core.set_view(320, 240, 1.0, 48.66, -2.05, 1.5, false)
                    .unwrap(),
            )
        };
        // A Lua build exports the cell's payload (what `ferrite precompute`
        // writes) and the S-102 rule instructions.
        let mut export = open();
        export.engine_mut().set_precomputed_export(true);
        view(&mut export);
        let exported = export.take_cache_writes();
        let payload = exported
            .iter()
            .find(|(name, _)| name.starts_with(".ferrite_pre_"))
            .expect("payload exported")
            .clone();
        let s102 = exported
            .iter()
            .find(|(name, _)| name.starts_with(".ferrite_s102_"))
            .expect("S-102 instructions written")
            .clone();

        // The user drops the payload: precomputed S-101, S-102 by Lua (its
        // instructions are written for the page to keep).
        let mut dropped = open();
        dropped.load_precomputed(&payload.0, &payload.1).unwrap();
        let a = view(&mut dropped);
        assert_eq!(a["portrayal"], "precomputed", "{a}");
        assert_eq!(a["bathymetry_files"], 1, "{a}");
        let written: Vec<String> = dropped
            .take_cache_writes()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(written, vec![s102.0.clone()]);

        // Reload: the page offers both entries, loads them, needs no Lua
        // cache, reads both and writes nothing.
        let mut reload = open();
        let mut wanted = reload.missing_cache_entries();
        wanted.sort();
        let mut expected = vec![payload.0.clone(), s102.0.clone()];
        expected.sort();
        assert_eq!(wanted, expected);
        for (name, bytes) in [&payload, &s102] {
            reload.load_cache_entry(name, bytes).unwrap();
        }
        assert!(!reload.lua_needed());
        assert!(reload.missing_cache_entries().is_empty());
        let b = view(&mut reload);
        assert_eq!(b["portrayal"], "precomputed", "{b}");
        assert_eq!(b["cache_reads"], 2, "{b}");
        assert_eq!(a["instructions"], b["instructions"]);
        assert!(reload.take_cache_writes().is_empty());
        assert!(
            draw(&mut dropped) == draw(&mut reload),
            "reloaded frame differs"
        );

        // A corrupt S-102 entry is rejected and the rule evaluated again.
        let mut corrupt = open();
        let mut bad = s102.1.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        corrupt.load_cache_entry(&s102.0, &bad).unwrap();
        corrupt.load_cache_entry(&payload.0, &payload.1).unwrap();
        let c = view(&mut corrupt);
        assert_eq!(c["instructions"], a["instructions"]);
        let rewritten = corrupt.take_cache_writes();
        assert_eq!(rewritten.len(), 1);
        assert_eq!(rewritten[0], s102);
    }

    /// Residency on a small chart set (threshold 0): zooming into one cell
    /// leaves the resident set, so `set_view` portrays again; moves that keep
    /// it re-project. Both match a fresh preparation of the same view.
    #[cfg(feature = "lua-wasm")]
    #[test]
    fn set_view_follows_residency_changes() {
        let root = repository();
        let dir = root.join(
            "TestData/SHOM-S101/NormalizedExchangeSets/S100_ENC_ROOT_AcrossTheCHannel_product_Edition1_2026-03-25T06_53_12/S100_ROOT/S-101/DATASET_FILES",
        );
        if !dir.is_dir() {
            eprintln!("TestData not present; skipped");
            return;
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            for file in std::fs::read_dir(entry.unwrap().path()).unwrap() {
                let path = file.unwrap().path();
                files.push((
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
        files.sort();
        let open = || {
            let mut core =
                WebEngineCore::new(catalogue_files(&root), Arc::new(SystemClock)).unwrap();
            core.set_residency_threshold(Some(0));
            for (name, bytes) in &files {
                core.add_file(name, bytes.clone()).unwrap();
            }
            core.open().unwrap();
            core
        };
        let summary = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        let mut moving = open();
        let overview = summary(
            moving
                .set_view(320, 240, 1.0, f64::NAN, f64::NAN, 1.0, false)
                .unwrap(),
        );
        let resident = |s: &serde_json::Value| s["resident_cells"].as_u64().unwrap();
        let overview_cells = resident(&overview);
        assert!(overview_cells > 0, "{overview}");
        // Zoom into Saint-Malo harbour (largest-scale cell), move a little,
        // then zoom out to the overview.
        let views = [
            (48.64, -2.03, 60.0),
            (48.641, -2.031, 70.0),
            (f64::NAN, f64::NAN, 1.0),
        ];
        let mut outcomes = Vec::new();
        for (lat, lon, zoom) in views {
            let s = summary(
                moving
                    .set_view(320, 240, 1.0, lat, lon, zoom, false)
                    .unwrap(),
            );
            let mut fresh = open();
            let f = summary(fresh.prepare(320, 240, 1.0, lat, lon, zoom, false).unwrap());
            assert_eq!(s["instructions"], f["instructions"], "{s} {f}");
            assert_eq!(s["resident_cells"], f["resident_cells"], "{s} {f}");
            assert!(
                draw(&mut moving) == draw(&mut fresh),
                "frame at {lat},{lon} x{zoom} differs"
            );
            outcomes.push((s["reprojected"].as_bool().unwrap(), resident(&s)));
        }
        // In: another resident set (portrayed again); a small move keeps it
        // (re-projected); out: the overview's set again (portrayed again).
        assert!(
            !outcomes[0].0 && outcomes[0].1 != overview_cells,
            "{outcomes:?}"
        );
        assert!(
            outcomes[1].0 && outcomes[1].1 == outcomes[0].1,
            "{outcomes:?}"
        );
        assert!(
            !outcomes[2].0 && outcomes[2].1 == overview_cells,
            "{outcomes:?}"
        );
    }
}
