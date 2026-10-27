//! S-102 through `ChartEngine` with no file system: catalogues and HDF5
//! products in a memory data source, decoded by the pure-Rust reader and
//! portrayed as the viewer portrays them.
#![cfg(feature = "s102-pure")]
use ferrite_engine::chart_engine::{ChartEngine, DatasetSource, EngineConfig, View};
use ferrite_engine::platform::{
    DataSource, MemoryCacheStore, MemoryDataSource, NativeDataSource, Platform,
};
#[cfg(feature = "lua-native")]
use ferrite_s102::layer::BathymetryTile;
use std::path::PathBuf;
use std::sync::Arc;

const SHOM: &str = "TestData/SHOM-S102/Data/S-102/S100_ENC_ROOT_SAINTMALO_5m_2026-05-26/S100_ROOT/S-102/DATASET_FILES";
const TRIAL: &str = "TestData/UKHO-S101/Complete_S10X_datasets/Complete S10X datasets/S-102 Trial Cells/102GBTD5N5090W00140/1/102GBTD5N5090W00140.h5";

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
/// A platform-absolute in-memory key (memory paths are authorization keys).
fn mem(path: &str) -> PathBuf {
    ferrite_engine::platform::memory_path(path)
}
fn shom(n: u32) -> PathBuf {
    repository().join(format!(
        "{SHOM}/102FR00SMALOG00000{n}/102FR00SMALOG00000{n}.H5"
    ))
}
fn present() -> bool {
    shom(1).is_file() && shom(4).is_file() && repository().join(TRIAL).is_file()
}

fn catalogues(memory: &mut MemoryDataSource, products: &[&str]) {
    let root = repository();
    for product in products {
        let copied = memory
            .copy_tree(
                &NativeDataSource,
                &root.join("Catalogues").join(product),
                &mem("/mem/Catalogues").join(product),
            )
            .unwrap();
        assert!(copied > 0, "{product}");
    }
}

fn new_engine(memory: MemoryDataSource, cache: Arc<MemoryCacheStore>) -> ChartEngine {
    assert!(!memory.local_paths());
    let platform = Platform::in_memory(memory).with_cache(cache);
    ChartEngine::new(EngineConfig::new(platform, mem("/mem/Catalogues"))).unwrap()
}

/// (bounds, width, height, rgba) of every tile, consuming the scene.
#[cfg(feature = "lua-native")]
fn tiles(scene: Arc<ferrite_engine::scene::PreparedScene>) -> Vec<([f64; 4], u32, u32, Vec<u8>)> {
    let scene = Arc::try_unwrap(scene).ok().unwrap();
    ferrite_s102::layer::bathymetry(&scene.extensions)
        .expect("S-102 extension")
        .take_tiles()
        .into_iter()
        .map(|tile| {
            let layer = match tile {
                BathymetryTile::Bound(material) => material.into_gpu_parts().0,
                BathymetryTile::Composed(layer) => layer,
            };
            let b = layer.bounds;
            (
                [b.min_x, b.min_y, b.max_x, b.max_y],
                layer.width,
                layer.height,
                layer.rgba,
            )
        })
        .collect()
}

#[test]
fn opens_and_prepares_s102_entirely_from_memory() {
    if !present() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let mut memory = MemoryDataSource::new();
    catalogues(&mut memory, &["FC/S-101", "PC/S-101", "PC/S-102"]);
    for n in [1, 4] {
        memory.insert(
            mem(&format!("/mem/data/102FR00SMALOG00000{n}.H5")),
            std::fs::read(shom(n)).unwrap(),
        );
    }
    memory.insert(
        mem("/mem/data/102GBTD5N5090W00140.h5"),
        std::fs::read(repository().join(TRIAL)).unwrap(),
    );
    let cache = Arc::new(MemoryCacheStore::default());
    let mut engine = new_engine(memory, cache.clone());
    assert!(
        engine.state().s102.pc.is_some(),
        "{:?}",
        engine.state().s102.pc_status
    );
    let job = engine.open(DatasetSource::Folder(mem("/mem/data")));
    let opened = job.wait().unwrap();
    assert_eq!(opened.bathymetry_count(), 3);
    let report = engine.install(opened).unwrap();
    assert!(report.issues.is_empty(), "{:#?}", report.issues);
    assert!(report.datasets.is_empty());
    assert_eq!(report.bathymetry.len(), 3, "{report:#?}");
    let coverage: Vec<_> = report
        .bathymetry
        .iter()
        .filter(|b| !b.inspection_only)
        .collect();
    assert_eq!(coverage.len(), 2);
    for b in &coverage {
        assert_eq!(b.product_specification, "INT.IHO.S-102.3.0.0");
        assert!(b.instances >= 1 && b.grid.0 > 0 && b.grid.1 > 0);
        assert_eq!(b.provenance, "signature not verified");
        assert!(b.bounds.is_some());
    }
    let trial = report
        .bathymetry
        .iter()
        .find(|b| b.inspection_only)
        .unwrap();
    assert_eq!(trial.instances, 0);
    assert!(trial.product_specification.starts_with("INT.IHO.S-102.2.1"));
    assert_eq!(engine.bathymetry().len(), 3);

    // The all-data view is the bathymetry extent.
    let bounds = engine.state().bounds;
    for b in &coverage {
        let r = b.bounds.unwrap();
        assert!(bounds.min_x <= r.min_x && bounds.max_x >= r.max_x);
        assert!(bounds.min_y <= r.min_y && bounds.max_y >= r.max_y);
    }

    #[cfg(feature = "lua-native")]
    prepare_with_lua_then_precomputed(engine, cache);
    #[cfg(not(feature = "lua-native"))]
    {
        // Without Lua and without exported rule instructions: a clear error.
        let _ = cache;
        let error = engine.prepare(&View::fit(800, 600)).err().unwrap();
        assert!(
            format!("{error:#}").contains("No precomputed S-102"),
            "{error:#}"
        );
    }
}

#[cfg(feature = "lua-native")]
fn prepare_with_lua_then_precomputed(mut engine: ChartEngine, cache: Arc<MemoryCacheStore>) {
    engine.set_precomputed_export(true);
    let scene = engine.prepare(&View::fit(800, 600)).unwrap();
    let bathymetry = ferrite_s102::layer::bathymetry(&scene.extensions).unwrap();
    assert!(!bathymetry.viewing_groups.is_empty());
    assert!(!bathymetry.is_empty());
    let lua = tiles(scene);
    let opaque = lua
        .iter()
        .map(|(_, _, _, rgba)| rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count())
        .sum::<usize>();
    assert!(opaque > 0, "no coloured bathymetry pixels");

    // A host without Lua portrays the same pixels from the exported rule
    // instructions in the cache store.
    let mut memory = MemoryDataSource::new();
    catalogues(&mut memory, &["FC/S-101", "PC/S-101", "PC/S-102"]);
    for n in [1, 4] {
        memory.insert(
            mem(&format!("/mem/data/102FR00SMALOG00000{n}.H5")),
            std::fs::read(shom(n)).unwrap(),
        );
    }
    let mut precomputed = new_engine(memory, cache);
    precomputed.set_portrayal_source(ferrite_engine::portrayal::PortrayalSource::Precomputed);
    precomputed
        .open_blocking(DatasetSource::Folder(mem("/mem/data")))
        .unwrap();
    let again = tiles(precomputed.prepare(&View::fit(800, 600)).unwrap());
    assert_eq!(again, lua);

    // Without the exported instructions a Lua-less host fails clearly.
    let mut memory = MemoryDataSource::new();
    catalogues(&mut memory, &["FC/S-101", "PC/S-101", "PC/S-102"]);
    memory.insert(mem("/mem/data/a.H5"), std::fs::read(shom(1)).unwrap());
    let mut cold = new_engine(memory, Arc::new(MemoryCacheStore::default()));
    cold.set_portrayal_source(ferrite_engine::portrayal::PortrayalSource::Precomputed);
    cold.open_blocking(DatasetSource::Folder(mem("/mem/data")))
        .unwrap();
    let error = cold.prepare(&View::fit(800, 600)).err().unwrap();
    assert!(
        format!("{error:#}").contains("No precomputed S-102"),
        "{error:#}"
    );
}

#[test]
fn bytes_source_without_s102_catalogue_is_refused_with_an_issue() {
    if !present() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let mut memory = MemoryDataSource::new();
    catalogues(&mut memory, &["FC/S-101", "PC/S-101"]);
    let mut engine = new_engine(memory, Arc::new(MemoryCacheStore::default()));
    assert!(engine.state().s102.pc.is_none());
    let report = engine
        .open_blocking(DatasetSource::Bytes(vec![(
            mem("/host/102FR00SMALOG000001.H5"),
            std::fs::read(shom(1)).unwrap().into(),
        )]))
        .unwrap();
    assert!(report.bathymetry.is_empty());
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.check == "S-102 load" && i.message.contains("S-102 PC unavailable")),
        "{report:#?}"
    );
    // Malformed HDF5 bytes are an issue, never a panic.
    let report = engine
        .open_blocking(DatasetSource::Bytes(vec![(
            mem("/host/broken.h5"),
            b"\x89HDF\r\n\x1a\nnot really".to_vec().into(),
        )]))
        .unwrap();
    assert!(
        report.issues.iter().any(|i| i.check == "S-102 load"),
        "{report:#?}"
    );
    assert!(ferrite_s102::layer::bathymetry(
        &engine.prepare(&View::fit(64, 64)).unwrap().extensions
    )
    .is_none());
}

/// The facade's tiles equal the viewer's own pipeline on the same file:
/// native HDF5 reader, file-based depth policy, Lua-evaluated PC rule.
#[cfg(all(feature = "s102-hdf5", feature = "lua-native"))]
#[test]
fn facade_tiles_match_the_viewer_pipeline() {
    use ferrite_kernel::NumericCoverageSource;
    use ferrite_s102::layer::depth_policy::DepthPolicy;
    use ferrite_s102::{BathymetryCoverage, BathymetryPortrayal};
    if !present() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let path = shom(4);
    let mut memory = MemoryDataSource::new();
    catalogues(&mut memory, &["FC/S-101", "PC/S-101", "PC/S-102"]);
    memory.insert(mem("/mem/data/one.H5"), std::fs::read(&path).unwrap());
    let mut engine = new_engine(memory, Arc::new(MemoryCacheStore::default()));
    engine
        .open_blocking(DatasetSource::Folder(mem("/mem/data")))
        .unwrap();
    let mut options = engine.s102_options().clone();
    options.tile_edge = 97; // several tiles, partial edges
    engine.set_s102_options(options);
    let facade = tiles(engine.prepare(&View::fit(640, 480)).unwrap());

    let pc = Arc::new(
        ferrite_portrayal_catalog::PortrayalCatalogue::load_bound(
            repository().join("Catalogues/PC/S-102"),
        )
        .unwrap(),
    );
    let settings = ferrite_render::host_settings::SettingsState::default();
    let portrayal = BathymetryPortrayal::from_bound_catalogue(
        pc,
        "Day",
        ferrite_s102::layer::depth_settings(&settings),
    )
    .unwrap();
    let coverages =
        BathymetryCoverage::open_with_backend(&path, ferrite_s102::h5::Backend::Native).unwrap();
    let policy = DepthPolicy::load(
        None,
        &path,
        &coverages
            .iter()
            .map(|c| c.vertical_datum)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(coverages.len(), 1);
    assert!(policy.identity(coverages[0].vertical_datum));
    let numeric: &dyn NumericCoverageSource = &coverages[0];
    let mut viewer = Vec::new();
    for window in numeric.numeric_geometry().windows(97, 97).unwrap() {
        let layer = portrayal
            .bound_raster_window(numeric, window, String::new())
            .unwrap()
            .into_gpu_parts()
            .0;
        let b = layer.bounds;
        viewer.push((
            [b.min_x, b.min_y, b.max_x, b.max_y],
            layer.width,
            layer.height,
            layer.rgba,
        ));
    }
    assert!(viewer.len() > 1);
    assert_eq!(facade, viewer);
}
