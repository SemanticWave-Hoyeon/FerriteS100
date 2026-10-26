//! The host-neutral part of the web bindings: an in-memory [`ChartEngine`]
//! fed with bytes, with plain JSON results. Builds natively too (tests).
use anyhow::{bail, ensure, Context, Result};
use ferrite_cli::{engine_config, feature_json, inspection_json, issue_json, palettes};
use ferrite_engine::chart_engine::{
    Camera, ChartEngine, DatasetSource, FeatureId, OpenReport, View, Zoom,
};
use ferrite_engine::platform::{
    Clock, MemoryCacheStore, MemoryDataSource, Platform, SequentialExecutor,
};
use ferrite_engine::scene::PreparedScene;
use ferrite_engine::{PortrayalSource, ViewportPixels};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where catalogue files live in the in-memory data source.
pub const CATALOGUE_ROOT: &str = "/catalogues";
/// Where added dataset files are named (their identity only).
pub const DATA_ROOT: &str = "/data";

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

/// The engine of a web page: catalogues and datasets from bytes, the
/// memory cache store holding precomputed S-101 payloads, a sequential
/// executor and the host's clock.
pub struct WebEngineCore {
    engine: ChartEngine,
    cache: Arc<MemoryCacheStore>,
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
        let cache = Arc::new(MemoryCacheStore::default());
        let platform = Platform::in_memory(data)
            .with_cache(Arc::clone(&cache) as _)
            .with_clock(clock)
            .with_executor(Arc::new(SequentialExecutor));
        let mut engine = ChartEngine::new(engine_config(platform, CATALOGUE_ROOT))?;
        // Chosen per preparation (`PortrayalChoice`); without Lua always payloads.
        engine.set_portrayal_source(PortrayalSource::Precomputed);
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
        })
    }

    pub fn engine(&self) -> &ChartEngine {
        &self.engine
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
        let root = ferrite_engine::platform::CacheStore::root(self.cache.as_ref())
            .context("No memory cache")?;
        ferrite_engine::platform::CacheStore::write(self.cache.as_ref(), &root.join(&file), bytes)
            .with_context(|| format!("Cannot store {name}"))?;
        self.payloads += 1;
        self.scene_current = false;
        Ok(())
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
        self.used_source =
            (!self.engine.state().cells.is_empty()).then(|| self.engine.portrayal_source());
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
            "resident_cells": self.engine.state().resident_cells.as_ref()
                .map(|r| r.iter().filter(|r| **r).count()),
            "cells": self.engine.state().cells.len(),
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
        let context = ferrite_engine::portrayal::validated_lua_context(
            &state.pc,
            Some(&self.engine.display().settings),
        )?;
        let mut engine = PortrayalEngine::new_with_sources(state.pc.sources())?;
        engine
            .set_shared_type_catalogue(Arc::new(TypeCatalogue::from_feature_catalogue(&state.fc)));
        engine.initialize()?;
        let mut out = String::new();
        for (index, cell) in state.cells.iter().enumerate() {
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
        for product in ["FC/S-101", "PC/S-101", "FC/S-124", "PC/S-124"] {
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
}
