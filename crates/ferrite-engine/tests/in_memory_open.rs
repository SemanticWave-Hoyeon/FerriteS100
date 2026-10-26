//! Opening datasets with no file system: catalogues, an S-101 cell and an
//! S-124 dataset are read into an in-memory data source first, then opened
//! and portrayed through `ChartEngine` on `Platform::in_memory`.
use ferrite_engine::chart_engine::{ChartEngine, DatasetId, DatasetSource, EngineConfig, View};
use ferrite_engine::platform::{DataSource, MemoryDataSource, NativeDataSource, Platform};
use ferrite_render::DrawingInstruction;
use std::path::{Path, PathBuf};

const S101_CELL: &str =
    "TestData/SHOM-S101/Data/diffusionS101_May_June/S-101_June2026/6940/101FR00369400_002.000";
const S124_DATASET: &str =
    "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML";

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Everything the engine reads, under paths that exist nowhere on disk.
fn memory_source(root: &Path) -> MemoryDataSource {
    let native = NativeDataSource;
    let mut memory = MemoryDataSource::new();
    for product in ["FC/S-101", "PC/S-101", "FC/S-124", "PC/S-124"] {
        let copied = memory
            .copy_tree(
                &native,
                &root.join("Catalogues").join(product),
                &Path::new("/in-memory/Catalogues").join(product),
            )
            .unwrap();
        assert!(copied > 0, "{product}");
    }
    memory.insert(
        "/in-memory/data/101FR00369400.000",
        std::fs::read(root.join(S101_CELL)).unwrap(),
    );
    memory.insert(
        "/in-memory/data/124GB00S1Y2026N36.GML",
        std::fs::read(root.join(S124_DATASET)).unwrap(),
    );
    memory
}

#[test]
fn opens_and_prepares_a_scene_entirely_from_memory() {
    let root = repository();
    if !root.join(S101_CELL).is_file() || !root.join(S124_DATASET).is_file() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let memory = memory_source(&root);
    assert!(!memory.local_paths());
    let platform = Platform::in_memory(memory);
    let cache = std::sync::Arc::new(ferrite_engine::platform::MemoryCacheStore::default());
    let platform = platform.with_cache(cache.clone());
    let mut engine =
        ChartEngine::new(EngineConfig::new(platform, "/in-memory/Catalogues")).unwrap();
    assert!(
        engine.overlays().s124.is_some(),
        "{:?}",
        engine.catalogue_errors()
    );

    let job = engine.open(DatasetSource::Folder("/in-memory/data".into()));
    let report = engine.install(job.wait().unwrap()).unwrap();
    let products: Vec<_> = report.datasets.iter().map(|d| d.product.as_str()).collect();
    assert_eq!(products, ["S-101", "S-124"], "{report:#?}");
    assert!(
        report
            .issues
            .iter()
            .all(|i| i.check != "S-101 load" && i.check != "GML load"),
        "{:#?}",
        report.issues
    );
    let cell = &report.datasets[0];
    assert_eq!(cell.id, DatasetId::Cell(0));
    assert_eq!(
        cell.source.as_deref(),
        Some(Path::new("/in-memory/data/101FR00369400.000"))
    );
    assert!(cell.features > 0);
    assert!(!engine.feature_types(DatasetId::Cell(0)).is_empty());
    assert!(!engine.feature_types(report.datasets[1].id).is_empty());

    let scene = engine.prepare(&View::fit(1024, 768)).unwrap();
    let instructions = scene.context.raw_instructions();
    assert!(instructions.iter().any(|i| i.cell_index() == Some(0)));
    assert!(
        instructions
            .iter()
            .any(ferrite_render::is_cellless_overlay_instruction),
        "S-124 overlay instructions are part of the scene"
    );
    assert_eq!(scene.coverage.current_dataset_count(), 1);
    // The portrayal cell cache went to the platform's memory store.
    assert_eq!(cache.len(), 1);
    // A second preparation reads the cell from that cache: same scene.
    let again = engine.prepare(&View::fit(1024, 768)).unwrap();
    assert_eq!(again.instruction_count(), scene.instruction_count());

    // Pick an ENC point symbol and describe it.
    let point = instructions
        .iter()
        .find_map(|i| match i {
            DrawingInstruction::Point(p) if p.feature_id.is_some() && p.cell_index == Some(0) => {
                Some(p.position)
            }
            _ => None,
        })
        .expect("a point symbol");
    let screen = scene.context.scaler.world_to_screen(point);
    let hits = engine.pick(&scene, [screen.x as f64, screen.y as f64], 4., None);
    let hit = hits
        .iter()
        .find(|h| h.cell_index == Some(0))
        .expect("an ENC hit");
    let report = engine.feature(hit.into()).expect("feature report");
    assert!(!report.feature_type.is_empty());
    assert_eq!(
        report.source.as_deref(),
        Some("/in-memory/data/101FR00369400.000")
    );
}

#[test]
fn signature_verification_needs_local_files() {
    let root = repository();
    if !root.join(S101_CELL).is_file() || !root.join(S124_DATASET).is_file() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let mut config = EngineConfig::new(
        Platform::in_memory(memory_source(&root)),
        "/in-memory/Catalogues",
    );
    config.signatures = ferrite_engine::chart_engine::SignaturePolicy::Required;
    let mut engine = ChartEngine::new(config).unwrap();
    let error = engine
        .open(DatasetSource::Folder("/in-memory/data".into()))
        .wait()
        .err()
        .expect("rejected");
    assert!(format!("{error:#}").contains("local files"), "{error:#}");
}
