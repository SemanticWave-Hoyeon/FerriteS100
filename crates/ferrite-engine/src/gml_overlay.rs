//! Non-UI parts of the S-124 / S-129 overlay: feature keys, dataset and
//! exchange-set discovery, the per-user S-124 state file, the viewing time
//! and the Part 9a conversion of the XSLT display list to host instructions.
//! The overlay registry ([`GmlOverlays`]) loads the product catalogues and
//! datasets, evaluates the S-124 lifecycle and portrays the datasets; its
//! user-interface glue (rows, symbol registration) stays in the application.
use crate::platform::{
    Clock, DataSource, EntryKind, FileRecord, NativeDataSource, Platform, SourceModules, UserRecord,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::host_settings::CatalogueStatus;
use ferrite_render::{DrawingInstruction, GeoBounds, RenderContext, Viewport, WorldPoint};
use ferrite_s100_gml::navwarn::{
    self, NavwarnInput, NavwarnRegistry, NavwarnUserState, PartTiming, PortrayalPolicy, SortOrder,
    Status, UserClassification,
};
use ferrite_s100_gml::portrayal::{
    self, ConvertOptions, FeatureInstructions, InputOptions, PortrayalInput, XsltPortrayal,
};
use ferrite_s100_gml::{Dataset, Geometry, ObjectKind, Product, Severity, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    sniff_product_in(&NativeDataSource, path)
}

/// [`sniff_product`] through a data source.
pub fn sniff_product_in(data: &dyn DataSource, path: &Path) -> Option<Product> {
    if !sniffable(path) {
        return None;
    }
    let head = data.read_limited(path, 64 * 1024 - 1).ok()?;
    sniff_head(&head)
}

/// [`sniff_product`] of a dataset already in memory.
pub fn sniff_product_bytes(path: &Path, bytes: &[u8]) -> Option<Product> {
    if !sniffable(path) {
        return None;
    }
    sniff_head(&bytes[..bytes.len().min(64 * 1024)])
}

fn sniffable(path: &Path) -> bool {
    (crate::dataset_discovery::is_dataset_file(path, "gml")
        || crate::dataset_discovery::is_dataset_file(path, "xml"))
        && !path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
}

fn sniff_head(head: &[u8]) -> Option<Product> {
    let text = String::from_utf8_lossy(head);
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
    discover_in(&NativeDataSource, root, on_system)
}

/// [`discover`] through a data source. Exchange-set validation reads the
/// exchange set by path and runs only on a source of local files (stage 5).
pub fn discover_in(
    data: &dyn DataSource,
    root: &Path,
    on_system: &HashSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut catalogues = Vec::new();
    for (count, entry) in data.walk(root).enumerate() {
        anyhow::ensure!(count < 100_000, "Overlay discovery exceeds entry limit");
        let entry = entry?;
        if entry.kind != EntryKind::File {
            continue;
        }
        let path = entry.path.as_path();
        if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("CATALOG.XML"))
        {
            catalogues.extend(path.parent().map(Path::to_path_buf));
        } else if is_import_file(path, false) || sniff_product_in(data, path).is_some() {
            found.push(entry.path);
            anyhow::ensure!(found.len() <= 4096, "Overlay dataset inputs exceed 4096");
        }
    }
    found.sort();
    // A cancellation-only exchange set holds no dataset files; it is still
    // checked (S-101-only sets report no S-124/S-129 products and are skipped).
    if data.local_paths() {
        log_exchange_set_checks(exchange_set_reports(catalogues, &found, on_system));
    } else if !catalogues.is_empty() {
        info!("Exchange-set validation skipped: needs local files (stage 5)");
    }
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
    pub record: Box<dyn UserRecord>,
    pub user: String,
    pub status: String,
}

fn user_state_file(path: &Path) -> FileRecord {
    FileRecord {
        path: path.to_path_buf(),
        temporary_prefix: ".navwarn-state",
    }
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
    read_user_record(&user_state_file(path))
}

/// [`read_user_state`] from any user record.
pub fn read_user_record(record: &dyn UserRecord) -> Result<Option<StoredUserState>> {
    let Some(bytes) = record.read(MAX_USER_STATE_BYTES)? else {
        return Ok(None);
    };
    parse_user_state(&bytes).map(Some)
}

/// Decode a stored user state document.
pub fn parse_user_state(bytes: &[u8]) -> Result<StoredUserState> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_USER_STATE_BYTES,
        "S-124 user state exceeds {MAX_USER_STATE_BYTES} bytes"
    );
    let doc: UserStateDocument = serde_json::from_slice(bytes)?;
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
    Ok(StoredUserState {
        state,
        user: doc.user,
        correct_pc_errata: doc.correct_pc_errata,
    })
}

/// Replace the stored state atomically (temporary file, then rename).
pub fn write_user_state(
    path: &Path,
    user: &str,
    state: &NavwarnUserState,
    correct_pc_errata: bool,
) -> Result<()> {
    let bytes = encode_user_state(user, state, correct_pc_errata, Utc::now())?;
    user_state_file(path).write(&bytes)?;
    Ok(())
}

/// The stored document of a user state, saved at `saved`.
pub fn encode_user_state(
    user: &str,
    state: &NavwarnUserState,
    correct_pc_errata: bool,
    saved: DateTime<Utc>,
) -> Result<Vec<u8>> {
    let mut classification: Vec<_> = state.classification.iter().collect();
    classification.sort();
    let mut received: Vec<_> = state.received.iter().collect();
    // Newest receipts are kept when over budget.
    received.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let doc = UserStateDocument {
        version: USER_STATE_VERSION,
        user: user.to_owned(),
        saved,
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
    Ok(serde_json::to_vec_pretty(&doc)?)
}

/// Keep an unreadable state file for inspection instead of overwriting it.
pub fn set_aside(path: &Path) {
    set_aside_record(&user_state_file(path), Utc::now());
}

fn set_aside_record(record: &dyn UserRecord, now: DateTime<Utc>) {
    record.set_aside(&format!(".unreadable-{}", now.format("%Y%m%dT%H%M%SZ")));
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

// ─────────────────────────────────────────────────────────────────────────
// Overlay registry: catalogues, loaded datasets, lifecycle and portrayal
// ─────────────────────────────────────────────────────────────────────────

/// Registers a product's portrayal catalogue as the symbol source of its
/// namespaced overlay symbols (the host's renderer resolves them); false
/// rejects the product prefix.
pub type SymbolRegistrar = fn(&str, &ferrite_portrayal_catalog::BoundPortrayalCatalogue) -> bool;

/// Viewing groups used by the S-124 2.0.0 XSLT rules but not declared in its
/// portrayal_catalogue.xml (TextPlacement text and co-located part counts).
const S124_IMPLICIT_GROUPS: [u32; 2] = [26, 12210];

// ─────────────────────────────────────────────────────────────────────────
// Catalogues
// ─────────────────────────────────────────────────────────────────────────

pub struct ProductCatalogues {
    pub fc: Arc<FeatureCatalogue>,
    /// Catalogue source bytes for validation, when not read by path.
    fc_source: Option<Arc<[u8]>>,
    xslt: XsltPortrayal,
    /// Catalogue used for Part 9a conversion: symbol references namespaced
    /// with the product prefix, implicit viewing groups declared (so the
    /// errata correction can pass them; literal portrayal never emits them).
    pc: PortrayalCatalogue,
    /// Viewing groups the rules use but the catalogue does not declare.
    /// Instructions in them are not drawn unless the user enables
    /// [`GmlOverlays::correct_pc_errata`].
    implicit_groups: HashSet<String>,
    pub fc_status: CatalogueStatus,
    pub pc_status: CatalogueStatus,
}

impl ProductCatalogues {
    fn load(
        data: &dyn DataSource,
        catalogues: &Path,
        product: Product,
        register_symbols: SymbolRegistrar,
    ) -> Result<Self> {
        let id = product.identifier();
        let fc_dir = catalogues.join("FC").join(id);
        let fc_file = crate::dataset_discovery::feature_catalogue_file_in(data, &fc_dir)
            .with_context(|| format!("{id} feature catalogue in {}", fc_dir.display()))?;
        tracing::info!("Loading Feature Catalogue: {}", fc_file.display());
        let fc_bytes = data
            .read_limited(&fc_file, FeatureCatalogue::MAX_XML_SIZE)
            .map_err(ferrite_feature_catalog::FCError::from)
            .with_context(|| format!("{id} feature catalogue {}", fc_file.display()))?;
        let fc = FeatureCatalogue::from_bytes_with_extras(&fc_file, &fc_bytes)
            .map(|(fc, _)| fc)
            .with_context(|| format!("{id} feature catalogue {}", fc_file.display()))?;
        anyhow::ensure!(
            fc.product_id.is_empty() || fc.product_id == id,
            "Expected {id} feature catalogue, found {}",
            fc.product_id
        );
        let pc_dir = catalogues.join("PC").join(id);
        let load_pc = || {
            data.portrayal_sources(&pc_dir)
                .and_then(|sources| PortrayalCatalogue::bound_from_sources(&pc_dir, sources))
                .with_context(|| format!("{id} portrayal catalogue {}", pc_dir.display()))
        };
        let bound = load_pc()?;
        anyhow::ensure!(
            register_symbols(id, &bound),
            "Invalid overlay symbol prefix {id}"
        );
        let mut pc = load_pc()?.into_catalogue();
        pc.namespace_symbol_references(id);
        let xslt = XsltPortrayal::load_from(
            &SourceModules(data),
            &pc_dir,
            ferrite_xslt::Limits::default(),
        )?;
        let declared_groups = pc.viewing_groups.groups.len();
        let mut implicit_groups = HashSet::new();
        if product == Product::S124 {
            for group in S124_IMPLICIT_GROUPS {
                if xslt.info.viewing_groups.contains(&group.to_string()) {
                    continue;
                }
                // Literal Part 9: the group cannot be enabled, so its
                // instructions (TextPlacement text, co-located part counts)
                // are not drawn. The host catalogue declares it anyway so
                // the errata correction can draw them on request.
                pc.viewing_groups
                    .declare_implicit(group, &format!("{id} implicit viewing group {group}"));
                implicit_groups.insert(group.to_string());
                warn!(
                    "Conformance note: {id} {} rules use viewing group {group}, which portrayal_catalogue.xml does not declare; its instructions are not drawn (S-100 Part 9 9-11.2.2). \"Correct known PC errata\" draws them as an implicitly declared overlay group",
                    pc.version
                );
            }
        }
        let fc_status = CatalogueStatus {
            loaded: true,
            product_id: id.into(),
            version: fc.version.clone(),
            path: fc_file.display().to_string(),
            item_count: fc.feature_types.len(),
            validation_message: Some(format!("Installed {id} Feature Catalogue")),
        };
        let pc_status = CatalogueStatus {
            loaded: true,
            product_id: id.into(),
            version: pc.version.clone(),
            path: pc_dir.display().to_string(),
            item_count: pc.symbols.symbols.len(),
            validation_message: Some(format!(
                "Installed {id} Portrayal Catalogue (XSLT, {declared_groups} viewing groups)"
            )),
        };
        info!(
            "{id} catalogues loaded: FC {} · PC {} ({})",
            fc.version,
            pc.version,
            pc_dir.display()
        );
        Ok(Self {
            fc: Arc::new(fc),
            // A local source re-reads the catalogue file for each validation,
            // as before; other sources keep the bytes read here.
            fc_source: (!data.local_paths()).then(|| Arc::from(fc_bytes)),
            xslt,
            pc,
            implicit_groups,
            fc_status,
            pc_status,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Loaded datasets
// ─────────────────────────────────────────────────────────────────────────

/// How the dataset entered the system.
#[derive(Debug, Clone)]
pub enum Provenance {
    /// Exchange set signature verified.
    Authenticated,
    /// Signature verification switched off by the user.
    Unverified,
    /// Converted from a non-S-100 source; never authoritative.
    Imported(&'static str),
}

impl Provenance {
    pub fn label(&self) -> String {
        match self {
            Self::Authenticated => "signature verified".into(),
            Self::Unverified => "signature not verified".into(),
            Self::Imported(format) => {
                format!("imported from {format}; not an official S-124 dataset, not for navigation")
            }
        }
    }
}

pub struct OverlayDataset {
    pub id: u32,
    pub dataset: Dataset,
    pub received: DateTime<Utc>,
    pub issues: Vec<ferrite_s100_gml::Issue>,
    pub provenance: Provenance,
    pub visible: bool,
    input: Option<PortrayalInput>,
    /// XSLT result and the context parameters / options it was made with.
    portrayal: Option<((bool, bool, bool), portrayal::Portrayal)>,
}

impl OverlayDataset {
    pub fn product(&self) -> Product {
        self.dataset.product
    }
    pub fn name(&self) -> String {
        let mut name = self.dataset.display_name();
        if let Some(title) = self.dataset.identification.title() {
            if !title.is_empty() && title != name {
                name = format!("{name} · {title}");
            }
        }
        name
    }
    pub fn bounds(&self, ids: Option<&HashSet<String>>) -> Option<GeoBounds> {
        let mut bounds: Option<GeoBounds> = None;
        for object in &self.dataset.objects {
            if ids.is_some_and(|ids| !ids.contains(&object.id)) {
                continue;
            }
            for c in object.geometry.iter().flat_map(|g| g.coords()) {
                let p = WorldPoint::new(c[0], c[1]);
                match &mut bounds {
                    Some(b) => b.expand(p),
                    None => bounds = Some(GeoBounds::new(c[0], c[1], c[0], c[1])),
                }
            }
        }
        bounds
    }
}

/// Outcome of [`GmlOverlays::add`].
pub enum Added {
    New,
    AlreadyOpen,
}

pub struct GmlOverlays {
    pub s124: Option<ProductCatalogues>,
    pub s129: Option<ProductCatalogues>,
    pub catalogue_errors: Vec<String>,
    pub datasets: Vec<OverlayDataset>,
    next_id: u32,
    pub user: NavwarnUserState,
    pub policy: PortrayalPolicy,
    pub sort: SortOrder,
    /// S-129 context parameter PlainBoundaries (PC default).
    pub plain_boundaries: bool,
    /// Apply S-129 feature `scaleMinimum` (S-129 says it controls visibility).
    pub apply_scale_minimum: bool,
    /// "Correct known PC errata" user option, off by default (catalogues
    /// applied literally per S-100 Part 9). On: S-129 text with transparency
    /// 1 is drawn opaque and S-124 instructions in undeclared viewing groups
    /// are drawn. Persisted with the per-user state.
    correct_pc_errata: bool,
    /// NavwarnPart chosen from the warning list: portrayed even when filtered,
    /// so it can be interrogated (S-124 11.2).
    pub list_selection: Option<(u32, String)>,
    registry: Option<NavwarnRegistry>,
    /// Dataset ids in registry input order.
    pub registry_ids: Vec<u32>,
    evaluated: Option<ferrite_kernel::time::Instant>,
    /// New-message indications of the last evaluation (registry order).
    new_flags: Vec<bool>,
    /// Viewing time selected by the user (the S-101 date-dependent
    /// portrayal setting); None evaluates at the system clock.
    pub viewing_time: Option<DateTime<Utc>>,
    /// Where [`Self::user`] is persisted for the current OS user.
    user_store: Option<UserStateStore>,
    revision: u64,
    cache: Option<(String, u64, Arc<Vec<DrawingInstruction>>)>,
    /// (cache revision, render context geometry revision) last installed.
    pub installed: Option<(u64, u64)>,
    pub ui_revision: u64,
    notes_logged: HashSet<String>,
    data: Arc<dyn DataSource>,
    clock: Arc<dyn Clock>,
}

impl GmlOverlays {
    pub fn load(base: &Path, register_symbols: SymbolRegistrar) -> Self {
        Self::load_with(&Platform::native(), base, register_symbols)
    }

    /// [`GmlOverlays::load`] on a platform: catalogues and datasets are read
    /// through its data source and lifecycles use its clock.
    pub fn load_with(platform: &Platform, base: &Path, register_symbols: SymbolRegistrar) -> Self {
        Self::load_catalogues(platform, &base.join("Catalogues"), register_symbols)
    }

    /// [`GmlOverlays::load_with`] from a catalogue folder holding
    /// `FC/<product>` and `PC/<product>`.
    pub fn load_catalogues(
        platform: &Platform,
        catalogues: &Path,
        register_symbols: SymbolRegistrar,
    ) -> Self {
        let data = Arc::clone(&platform.data);
        let mut errors = Vec::new();
        let mut load = |product| match ProductCatalogues::load(
            data.as_ref(),
            catalogues,
            product,
            register_symbols,
        ) {
            Ok(c) => Some(c),
            Err(error) => {
                let message = format!("{} catalogues unavailable: {error:#}", product.identifier());
                warn!("{message}");
                errors.push(message);
                None
            }
        };
        let s124 = load(Product::S124);
        let s129 = load(Product::S129);
        let plain_boundaries = s129
            .as_ref()
            .and_then(|c| {
                c.xslt
                    .info
                    .context
                    .iter()
                    .find(|(k, _)| k == "PlainBoundaries")
                    .map(|(_, v)| v.eq_ignore_ascii_case("true") || v == "1")
            })
            .unwrap_or(true);
        if let Some(c) = &s129 {
            warn!(
                "Conformance note: S-129 {} InformationBox rule sets text transparency 1, which is fully transparent by S-100 Part 9 9-12.2.2.3; the text is not visible. \"Correct known PC errata\" draws it opaque",
                c.pc.version
            );
        }
        Self {
            s124,
            s129,
            catalogue_errors: errors,
            datasets: Vec::new(),
            next_id: 1,
            user: NavwarnUserState::default(),
            policy: PortrayalPolicy::default(),
            sort: SortOrder::Received,
            plain_boundaries,
            apply_scale_minimum: true,
            correct_pc_errata: false,
            list_selection: None,
            registry: None,
            registry_ids: Vec::new(),
            evaluated: None,
            new_flags: Vec::new(),
            viewing_time: None,
            user_store: None,
            revision: 1,
            cache: None,
            installed: None,
            ui_revision: 0,
            notes_logged: HashSet::new(),
            data,
            clock: Arc::clone(&platform.clock),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.utc()
    }

    pub fn is_empty(&self) -> bool {
        self.datasets.is_empty()
    }

    pub fn catalogues(&self, product: Product) -> Option<&ProductCatalogues> {
        match product {
            Product::S124 => self.s124.as_ref(),
            Product::S129 => self.s129.as_ref(),
        }
    }

    /// Something visible changed: re-evaluate and re-portray.
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.registry = None;
    }

    pub fn dataset(&self, id: u32) -> Option<&OverlayDataset> {
        self.datasets.iter().find(|d| d.id == id)
    }

    /// The S-124/S-129 datasets on the system for exchange-set check
    /// S-158-129 19: file name and datasetFileIdentifier of every
    /// authoritative dataset (imported KML/GPX/shapefiles are excluded), except S-124 datasets whose
    /// message status is Cancelled (any reason). S-129 has no cancelled
    /// state in the overlay; an S-129 dataset stays on the system until
    /// removed. Products share one set; their file names carry the product
    /// prefix (124 / 129), so names do not collide in practice.
    pub fn on_system(&mut self) -> HashSet<String> {
        let cancelled: HashSet<u32> = if self.datasets.iter().any(|d| d.product() == Product::S124)
        {
            self.registry();
            let registry = self.registry.as_ref().expect("evaluated");
            registry
                .messages
                .iter()
                .zip(&self.registry_ids)
                .filter(|(m, _)| m.status.is_cancelled())
                .map(|(_, id)| *id)
                .collect()
        } else {
            HashSet::new()
        };
        let mut names = HashSet::new();
        for d in &self.datasets {
            if d.dataset.imported || cancelled.contains(&d.id) {
                continue;
            }
            if let Some(name) = d.dataset.source.as_ref().and_then(|p| p.file_name()) {
                names.insert(name.to_string_lossy().into_owned());
            }
            if let Some(id) = d.dataset.identification.file_identifier() {
                names.insert(id.to_owned());
            }
        }
        names
    }

    /// Read an S-124/S-129 GML dataset (from `bytes_path`, the authenticated
    /// snapshot when verified) or import a KML/GPX/shapefile/ZIP.
    pub fn read(&self, path: &Path, bytes_path: &Path) -> Result<(Dataset, Option<&'static str>)> {
        if is_import_file(path, true) {
            let fc = self.s124.as_ref().map(|c| c.fc.as_ref());
            let bytes = self
                .data
                .read(path)
                .with_context(|| path.display().to_string())?;
            let bytes_format = ferrite_s100_gml::import::ImportFormat::detect(path, &bytes)
                .context("Unrecognised import format")?;
            // Shapefile companions are found beside the file: native only.
            let dataset = if self.data.local_paths() {
                ferrite_s100_gml::import::import_path_with(path, fc, self.now())?
            } else {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("import");
                let mut dataset = ferrite_s100_gml::import::import_bytes(
                    &bytes,
                    bytes_format,
                    name,
                    fc,
                    self.now(),
                )
                .with_context(|| path.display().to_string())?;
                dataset.source = Some(path.to_path_buf());
                dataset
            };
            return Ok((dataset, Some(bytes_format.label())));
        }
        let product = sniff_product_in(self.data.as_ref(), path)
            .context("Not an S-124 or S-129 GML dataset")?;
        let catalogues = self
            .catalogues(product)
            .with_context(|| format!("{} catalogues are not installed", product.identifier()))?;
        let limits = ferrite_s100_gml::ReadLimits::default();
        let meta = self
            .data
            .len(bytes_path)
            .with_context(|| format!("Cannot open {}", bytes_path.display()))?;
        anyhow::ensure!(
            meta as usize <= limits.bytes,
            "{} exceeds the {} byte dataset limit",
            bytes_path.display(),
            limits.bytes
        );
        let bytes = self
            .data
            .read(bytes_path)
            .with_context(|| format!("Cannot read {}", bytes_path.display()))?;
        let mut dataset = ferrite_s100_gml::read_bytes(&bytes, Some(&catalogues.fc), limits)
            .with_context(|| bytes_path.display().to_string())?;
        dataset.source = Some(path.to_path_buf());
        Ok((dataset, None))
    }

    /// Read an S-124/S-129 GML dataset supplied as bytes; `path` names it.
    pub fn read_bytes(&self, path: &Path, bytes: &[u8]) -> Result<Dataset> {
        let product =
            sniff_product_bytes(path, bytes).context("Not an S-124 or S-129 GML dataset")?;
        let catalogues = self
            .catalogues(product)
            .with_context(|| format!("{} catalogues are not installed", product.identifier()))?;
        let mut dataset = ferrite_s100_gml::read_bytes(
            bytes,
            Some(&catalogues.fc),
            ferrite_s100_gml::ReadLimits::default(),
        )
        .with_context(|| path.display().to_string())?;
        dataset.source = Some(path.to_path_buf());
        Ok(dataset)
    }

    pub fn add(&mut self, dataset: Dataset, provenance: Provenance) -> Result<Added> {
        if self
            .datasets
            .iter()
            .any(|d| d.dataset.sha256 == dataset.sha256)
        {
            return Ok(Added::AlreadyOpen);
        }
        let catalogues = self.catalogues(dataset.product).with_context(|| {
            format!(
                "{} catalogues are not installed",
                dataset.product.identifier()
            )
        })?;
        let size = dataset
            .source
            .as_deref()
            .filter(|_| !dataset.imported)
            .and_then(|p| self.data.len(p).ok());
        let mut issues = dataset.issues.clone();
        let validator = match &catalogues.fc_source {
            Some(source) => {
                ferrite_s100_gml::validate::Validator::with_catalogue_bytes(&catalogues.fc, source)
            }
            None => ferrite_s100_gml::validate::Validator::new(&catalogues.fc),
        };
        issues.extend(validator.validate(&dataset, size));
        let id = self.next_id;
        self.next_id += 1;
        let name = dataset.display_name();
        let errors = issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .count();
        info!(
            "{} dataset {name} opened ({}): {} objects, {} validation errors, {} warnings",
            dataset.product.identifier(),
            provenance.label(),
            dataset.objects.len(),
            errors,
            issues.len() - errors
        );
        // One line per check, then the errors themselves; warnings in detail
        // only at debug level (the S-129 test data alone has hundreds).
        let mut by_check: std::collections::BTreeMap<(&str, bool), usize> = Default::default();
        for issue in &issues {
            *by_check
                .entry((issue.check.as_str(), issue.severity == Severity::Error))
                .or_default() += 1;
        }
        for ((check, error), count) in by_check {
            let kind = if error { "errors" } else { "warnings" };
            warn!(
                "{} {name}: {count} validation {kind} [{check}]",
                dataset.product.identifier()
            );
        }
        for issue in issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .take(50)
        {
            warn!("{} {name}: {issue}", dataset.product.identifier());
        }
        for issue in issues.iter().filter(|i| i.severity == Severity::Warning) {
            tracing::debug!("{} {name}: {issue}", dataset.product.identifier());
        }
        // The first receipt survives restarts (11.1.1 c: 24 h indication).
        let received = if dataset.product == Product::S124 {
            let (received, recorded) = self
                .user
                .receive(&navwarn::dataset_key(&dataset), self.now());
            if recorded {
                self.persist_user_state();
            }
            received
        } else {
            self.now()
        };
        self.datasets.push(OverlayDataset {
            id,
            dataset,
            received,
            issues,
            provenance,
            visible: true,
            input: None,
            portrayal: None,
        });
        self.invalidate();
        Ok(Added::New)
    }

    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.datasets.len();
        self.datasets.retain(|d| d.id != id);
        if self.list_selection.as_ref().is_some_and(|(d, _)| *d == id) {
            self.list_selection = None;
        }
        let removed = self.datasets.len() != before;
        if removed {
            self.invalidate();
        }
        removed
    }

    pub fn set_visible(&mut self, id: u32, visible: bool) -> bool {
        match self.datasets.iter_mut().find(|d| d.id == id) {
            Some(d) if d.visible != visible => {
                d.visible = visible;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    /// Bounds of every visible dataset's geometry.
    pub fn bounds(&self) -> Option<GeoBounds> {
        let mut out: Option<GeoBounds> = None;
        for b in self
            .datasets
            .iter()
            .filter(|d| d.visible)
            .filter_map(|d| d.bounds(None))
        {
            match &mut out {
                Some(o) => {
                    o.expand(WorldPoint::new(b.min_x, b.min_y));
                    o.expand(WorldPoint::new(b.max_x, b.max_y));
                }
                None => out = Some(b),
            }
        }
        out
    }

    // ── S-124 lifecycle ──────────────────────────────────────────────────

    /// Evaluate the S-124 lifecycle now, or reuse the evaluation of the last
    /// minute. Returns true when the result differs from the previous one.
    pub fn evaluate(&mut self, force: bool) -> bool {
        if !force
            && self.registry.is_some()
            && self
                .evaluated
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
        {
            return false;
        }
        let ids: Vec<u32> = self
            .datasets
            .iter()
            .filter(|d| d.product() == Product::S124)
            .map(|d| d.id)
            .collect();
        let inputs: Vec<NavwarnInput> = self
            .datasets
            .iter()
            .filter(|d| d.product() == Product::S124)
            .map(|d| NavwarnInput {
                dataset: &d.dataset,
                received: d.received,
            })
            .collect();
        let registry = navwarn::evaluate(&inputs, self.lifecycle_time());
        let signature = |r: &NavwarnRegistry| -> Vec<(Status, Vec<String>)> {
            r.messages
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let mut suppress: Vec<String> = r
                        .feature_filter(i, &self.user, &self.policy, &[])
                        .suppress
                        .into_iter()
                        .collect();
                    suppress.sort();
                    (m.status.clone(), suppress)
                })
                .collect()
        };
        let changed = self.registry_ids != ids
            || self
                .registry
                .as_ref()
                .is_none_or(|old| signature(old) != signature(&registry));
        let clock = self.now();
        let new_flags: Vec<bool> = (0..registry.messages.len())
            .map(|i| {
                let meets = self.user.classification_of(&registry.messages[i])
                    != UserClassification::Information;
                registry.is_new_at(i, &self.user, meets, clock)
            })
            .collect();
        if new_flags != self.new_flags {
            // The 24 h new-message indication expired (list only).
            self.new_flags = new_flags;
            self.ui_revision = 0;
        }
        self.registry = Some(registry);
        self.registry_ids = ids;
        self.evaluated = Some(ferrite_kernel::time::Instant::now());
        if changed && !force {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// When the periodic lifecycle re-check is due (None without datasets).
    pub fn next_check(&self) -> Option<ferrite_kernel::time::Instant> {
        if self.datasets.is_empty() {
            return None;
        }
        let now = ferrite_kernel::time::Instant::now();
        Some(
            self.evaluated
                .and_then(|t| t.checked_add(std::time::Duration::from_secs(60)))
                .unwrap_or(now),
        )
    }

    /// Instant the S-124 lifecycle (status, fixedDateRange) is evaluated at:
    /// the user's viewing time, else the system clock.
    pub fn lifecycle_time(&self) -> DateTime<Utc> {
        self.viewing_time.unwrap_or_else(|| self.now())
    }

    /// Follow the viewing time setting (None: system clock). Returns true
    /// when it changed; the lifecycle is then re-evaluated and re-portrayed.
    pub fn set_viewing_time(&mut self, time: Option<DateTime<Utc>>) -> bool {
        if self.viewing_time == time {
            return false;
        }
        self.viewing_time = time;
        self.invalidate();
        self.ui_revision = 0;
        true
    }

    // ── Per-user state (11.1.1) ──────────────────────────────────────────

    /// Load the persisted user state from `path` and keep saving it there.
    /// A missing file starts empty; an unreadable one is set aside and the
    /// application starts with an empty state (never fails).
    pub fn attach_user_store(&mut self, path: PathBuf, user: String) {
        self.attach_user_record(Box::new(user_state_file(&path)), user);
    }

    /// [`GmlOverlays::attach_user_store`] for any user record (e.g. from
    /// [`crate::platform::UserStore::navwarn_state`]).
    pub fn attach_user_record(&mut self, record: Box<dyn UserRecord>, user: String) {
        let status = match read_user_record(record.as_ref()) {
            Ok(Some(StoredUserState {
                state: stored,
                user: stored_user,
                correct_pc_errata,
            })) => {
                if stored_user != user {
                    warn!(
                        "S-124 user state {} was saved by user {stored_user:?}, loaded for {user:?}",
                        record.describe()
                    );
                }
                // Loaded state first; anything recorded this session wins.
                let mut merged = stored;
                merged
                    .classification
                    .extend(self.user.classification.drain());
                merged.displayed.extend(self.user.displayed.drain());
                for (k, t) in self.user.received.drain() {
                    let e = merged.received.entry(k).or_insert(t);
                    *e = (*e).min(t);
                }
                self.user = merged;
                if correct_pc_errata != self.correct_pc_errata {
                    self.correct_pc_errata = correct_pc_errata;
                    self.log_errata_mode();
                }
                info!(
                    "S-124 user state for {user}: {} classifications, {} displayed, {} receipts ({})",
                    self.user.classification.len(),
                    self.user.displayed.len(),
                    self.user.received.len(),
                    record.describe()
                );
                format!("User {user} · state restored")
            }
            Ok(None) => format!("User {user} · new state"),
            Err(error) => {
                warn!(
                    "S-124 user state {} unreadable, starting empty: {error:#}",
                    record.describe()
                );
                set_aside_record(record.as_ref(), self.now());
                format!("User {user} · saved state unreadable, started empty")
            }
        };
        // Datasets opened before the store was attached keep their earliest receipt.
        for d in self
            .datasets
            .iter_mut()
            .filter(|d| d.product() == Product::S124)
        {
            let (received, _) = self
                .user
                .receive(&navwarn::dataset_key(&d.dataset), d.received);
            d.received = received;
        }
        self.user_store = Some(UserStateStore {
            record,
            user,
            status,
        });
        self.invalidate();
        self.ui_revision = 0;
    }

    /// Status line of the per-user state, for the warning list.
    pub fn user_status(&self) -> String {
        self.user_store
            .as_ref()
            .map(|s| s.status.clone())
            .unwrap_or_else(|| "User state not saved".into())
    }

    /// The "Correct known PC errata" option.
    pub fn correct_pc_errata(&self) -> bool {
        self.correct_pc_errata
    }

    /// Switch the "Correct known PC errata" option; re-portrays and saves it
    /// with the per-user state.
    pub fn set_correct_pc_errata(&mut self, on: bool) {
        if on == self.correct_pc_errata {
            return;
        }
        self.correct_pc_errata = on;
        self.log_errata_mode();
        self.invalidate();
        self.persist_user_state();
    }

    fn log_errata_mode(&self) {
        if self.correct_pc_errata {
            info!("Correct known PC errata: on (S-129 transparency-1 text drawn opaque, S-124 undeclared viewing groups drawn)");
        } else {
            info!("Correct known PC errata: off (portrayal catalogues applied literally per S-100 Part 9)");
        }
    }

    fn persist_user_state(&mut self) {
        let now = self.now();
        let Some(store) = &mut self.user_store else {
            return;
        };
        self.user.prune(now);
        let written = encode_user_state(&store.user, &self.user, self.correct_pc_errata, now)
            .and_then(|bytes| Ok(store.record.write(&bytes)?));
        match written {
            Ok(()) => store.status = format!("User {} · state saved", store.user),
            Err(error) => {
                warn!(
                    "S-124 user state not saved to {}: {error:#}",
                    store.record.describe()
                );
                store.status = format!("User {} · state NOT saved: {error}", store.user);
            }
        }
        self.ui_revision = 0;
    }

    fn registry_index(&self, id: u32) -> Option<usize> {
        self.registry_ids.iter().position(|d| *d == id)
    }

    pub fn registry(&mut self) -> &NavwarnRegistry {
        if self.registry.is_none() {
            self.evaluate(true);
        }
        self.registry.as_ref().expect("evaluated")
    }

    pub fn classify(&mut self, id: u32, class: UserClassification) {
        self.registry();
        let Some(index) = self.registry_index(id) else {
            return;
        };
        let registry = self.registry.as_ref().expect("evaluated");
        if let Some(m) = registry.message(index) {
            if self.user.classification_of(m) != class {
                self.user.classify(m, class);
                self.persist_user_state();
            }
            self.invalidate();
        }
    }

    pub fn mark_displayed(&mut self, id: u32) {
        self.registry();
        if let Some(m) = self
            .registry_index(id)
            .and_then(|i| self.registry.as_ref().expect("evaluated").message(i))
        {
            if !self.user.displayed.contains(&m.dataset_key) {
                let m = m.clone();
                self.user.mark_displayed(&m);
                self.persist_user_state();
                self.ui_revision = 0;
            }
        }
    }

    // ── Portrayal ────────────────────────────────────────────────────────

    /// Overlay drawing instructions for the palette, or None when no
    /// overlay dataset is loaded (the host then behaves exactly as before).
    pub fn instructions(&mut self, profile: &str) -> Option<(u64, Arc<Vec<DrawingInstruction>>)> {
        if self.datasets.is_empty() {
            return None;
        }
        self.evaluate(false);
        if let Some((p, r, list)) = &self.cache {
            if p == profile && *r == self.revision {
                return Some((*r, Arc::clone(list)));
            }
        }
        let list = Arc::new(self.portray(profile));
        self.cache = Some((profile.to_owned(), self.revision, Arc::clone(&list)));
        self.ui_revision = 0;
        Some((self.revision, list))
    }

    fn portray(&mut self, profile: &str) -> Vec<DrawingInstruction> {
        let started = ferrite_kernel::time::Instant::now();
        self.registry();
        let mut out = Vec::new();
        let errata = self.correct_pc_errata;
        let options_key = (self.plain_boundaries, self.apply_scale_minimum, errata);
        let plain = if self.plain_boundaries {
            "true"
        } else {
            "false"
        };
        let mut notes = Vec::new();
        for index in 0..self.datasets.len() {
            let product = self.datasets[index].product();
            let Some(catalogues) = (match product {
                Product::S124 => self.s124.as_ref(),
                Product::S129 => self.s129.as_ref(),
            }) else {
                continue;
            };
            let entry = &mut self.datasets[index];
            if !entry.visible {
                continue;
            }
            if entry.input.is_none() {
                entry.input = Some(portrayal::input_xml(
                    &entry.dataset,
                    Some(&catalogues.fc),
                    &InputOptions::default(),
                ));
            }
            let key = match product {
                Product::S124 => (true, true, errata),
                Product::S129 => options_key,
            };
            if entry.portrayal.as_ref().is_none_or(|(k, _)| *k != key) {
                let (overrides, options) = match product {
                    Product::S124 => (
                        Vec::new(),
                        ConvertOptions {
                            implicit_viewing_groups: if errata {
                                catalogues.implicit_groups.clone()
                            } else {
                                HashSet::new()
                            },
                            ..ConvertOptions::default()
                        },
                    ),
                    Product::S129 => (
                        vec![("PlainBoundaries", plain)],
                        ConvertOptions {
                            apply_feature_scale_minimum: self.apply_scale_minimum,
                            opaque_invisible_text: errata,
                            ..ConvertOptions::default()
                        },
                    ),
                };
                match catalogues.xslt.portray(
                    entry.input.as_ref().expect("input"),
                    &overrides,
                    &options,
                ) {
                    Ok(p) => {
                        for note in &p.notes {
                            notes.push(format!(
                                "{} {}: {note}",
                                product.identifier(),
                                entry.dataset.display_name()
                            ));
                        }
                        entry.portrayal = Some((key, p));
                    }
                    Err(error) => {
                        warn!(
                            "{} portrayal of {} failed: {error:#}",
                            product.identifier(),
                            entry.dataset.display_name()
                        );
                        entry.portrayal = None;
                        continue;
                    }
                }
            }
            let suppress: HashSet<String> = match product {
                Product::S124 => {
                    let selected: Vec<&str> = self
                        .list_selection
                        .iter()
                        .filter(|(d, _)| *d == entry.id)
                        .map(|(_, f)| f.as_str())
                        .collect();
                    match self.registry_ids.iter().position(|d| *d == entry.id) {
                        Some(i) => {
                            self.registry
                                .as_ref()
                                .expect("evaluated")
                                .feature_filter(i, &self.user, &self.policy, &selected)
                                .suppress
                        }
                        None => continue,
                    }
                }
                Product::S129 => HashSet::new(),
            };
            let (_, portrayal) = entry.portrayal.as_ref().expect("portrayal");
            let features: Vec<&FeatureInstructions> = portrayal
                .features
                .iter()
                .filter(|f| !suppress.contains(&f.feature_id))
                .collect();
            convert(
                &catalogues.pc,
                entry.input.as_ref().expect("input"),
                &features,
                entry.id,
                profile,
                &mut out,
            );
        }
        for note in notes {
            if self.notes_logged.insert(note.clone()) {
                info!("Conformance note: {note}");
            }
        }
        info!(
            "Overlay portrayal: {} instructions from {} datasets in {:.1} ms",
            out.len(),
            self.datasets.len(),
            started.elapsed().as_secs_f64() * 1e3
        );
        out
    }

    // ── Picking ──────────────────────────────────────────────────────────

    pub fn locate(&self, key: i64) -> Option<(&OverlayDataset, &portrayal::InputFeature)> {
        let (slot, feature) = decode_key(key)?;
        let entry = self.dataset(slot)?;
        let feature = entry.input.as_ref()?.features.get(feature)?;
        Some((entry, feature))
    }

    /// Selected-object report of an overlay feature key.
    pub fn describe(&mut self, key: i64) -> Option<OverlayPick> {
        self.registry();
        let (entry, feature) = self.locate(key)?;
        let catalogues = self.catalogues(entry.product())?;
        let fc = catalogues.fc.as_ref();
        let product = entry.product().identifier();
        let mut attributes = vec![
            ("Dataset".to_owned(), entry.name()),
            (
                "Product".to_owned(),
                format!(
                    "{product} {}",
                    entry.dataset.identification.product_edition().unwrap_or("")
                )
                .trim()
                .to_owned(),
            ),
            ("Provenance".to_owned(), entry.provenance.label()),
        ];
        if entry.product() == Product::S124 {
            if let Some(m) = self
                .registry_index(entry.id)
                .and_then(|i| self.registry.as_ref()?.message(i))
            {
                attributes.push(("Warning".into(), m.caption()));
                attributes.push(("Warning status".into(), m.status.label().into()));
                if let Some(t) = m.publication_time {
                    attributes.push(("Publication time".into(), format_time(t)));
                }
            }
        }
        let object = feature.object.and_then(|i| entry.dataset.objects.get(i));
        let mut definition = None;
        let mut feature_type = feature.code.clone();
        if let Some(object) = object {
            if let Some(t) = fc.feature_types.get(&object.code) {
                definition = t.definition.clone();
                feature_type = format!("{} ({})", t.name, object.code);
            }
            attributes.push(("gml:id".into(), object.id.clone()));
            attribute_rows(fc, &object.attributes, "", &mut attributes);
            for r in &object.references {
                let target = r
                    .target
                    .and_then(|t| entry.dataset.objects.get(t))
                    .map(|t| format!("{} {}", t.code, t.id))
                    .unwrap_or_else(|| r.href.clone());
                attributes.push((format!("→ {}", r.role), target));
            }
            // S-124 parts: the preamble carries the message identity.
            for r in object.references.iter().filter_map(|r| r.target) {
                let Some(info) = entry.dataset.objects.get(r) else {
                    continue;
                };
                if info.kind == ObjectKind::Information && info.code == "NavwarnPreamble" {
                    attribute_rows(fc, &info.attributes, "Preamble › ", &mut attributes);
                }
            }
        } else {
            attributes.push((
                "Feature".into(),
                format!("{} (portrayal feature)", feature.code),
            ));
        }
        Some(OverlayPick {
            feature_type,
            primitive: feature.primitive.to_owned(),
            source: format!(
                "{} · {product} overlay · {}",
                entry
                    .dataset
                    .source
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| entry.name()),
                entry.provenance.label()
            ),
            attributes,
            definition,
            foid: object.map(|o| o.id.clone()),
        })
    }

    /// Selection highlight of an overlay feature key: for S-124 the features
    /// [`navwarn::FeatureFilter::highlight`] names (the NavwarnPart and its
    /// NavwarnAreaAffected), else nothing extra.
    pub fn highlight_geometry(&mut self, key: i64) -> Vec<Vec<WorldPoint>> {
        self.registry();
        let Some((entry, feature)) = self.locate(key) else {
            return Vec::new();
        };
        if entry.product() != Product::S124 {
            return Vec::new();
        }
        let Some(index) = self.registry_index(entry.id) else {
            return Vec::new();
        };
        let filter = self.registry.as_ref().expect("evaluated").feature_filter(
            index,
            &self.user,
            &self.policy,
            &[feature.id.as_str()],
        );
        let mut out = Vec::new();
        for id in &filter.highlight {
            if let Some(object) = entry.dataset.object(id) {
                for g in &object.geometry {
                    geometry_paths(g, &mut out);
                }
            }
        }
        out
    }

    /// The overlay feature key of a dataset object (gml:id), if portrayed input.
    pub fn key_of(&self, id: u32, object_id: &str) -> Option<i64> {
        let entry = self.dataset(id)?;
        let feature = entry.input.as_ref()?.feature_index(object_id)?;
        Some(overlay_key(id, feature))
    }

    /// Geographic bounds of a message part (with its associated features) or
    /// of the whole dataset.
    pub fn focus_bounds(&mut self, id: u32, part: Option<&str>) -> Option<GeoBounds> {
        self.registry();
        let entry = self.dataset(id)?;
        let ids: Option<HashSet<String>> = match (part, self.registry_index(id)) {
            (Some(part), Some(index)) => {
                let m = self.registry.as_ref()?.message(index)?;
                let p = m.parts.iter().find(|p| p.id == part)?;
                Some(
                    std::iter::once(p.id.clone())
                        .chain(p.areas_affected.iter().cloned())
                        .chain(p.text_placements.iter().cloned())
                        .collect(),
                )
            }
            _ => None,
        };
        entry.bounds(ids.as_ref()).or_else(|| entry.bounds(None))
    }

    /// First NavwarnPart with geometry of a message.
    pub fn first_part(&mut self, id: u32) -> Option<String> {
        self.registry();
        let index = self.registry_index(id)?;
        let m = self.registry.as_ref()?.message(index)?;
        m.parts
            .iter()
            .find(|p| p.has_geometry)
            .map(|p| p.id.clone())
    }
}

/// Fields of a selected overlay object.
pub struct OverlayPick {
    pub feature_type: String,
    pub primitive: String,
    pub source: String,
    pub attributes: Vec<(String, String)>,
    pub definition: Option<String>,
    pub foid: Option<String>,
}

pub fn format_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%MZ").to_string()
}

pub fn timing_label(t: PartTiming) -> String {
    match t {
        PartTiming::NoRange => "in force".into(),
        PartTiming::InRange => "in its date range".into(),
        PartTiming::BeforeRange { starts } => format!("starts {}", format_time(starts)),
        PartTiming::AfterRange { ended } => format!("ended {}", format_time(ended)),
        PartTiming::OutsideRecurring => "outside its recurring dates".into(),
    }
}

pub fn cancel_reason(reason: &navwarn::CancelReason, registry: &NavwarnRegistry) -> String {
    let caption = |i: usize| {
        registry
            .message(i)
            .map(|m| m.caption())
            .unwrap_or_else(|| format!("dataset {i}"))
    };
    match reason {
        navwarn::CancelReason::CancellationDate(t) => {
            format!("cancellation date {}", format_time(*t))
        }
        navwarn::CancelReason::CancelledBy { dataset } => format!("by {}", caption(*dataset)),
        navwarn::CancelReason::NotInForceBulletin { bulletin } => {
            format!("not in force per {}", caption(*bulletin))
        }
        navwarn::CancelReason::SupersededInForceBulletin { by } => {
            format!("superseded by {}", caption(*by))
        }
        navwarn::CancelReason::ReplacedByAuthoritative { by } => {
            format!("replaced by official {}", caption(*by))
        }
    }
}

/// FC names and enumeration labels for attribute values.
fn attribute_rows(
    fc: &FeatureCatalogue,
    attributes: &[ferrite_s100_gml::Attribute],
    prefix: &str,
    out: &mut Vec<(String, String)>,
) {
    for a in attributes {
        let simple = fc.simple_attributes.get(&a.code);
        let name = simple
            .map(|s| s.name.clone())
            .or_else(|| fc.complex_attributes.get(&a.code).map(|c| c.name.clone()))
            .unwrap_or_else(|| a.code.clone());
        match &a.value {
            Value::Simple { text, code } => {
                let listed = code
                    .as_deref()
                    .or(Some(text.as_str()))
                    .and_then(|c| c.trim().parse::<u32>().ok())
                    .and_then(|c| {
                        simple?
                            .listed_values
                            .iter()
                            .find(|v| v.code == c)
                            .map(|v| format!("{} ({c})", v.label))
                    });
                let value = listed.unwrap_or_else(|| {
                    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    match code {
                        Some(c) if !t.is_empty() && t != *c => format!("{t} ({c})"),
                        Some(c) => c.clone(),
                        None => t,
                    }
                });
                out.push((format!("{prefix}{name}"), value));
            }
            Value::Complex(children) => {
                attribute_rows(fc, children, &format!("{prefix}{name} › "), out)
            }
            Value::Nil { reason } => out.push((
                format!("{prefix}{name}"),
                format!(
                    "unknown{}",
                    reason
                        .as_ref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ),
            )),
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
