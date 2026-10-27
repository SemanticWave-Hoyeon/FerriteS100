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

/// A platform-absolute in-memory key (memory paths are authorization keys).
fn mem(path: &str) -> PathBuf {
    ferrite_engine::platform::memory_path(path)
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
                &mem("/in-memory/Catalogues").join(product),
            )
            .unwrap();
        assert!(copied > 0, "{product}");
    }
    memory.insert(
        mem("/in-memory/data/101FR00369400.000"),
        std::fs::read(root.join(S101_CELL)).unwrap(),
    );
    memory.insert(
        mem("/in-memory/data/124GB00S1Y2026N36.GML"),
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
        ChartEngine::new(EngineConfig::new(platform, mem("/in-memory/Catalogues"))).unwrap();
    assert!(
        engine
            .overlays()
            .catalogues(ferrite_product_api::ProductId("S-124"))
            .is_some(),
        "{:?}",
        engine.catalogue_errors()
    );

    let job = engine.open(DatasetSource::Folder(mem("/in-memory/data")));
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
        Some(mem("/in-memory/data/101FR00369400.000").as_path())
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
        report.source.as_deref().map(Path::new),
        Some(mem("/in-memory/data/101FR00369400.000").as_path())
    );
}

/// Required signatures apply to S-101 from memory too: a cell outside any
/// signed exchange set is rejected (exchange sets: `in_memory_exchange_sets`).
#[test]
fn unsigned_s101_from_memory_is_rejected_under_required_signatures() {
    let root = repository();
    if !root.join(S101_CELL).is_file() || !root.join(S124_DATASET).is_file() {
        eprintln!("TestData not present; skipped");
        return;
    }
    let mut config = EngineConfig::new(
        Platform::in_memory(memory_source(&root)),
        mem("/in-memory/Catalogues"),
    );
    config.signatures = ferrite_engine::chart_engine::SignaturePolicy::Required;
    let mut engine = ChartEngine::new(config).unwrap();
    let error = engine
        .open(DatasetSource::Folder(mem("/in-memory/data")))
        .wait()
        .err()
        .expect("rejected");
    assert!(
        format!("{error:#}").contains("Unsigned dataset rejected"),
        "{error:#}"
    );
}

const S124_EXCHANGE: &str = "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT";
/// 2026-02-17T10:18:33Z, the exchange set's issue time (within the GB00
/// producer certificate validity 2025-06-16 .. 2026-06-16).
const S124_ISSUED: u64 = 1_771_323_513;

/// S-124 catalogues and a copy of the signed UKHO S-124 exchange set, in
/// memory only.
fn signed_exchange_source(root: &Path) -> MemoryDataSource {
    let native = NativeDataSource;
    let mut memory = MemoryDataSource::new();
    for product in ["FC/S-101", "PC/S-101", "FC/S-124", "PC/S-124"] {
        memory
            .copy_tree(
                &native,
                &root.join("Catalogues").join(product),
                &mem("/in-memory/Catalogues").join(product),
            )
            .unwrap();
    }
    let copied = memory
        .copy_tree(
            &native,
            &root.join(S124_EXCHANGE),
            &mem("/in-memory/xs/S100_ROOT"),
        )
        .unwrap();
    assert!(copied >= 7, "{copied}");
    memory
}

fn open_signed(
    memory: MemoryDataSource,
    at: Option<u64>,
) -> ferrite_engine::chart_engine::OpenReport {
    use ferrite_engine::platform::FixedClock;
    let root = repository();
    let mut platform = Platform::in_memory(memory);
    if let Some(seconds) = at {
        platform = platform.with_clock(std::sync::Arc::new(FixedClock(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
        )));
    }
    let mut config = EngineConfig::new(platform, mem("/in-memory/Catalogues"));
    config.signatures = ferrite_engine::chart_engine::SignaturePolicy::Required;
    config.trust_anchors = vec![(
        "IHO".into(),
        std::fs::read(root.join("Trust/IHO-S100-5.2.pem")).unwrap(),
    )];
    let mut engine = ChartEngine::new(config).unwrap();
    let job = engine.open(DatasetSource::Folder(mem("/in-memory/xs")));
    engine.install(job.wait().unwrap()).unwrap()
}

#[test]
fn required_signatures_verify_an_in_memory_exchange_set() {
    let root = repository();
    if !root.join(S124_EXCHANGE).join("CATALOG.SIGN").is_file()
        || !root.join("Trust/IHO-S100-5.2.pem").is_file()
    {
        eprintln!("TestData not present; skipped");
        return;
    }
    let memory = signed_exchange_source(&root);
    assert!(!memory.local_paths());

    // Verified at the issue time: every S-124 dataset opens as verified.
    let report = open_signed(memory.clone(), Some(S124_ISSUED));
    assert_eq!(report.datasets.len(), 5, "{report:#?}");
    assert!(
        report
            .datasets
            .iter()
            .all(|d| d.product == "S-124" && d.provenance == "signature verified"),
        "{report:#?}"
    );
    assert!(
        report.issues.iter().all(|i| i.check != "GML load"),
        "{:#?}",
        report.issues
    );

    // One altered byte in one dataset: the exchange set no longer verifies.
    let mut tampered = memory.clone();
    let dataset = &mem("/in-memory/xs/S100_ROOT/S-124/DATASET_FILES/124GB00S1Y2026N36.GML");
    let mut bytes = memory.read(dataset).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x20;
    tampered.insert(dataset, bytes);
    let report = open_signed(tampered, Some(S124_ISSUED));
    assert!(report.datasets.is_empty(), "{report:#?}");
    let rejected: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.check == "GML load")
        .collect();
    assert_eq!(rejected.len(), 5, "{:#?}", report.issues);
    assert!(
        rejected
            .iter()
            .any(|i| i.message.contains("Invalid data signature")),
        "{rejected:#?}"
    );

    // Today the producer certificate has expired: rejected, not downgraded.
    let report = open_signed(memory, None);
    assert!(report.datasets.is_empty(), "{report:#?}");
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.check == "GML load" && i.message.contains("expired")),
        "{:#?}",
        report.issues
    );
}
