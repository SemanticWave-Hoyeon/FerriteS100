//! Non-UI parts of the S-124 / S-129 overlay: feature keys, dataset and
//! exchange-set discovery, the per-user S-124 state file, the viewing time
//! and the Part 9a conversion of the XSLT display list to host instructions.
//! The overlay registry and its user-interface glue stay in the application.
use anyhow::Result;
use chrono::{DateTime, Utc};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{DrawingInstruction, RenderContext, Viewport, WorldPoint};
use ferrite_s100_gml::navwarn::{NavwarnUserState, UserClassification};
use ferrite_s100_gml::portrayal::{self, FeatureInstructions, PortrayalInput};
use ferrite_s100_gml::{Geometry, Product, Severity};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Set on every overlay feature key (S-101 record keys stay below 2^40).
pub const OVERLAY_KEY_FLAG: i64 = 1 << 62;
const SLOT_SHIFT: u32 = 24;
const FEATURE_MASK: i64 = (1 << SLOT_SHIFT) - 1;
const SLOT_MASK: i64 = (1 << 38) - 1;
/// Always displayed viewing group of host overlays (DISPLBASE).
pub const OVERLAY_VIEWING_GROUP: u32 = 21010;

pub fn overlay_key(slot: u32, feature: usize) -> i64 {
    OVERLAY_KEY_FLAG | ((slot as i64 & SLOT_MASK) << SLOT_SHIFT) | (feature as i64 & FEATURE_MASK)
}

pub fn decode_key(key: i64) -> Option<(u32, usize)> {
    let slot = ((key >> SLOT_SHIFT) & SLOT_MASK) as u32;
    (key & OVERLAY_KEY_FLAG != 0 && key > 0).then_some((slot, (key & FEATURE_MASK) as usize))
}

// ─────────────────────────────────────────────────────────────────────────
// Discovery
// ─────────────────────────────────────────────────────────────────────────

const IMPORT_EXTENSIONS: [&str; 3] = ["kml", "gpx", "shp"];

/// S-124 / S-129 from the root element of a GML file (namespace or
/// productIdentifier in the first 64 KiB).
pub fn sniff_product(path: &Path) -> Option<Product> {
    use std::io::Read;
    if !(crate::dataset_discovery::is_dataset_file(path, "gml")
        || crate::dataset_discovery::is_dataset_file(path, "xml"))
        || path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
    {
        return None;
    }
    let mut head = Vec::with_capacity(64 * 1024);
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    let text = String::from_utf8_lossy(&head);
    let mut reader = quick_xml::Reader::from_str(text.trim_start_matches('\u{feff}'));
    loop {
        use quick_xml::events::Event;
        match reader.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) => {
                if e.local_name().as_ref() != b"Dataset" {
                    return None;
                }
                for attr in e.attributes().flatten() {
                    let value = String::from_utf8_lossy(&attr.value);
                    if attr.key.as_ref().starts_with(b"xmlns") {
                        if value.contains("/S124/") {
                            return Some(Product::S124);
                        }
                        if value.contains("/S129/") {
                            return Some(Product::S129);
                        }
                    }
                }
                break;
            }
            Event::Eof => return None,
            _ => {}
        }
    }
    for product in [Product::S124, Product::S129] {
        if text.contains(&format!("productIdentifier>{}<", product.identifier())) {
            return Some(product);
        }
    }
    None
}

pub fn is_import_file(path: &Path, allow_zip: bool) -> bool {
    IMPORT_EXTENSIONS
        .iter()
        .any(|e| crate::dataset_discovery::is_dataset_file(path, e))
        || (allow_zip && crate::dataset_discovery::is_dataset_file(path, "zip"))
}

/// An explicitly selected file that opens as an overlay dataset.
pub fn is_overlay_file(path: &Path) -> bool {
    is_import_file(path, true) || sniff_product(path).is_some()
}

/// Overlay datasets under a folder: S-124/S-129 GML and imported KML, GPX
/// and extracted shapefiles. ZIP archives are opened only as explicit files
/// (a folder may hold both an archive and its extracted copy).
///
/// `on_system` holds the file names of the S-124/S-129 datasets already
/// loaded and not cancelled ([`GmlOverlays::on_system`]); exchange-set check
/// S-158-129 19 (cancellation of a dataset not on the system) uses it.
pub fn discover(root: &Path, on_system: &HashSet<String>) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut catalogues = Vec::new();
    for (count, entry) in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        anyhow::ensure!(count < 100_000, "Overlay discovery exceeds entry limit");
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
        {
            catalogues.extend(path.parent().map(Path::to_path_buf));
        } else if is_import_file(path, false) || sniff_product(path).is_some() {
            found.push(entry.into_path());
            anyhow::ensure!(found.len() <= 4096, "Overlay dataset inputs exceed 4096");
        }
    }
    found.sort();
    // A cancellation-only exchange set holds no dataset files; it is still
    // checked (S-101-only sets report no S-124/S-129 products and are skipped).
    log_exchange_set_checks(exchange_set_reports(catalogues, &found, on_system));
    Ok(found)
}

/// Base file name of an S-100 file URI or path, as check 19 compares them.
fn uri_base_name(uri: &str) -> &str {
    uri.rsplit(['/', '\\', ':']).next().unwrap_or(uri)
}

/// Exchange-set validation (S-158-129 11, 17-20; S-124 9.5-9.8, 12) of the
/// exchange sets under an opened folder. Check 19 sees the datasets on the
/// system: `on_system`, the S-124/S-129 files opened in the same batch from
/// outside the exchange set, less those cancelled by an exchange set
/// validated earlier in the batch.
pub fn exchange_set_reports(
    mut roots: Vec<PathBuf>,
    found: &[PathBuf],
    on_system: &HashSet<String>,
) -> Vec<(
    PathBuf,
    Result<ferrite_s100_gml::exchange::ExchangeSetReport>,
)> {
    roots.sort();
    let mut outer: Vec<PathBuf> = Vec::new();
    for root in roots {
        // A nested CATALOG.XML belongs to the enclosing exchange set.
        if !outer.iter().any(|o| root.starts_with(o)) {
            outer.push(root);
        }
    }
    let batch: Vec<&PathBuf> = found
        .iter()
        .filter(|p| sniff_product(p).is_some())
        .collect();
    let mut cancelled: HashSet<String> = HashSet::new();
    let mut reports = Vec::new();
    for root in outer {
        let mut system = on_system.clone();
        system.extend(
            batch
                .iter()
                .filter(|p| !p.starts_with(&root))
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned()),
        );
        system.retain(|n| !cancelled.contains(&n.to_ascii_uppercase()));
        let report = ferrite_s100_gml::exchange::validate_exchange_set(&root, Some(&system));
        if let Ok(r) = &report {
            for d in r.catalogue.iter().flat_map(|c| &c.datasets) {
                if d.is_cancellation() {
                    cancelled.insert(uri_base_name(&d.file_name).to_ascii_uppercase());
                }
            }
        }
        reports.push((root, report));
    }
    reports
}

/// Logs exchange-set validation like dataset validation.
fn log_exchange_set_checks(
    reports: Vec<(
        PathBuf,
        Result<ferrite_s100_gml::exchange::ExchangeSetReport>,
    )>,
) {
    for (root, report) in reports {
        let report = match report {
            Ok(r) if !r.products.is_empty() => r,
            Ok(_) => continue,
            Err(e) => {
                warn!("Exchange set {}: validation failed: {e:#}", root.display());
                continue;
            }
        };
        let errors = report
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .count();
        info!(
            "Exchange set {}: {} validation errors, {} warnings",
            root.display(),
            errors,
            report.issues.len() - errors
        );
        let mut by_check: BTreeMap<(&str, bool), usize> = BTreeMap::new();
        for issue in &report.issues {
            *by_check
                .entry((issue.check.as_str(), issue.severity == Severity::Error))
                .or_default() += 1;
        }
        for ((check, error), count) in by_check {
            let kind = if error { "errors" } else { "warnings" };
            warn!("Exchange set: {count} validation {kind} [{check}]");
        }
        for issue in report
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .take(50)
        {
            warn!("Exchange set: {issue}");
        }
        for issue in report
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
        {
            tracing::debug!("Exchange set: {issue}");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Per-user persistence (S-124 11.1.1: user interface linked to an individual
// user, so that classifications and indications survive watch handover)
// ─────────────────────────────────────────────────────────────────────────

pub struct UserStateStore {
    pub path: PathBuf,
    pub user: String,
    pub status: String,
}

const USER_STATE_VERSION: u8 = 1;
const MAX_USER_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_USER_STATE_ENTRIES: usize = 20_000;
const MAX_USER_STATE_KEY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredClassification {
    OnChart,
    OffChart,
    Information,
}

/// On-disk form of [`NavwarnUserState`]; classifications keyed by message
/// identity key ([`ferrite_s100_gml::navwarn::MessageInfo::key`]), the rest by dataset key.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UserStateDocument {
    version: u8,
    /// OS user the state belongs to.
    user: String,
    saved: DateTime<Utc>,
    classification: BTreeMap<String, StoredClassification>,
    displayed: BTreeSet<String>,
    received: BTreeMap<String, DateTime<Utc>>,
    /// "Correct known PC errata" option (absent in older files: off).
    #[serde(default)]
    correct_pc_errata: bool,
}

/// A user state file read back.
pub struct StoredUserState {
    pub state: NavwarnUserState,
    /// OS user the state was saved for.
    pub user: String,
    pub correct_pc_errata: bool,
}

/// The stored state and the user it was saved for; None when there is no file.
pub fn read_user_state(path: &Path) -> Result<Option<StoredUserState>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_USER_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_USER_STATE_BYTES,
        "S-124 user state exceeds {MAX_USER_STATE_BYTES} bytes"
    );
    let doc: UserStateDocument = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        doc.version == USER_STATE_VERSION,
        "Unsupported S-124 user state version {}",
        doc.version
    );
    anyhow::ensure!(
        doc.classification.len() <= MAX_USER_STATE_ENTRIES
            && doc.displayed.len() <= MAX_USER_STATE_ENTRIES
            && doc.received.len() <= MAX_USER_STATE_ENTRIES,
        "S-124 user state has too many entries"
    );
    let key_ok = |k: &String| !k.is_empty() && k.len() <= MAX_USER_STATE_KEY;
    anyhow::ensure!(
        doc.classification.keys().all(key_ok)
            && doc.displayed.iter().all(key_ok)
            && doc.received.keys().all(key_ok),
        "S-124 user state has an invalid key"
    );
    let state = NavwarnUserState {
        classification: doc
            .classification
            .into_iter()
            .filter_map(|(k, c)| {
                let c = match c {
                    StoredClassification::OnChart => return None,
                    StoredClassification::OffChart => UserClassification::OffChart,
                    StoredClassification::Information => UserClassification::Information,
                };
                Some((k, c))
            })
            .collect(),
        displayed: doc.displayed.into_iter().collect(),
        received: doc.received.into_iter().collect(),
    };
    Ok(Some(StoredUserState {
        state,
        user: doc.user,
        correct_pc_errata: doc.correct_pc_errata,
    }))
}

/// Replace the stored state atomically (temporary file, then rename).
pub fn write_user_state(
    path: &Path,
    user: &str,
    state: &NavwarnUserState,
    correct_pc_errata: bool,
) -> Result<()> {
    use std::io::Write;
    let mut classification: Vec<_> = state.classification.iter().collect();
    classification.sort();
    let mut received: Vec<_> = state.received.iter().collect();
    // Newest receipts are kept when over budget.
    received.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let doc = UserStateDocument {
        version: USER_STATE_VERSION,
        user: user.to_owned(),
        saved: Utc::now(),
        classification: classification
            .into_iter()
            .filter(|(k, _)| k.len() <= MAX_USER_STATE_KEY)
            .take(MAX_USER_STATE_ENTRIES)
            .map(|(k, c)| {
                let c = match c {
                    UserClassification::OnChart => StoredClassification::OnChart,
                    UserClassification::OffChart => StoredClassification::OffChart,
                    UserClassification::Information => StoredClassification::Information,
                };
                (k.clone(), c)
            })
            .collect(),
        displayed: state
            .displayed
            .iter()
            .filter(|k| k.len() <= MAX_USER_STATE_KEY)
            .take(MAX_USER_STATE_ENTRIES)
            .cloned()
            .collect(),
        received: received
            .into_iter()
            .filter(|(k, _)| k.len() <= MAX_USER_STATE_KEY)
            .take(MAX_USER_STATE_ENTRIES)
            .map(|(k, t)| (k.clone(), *t))
            .collect(),
        correct_pc_errata,
    };
    let bytes = serde_json::to_vec_pretty(&doc)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temporary = parent.join(format!(".navwarn-state-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Keep an unreadable state file for inspection instead of overwriting it.
pub fn set_aside(path: &Path) {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".unreadable-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    if let Err(error) = std::fs::rename(path, &name) {
        warn!("Could not set aside {}: {error}", path.display());
    }
}

/// Viewing time of the S-101 date-dependent portrayal setting as an instant:
/// a fixed date and time, or a fixed date at 00:00 of the source offset (as
/// the S-101 date visibility evaluates it). None (system clock) for the live
/// clock, "show all dates" and unreadable values.
pub fn viewing_time(settings: &ferrite_render::DisplaySettings) -> Option<DateTime<Utc>> {
    if !settings.date_dependent {
        return None;
    }
    if let Some(value) = settings.current_datetime.as_deref() {
        return ferrite_kernel::parse_viewing_instant(value)
            .ok()
            .map(|t| t.with_timezone(&Utc));
    }
    let date = ferrite_kernel::parse_viewing_date(settings.current_date.as_deref()?).ok()?;
    let offset = chrono::FixedOffset::east_opt(settings.local_time_offset_seconds)?;
    date.and_hms_opt(0, 0, 0)?
        .and_local_timezone(offset)
        .single()
        .map(|t| t.with_timezone(&Utc))
}

pub fn geometry_paths(g: &Geometry, out: &mut Vec<Vec<WorldPoint>>) {
    let path = |c: &[[f64; 2]]| c.iter().map(|p| WorldPoint::new(p[0], p[1])).collect();
    match g {
        Geometry::Surface {
            exterior,
            interiors,
        } => {
            out.push(path(exterior));
            out.extend(interiors.iter().map(|r| path(r)));
        }
        Geometry::Curve(c) => out.push(path(c)),
        Geometry::Point(p) => out.push(marker(*p)),
        Geometry::MultiPoint(points) => out.extend(points.iter().map(|p| marker(*p))),
    }
}

/// A small closed diamond around a point (about 100 m).
fn marker(p: [f64; 2]) -> Vec<WorldPoint> {
    let d = 0.001;
    vec![
        WorldPoint::new(p[0] - d, p[1]),
        WorldPoint::new(p[0], p[1] + d),
        WorldPoint::new(p[0] + d, p[1]),
        WorldPoint::new(p[0], p[1] - d),
        WorldPoint::new(p[0] - d, p[1]),
    ]
}

/// Part 9a strings of one dataset to host instructions marked as overlays.
pub fn convert(
    pc: &PortrayalCatalogue,
    input: &PortrayalInput,
    features: &[&FeatureInstructions],
    slot: u32,
    profile: &str,
    out: &mut Vec<DrawingInstruction>,
) {
    if features.is_empty() {
        return;
    }
    let key_of = |i: usize| overlay_key(slot, i);
    let cell = portrayal::synthetic_cell(input, key_of);
    let mut results = Vec::with_capacity(features.len());
    for f in features {
        match ferrite_lua::PortrayalResult::parse(
            &f.host_feature_id(key_of(f.feature)),
            &f.instructions,
            "",
        ) {
            Ok(r) => results.push(r),
            Err(error) => warn!(
                "{} {}: drawing instructions rejected: {error:#}",
                input.dataset_name, f.feature_id
            ),
        }
    }
    let new_context = || RenderContext::new(Viewport::new(1.0, 1.0));
    let mut context = new_context();
    if let Err(error) =
        ferrite_s101::convert_lua_results_for_cell(&results, &cell, pc, &mut context, 0, profile)
    {
        // One bad feature must not hide the rest of the dataset.
        warn!(
            "{}: overlay conversion failed ({error:#}); converting features separately",
            input.dataset_name
        );
        context = new_context();
        for result in &results {
            let mut single = new_context();
            match ferrite_s101::convert_lua_results_for_cell(
                std::slice::from_ref(result),
                &cell,
                pc,
                &mut single,
                0,
                profile,
            ) {
                Ok(()) => context.append_converted(single.raw_instructions().to_vec()),
                Err(error) => warn!(
                    "{} {}: feature not portrayed: {error:#}",
                    input.dataset_name, result.feature_id
                ),
            }
        }
    }
    for instruction in context.raw_instructions() {
        let mut instruction = instruction.clone();
        instruction.set_cell_index(None);
        instruction.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
        let (plane, priority) = (instruction.display_plane(), instruction.priority().0);
        instruction.set_display_parameters(plane, priority, OVERLAY_VIEWING_GROUP);
        match instruction {
            // The host draws single-line labels; the S-129 information box
            // is several lines (Part 9 text may contain line feeds).
            DrawingInstruction::Text(text) if text.text.contains('\n') => {
                let line_mm = text.font_size as f64 * 25.4 / 72.0 * 1.2;
                for (i, line) in text.text.lines().enumerate() {
                    let mut t = text.clone();
                    t.text = line.trim_end().to_owned();
                    t.offset.y -= (line_mm * i as f64) as f32;
                    if !t.text.is_empty() {
                        out.push(DrawingInstruction::Text(t));
                    }
                }
            }
            instruction => out.push(instruction),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_gml::navwarn;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn keys_round_trip_and_never_collide_with_record_keys() {
        for (slot, feature) in [(1u32, 0usize), (7, 123), (u32::MAX >> 1, (1 << 24) - 1)] {
            let key = overlay_key(slot, feature);
            assert!(key > 0);
            assert_eq!(decode_key(key), Some((slot & SLOT_MASK as u32, feature)));
        }
        // S-101 record keys: rcnm << 32 | rcid.
        let record = ferrite_s100_core::RecordId::new(100, u32::MAX).key();
        assert_eq!(decode_key(record), None);
        assert_eq!(decode_key(0), None);
    }

    #[test]
    fn discovery_classifies_products_and_imports() {
        let s124 = root().join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S2Y2026N43.GML",
        );
        let s129 = root().join("TestData/IHO-S129-Test-Datasets/12900MCTDS130TS.gml");
        if !s124.exists() || !s129.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        assert_eq!(sniff_product(&s124), Some(Product::S124));
        assert_eq!(sniff_product(&s129), Some(Product::S129));
        assert_eq!(
            sniff_product(&root().join("Catalogues/FC/S-124/124_FC_2.0.0.xml")),
            None
        );
        let found = discover(
            &root().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB"),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(found.len(), 5);
        let shom = root().join("TestData/SHOM-PING-S124");
        if shom.exists() {
            let found = discover(&shom, &HashSet::new()).unwrap();
            // 4 KML, 1 GPX, 1 extracted shapefile; the ZIP only when selected.
            assert_eq!(found.len(), 6, "{found:?}");
            assert!(is_overlay_file(&shom.join("shapefile/shapefiles.zip")));
        }
    }

    /// The exchange-set signature path the app uses for S-101 is generic:
    /// S-124 datasets listed in a signed CATALOG.XML go through it unchanged.
    #[test]
    fn s124_exchange_set_signatures_verify() {
        use ferrite_security::{authorize_datasets, TrustAnchors, UnsignedPolicy};
        let base = root();
        let dir = base.join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT");
        if !dir.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut anchors = TrustAnchors::default();
        anchors
            .install_pem(
                "IHO",
                &std::fs::read(base.join("Trust/IHO-S100-5.2.pem")).unwrap(),
            )
            .unwrap();
        let paths = discover(&dir, &HashSet::new()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        match authorize_datasets(&paths, &anchors, now, UnsignedPolicy::Reject) {
            Ok(report) => {
                assert_eq!(report.signed_count, paths.len());
                assert!(report.snapshots.values().all(|s| s.is_some()));
            }
            // Known: the UKHO 2026 S-124 set does not verify today. Its GB00
            // certificate (valid 2025-06-16..2026-06-16) has expired, some
            // signatures lack an issueDate, and older ferrite-security builds
            // also rejected an unused, unchained "Chersoft" certificate. The
            // set must fail closed; the reason is not part of this contract.
            Err(error) => assert!(
                ["expired", "issueDate", "Missing intermediate certificate"]
                    .iter()
                    .any(|reason| format!("{error:#}").contains(reason)),
                "{error:#}"
            ),
        }
    }

    fn t(s: &str) -> DateTime<Utc> {
        navwarn::parse_date_time(s).unwrap()
    }

    #[test]
    fn viewing_time_matches_s101_date_selection() {
        use ferrite_render::DisplaySettings;
        let mut s = DisplaySettings {
            date_dependent: true,
            current_date: None,
            current_datetime: None,
            ..Default::default()
        };
        assert_eq!(viewing_time(&s), None, "live clock");
        s.current_date = Some("2026-02-15".into());
        s.local_time_offset_seconds = 3600;
        assert_eq!(viewing_time(&s), Some(t("2026-02-14T23:00:00Z")));
        s.current_datetime = Some("20260215T120000Z".into());
        assert_eq!(viewing_time(&s), Some(t("2026-02-15T12:00:00Z")));
        s.current_datetime = Some("garbage".into());
        assert_eq!(viewing_time(&s), None);
        s.current_datetime = None;
        s.date_dependent = false;
        assert_eq!(viewing_time(&s), None, "show all dates");
        // The same conversion the temporal view applies.
        let view = ferrite_render::TemporalView {
            mode: ferrite_render::TemporalViewMode::Instant,
            date: "2026-02-15".into(),
            instant: "2026-02-15T06:30:00Z".into(),
            source_offset: "Z".into(),
        };
        let mut s = DisplaySettings::default();
        view.apply(&mut s).unwrap();
        assert_eq!(viewing_time(&s), Some(t("2026-02-15T06:30:00Z")));
    }
}
