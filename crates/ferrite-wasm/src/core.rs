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
    zoom: f64,
    size: [u32; 2],
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
        // Without Lua (the web build) S-101 cells always draw from payloads.
        engine.set_portrayal_source(PortrayalSource::Precomputed);
        Ok(Self {
            engine,
            cache,
            pending: Vec::new(),
            opened: OpenReport::default(),
            scene: None,
            zoom: 1.0,
            size: [0, 0],
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
        Ok(())
    }

    /// Open the queued files; the report of this open as JSON.
    pub fn open(&mut self) -> Result<String> {
        ensure!(!self.pending.is_empty(), "No files added");
        let files = std::mem::take(&mut self.pending);
        let report = self.engine.open_blocking(DatasetSource::Bytes(files))?;
        self.scene = None;
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
        self.engine.set_display(display)
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
        self.scene = None;
        let scene = self.engine.prepare(&view)?;
        let scene =
            Arc::try_unwrap(scene).map_err(|_| anyhow::anyhow!("Prepared scene is shared"))?;
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
        });
        self.scene = Some(scene);
        self.zoom = factor;
        self.size = [width, height];
        Ok(summary.to_string())
    }

    /// The prepared scene, its zoom factor and pixel size.
    pub fn scene_mut(&mut self) -> Option<(&ChartEngine, &mut PreparedScene, f64, [u32; 2])> {
        let (zoom, size) = (self.zoom, self.size);
        let engine = &self.engine;
        self.scene.as_mut().map(|scene| (engine, scene, zoom, size))
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
}
