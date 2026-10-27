//! Portrayal sources and host byte inputs through `ChartEngine`: precomputed
//! cell payloads exported by a Lua run reproduce the Lua scene exactly, a
//! missing payload is a clear error, and datasets handed over as bytes open
//! like files.
use ferrite_engine::chart_engine::{ChartEngine, DatasetSource, EngineConfig, View};
use ferrite_engine::platform::{MemoryCacheStore, MemoryDataSource, NativeDataSource, Platform};
use ferrite_engine::PortrayalSource;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const S101_CELL: &str =
    "TestData/SHOM-S101/Data/diffusionS101_May_June/S-101_June2026/6940/101FR00369400_002.000";

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A platform-absolute in-memory key (memory paths are authorization keys).
fn mem(path: &str) -> PathBuf {
    ferrite_engine::platform::memory_path(path)
}

fn catalogues(root: &Path) -> MemoryDataSource {
    let mut memory = MemoryDataSource::new();
    for product in ["FC/S-101", "PC/S-101"] {
        memory
            .copy_tree(
                &NativeDataSource,
                &root.join("Catalogues").join(product),
                &mem("/in-memory/Catalogues").join(product),
            )
            .unwrap();
    }
    memory
}

fn engine(memory: MemoryDataSource, cache: Arc<MemoryCacheStore>) -> ChartEngine {
    let platform = Platform::in_memory(memory).with_cache(cache);
    ChartEngine::new(EngineConfig::new(platform, mem("/in-memory/Catalogues"))).unwrap()
}

fn cell_bytes(root: &Path) -> Option<Vec<(PathBuf, Arc<[u8]>)>> {
    let bytes = std::fs::read(root.join(S101_CELL)).ok()?;
    Some(vec![(mem("/host/101FR00369400.000"), Arc::from(bytes))])
}

fn scene_payload(engine: &mut ChartEngine) -> anyhow::Result<Vec<u8>> {
    let scene = engine.prepare(&View::fit(800, 600))?;
    Ok(
        ferrite_s101::layer::portrayal_cache::encode_cache_payload(
            scene.context.raw_instructions(),
        )
        .unwrap(),
    )
}

#[test]
fn precomputed_payloads_reproduce_the_lua_scene() {
    let root = repository();
    let Some(bytes) = cell_bytes(&root) else {
        eprintln!("TestData not present; skipped");
        return;
    };
    let cache = Arc::new(MemoryCacheStore::default());

    // Native build: portray with Lua and export the cell payloads.
    let mut lua = engine(catalogues(&root), Arc::clone(&cache));
    let report = lua
        .open_blocking(DatasetSource::Bytes(bytes.clone()))
        .unwrap();
    assert_eq!(report.datasets.len(), 1, "{report:#?}");
    if lua.portrayal_source() != PortrayalSource::Lua {
        eprintln!("lua-native not built; export skipped");
        return;
    }
    lua.set_precomputed_export(true);
    let expected = scene_payload(&mut lua).unwrap();
    assert_eq!(cache.len(), 1, "one exported cell payload");

    // A host without Lua: the same scene from the payloads alone.
    let mut precomputed = engine(catalogues(&root), Arc::clone(&cache));
    precomputed.set_portrayal_source(PortrayalSource::Precomputed);
    precomputed
        .open_blocking(DatasetSource::Bytes(bytes.clone()))
        .unwrap();
    assert_eq!(scene_payload(&mut precomputed).unwrap(), expected);

    // Other settings are other payloads: missing, and reported as such.
    let mut display = precomputed.display().clone();
    display.settings.two_shades = !display.settings.two_shades;
    precomputed.set_display(display).unwrap();
    let error = scene_payload(&mut precomputed).unwrap_err();
    assert!(
        format!("{error:#}").contains("No precomputed portrayal"),
        "{error:#}"
    );

    // An empty store fails the same way.
    let mut empty = engine(catalogues(&root), Arc::new(MemoryCacheStore::default()));
    empty.set_portrayal_source(PortrayalSource::Precomputed);
    empty.open_blocking(DatasetSource::Bytes(bytes)).unwrap();
    let error = scene_payload(&mut empty).unwrap_err();
    assert!(
        format!("{error:#}").contains("No precomputed portrayal"),
        "{error:#}"
    );
}

#[test]
fn host_bytes_open_like_files() {
    let root = repository();
    let Some(bytes) = cell_bytes(&root) else {
        eprintln!("TestData not present; skipped");
        return;
    };
    let mut engine = engine(catalogues(&root), Arc::new(MemoryCacheStore::default()));
    let report = engine
        .open_blocking(DatasetSource::Bytes(
            bytes
                .into_iter()
                .chain([(mem("/host/notes.txt"), Arc::from(&b"x"[..]))])
                .collect(),
        ))
        .unwrap();
    assert_eq!(report.datasets.len(), 1);
    assert_eq!(report.datasets[0].product, "S-101");
    assert_eq!(
        report.datasets[0].source.as_deref(),
        Some(mem("/host/101FR00369400.000").as_path())
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("Not a supported dataset")),
        "{:?}",
        report.notices
    );
}
