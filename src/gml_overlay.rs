//! S-124 (Navigational Warnings 2.0.0) and S-129 (Under Keel Clearance
//! Management 2.0.0) datasets shown as an always-on overlay on the ENC
//! (S-98 Level 1 style).
//!
//! Datasets are S-100 Part 10b GML, portrayed with their own XSLT portrayal
//! catalogues (`ferrite_s100_gml::portrayal`). The display list is converted
//! to the host's drawing instructions through the same Part 9a conversion the
//! S-101 Lua portrayal uses, on a synthetic cell, and then marked as host
//! overlay instructions: no source cell, exempt from ENC coverage and placed
//! in the always displayed viewing group (21010, as plugin overlays), so the
//! S-101 display mode and layer selection never hide them and S-101 output is
//! untouched when no overlay dataset is loaded.
//!
//! Feature keys carry [`OVERLAY_KEY_FLAG`] and cannot collide with S-101
//! record keys; picking decodes them back to the dataset and object.
use crate::ChartApp;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{DrawingInstruction, GeoBounds, RenderContext, Viewport, WorldPoint};
use ferrite_s100_gml::navwarn::{
    self, NavwarnInput, NavwarnRegistry, NavwarnUserState, PartTiming, PortrayalPolicy, SortOrder,
    Status, UserClassification,
};
use ferrite_s100_gml::portrayal::{
    self, ConvertOptions, FeatureInstructions, InputOptions, PortrayalInput, XsltPortrayal,
};
use ferrite_s100_gml::{Dataset, Geometry, ObjectKind, Product, Severity, Value};
use ferrite_wgpu::overlay_ui::{
    ControlPointRow, NavwarnClassification, NavwarnPartRow, NavwarnRow, NavwarnSort, UkcPlanRow,
};
use ferrite_wgpu::CatalogueStatus;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Set on every overlay feature key (S-101 record keys stay below 2^40).
pub(crate) const OVERLAY_KEY_FLAG: i64 = 1 << 62;
const SLOT_SHIFT: u32 = 24;
const FEATURE_MASK: i64 = (1 << SLOT_SHIFT) - 1;
const SLOT_MASK: i64 = (1 << 38) - 1;
/// Always displayed viewing group of host overlays (DISPLBASE).
const OVERLAY_VIEWING_GROUP: u32 = 21010;
/// Viewing groups used by the S-124 2.0.0 XSLT rules but not declared in its
/// portrayal_catalogue.xml (TextPlacement text and co-located part counts).
const S124_IMPLICIT_GROUPS: [u32; 2] = [26, 12210];

pub(crate) fn overlay_key(slot: u32, feature: usize) -> i64 {
    OVERLAY_KEY_FLAG | ((slot as i64 & SLOT_MASK) << SLOT_SHIFT) | (feature as i64 & FEATURE_MASK)
}

pub(crate) fn decode_key(key: i64) -> Option<(u32, usize)> {
    let slot = ((key >> SLOT_SHIFT) & SLOT_MASK) as u32;
    (key & OVERLAY_KEY_FLAG != 0 && key > 0).then_some((slot, (key & FEATURE_MASK) as usize))
}

pub(crate) fn is_overlay_instruction(instruction: &DrawingInstruction) -> bool {
    ferrite_wgpu::is_cellless_overlay_instruction(instruction)
        && instruction
            .feature_id()
            .is_some_and(|k| decode_key(k).is_some())
}

// ─────────────────────────────────────────────────────────────────────────
// Discovery
// ─────────────────────────────────────────────────────────────────────────

const IMPORT_EXTENSIONS: [&str; 3] = ["kml", "gpx", "shp"];

/// S-124 / S-129 from the root element of a GML file (namespace or
/// productIdentifier in the first 64 KiB).
pub(crate) fn sniff_product(path: &Path) -> Option<Product> {
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

fn is_import_file(path: &Path, allow_zip: bool) -> bool {
    IMPORT_EXTENSIONS
        .iter()
        .any(|e| crate::dataset_discovery::is_dataset_file(path, e))
        || (allow_zip && crate::dataset_discovery::is_dataset_file(path, "zip"))
}

/// An explicitly selected file that opens as an overlay dataset.
pub(crate) fn is_overlay_file(path: &Path) -> bool {
    is_import_file(path, true) || sniff_product(path).is_some()
}

/// Overlay datasets under a folder: S-124/S-129 GML and imported KML, GPX
/// and extracted shapefiles. ZIP archives are opened only as explicit files
/// (a folder may hold both an archive and its extracted copy).
pub(crate) fn discover(root: &Path) -> Result<Vec<PathBuf>> {
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
    if !found.is_empty() {
        log_exchange_set_checks(catalogues);
    }
    Ok(found)
}

/// Exchange-set validation (S-158-129 11, 17-20; S-124 9.5-9.8, 12) of the
/// S-124 / S-129 exchange sets under an opened folder, logged like dataset
/// validation. Check 19 needs the system state and is not evaluated here.
fn log_exchange_set_checks(mut roots: Vec<PathBuf>) {
    roots.sort();
    let mut outer: Vec<PathBuf> = Vec::new();
    for root in roots {
        // A nested CATALOG.XML belongs to the enclosing exchange set.
        if !outer.iter().any(|o| root.starts_with(o)) {
            outer.push(root);
        }
    }
    for root in outer {
        let report = match ferrite_s100_gml::exchange::validate_exchange_set(&root, None) {
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
// Catalogues
// ─────────────────────────────────────────────────────────────────────────

pub(crate) struct ProductCatalogues {
    pub fc: Arc<FeatureCatalogue>,
    xslt: XsltPortrayal,
    /// Catalogue used for Part 9a conversion: symbol references namespaced
    /// with the product prefix, implicit viewing groups declared.
    pc: PortrayalCatalogue,
    pub fc_status: CatalogueStatus,
    pub pc_status: CatalogueStatus,
}

impl ProductCatalogues {
    fn load(base: &Path, product: Product) -> Result<Self> {
        let id = product.identifier();
        let fc_dir = base.join("Catalogues/FC").join(id);
        let fc_file = crate::dataset_discovery::feature_catalogue_file(&fc_dir)
            .with_context(|| format!("{id} feature catalogue in {}", fc_dir.display()))?;
        let fc = FeatureCatalogue::load(&fc_file)
            .with_context(|| format!("{id} feature catalogue {}", fc_file.display()))?;
        anyhow::ensure!(
            fc.product_id.is_empty() || fc.product_id == id,
            "Expected {id} feature catalogue, found {}",
            fc.product_id
        );
        let pc_dir = base.join("Catalogues/PC").join(id);
        let bound = PortrayalCatalogue::load_bound(&pc_dir)
            .with_context(|| format!("{id} portrayal catalogue {}", pc_dir.display()))?;
        anyhow::ensure!(
            ferrite_wgpu::register_overlay_symbol_source(
                id,
                ferrite_wgpu::OverlaySymbolSource::from_bound(&bound)
            ),
            "Invalid overlay symbol prefix {id}"
        );
        let mut pc = PortrayalCatalogue::load(&pc_dir)
            .with_context(|| format!("{id} portrayal catalogue {}", pc_dir.display()))?;
        pc.namespace_symbol_references(id);
        let mut xslt = XsltPortrayal::load(&pc_dir)?;
        if product == Product::S124 {
            for group in S124_IMPLICIT_GROUPS {
                // The rules emit these groups; dropping them would lose the
                // TextPlacement text and co-located part counts.
                if pc
                    .viewing_groups
                    .declare_implicit(group, &format!("{id} implicit viewing group {group}"))
                {
                    warn!(
                        "Conformance: {id} {} rules use viewing group {group}, which portrayal_catalogue.xml does not declare; treated as an implicitly declared, always displayed overlay group",
                        pc.version
                    );
                }
                xslt.info.viewing_groups.insert(group.to_string());
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
                "Installed {id} Portrayal Catalogue (XSLT, {} viewing groups)",
                pc.viewing_groups.groups.len()
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
            xslt,
            pc,
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
pub(crate) enum Provenance {
    /// Exchange set signature verified.
    Authenticated,
    /// Signature verification switched off by the user.
    Unverified,
    /// Converted from a non-S-100 source; never authoritative.
    Imported(&'static str),
}

impl Provenance {
    fn label(&self) -> String {
        match self {
            Self::Authenticated => "signature verified".into(),
            Self::Unverified => "signature not verified".into(),
            Self::Imported(format) => {
                format!("imported from {format}; not an official S-124 dataset, not for navigation")
            }
        }
    }
}

pub(crate) struct OverlayDataset {
    pub id: u32,
    pub dataset: Dataset,
    pub received: DateTime<Utc>,
    pub issues: Vec<ferrite_s100_gml::Issue>,
    pub provenance: Provenance,
    pub visible: bool,
    input: Option<PortrayalInput>,
    /// XSLT result and the context parameters / options it was made with.
    portrayal: Option<((bool, bool), portrayal::Portrayal)>,
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
    fn bounds(&self, ids: Option<&HashSet<String>>) -> Option<GeoBounds> {
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
pub(crate) enum Added {
    New,
    AlreadyOpen,
}

pub(crate) struct GmlOverlays {
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
    /// NavwarnPart chosen from the warning list: portrayed even when filtered,
    /// so it can be interrogated (S-124 11.2).
    pub list_selection: Option<(u32, String)>,
    registry: Option<NavwarnRegistry>,
    /// Dataset ids in registry input order.
    registry_ids: Vec<u32>,
    evaluated: Option<std::time::Instant>,
    /// New-message indications of the last evaluation (registry order).
    new_flags: Vec<bool>,
    /// Viewing time selected by the user (the S-101 date-dependent
    /// portrayal setting); None evaluates at the system clock.
    viewing_time: Option<DateTime<Utc>>,
    /// Where [`Self::user`] is persisted for the current OS user.
    user_store: Option<UserStateStore>,
    revision: u64,
    cache: Option<(String, u64, Arc<Vec<DrawingInstruction>>)>,
    /// (cache revision, render context geometry revision) last installed.
    pub installed: Option<(u64, u64)>,
    pub ui_revision: u64,
    notes_logged: HashSet<String>,
}

impl GmlOverlays {
    pub fn load(base: &Path) -> Self {
        let mut errors = Vec::new();
        let mut load = |product| match ProductCatalogues::load(base, product) {
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
        if s129.is_some() {
            warn!("Conformance: S-129 2.0.0 InformationBox rule sets text transparency 1 (fully transparent by S-100 Part 9 9-12.2.2.3); the host renders that text opaque, treating the value as opacity as the catalogue evidently intends");
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
        }
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

    /// Read an S-124/S-129 GML dataset (from `bytes_path`, the authenticated
    /// snapshot when verified) or import a KML/GPX/shapefile/ZIP.
    pub fn read(&self, path: &Path, bytes_path: &Path) -> Result<(Dataset, Option<&'static str>)> {
        if is_import_file(path, true) {
            let fc = self.s124.as_ref().map(|c| c.fc.as_ref());
            let bytes_format = ferrite_s100_gml::import::ImportFormat::detect(
                path,
                &std::fs::read(path).with_context(|| path.display().to_string())?,
            )
            .context("Unrecognised import format")?;
            let dataset = ferrite_s100_gml::import::import_path_with(path, fc, Utc::now())?;
            return Ok((dataset, Some(bytes_format.label())));
        }
        let product = sniff_product(path).context("Not an S-124 or S-129 GML dataset")?;
        let catalogues = self
            .catalogues(product)
            .with_context(|| format!("{} catalogues are not installed", product.identifier()))?;
        let mut dataset = ferrite_s100_gml::read_file(
            bytes_path,
            Some(&catalogues.fc),
            ferrite_s100_gml::ReadLimits::default(),
        )?;
        dataset.source = Some(path.to_path_buf());
        Ok((dataset, None))
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
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());
        let mut issues = dataset.issues.clone();
        issues.extend(
            ferrite_s100_gml::validate::Validator::new(&catalogues.fc).validate(&dataset, size),
        );
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
                .receive(&navwarn::dataset_key(&dataset), Utc::now());
            if recorded {
                self.persist_user_state();
            }
            received
        } else {
            Utc::now()
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
        let clock = Utc::now();
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
        self.evaluated = Some(std::time::Instant::now());
        if changed && !force {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// When the periodic lifecycle re-check is due (None without datasets).
    pub fn next_check(&self) -> Option<std::time::Instant> {
        if self.datasets.is_empty() {
            return None;
        }
        let now = std::time::Instant::now();
        Some(
            self.evaluated
                .and_then(|t| t.checked_add(std::time::Duration::from_secs(60)))
                .unwrap_or(now),
        )
    }

    /// Instant the S-124 lifecycle (status, fixedDateRange) is evaluated at:
    /// the user's viewing time, else the system clock.
    pub fn lifecycle_time(&self) -> DateTime<Utc> {
        self.viewing_time.unwrap_or_else(Utc::now)
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
        let status = match read_user_state(&path) {
            Ok(Some((stored, stored_user))) => {
                if stored_user != user {
                    warn!(
                        "S-124 user state {} was saved by user {stored_user:?}, loaded for {user:?}",
                        path.display()
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
                info!(
                    "S-124 user state for {user}: {} classifications, {} displayed, {} receipts ({})",
                    self.user.classification.len(),
                    self.user.displayed.len(),
                    self.user.received.len(),
                    path.display()
                );
                format!("User {user} · state restored")
            }
            Ok(None) => format!("User {user} · new state"),
            Err(error) => {
                warn!(
                    "S-124 user state {} unreadable, starting empty: {error:#}",
                    path.display()
                );
                set_aside(&path);
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
        self.user_store = Some(UserStateStore { path, user, status });
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

    fn persist_user_state(&mut self) {
        let Some(store) = &mut self.user_store else {
            return;
        };
        self.user.prune(Utc::now());
        match write_user_state(&store.path, &store.user, &self.user) {
            Ok(()) => store.status = format!("User {} · state saved", store.user),
            Err(error) => {
                warn!(
                    "S-124 user state not saved to {}: {error:#}",
                    store.path.display()
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
        let started = std::time::Instant::now();
        self.registry();
        let mut out = Vec::new();
        let options_key = (self.plain_boundaries, self.apply_scale_minimum);
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
                Product::S124 => (true, true),
                Product::S129 => options_key,
            };
            if entry.portrayal.as_ref().is_none_or(|(k, _)| *k != key) {
                let (overrides, options) = match product {
                    Product::S124 => (Vec::new(), ConvertOptions::default()),
                    Product::S129 => (
                        vec![("PlainBoundaries", plain)],
                        ConvertOptions {
                            apply_feature_scale_minimum: self.apply_scale_minimum,
                            opaque_invisible_text: true,
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

    fn locate(&self, key: i64) -> Option<(&OverlayDataset, &portrayal::InputFeature)> {
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

    // ── UI rows ──────────────────────────────────────────────────────────

    pub fn navwarn_rows(&mut self) -> (Vec<NavwarnRow>, Vec<String>, usize) {
        self.evaluate(false);
        let registry = self.registry().clone();
        let mut rows = Vec::new();
        let mut new_count = 0;
        let clock = Utc::now();
        for index in registry.sorted(self.sort, &self.user) {
            let m = &registry.messages[index];
            let Some(entry) = self
                .registry_ids
                .get(index)
                .and_then(|id| self.dataset(*id))
            else {
                continue;
            };
            let class = self.user.classification_of(m);
            let meets = class != UserClassification::Information;
            let new = registry.is_new_at(index, &self.user, meets, clock);
            new_count += new as usize;
            rows.push(NavwarnRow {
                dataset: entry.id,
                caption: m.caption(),
                status: match &m.status {
                    Status::Cancelled(reason) => {
                        format!("Cancelled ({})", cancel_reason(reason, &registry))
                    }
                    s => s.label().to_owned(),
                },
                cancelled: m.status.is_cancelled(),
                imported: m.imported,
                new,
                portrayed: entry.visible
                    && registry.dataset_portrayed(index, &self.user, &self.policy),
                received: format_time(m.received),
                issued: m
                    .publication_time
                    .map(format_time)
                    .unwrap_or_else(|| "-".into()),
                warning_type: m
                    .warning_type
                    .map(|t| {
                        navwarn::warning_type_label(t)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("type {t}"))
                    })
                    .or_else(|| m.navwarn_type_general.clone())
                    .unwrap_or_default(),
                producer_series: m.series().map(|s| s.to_string()).unwrap_or_default(),
                general_area: m
                    .general_area
                    .iter()
                    .chain(&m.locality)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                classification: match class {
                    UserClassification::OnChart => NavwarnClassification::OnChart,
                    UserClassification::OffChart => NavwarnClassification::OffChart,
                    UserClassification::Information => NavwarnClassification::Information,
                },
                parts: m
                    .parts
                    .iter()
                    .map(|p| NavwarnPartRow {
                        id: p.id.clone(),
                        summary: p.abbreviated(120),
                        timing: timing_label(p.timing),
                        has_geometry: p.has_geometry,
                    })
                    .collect(),
                source: format!(
                    "{} · {}",
                    entry
                        .dataset
                        .source
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    entry.provenance.label()
                ),
            });
        }
        let mut missing = Vec::new();
        for b in registry.missing_indications() {
            let ids: Vec<String> = b
                .missing
                .iter()
                .map(|m| {
                    if m.imported_copy.is_some() {
                        format!("{} (imported copy only)", m.id)
                    } else {
                        m.id.to_string()
                    }
                })
                .collect();
            missing.push(format!(
                "In-force bulletin {} lists warnings not held: {}",
                b.series,
                ids.join(", ")
            ));
        }
        (rows, missing, new_count)
    }

    pub fn ukc_rows(&self) -> Vec<UkcPlanRow> {
        let mut out = Vec::new();
        for entry in self
            .datasets
            .iter()
            .filter(|d| d.product() == Product::S129)
        {
            let ds = &entry.dataset;
            let plan = ds
                .objects_of("UnderKeelClearancePlan")
                .next()
                .map(|(_, o)| o);
            let v = |path: &[&str]| plan.and_then(|p| p.value(path)).unwrap_or("").to_owned();
            let label = |code: &str| {
                plan.and_then(|p| p.attribute(code))
                    .and_then(|a| a.text())
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| t.trim().to_owned())
                    .unwrap_or_else(|| v(&[code]))
            };
            let window = |o: &ferrite_s100_gml::Object| match (
                o.value(&["fixedTimeRange", "timeStart"]),
                o.value(&["fixedTimeRange", "timeEnd"]),
            ) {
                (None, None) => String::new(),
                (a, b) => format!("{} – {}", a.unwrap_or("…"), b.unwrap_or("…")),
            };
            let mut cps: Vec<(String, ControlPointRow)> = ds
                .objects_of("UnderKeelClearanceControlPoint")
                .map(|(_, o)| {
                    let eta = o.value(&["expectedPassingTime"]).unwrap_or("").to_owned();
                    (
                        eta.clone(),
                        ControlPointRow {
                            name: o
                                .value(&["featureName", "name"])
                                .unwrap_or(&o.id)
                                .to_owned(),
                            eta,
                            window: window(o),
                            speed: o
                                .value(&["expectedPassingSpeed"])
                                .map(|s| format!("{s} kn"))
                                .unwrap_or_default(),
                            margin: o
                                .value(&["distanceAboveUKCLimit"])
                                .map(|s| format!("{s} m"))
                                .unwrap_or_default(),
                        },
                    )
                })
                .collect();
            cps.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(UkcPlanRow {
                dataset: entry.id,
                name: entry.name(),
                vessel: v(&["vesselID"]),
                draught: plan
                    .and_then(|p| p.value(&["maximumDraught"]))
                    .map(|d| format!("{d} m"))
                    .unwrap_or_default(),
                purpose: label("underKeelClearancePurpose"),
                generation_time: v(&["generationTime"]),
                validity: plan.map(window).unwrap_or_default(),
                route: [v(&["sourceRouteName"]), v(&["sourceRouteVersion"])]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
                control_points: cps.into_iter().map(|(_, r)| r).collect(),
                issues: entry.issues.len(),
            });
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Per-user persistence (S-124 11.1.1: user interface linked to an individual
// user, so that classifications and indications survive watch handover)
// ─────────────────────────────────────────────────────────────────────────

struct UserStateStore {
    path: PathBuf,
    user: String,
    status: String,
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
/// identity key ([`navwarn::MessageInfo::key`]), the rest by dataset key.
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
}

/// Name of the OS user running the application.
pub(crate) fn os_user_name() -> String {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|v| v.to_string_lossy().trim().to_owned())
        .find(|v| !v.is_empty())
        .or_else(|| {
            dirs::home_dir().and_then(|h| h.file_name().map(|n| n.to_string_lossy().into_owned()))
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Per-OS-user location of the S-124 user state, in the same application
/// data folder as the other persisted state: `FerriteS100/navwarn/<user>.json`
/// under the user's own data directory. `FERRITE_NAVWARN_STATE_PATH`
/// overrides it; tests and hidden audits persist nothing.
pub(crate) fn default_user_state_path(user: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FERRITE_NAVWARN_STATE_PATH") {
        return Some(PathBuf::from(path));
    }
    if cfg!(test) || ferrite_wgpu::background_test::enabled() {
        return None;
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let file: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file = file.trim_start_matches('.');
    let file = if file.is_empty() { "unknown" } else { file };
    Some(
        base?
            .join("FerriteS100/navwarn")
            .join(format!("{file}.json")),
    )
}

/// The stored state and the user it was saved for; None when there is no file.
fn read_user_state(path: &Path) -> Result<Option<(NavwarnUserState, String)>> {
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
    Ok(Some((state, doc.user)))
}

/// Replace the stored state atomically (temporary file, then rename).
fn write_user_state(path: &Path, user: &str, state: &NavwarnUserState) -> Result<()> {
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
fn set_aside(path: &Path) {
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
pub(crate) fn viewing_time(settings: &ferrite_render::DisplaySettings) -> Option<DateTime<Utc>> {
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

/// Fields of a selected overlay object.
pub(crate) struct OverlayPick {
    pub feature_type: String,
    pub primitive: String,
    pub source: String,
    pub attributes: Vec<(String, String)>,
    pub definition: Option<String>,
    pub foid: Option<String>,
}

pub(crate) fn sort_order(sort: NavwarnSort) -> SortOrder {
    match sort {
        NavwarnSort::Received => SortOrder::Received,
        NavwarnSort::IssueTime => SortOrder::IssueTime,
        NavwarnSort::WarningType => SortOrder::WarningType,
        NavwarnSort::ProducerSeries => SortOrder::ProducerSeries,
        NavwarnSort::Classification => SortOrder::UserClassification,
    }
}

fn format_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%MZ").to_string()
}

fn timing_label(t: PartTiming) -> String {
    match t {
        PartTiming::NoRange => "in force".into(),
        PartTiming::InRange => "in its date range".into(),
        PartTiming::BeforeRange { starts } => format!("starts {}", format_time(starts)),
        PartTiming::AfterRange { ended } => format!("ended {}", format_time(ended)),
        PartTiming::OutsideRecurring => "outside its recurring dates".into(),
    }
}

fn cancel_reason(reason: &navwarn::CancelReason, registry: &NavwarnRegistry) -> String {
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

fn geometry_paths(g: &Geometry, out: &mut Vec<Vec<WorldPoint>>) {
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
fn convert(
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
// Application glue
// ─────────────────────────────────────────────────────────────────────────

impl ChartApp {
    /// Replace the host overlay instructions (plugins, S-124/S-129) of the
    /// live scene. Without overlay datasets this is exactly the plugin-only
    /// behaviour; with them the instructions are reinstalled only when they
    /// or the scene changed.
    pub(crate) fn sync_host_overlay_instructions(&mut self) {
        self.sync_overlay_viewing_time();
        let overlays = self.gml_overlays.instructions(&self.current_profile_name);
        let plugin = self.plugin_system.get_render_instructions();
        let Some((revision, list)) = overlays else {
            self.gml_overlays.installed = None;
            self.render_context.remove_coverage_exempt_instructions();
            for mut instr in plugin {
                instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
                self.render_context.add_instruction(instr);
            }
            return;
        };
        if plugin.is_empty()
            && self.gml_overlays.installed
                == Some((revision, self.render_context.geometry_revision()))
        {
            return;
        }
        self.render_context.remove_coverage_exempt_instructions();
        for mut instr in plugin {
            instr.set_portrayal_origin(ferrite_render::PortrayalOrigin::CoverageExempt);
            self.render_context.add_instruction(instr);
        }
        self.render_context.append_converted(list.to_vec());
        self.gml_overlays.installed = Some((revision, self.render_context.geometry_revision()));
    }

    /// Overlay instructions for a context being prepared with `profile`.
    pub(crate) fn append_overlay_instructions(
        &mut self,
        context: &mut RenderContext,
        profile: &str,
    ) {
        self.sync_overlay_viewing_time();
        if let Some((_, list)) = self.gml_overlays.instructions(profile) {
            context.append_converted(list.to_vec());
        }
    }

    /// S-124 status and fixedDateRange follow the viewing time selected for
    /// the S-101 date-dependent portrayal (system clock when live).
    pub(crate) fn sync_overlay_viewing_time(&mut self) -> bool {
        let time = viewing_time(&self.render_context.settings);
        self.gml_overlays.set_viewing_time(time)
    }

    /// Picking and selection work with overlays even without ENC cells.
    pub(crate) fn picking_enabled(&self) -> bool {
        self.chart_loaded || !self.gml_overlays.is_empty()
    }

    /// Open one overlay dataset (GML or import) from the open-dataset queue.
    pub(crate) fn open_overlay_dataset(&mut self, path: &Path) -> Result<Added> {
        let import = is_import_file(path, true);
        let verify = self.signature_verification_enabled();
        let (dataset, provenance) = if import {
            anyhow::ensure!(
                !verify,
                "Imported navigational warnings cannot be authenticated; switch off dataset signature verification to view them (they are never official S-124 data)"
            );
            let (dataset, format) = self.gml_overlays.read(path, path)?;
            (dataset, Provenance::Imported(format.unwrap_or("import")))
        } else {
            let report = self.authenticate_paths(&[path.to_path_buf()])?;
            let snapshot =
                crate::dataset_signature_policy::dataset_snapshot(&report, path, verify)?;
            let bytes_path = snapshot
                .as_ref()
                .map(|s| s.path().to_path_buf())
                .unwrap_or_else(|| path.to_path_buf());
            let (dataset, _) = self.gml_overlays.read(path, &bytes_path)?;
            let provenance = if snapshot.is_some() {
                Provenance::Authenticated
            } else {
                Provenance::Unverified
            };
            (dataset, provenance)
        };
        let added = self.gml_overlays.add(dataset, provenance)?;
        if matches!(added, Added::New) {
            self.overlays_changed(true);
        }
        Ok(added)
    }

    /// After datasets were added or removed (or options changed): fit the
    /// view when there is no ENC, refresh the tree, lists and the scene.
    pub(crate) fn overlays_changed(&mut self, fit_without_charts: bool) {
        self.gml_overlays.ui_revision = 0;
        // Charts own the view; without them the overlays define it.
        if !self.chart_loaded && self.loading_state.is_none() && self.pending_auto_chart.is_empty()
        {
            match self.gml_overlays.bounds() {
                Some(mut b) if fit_without_charts => {
                    let pad = (b.width().max(b.height()) * 0.1).max(0.01);
                    b = GeoBounds::new(b.min_x - pad, b.min_y - pad, b.max_x + pad, b.max_y + pad);
                    self.bounds = b;
                    self.zoom_level = 1.0;
                    self.zoom_target = 1.0;
                    self.pan_offset = (0.0, 0.0);
                    self.render_context.set_bounds(b);
                }
                None if self.gml_overlays.is_empty() => {
                    self.bounds = GeoBounds::new(-180.0, -90.0, 180.0, 90.0);
                }
                _ => {}
            }
        }
        self.refresh_dataset_tree();
        if self.renderer.is_some() {
            self.update_view();
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Centre and zoom the chart on geographic bounds.
    pub(crate) fn focus_geo_bounds(&mut self, b: GeoBounds) {
        let c = b.center();
        if let Some(pan) = self
            .render_context
            .scaler
            .projection()
            .pan_to(self.bounds, c)
        {
            self.pan_offset = (pan[0], pan[1]);
        }
        let w = b.width().max(0.005);
        let h = b.height().max(0.005);
        let zoom = (self.bounds.width() / w).min(self.bounds.height() / h) * 0.6;
        if zoom.is_finite() {
            let zoom = zoom.clamp(crate::navigation::MIN_ZOOM, crate::navigation::MAX_ZOOM);
            self.zoom_level = zoom;
            self.zoom_target = zoom;
        }
        self.zoom_animating = false;
        self.update_view();
    }

    /// Select an overlay feature on the chart (and show its details).
    pub(crate) fn select_overlay_feature(&mut self, key: i64) {
        let Some(pick) = self.gml_overlays.describe(key) else {
            return;
        };
        let world = self
            .gml_overlays
            .locate(key)
            .and_then(|(entry, feature)| {
                let object = entry.dataset.objects.get(feature.object?)?;
                let c = object.geometry.first()?.coords().next().copied()?;
                Some((c[0], c[1]))
            })
            .unwrap_or((0.0, 0.0));
        let selected = ferrite_wgpu::SelectedFeature {
            feature_type: pick.feature_type,
            feature_id: key,
            foid: pick.foid,
            cell_index: None,
            primitive_type: pick.primitive,
            source: Some(pick.source),
            attributes: pick.attributes,
            world_pos: world,
            longitude_shift: 0.0,
            definition: pick.definition,
            symbol_name: None,
        };
        self.select_feature(Some(selected));
    }

    /// Keep the overlay windows' rows current and act on their requests.
    pub(crate) fn service_overlay_ui(&mut self) {
        if self.sync_overlay_viewing_time() | self.gml_overlays.evaluate(false) {
            // Lifecycle changed with time (cancellation date, date ranges).
            self.gml_overlays.ui_revision = 0;
            self.update_view();
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let requests = renderer.ui_state.overlays.take_requests();
        let visibility = renderer.ui_state.overlay_visibility_requested.take();
        let ui = &renderer.ui_state.overlays;
        let options = (
            ui.plain_boundaries,
            ui.apply_scale_minimum,
            ui.show_before_range,
            ui.show_information,
        );
        let sort = sort_order(ui.sort);
        let mut changed = false;
        if requests.options_changed {
            let g = &mut self.gml_overlays;
            g.plain_boundaries = options.0;
            g.apply_scale_minimum = options.1;
            g.policy.show_before_fixed_date_range = options.2;
            g.policy.show_information = options.3;
            g.invalidate();
            changed = true;
        }
        if requests.sort_changed {
            self.gml_overlays.sort = sort;
            self.gml_overlays.ui_revision = 0;
        }
        if let Some((id, class)) = requests.classify {
            self.gml_overlays.classify(
                id,
                match class {
                    NavwarnClassification::OnChart => UserClassification::OnChart,
                    NavwarnClassification::OffChart => UserClassification::OffChart,
                    NavwarnClassification::Information => UserClassification::Information,
                },
            );
            changed = true;
        }
        if let Some((id, visible)) = visibility {
            if self.gml_overlays.set_visible(id, visible) {
                changed = true;
            }
        }
        if changed {
            self.gml_overlays.ui_revision = 0;
            self.update_view();
            self.refresh_visible_selection();
        }
        if let Some((id, part)) = requests.select {
            self.select_overlay_message(id, part);
        }
        if let Some(id) = requests.select_plan {
            if let Some(b) = self.gml_overlays.dataset(id).and_then(|d| d.bounds(None)) {
                self.focus_geo_bounds(b);
            }
        }
        self.publish_overlay_rows();
    }

    fn select_overlay_message(&mut self, id: u32, part: Option<String>) {
        let product = self.gml_overlays.dataset(id).map(|d| d.product());
        let part = part.or_else(|| {
            (product == Some(Product::S124))
                .then(|| self.gml_overlays.first_part(id))
                .flatten()
        });
        self.gml_overlays.mark_displayed(id);
        if let Some(part) = &part {
            if self.gml_overlays.list_selection.as_ref() != Some(&(id, part.clone())) {
                self.gml_overlays.list_selection = Some((id, part.clone()));
                self.gml_overlays.invalidate();
            }
        }
        if let Some(b) = self.gml_overlays.focus_bounds(id, part.as_deref()) {
            self.focus_geo_bounds(b);
        } else {
            self.update_view();
        }
        if let Some(key) = part.and_then(|p| self.gml_overlays.key_of(id, &p)) {
            self.select_overlay_feature(key);
        }
        self.gml_overlays.ui_revision = 0;
    }

    fn publish_overlay_rows(&mut self) {
        if self.gml_overlays.ui_revision != 0 {
            return;
        }
        self.gml_overlays.ui_revision = 1;
        let (messages, missing, new_count) = self.gml_overlays.navwarn_rows();
        let plans = self.gml_overlays.ukc_rows();
        let g = &self.gml_overlays;
        let lifecycle_time = match g.viewing_time {
            Some(t) => format!("Evaluated at viewing time {}", format_time(t)),
            None => format!("Evaluated at {} (system clock)", format_time(Utc::now())),
        };
        let hidden: std::collections::BTreeSet<u32> = g
            .datasets
            .iter()
            .filter(|d| !d.visible)
            .map(|d| d.id)
            .collect();
        if let Some(renderer) = &mut self.renderer {
            let ui = &mut renderer.ui_state.overlays;
            ui.messages = messages;
            ui.missing = missing;
            ui.new_count = new_count;
            ui.plans = plans;
            ui.plain_boundaries = g.plain_boundaries;
            ui.apply_scale_minimum = g.apply_scale_minimum;
            ui.show_before_range = g.policy.show_before_fixed_date_range;
            ui.show_information = g.policy.show_information;
            ui.user_status = g.user_status();
            ui.lifecycle_time = lifecycle_time;
            renderer.ui_state.overlay_hidden = hidden;
        }
    }

    /// Data-layer tree products and catalogue layers of the overlays.
    pub(crate) fn overlay_tree(
        &self,
    ) -> (
        Vec<ferrite_wgpu::DatasetProductLayer>,
        Vec<ferrite_wgpu::DatasetCatalogueLayer>,
    ) {
        let g = &self.gml_overlays;
        let mut layers = Vec::new();
        let mut catalogues = Vec::new();
        for product in [Product::S124, Product::S129] {
            let id = product.identifier();
            let files: Vec<_> = g
                .datasets
                .iter()
                .filter(|d| d.product() == product)
                .map(|d| {
                    let errors = d
                        .issues
                        .iter()
                        .filter(|i| i.severity == Severity::Error)
                        .count();
                    ferrite_wgpu::DatasetLayerEntry {
                        id: ferrite_wgpu::DatasetLayerId::Overlay {
                            product: id.into(),
                            id: d.id,
                        },
                        name: if d.dataset.imported {
                            format!("{} (imported)", d.name())
                        } else {
                            d.name()
                        },
                        source: d
                            .dataset
                            .source
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        detail: format!(
                            "{} · {} objects · {} validation errors, {} warnings",
                            d.provenance.label(),
                            d.dataset.objects.len(),
                            errors,
                            d.issues.len() - errors
                        ),
                    }
                })
                .collect();
            let c = g.catalogues(product);
            match c {
                Some(c) => catalogues.push(ferrite_wgpu::DatasetCatalogueLayer {
                    product: id.into(),
                    fc: c.fc_status.clone(),
                    pc: c.pc_status.clone(),
                }),
                None => {
                    let failed = CatalogueStatus {
                        product_id: id.into(),
                        validation_message: g
                            .catalogue_errors
                            .iter()
                            .find(|e| e.starts_with(id))
                            .cloned(),
                        ..Default::default()
                    };
                    catalogues.push(ferrite_wgpu::DatasetCatalogueLayer {
                        product: id.into(),
                        fc: failed.clone(),
                        pc: failed,
                    });
                }
            }
            if files.is_empty() {
                continue;
            }
            layers.push(ferrite_wgpu::DatasetProductLayer {
                product: id.into(),
                fc: c
                    .map(|c| format!("FC {} · {}", c.fc_status.version, c.fc_status.path))
                    .unwrap_or_else(|| "FC unavailable".into()),
                pc: c
                    .map(|c| {
                        format!(
                            "PC {} · {} · overlay",
                            c.pc_status.version, c.pc_status.path
                        )
                    })
                    .unwrap_or_else(|| "PC unavailable".into()),
                files,
            });
        }
        (layers, catalogues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
        let found =
            discover(&root().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB")).unwrap();
        assert_eq!(found.len(), 5);
        let shom = root().join("TestData/SHOM-PING-S124");
        if shom.exists() {
            let found = discover(&shom).unwrap();
            // 4 KML, 1 GPX, 1 extracted shapefile; the ZIP only when selected.
            assert_eq!(found.len(), 6, "{found:?}");
            assert!(is_overlay_file(&shom.join("shapefile/shapefiles.zip")));
        }
    }

    /// The installed catalogues, the UKHO S-124 exchange set, the IHO S-129
    /// test datasets and the SHOM imports all produce overlay instructions.
    #[test]
    fn test_data_produces_overlay_instructions() {
        let base = root();
        let data = base.join("TestData");
        if !data.join("IHO-S129-Test-Datasets").exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = GmlOverlays::load(&base);
        assert!(g.catalogue_errors.is_empty(), "{:?}", g.catalogue_errors);
        let mut paths = discover(&data.join("UKHO-S100-2026/ExchangeSets/S-124_GB")).unwrap();
        paths.extend(discover(&data.join("IHO-S129-Test-Datasets")).unwrap());
        let shom = data.join("SHOM-PING-S124");
        for dir in ["kml", "gpx"] {
            paths.extend(discover(&shom.join(dir)).unwrap());
        }
        paths.push(shom.join("shapefile/shapefiles.zip"));
        for path in &paths {
            let (dataset, format) = g.read(path, path).unwrap();
            let provenance = match format {
                Some(f) => Provenance::Imported(f),
                None => Provenance::Unverified,
            };
            assert!(matches!(g.add(dataset, provenance).unwrap(), Added::New));
        }
        let (_, list) = g.instructions("Day").unwrap();
        let per_dataset = |id: u32| {
            list.iter()
                .filter(|i| decode_key(i.feature_id().unwrap()).unwrap().0 == id)
                .count()
        };
        for d in &g.datasets {
            eprintln!("{} {:?}: {}", d.name(), d.product(), per_dataset(d.id));
        }
        assert!(list.iter().all(is_overlay_instruction));
        assert!(list
            .iter()
            .all(|i| i.viewing_groups().all(|v| v.0 == OVERLAY_VIEWING_GROUP)));
        let by_product = |p: Product| {
            g.datasets
                .iter()
                .filter(|d| d.product() == p)
                .map(|d| per_dataset(d.id))
                .sum::<usize>()
        };
        assert!(by_product(Product::S124) > 0);
        assert!(by_product(Product::S129) > 0);
        for d in g.datasets.iter().filter(|d| d.dataset.imported) {
            assert!(per_dataset(d.id) > 0, "{} not portrayed", d.name());
        }
        // Every S-129 dataset is portrayed; text uses the opaque decision.
        for d in g.datasets.iter().filter(|d| d.product() == Product::S129) {
            assert!(per_dataset(d.id) > 0);
        }
        assert!(list
            .iter()
            .any(|i| matches!(i, DrawingInstruction::Text(_))));
        // The S-124 implicit groups keep TextPlacement text.
        let s124_text = list.iter().any(|i| {
            matches!(i, DrawingInstruction::Text(_))
                && g.dataset(decode_key(i.feature_id().unwrap()).unwrap().0)
                    .is_some_and(|d| d.product() == Product::S124 && !d.dataset.imported)
        });
        eprintln!("S-124 text instructions present: {s124_text}");
        // Picking: attributes come with FC names.
        let key = list
            .iter()
            .filter_map(|i| i.feature_id())
            .find(|k| {
                g.locate(*k)
                    .is_some_and(|(d, f)| d.product() == Product::S124 && f.code == "NavwarnPart")
            })
            .unwrap();
        let pick = g.describe(key).unwrap();
        assert!(
            pick.feature_type.contains("NavwarnPart"),
            "{}",
            pick.feature_type
        );
        assert!(pick.attributes.iter().any(|(k, _)| k == "Warning"));
        // UI rows.
        let (rows, _, _) = g.navwarn_rows();
        assert_eq!(
            rows.len(),
            g.datasets
                .iter()
                .filter(|d| d.product() == Product::S124)
                .count()
        );
        assert!(rows.iter().any(|r| r.imported));
        let plans = g.ukc_rows();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].control_points.len(), 15);
        assert!(!plans[0].vessel.is_empty());
        // Toggling scaleMinimum changes the S-129 output.
        g.apply_scale_minimum = false;
        g.invalidate();
        let (_, list2) = g.instructions("Night").unwrap();
        assert!(list2.len() >= list.len());
        // Hiding a dataset removes its instructions.
        let first = g.datasets[0].id;
        assert!(g.set_visible(first, false));
        let (_, list3) = g.instructions("Night").unwrap();
        assert!(list3
            .iter()
            .all(|i| decode_key(i.feature_id().unwrap()).unwrap().0 != first));
    }
    /// TextPlacement uses viewing group 26, which the S-124 2.0.0 catalogue
    /// does not declare: the text must still be portrayed.
    #[test]
    fn s124_text_placement_uses_implicit_viewing_group() {
        let base = root();
        let path = base.join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S2Y2026N43.GML",
        );
        if !path.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = GmlOverlays::load(&base);
        let text = std::fs::read_to_string(&path).unwrap().replace(
            "</S124:members>",
            "<S124:TextPlacement gml:id=\"TP1\"><S124:text>TEST LABEL</S124:text>\
             <S124:geometry><S100:pointProperty><S100:Point gml:id=\"TPG\"><gml:pos>50.70 -1.10</gml:pos>\
             </S100:Point></S100:pointProperty></S124:geometry></S124:TextPlacement></S124:members>",
        );
        let fc = g.s124.as_ref().unwrap().fc.clone();
        let dataset =
            ferrite_s100_gml::read_bytes(text.as_bytes(), Some(&fc), Default::default()).unwrap();
        assert!(matches!(
            g.add(dataset, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (_, list) = g.instructions("Day").unwrap();
        assert!(
            list.iter()
                .any(|i| matches!(i, DrawingInstruction::Text(t) if t.text.contains("TEST LABEL"))),
            "{list:#?}"
        );
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
        let paths = discover(&dir).unwrap();
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
    /// Minimal S-124 dataset: one warning with one point part.
    fn s124_dataset(g: &GmlOverlays, number: u32, extra_preamble: &str, part: &str) -> Dataset {
        let xml = format!(
            "<?xml version=\"1.0\"?><S124:Dataset gml:id=\"DS\" xmlns:S124=\"http://www.iho.int/S124/gml/2.0\" \
             xmlns:S100=\"http://www.iho.int/s100gml/5.0\" xmlns:gml=\"http://www.opengis.net/gml/3.2\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\"><S100:DatasetIdentificationInformation>\
             <S100:productIdentifier>S-124</S100:productIdentifier></S100:DatasetIdentificationInformation><S124:members>\
             <S124:NavwarnPreamble gml:id=\"PRE\"><S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction>\
             <S124:nameOfSeries>WZ</S124:nameOfSeries><S124:warningNumber>{number}</S124:warningNumber>\
             <S124:warningType code=\"2\">x</S124:warningType><S124:year>26</S124:year></S124:messageSeriesIdentifier>\
             <S124:navwarnTitle><S124:language>eng</S124:language><S124:text>TITLE {number}</S124:text></S124:navwarnTitle>\
             <S124:intService>true</S124:intService><S124:navwarnTypeGeneral code=\"1\">Aids to Navigation Changes</S124:navwarnTypeGeneral>\
             <S124:publicationTime>20260201T000000Z</S124:publicationTime>{extra_preamble}</S124:NavwarnPreamble>\
             <S124:NavwarnPart gml:id=\"P1\"><S124:warningInformation><S124:information><S124:language>eng</S124:language>\
             <S124:text>BUOY OFF STATION</S124:text></S124:information></S124:warningInformation>{part}<S124:header xlink:href=\"#PRE\"/>\
             <S124:geometry><S100:pointProperty><S100:Point gml:id=\"G1\"><gml:pos>50.0 -1.0</gml:pos></S100:Point></S100:pointProperty></S124:geometry>\
             </S124:NavwarnPart></S124:members></S124:Dataset>"
        );
        let fc = g.s124.as_ref().expect("S-124 catalogues").fc.clone();
        ferrite_s100_gml::read_bytes(xml.as_bytes(), Some(&fc), Default::default()).unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        navwarn::parse_date_time(s).unwrap()
    }

    /// S-124 11.1.1: classifications, "displayed" marks and first receipt
    /// survive a restart; a corrupt file never blocks startup.
    #[test]
    fn navwarn_user_state_persists_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navwarn").join("alice.json");
        let start = |path: &Path| {
            let mut g = GmlOverlays::load(&root());
            g.attach_user_store(path.to_path_buf(), "alice".into());
            g
        };
        // Session 1: receive, classify, display.
        let mut g = start(&path);
        assert!(g.user_status().contains("alice"), "{}", g.user_status());
        let a = s124_dataset(&g, 70, "", "");
        let b = s124_dataset(&g, 71, "", "");
        let (key_a, key_b) = (navwarn::dataset_key(&a), navwarn::dataset_key(&b));
        assert!(matches!(
            g.add(a.clone(), Provenance::Unverified).unwrap(),
            Added::New
        ));
        assert!(matches!(
            g.add(b.clone(), Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (id_a, id_b) = (g.datasets[0].id, g.datasets[1].id);
        let received_a = g.datasets[0].received;
        g.classify(id_a, UserClassification::OffChart);
        g.classify(id_b, UserClassification::Information);
        g.mark_displayed(id_a);
        assert!(path.exists());
        let message_key = g.registry().messages[0].key.clone();
        assert!(!message_key.starts_with("sha256:"), "{message_key}");
        let (rows, _, new_count) = g.navwarn_rows();
        assert!(!rows[0].new || rows[0].dataset != id_a);
        assert_eq!(new_count, 0); // a displayed, b Information
        drop(g);

        // Session 2 (restart): same OS user, same datasets.
        let mut g = start(&path);
        assert!(g.user_status().contains("restored"), "{}", g.user_status());
        assert_eq!(
            g.user.classification.get(&message_key),
            Some(&UserClassification::OffChart)
        );
        assert!(g.user.displayed.contains(&key_a));
        assert!(g.user.received.contains_key(&key_b));
        assert!(matches!(
            g.add(a.clone(), Provenance::Unverified).unwrap(),
            Added::New
        ));
        assert_eq!(g.datasets[0].received, received_a, "first receipt kept");
        let id = g.datasets[0].id;
        let (rows, _, _) = g.navwarn_rows();
        let row = rows.iter().find(|r| r.dataset == id).unwrap();
        assert_eq!(row.classification, NavwarnClassification::OffChart);
        assert!(!row.new && !row.portrayed);
        // Reclassifying on chart removes the entry from the file.
        g.classify(id, UserClassification::OnChart);
        drop(g);
        let g = start(&path);
        assert!(!g.user.classification.contains_key(&message_key));
        assert_eq!(g.user.classification.len(), 1); // b stays Information

        // Another OS user has a separate file.
        let bob = dir.path().join("navwarn").join("bob.json");
        let g = start(&bob);
        assert!(g.user.classification.is_empty() && g.user.received.is_empty());

        // Corrupt file: empty state, file set aside, saving works again.
        std::fs::write(&path, b"{not json").unwrap();
        let mut g = start(&path);
        assert!(g.user.classification.is_empty());
        assert!(
            g.user_status().contains("unreadable"),
            "{}",
            g.user_status()
        );
        assert!(!path.exists());
        assert!(std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".unreadable-")));
        assert!(matches!(
            g.add(a, Provenance::Unverified).unwrap(),
            Added::New
        ));
        assert!(path.exists());
        assert!(read_user_state(&path).unwrap().is_some());
        // Unsupported version and oversized keys are rejected, not trusted.
        std::fs::write(
            &path,
            br#"{"version":9,"user":"alice","saved":"2026-01-01T00:00:00Z","classification":{},"displayed":[],"received":{}}"#,
        )
        .unwrap();
        assert!(read_user_state(&path).is_err());
        // No store: nothing written, startup unaffected.
        let mut g = GmlOverlays::load(&root());
        assert_eq!(g.user_status(), "User state not saved");
        let c = s124_dataset(&g, 72, "", "");
        assert!(matches!(
            g.add(c, Provenance::Unverified).unwrap(),
            Added::New
        ));
    }

    #[test]
    fn user_state_path_is_per_os_user() {
        if std::env::var_os("FERRITE_NAVWARN_STATE_PATH").is_some() {
            return;
        }
        // Tests never persist to the real application folder.
        assert_eq!(default_user_state_path("alice"), None);
        assert!(!os_user_name().is_empty());
    }

    /// S-124 status and fixedDateRange follow the S-101 viewing time; the
    /// 24 h new-message indication keeps running on the real clock.
    #[test]
    fn navwarn_lifecycle_follows_viewing_time() {
        let mut g = GmlOverlays::load(&root());
        let range = "<S124:fixedDateRange><S124:dateEnd>2026-02-12</S124:dateEnd>\
                     <S124:dateStart>2026-02-10</S124:dateStart></S124:fixedDateRange>";
        let ds = s124_dataset(
            &g,
            80,
            "<S124:cancellationDate>20260220T000000Z</S124:cancellationDate>",
            range,
        );
        assert!(matches!(
            g.add(ds, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let id = g.datasets[0].id;
        let status_at = |g: &mut GmlOverlays, time: Option<&str>| {
            g.set_viewing_time(time.map(t));
            let r = g.registry();
            (
                r.messages[0].status.clone(),
                r.messages[0].parts[0].timing,
                r.now,
            )
        };
        let (status, timing, now) = status_at(&mut g, Some("2026-02-05T00:00:00Z"));
        assert_eq!(status, Status::NotYetActive);
        assert!(matches!(timing, PartTiming::BeforeRange { .. }));
        assert_eq!(now, t("2026-02-05T00:00:00Z"));
        let (status, timing, _) = status_at(&mut g, Some("2026-02-11T00:00:00Z"));
        assert_eq!(status, Status::Active);
        assert_eq!(timing, PartTiming::InRange);
        // In range: portrayed even with the strict policy.
        g.policy.show_before_fixed_date_range = false;
        g.invalidate();
        let (_, list) = g.instructions("Day").unwrap();
        assert!(!list.is_empty());
        let (status, timing, _) = status_at(&mut g, Some("2026-02-13T00:00:00Z"));
        assert_eq!(status, Status::Active);
        assert!(matches!(timing, PartTiming::AfterRange { .. }));
        let (_, list) = g.instructions("Day").unwrap();
        assert!(list.is_empty(), "part after its range is not portrayed");
        let (status, _, _) = status_at(&mut g, Some("2026-02-20T00:00:00Z"));
        assert!(status.is_cancelled());
        // Received now (real clock): still new although the viewing time is
        // months earlier.
        let (rows, _, new_count) = g.navwarn_rows();
        assert!(rows[0].new && new_count == 1);
        assert!(rows[0].status.starts_with("Cancelled"));
        // No viewing time: the system clock (2026-02-20 has passed).
        let (status, _, now) = status_at(&mut g, None);
        assert!(status.is_cancelled());
        assert!((Utc::now() - now).num_seconds().abs() < 60);
        // Unchanged selection: no re-evaluation is forced.
        assert!(!g.set_viewing_time(None));
        assert!(g.set_viewing_time(Some(t("2026-02-11T00:00:00Z"))));
        assert!(g.next_check().is_some());
        assert_eq!(g.dataset(id).map(|d| d.product()), Some(Product::S124));
    }

    /// The object panel previews S-124 symbols from the registered overlay
    /// source (no source cell).
    #[test]
    fn overlay_symbol_preview_resolves_registered_source() {
        let mut g = GmlOverlays::load(&root());
        let ds = s124_dataset(&g, 90, "", "");
        assert!(matches!(
            g.add(ds, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (_, list) = g.instructions("Day").unwrap();
        let (key, symbol) = list
            .iter()
            .find_map(|i| match i {
                DrawingInstruction::Point(p) if p.symbol_ref.starts_with("S-124/") => {
                    Some((p.feature_id?, p.symbol_ref.clone()))
                }
                _ => None,
            })
            .expect("S-124 point symbol");
        let pick = g.describe(key).unwrap();
        let feature = ferrite_wgpu::SelectedFeature {
            feature_type: pick.feature_type,
            feature_id: key,
            foid: pick.foid,
            cell_index: None,
            primitive_type: pick.primitive,
            source: Some(pick.source),
            attributes: pick.attributes,
            world_pos: (0., 0.),
            longitude_shift: 0.,
            definition: pick.definition,
            symbol_name: Some(symbol),
        };
        let source = ferrite_wgpu::overlay_symbol_source("S-124").unwrap();
        let active = source
            .profiles()
            .get_default()
            .or_else(|| source.profiles().profiles.values().next())
            .unwrap()
            .clone();
        let preview = ferrite_wgpu::prepare_overlay_symbol_preview(&feature, &active)
            .unwrap()
            .expect("overlay preview");
        assert!(preview.matches(&feature, &active.id));
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

    /// Selecting a NavwarnPart highlights its NavwarnAreaAffected (11.2),
    /// which has no portrayal of its own.
    #[test]
    fn selected_part_highlights_area_affected() {
        let base = root();
        let path = base.join(
            "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES/124GB00S2Y2026N44.GML",
        );
        if !path.exists() {
            eprintln!("skipping: TestData not present");
            return;
        }
        let mut g = GmlOverlays::load(&base);
        let (dataset, _) = g.read(&path, &path).unwrap();
        assert!(matches!(
            g.add(dataset, Provenance::Unverified).unwrap(),
            Added::New
        ));
        let (_, list) = g.instructions("Day").unwrap();
        let id = g.datasets[0].id;
        // The area affected is never drawn by the catalogue.
        let area = g.key_of(id, "P1_A1").unwrap();
        assert!(list.iter().all(|i| i.feature_id() != Some(area)));
        let part = g.key_of(id, "P1").unwrap();
        let highlight = g.highlight_geometry(part);
        assert!(highlight.iter().any(|ring| ring.len() > 5), "{highlight:?}");
        // Listing selection zooms to the part and its area.
        assert!(g.focus_bounds(id, Some("P1")).is_some());
        assert_eq!(g.first_part(id).as_deref(), Some("P1"));
    }
}
