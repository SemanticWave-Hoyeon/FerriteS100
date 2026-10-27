//! S-101 exchange sets opened from memory with required signatures: the same
//! verification, update planning and cancellation handling as the native
//! folder path, with the cancellation history in a memory user store; and an
//! S-421 route captured from memory.
use base64::{engine::general_purpose::STANDARD, Engine};
use ferrite_engine::chart_engine::{
    ChartEngine, DatasetSource, EngineConfig, OpenReport, SignaturePolicy,
};
use ferrite_engine::platform::{
    DataSource, FixedClock, MemoryDataSource, MemoryUserStore, NativeDataSource, NativeUserStore,
    Platform, MEMORY_HISTORY_KEY,
};
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    sign::Signer,
    x509::{
        extension::{BasicConstraints, KeyUsage},
        X509NameBuilder, X509,
    },
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 2026-03-26T00:00:00Z: after the SHOM May 2026 exchange sets were issued,
/// within their producer certificates.
const VERIFIED_AT: i64 = 1_774_483_200;
const SHOM_MAY: &str = "TestData/SHOM-S101/Data/diffusionS101_May_June/S-101_May2026";
const ED1ER2: &str = "S100_ENC_ROOT_AcrossTheCHannel_101FR0057130_ed1er2_2026-03-25T07_54_05";
#[cfg(feature = "s421")]
const S421_ROUTE: &str =
    "Standards/S-421/1.0-published-support/S-421_Ver1.0/TestData/RTE-TEST-GMIN.s421.gml";

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A platform-absolute in-memory key (memory paths are authorization keys).
fn mem(path: &str) -> PathBuf {
    ferrite_engine::platform::memory_path(path)
}

fn present(paths: &[&str]) -> bool {
    let root = repository();
    let ok = paths.iter().all(|p| root.join(p).exists())
        && root.join("Trust/IHO-S100-5.2.pem").is_file();
    if !ok {
        eprintln!("TestData not present; skipped");
    }
    ok
}

/// The S-101 catalogues under `/m/Catalogues`.
fn catalogues() -> MemoryDataSource {
    let root = repository();
    let mut memory = MemoryDataSource::new();
    for product in ["FC/S-101", "PC/S-101"] {
        memory
            .copy_tree(
                &NativeDataSource,
                &root.join("Catalogues").join(product),
                &mem("/m/Catalogues").join(product),
            )
            .unwrap();
    }
    memory
}

fn clock() -> Arc<FixedClock> {
    Arc::new(FixedClock(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(VERIFIED_AT as u64),
    ))
}

fn iho() -> (String, Vec<u8>) {
    (
        "IHO".into(),
        std::fs::read(repository().join("Trust/IHO-S100-5.2.pem")).unwrap(),
    )
}

fn required(platform: Platform, catalogues: impl Into<PathBuf>) -> ChartEngine {
    let mut config = EngineConfig::new(platform, catalogues);
    config.signatures = SignaturePolicy::Required;
    config.trust_anchors = vec![iho()];
    ChartEngine::new(config).unwrap()
}

fn memory_engine(memory: MemoryDataSource, user: &MemoryUserStore) -> ChartEngine {
    let mut platform = Platform::in_memory(memory).with_clock(clock());
    platform.user = Arc::new(user.clone());
    required(platform, mem("/m/Catalogues"))
}

fn native_engine(history: &Path) -> ChartEngine {
    let platform = Platform::native_with_user_store(NativeUserStore {
        history_path: history.to_path_buf(),
        navwarn_path: Arc::new(|_| None),
    })
    .with_clock(clock());
    required(platform, repository().join("Catalogues"))
}

/// What must agree between the native and the in-memory open: datasets
/// (name, edition.update, features, provenance), parsed source identities
/// and the exchange-set checks.
fn state(engine: &ChartEngine, report: &OpenReport) -> Vec<String> {
    let mut rows: Vec<String> = report
        .datasets
        .iter()
        .map(|d| {
            format!(
                "{} {} {} {} {}",
                d.product, d.name, d.edition, d.features, d.provenance
            )
        })
        .collect();
    let state = engine.state();
    for (index, cell) in state.s101.cells.iter().enumerate() {
        rows.push(format!(
            "{} {}.{} {:02x?}",
            cell.dsid.dataset_name,
            cell.dsid.edition_number,
            cell.dsid.update_number,
            state
                .s101
                .loaded_source_identities
                .get(index)
                .unwrap()
                .sha256()
        ));
    }
    let (issues, notices) = engine.exchange_set_checks();
    rows.extend(issues.iter().map(|i| format!("issue {i}")));
    rows.extend(notices);
    rows.sort();
    rows
}

#[test]
fn required_signatures_plan_in_memory_exchange_sets_like_the_native_folder_path() {
    if !present(&[SHOM_MAY]) {
        return;
    }
    let root = repository();
    let temp = tempfile::tempdir().unwrap();
    for folder in [SHOM_MAY.to_string(), format!("{SHOM_MAY}/{ED1ER2}")] {
        let mut native = native_engine(&temp.path().join("history.json"));
        let native_report = native
            .open_blocking(DatasetSource::Folder(root.join(&folder)))
            .unwrap();

        let mut memory = catalogues();
        memory
            .copy_tree(&NativeDataSource, &root.join(&folder), &mem("/xs"))
            .unwrap();
        assert!(!memory.local_paths());
        let user = MemoryUserStore::default();
        let mut engine = memory_engine(memory, &user);
        let report = engine
            .open_blocking(DatasetSource::Folder(mem("/xs")))
            .unwrap();

        assert!(!report.datasets.is_empty(), "{report:#?}");
        assert!(
            report
                .datasets
                .iter()
                .all(|d| d.provenance == "signature verified"),
            "{report:#?}"
        );
        assert!(report.issues.is_empty(), "{:#?}", report.issues);
        assert_eq!(state(&engine, &report), state(&native, &native_report));
        assert_eq!(report.notices.len(), native_report.notices.len());
        // Source paths are the memory identities, not files.
        assert!(engine
            .datasets()
            .iter()
            .all(|d| d.source.as_ref().is_some_and(|p| p.starts_with(mem("/xs")))));
        assert!(!temp.path().join("history.json").exists());
    }
    // The whole May set: ed2 of 101FR00571300 supersedes its edition 1
    // chains, plus three other edition 1 cells. ed1er2 alone: base + 2 updates.
    let mut memory = catalogues();
    memory
        .copy_tree(
            &NativeDataSource,
            &root.join(SHOM_MAY).join(ED1ER2),
            &mem("/xs"),
        )
        .unwrap();
    let mut engine = memory_engine(memory, &MemoryUserStore::default());
    engine
        .open_blocking(DatasetSource::Folder(mem("/xs")))
        .unwrap();
    let cell = &engine.state().s101.cells[0];
    assert_eq!(cell.dsid.dataset_name, "101FR00571300.000");
    assert_eq!((cell.dsid.edition_number, cell.dsid.update_number), (1, 2));
    assert_eq!(engine.state().s101.loaded_chain_paths[0].len(), 3);
}

/// The ed1er2 exchange set under `/xs/load`.
fn ed1er2_source() -> MemoryDataSource {
    let mut memory = catalogues();
    memory
        .copy_tree(
            &NativeDataSource,
            &repository().join(SHOM_MAY).join(ED1ER2),
            &mem("/xs/load"),
        )
        .unwrap();
    memory
}

const UPDATE_2: &str = "/xs/load/S100_ROOT/S-101/DATASET_FILES/101FR00571300/101FR00571300.002";

#[test]
fn tampered_update_in_memory_is_rejected() {
    if !present(&[SHOM_MAY]) {
        return;
    }
    let mut memory = ed1er2_source();
    let mut bytes = memory.read(&mem(UPDATE_2)).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x01;
    memory.insert(mem(UPDATE_2), bytes);
    let mut engine = memory_engine(memory, &MemoryUserStore::default());
    let error = engine
        .open_blocking(DatasetSource::Folder(mem("/xs")))
        .err()
        .expect("tampered update rejected");
    assert!(format!("{error:#}").contains("signature"), "{error:#}");
    assert!(engine.state().s101.cells.is_empty());

    // Removing the update from the exchange set does not downgrade either.
    let complete = ed1er2_source();
    let mut missing = MemoryDataSource::new();
    for entry in complete.walk(&mem("/")).filter_map(Result::ok) {
        if entry.kind == ferrite_engine::platform::EntryKind::File && entry.path != mem(UPDATE_2) {
            missing.insert(entry.path.clone(), complete.read(&entry.path).unwrap());
        }
    }
    let mut engine = memory_engine(missing, &MemoryUserStore::default());
    assert!(engine
        .open_blocking(DatasetSource::Folder(mem("/xs")))
        .is_err());
}

// ── A signed cancellation (synthetic producer, independently installed root) ──

const XC: &str = "http://www.iho.int/s100/xc/5.2";
const SE: &str = "http://www.iho.int/s100/se/5.2";
const CANCEL: &str = "/xs/cancel/S100_ROOT/S-101/DATASET_FILES/101FR00571300/101FR00571300.003";

fn key() -> PKey<Private> {
    PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::SECP384R1).unwrap()).unwrap())
        .unwrap()
}

fn certificate(name: &str, key: &PKey<Private>, issuer: Option<(&X509, &PKey<Private>)>) -> X509 {
    let ca = issuer.is_none();
    let mut subject = X509NameBuilder::new().unwrap();
    subject.append_entry_by_text("CN", name).unwrap();
    let subject = subject.build();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    let serial = BigNum::from_u32(if ca { 1 } else { 2 }).unwrap();
    b.set_serial_number(&serial.to_asn1_integer().unwrap())
        .unwrap();
    b.set_subject_name(&subject).unwrap();
    b.set_issuer_name(issuer.map(|i| i.0.subject_name()).unwrap_or(&subject))
        .unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(&Asn1Time::from_unix(VERIFIED_AT - 86_400).unwrap())
        .unwrap();
    b.set_not_after(&Asn1Time::from_unix(VERIFIED_AT + 86_400).unwrap())
        .unwrap();
    let mut constraints = BasicConstraints::new();
    constraints.critical();
    if ca {
        constraints.ca();
    }
    b.append_extension(constraints.build().unwrap()).unwrap();
    let mut usage = KeyUsage::new();
    usage.critical();
    if ca {
        usage.key_cert_sign().crl_sign();
    } else {
        usage.digital_signature();
    }
    b.append_extension(usage.build().unwrap()).unwrap();
    b.sign(issuer.map(|i| i.1).unwrap_or(key), MessageDigest::sha384())
        .unwrap();
    b.build()
}

fn sign(key: &PKey<Private>, bytes: &[u8]) -> String {
    let mut signer = Signer::new(MessageDigest::sha384(), key).unwrap();
    signer.update(bytes).unwrap();
    STANDARD.encode(signer.sign_to_vec().unwrap())
}

/// A physical S-101 B-7 cancellation: DDR and one DSID record (edition 0).
fn cancellation(filename: &str) -> Vec<u8> {
    let definition = b"000000000\x1fDataset identification\x1fRCNM!RCID!ENSP!ENED!PRSP!PRED!PROF!DSNM!DSTL!DSRD!DSLG!DSAB!DSED!*DSTC\x1f(b11,b14,7A(),A(8),3A(),b11)\x1e";
    let mut bytes = b"000003LE1 0000038 ! 4504".to_vec();
    bytes[..5].copy_from_slice(format!("{:05}", 38 + definition.len()).as_bytes());
    bytes.extend_from_slice(format!("DSID{:04}00000", definition.len()).as_bytes());
    bytes.push(0x1e);
    bytes.extend(definition);
    let mut field = vec![10, 1, 0, 0, 0];
    field.extend_from_slice(format!("S-100 Part 10a\x1f5.2\x1fINT.IHO.S-101.2.0\x1f2.0\x1f2\x1f{filename}\x1fCancelled\x1f20260325EN\x1f\x1f0\x1f").as_bytes());
    field.extend([14, 18, 0x1e]);
    let mut leader = b"000003DE1 0000038 ! 4504".to_vec();
    leader[..5].copy_from_slice(format!("{:05}", 38 + field.len()).as_bytes());
    bytes.extend(leader);
    bytes.extend_from_slice(format!("DSID{:04}00000", field.len()).as_bytes());
    bytes.push(0x1e);
    bytes.extend(field);
    bytes
}

/// A signed exchange set `/xs/cancel/S100_ROOT` cancelling 101FR00571300
/// (edition 1, update 3) and the PEM of its scheme administrator root.
fn signed_cancellation(memory: &mut MemoryDataSource) -> (String, Vec<u8>) {
    let root_key = key();
    let root = certificate("TEST ROOT", &root_key, None);
    let leaf_key = key();
    let leaf = certificate("TEST PRODUCER", &leaf_key, Some((&root, &root_key)));
    let container = format!(
        "<se:schemeAdministrator id=\"TEST\"/><se:certificate id=\"LEAF\" issuer=\"TEST\">{}</se:certificate>",
        STANDARD.encode(leaf.to_der().unwrap())
    );
    let data = cancellation("101FR00571300.003");
    let catalogue = format!(
        "<xc:S100_ExchangeCatalogue xmlns:xc=\"{XC}\" xmlns:se=\"{SE}\"><xc:certificates>{container}</xc:certificates><xc:datasetDiscoveryMetadata><xc:S100_DatasetDiscoveryMetadata><xc:fileName>file:/S-101/DATASET_FILES/101FR00571300/101FR00571300.003</xc:fileName><xc:purpose>cancellation</xc:purpose><xc:editionNumber>1</xc:editionNumber><xc:updateNumber>3</xc:updateNumber><xc:issueDate>2026-03-25</xc:issueDate><xc:productSpecification><xc:version>2.0</xc:version><xc:productIdentifier>S-101</xc:productIdentifier><xc:number>214</xc:number></xc:productSpecification><xc:digitalSignatureReference>ECDSA-384-SHA2</xc:digitalSignatureReference><xc:digitalSignatureValue><se:S100_SE_SignatureOnData id=\"DATA\" certificateRef=\"LEAF\" dataStatus=\"unencrypted\">{}</se:S100_SE_SignatureOnData></xc:digitalSignatureValue></xc:S100_DatasetDiscoveryMetadata></xc:datasetDiscoveryMetadata></xc:S100_ExchangeCatalogue>",
        sign(&leaf_key, &data)
    );
    let signature = format!(
        "<se:StandaloneDigitalSignature xmlns:se=\"{SE}\"><se:filename>CATALOG.XML</se:filename><se:certificates>{container}</se:certificates><se:digitalSignature id=\"CAT\" certificateRef=\"LEAF\">{}</se:digitalSignature></se:StandaloneDigitalSignature>",
        sign(&leaf_key, catalogue.as_bytes())
    );
    memory.insert(mem(CANCEL), data);
    memory.insert(
        mem("/xs/cancel/S100_ROOT/CATALOG.XML"),
        catalogue.into_bytes(),
    );
    memory.insert(
        mem("/xs/cancel/S100_ROOT/CATALOG.SIGN"),
        signature.into_bytes(),
    );
    ("TEST".into(), root.to_pem().unwrap())
}

#[test]
fn signed_cancellation_from_memory_removes_the_cell_and_persists_history() {
    if !present(&[SHOM_MAY]) {
        return;
    }
    let mut memory = ed1er2_source();
    let anchor = signed_cancellation(&mut memory);
    let user = MemoryUserStore::default();
    let mut platform = Platform::in_memory(memory).with_clock(clock());
    platform.user = Arc::new(user.clone());
    let mut config = EngineConfig::new(platform, mem("/m/Catalogues"));
    config.signatures = SignaturePolicy::Required;
    config.trust_anchors = vec![iho(), anchor];
    let mut engine = ChartEngine::new(config).unwrap();

    let report = engine
        .open_blocking(DatasetSource::Folder(mem("/xs/load")))
        .unwrap();
    assert_eq!(report.datasets.len(), 1, "{report:#?}");
    assert!(engine.cancellation_history().unwrap().is_empty());

    // A cancellation discovered in a folder is never applied.
    let report = engine
        .open_blocking(DatasetSource::Folder(mem("/xs/cancel")))
        .unwrap();
    assert!(report.cancellations.is_empty());
    assert!(report
        .notices
        .iter()
        .any(|n| n.contains("explicit selection required")));
    assert_eq!(engine.state().s101.cells.len(), 1);
    assert!(user.get(MEMORY_HISTORY_KEY).is_none());

    // Explicitly selected: verified, validated against the loaded edition 1
    // update 2 (authenticated issueDate 2026-03-24), applied and recorded.
    let report = engine
        .open_blocking(DatasetSource::Files(vec![mem(CANCEL)]))
        .unwrap();
    assert!(report.issues.is_empty(), "{:#?}", report.issues);
    assert_eq!(report.cancellations.len(), 1, "{report:#?}");
    let record = &report.cancellations[0];
    assert_eq!(record.key.1, "101FR00571300");
    assert_eq!(
        (record.cancelled_edition, record.cancellation_update),
        (1, 3)
    );
    assert!(engine.state().s101.cells.is_empty());
    assert!(engine.datasets().is_empty());
    let stored = String::from_utf8(user.get(MEMORY_HISTORY_KEY).unwrap()).unwrap();
    assert!(stored.contains("101FR00571300") && stored.contains("2026-03-25"));
    assert_eq!(engine.cancellation_history().unwrap().records().count(), 1);

    // The cancelled name with its older issueDate cannot come back.
    let error = engine
        .open_blocking(DatasetSource::Folder(mem("/xs/load")))
        .err()
        .expect("reuse rejected");
    assert!(
        format!("{error:#}").contains("cancellation history"),
        "{error:#}"
    );
    assert!(engine.state().s101.cells.is_empty());
    // Selected explicitly, the stale chain is skipped with a notice.
    let report = engine
        .open_blocking(DatasetSource::Files(vec![mem(UPDATE_2)]))
        .unwrap();
    assert!(report.datasets.is_empty(), "{report:#?}");
    assert!(report
        .notices
        .iter()
        .any(|n| n.contains("cancellation history")));

    // A second engine on the same user store sees the history; the repeated
    // announcement of an absent dataset is an exact replay.
    let mut memory = catalogues();
    let anchor = signed_cancellation(&mut memory);
    let mut platform = Platform::in_memory(memory).with_clock(clock());
    platform.user = Arc::new(user.clone());
    let mut config = EngineConfig::new(platform, mem("/m/Catalogues"));
    config.signatures = SignaturePolicy::Required;
    config.trust_anchors = vec![anchor];
    let mut engine = ChartEngine::new(config).unwrap();
    assert_eq!(engine.cancellation_history().unwrap().records().count(), 1);
    let report = engine
        .open_blocking(DatasetSource::Files(vec![mem(CANCEL)]))
        .unwrap();
    // A fresh synthetic producer signs different bytes than the recorded
    // one; the record itself (key, edition, update, date) is the same.
    assert_eq!(report.cancellations.len(), 1, "{report:#?}");
    assert!(report.notices.iter().any(|n| n.contains("announcement")));
}

#[test]
#[cfg(feature = "s421")]
fn s421_route_is_captured_from_memory() {
    let root = repository();
    if !root.join(S421_ROUTE).is_file() {
        eprintln!("S-421 test data not present; skipped");
        return;
    }
    let bytes = std::fs::read(root.join(S421_ROUTE)).unwrap();
    let mut memory = catalogues();
    memory.insert(mem("/routes/RTE-TEST-GMIN.s421.gml"), bytes.clone());

    let (routes, notices) =
        ferrite_engine::products::s421::discover_routes_in(&memory, &mem("/routes")).unwrap();
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(routes.len(), 1);
    assert_eq!(
        routes[0].path(),
        mem("/routes/RTE-TEST-GMIN.s421.gml").as_path()
    );
    assert_eq!(routes[0].allowed_xml(false).unwrap().as_bytes(), &bytes[..]);

    let mut engine = ChartEngine::new(EngineConfig::new(
        Platform::in_memory(memory.clone()),
        mem("/m/Catalogues"),
    ))
    .unwrap();
    let report = engine
        .open_blocking(DatasetSource::Folder(mem("/routes")))
        .unwrap();
    assert_eq!(engine.routes().len(), 1, "{report:#?}");
    assert_eq!(engine.routes()[0].sha256(), routes[0].sha256());
    assert!(report.notices.iter().any(|n| n.contains("S-421 route")));
    // Opening it again is a no-op.
    let report = engine
        .open_blocking(DatasetSource::Files(vec![mem(
            "/routes/RTE-TEST-GMIN.s421.gml",
        )]))
        .unwrap();
    assert_eq!(engine.routes().len(), 1);
    assert!(report.notices.iter().any(|n| n.contains("Already open")));

    // Required signatures: bare GML routes are not admitted.
    let mut engine = required(Platform::in_memory(memory), mem("/m/Catalogues"));
    let report = engine
        .open_blocking(DatasetSource::Folder(mem("/routes")))
        .unwrap();
    assert!(engine.routes().is_empty());
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("Authenticated S421")),
        "{report:#?}"
    );
}
