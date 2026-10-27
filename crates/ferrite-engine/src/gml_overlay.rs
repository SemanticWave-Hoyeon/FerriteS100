//! The GML overlay layer of the engine (S-124, S-129 and any other S-100
//! Part 10b product module registered in [`crate::products`]): feature keys,
//! dataset and exchange-set discovery, the viewing time, the Part 9a
//! conversion of the XSLT display list to host instructions and the overlay
//! host ([`GmlOverlays`]), which loads the registered products' catalogues,
//! holds the datasets of all of them and portrays them through the shared
//! XSLT pipeline. Everything product-specific (lifecycles, per-user state,
//! portrayal options, imports, validation rules, feature details) is in the
//! product crates behind [`GmlOverlayProduct`]; user-interface glue (rows,
//! symbol registration) stays in the application.
use crate::platform::{
    DataSource, EntryKind, NativeDataSource, Platform, SourceModules, UserRecord,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_product_api::open::{InstallContext, Loaded, OpenContext, OpenNotes, OpenStage};
use ferrite_product_api::report::{
    DatasetId, DatasetSummary, InstallReport, IssueSeverity, ValidationIssue,
};
use ferrite_product_api::{
    DatasetInput, FeaturePick, Opened, PortrayalRequest, ProductId, ProductLayer, SceneContribution,
};
use ferrite_render::host_settings::CatalogueStatus;
use ferrite_render::{DrawingInstruction, GeoBounds, RenderContext, Viewport, WorldPoint};
use ferrite_s100_gml::overlay::{self, OverlayCore};
use ferrite_s100_gml::portrayal::{self, FeatureInstructions, PortrayalInput, XsltPortrayal};
use ferrite_s100_gml::product::GmlOverlayProduct;
use ferrite_s100_gml::{Dataset, Geometry, ObjectKind, Product, Severity, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

pub use ferrite_product_api::Provenance;
pub use ferrite_s100_gml::overlay::{format_time, OverlayDataset};

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

/// The product (of those registered) of a GML file: namespace or
/// productIdentifier in the first 64 KiB.
pub fn sniff_product(path: &Path) -> Option<Product> {
    sniff_product_in(&NativeDataSource, path)
}

/// [`sniff_product`] through a data source.
pub fn sniff_product_in(data: &dyn DataSource, path: &Path) -> Option<Product> {
    if !overlay::sniffable_name(path) {
        return None;
    }
    let head = data.read_limited(path, 64 * 1024 - 1).ok()?;
    overlay::sniff_head(&head, &crate::products::gml_products())
}

/// [`sniff_product`] of a dataset already in memory.
pub fn sniff_product_bytes(path: &Path, bytes: &[u8]) -> Option<Product> {
    overlay::sniff(path, bytes, &crate::products::gml_products())
}

/// A non-S-100 source a registered product imports (S-124: KML, GPX,
/// shapefile; ZIP archives only when `allow_zip`).
pub fn is_import_file(path: &Path, allow_zip: bool) -> bool {
    let rules = crate::products::gml_rules();
    rules
        .iter()
        .flat_map(|r| r.import_extensions())
        .any(|e| crate::dataset_discovery::is_dataset_file(path, e))
        || (allow_zip
            && rules.iter().any(|r| !r.import_extensions().is_empty())
            && crate::dataset_discovery::is_dataset_file(path, "zip"))
}

/// An explicitly selected file that opens as an overlay dataset.
pub fn is_overlay_file(path: &Path) -> bool {
    is_import_file(path, true) || sniff_product(path).is_some()
}

/// Overlay datasets under a folder: GML datasets of the registered products
/// and the sources they import (S-124: KML, GPX and extracted shapefiles). ZIP archives are opened only as explicit files
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

/// Exchange-set validation (the registered products' rules: S-158-129 11,
/// 17-20; S-124 9.5-9.8, 12) of the exchange sets under an opened folder. Check 19 sees the datasets on the
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
        let report = ferrite_s100_gml::exchange::validate_exchange_set(
            &root,
            Some(&system),
            &crate::products::gml_rules(),
        );
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

/// The Part 9a conversion of drawing instructions to host instructions,
/// shared with S-101 cell portrayal (`ferrite_s101`, linked with any GML
/// product; without one no overlay dataset is ever opened).
fn convert_part9a(
    results: &[ferrite_lua::PortrayalResult],
    cell: &ferrite_s100_core::S101Cell,
    pc: &PortrayalCatalogue,
    context: &mut RenderContext,
    profile: &str,
) -> Result<()> {
    #[cfg(any(feature = "s101", feature = "s124", feature = "s129"))]
    return ferrite_s101::convert_lua_results_for_cell(results, cell, pc, context, 0, profile);
    #[cfg(not(any(feature = "s101", feature = "s124", feature = "s129")))]
    {
        let _ = (results, cell, pc, context, profile);
        anyhow::bail!("No GML product is built")
    }
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
    if let Err(error) = convert_part9a(&results, &cell, pc, &mut context, profile) {
        // One bad feature must not hide the rest of the dataset.
        warn!(
            "{}: overlay conversion failed ({error:#}); converting features separately",
            input.dataset_name
        );
        context = new_context();
        for result in &results {
            let mut single = new_context();
            match convert_part9a(
                std::slice::from_ref(result),
                &cell,
                pc,
                &mut single,
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
// Catalogues
// ─────────────────────────────────────────────────────────────────────────

/// Registers a product's portrayal catalogue as the symbol source of its
/// namespaced overlay symbols (the host's renderer resolves them); false
/// rejects the product prefix.
pub type SymbolRegistrar = fn(&str, &ferrite_portrayal_catalog::BoundPortrayalCatalogue) -> bool;

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
        product: &dyn GmlOverlayProduct,
        register_symbols: SymbolRegistrar,
    ) -> Result<Self> {
        let id = product.product().identifier();
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
        for &group in product.undeclared_viewing_groups() {
            if xslt.info.viewing_groups.contains(&group.to_string()) {
                continue;
            }
            // Literal Part 9: the group cannot be enabled, so its
            // instructions (S-124: TextPlacement text, co-located part
            // counts) are not drawn. The host catalogue declares it anyway
            // so the errata correction can draw them on request.
            pc.viewing_groups
                .declare_implicit(group, &format!("{id} implicit viewing group {group}"));
            implicit_groups.insert(group.to_string());
            warn!(
                "Conformance note: {id} {} rules use viewing group {group}, which portrayal_catalogue.xml does not declare; its instructions are not drawn (S-100 Part 9 9-11.2.2). \"Correct known PC errata\" draws them as an implicitly declared overlay group",
                pc.version
            );
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
// Overlay host: registered product modules, catalogues, datasets, portrayal
// ─────────────────────────────────────────────────────────────────────────

/// Outcome of [`GmlOverlays::add`].
pub enum Added {
    New,
    AlreadyOpen,
}

/// Fields of a selected overlay object.
pub type OverlayPick = FeaturePick;

/// The GML overlay layer: the product modules registered in this build
/// ([`crate::products::gml_rules`]), their catalogues and the loaded
/// datasets of all of them ([`OverlayCore`], reachable through `Deref`).
/// Product-specific behaviour (lifecycles, per-user state, portrayal
/// options, imports, feature details) is reached through the modules'
/// [`GmlOverlayProduct`] hooks; hosts that know a product use
/// [`GmlOverlays::product`] / [`GmlOverlays::product_mut`].
pub struct GmlOverlays {
    core: OverlayCore,
    products: Vec<Box<dyn GmlOverlayProduct>>,
    /// Catalogues of each registered product (None: not installed).
    catalogues: Vec<Option<ProductCatalogues>>,
    pub catalogue_errors: Vec<String>,
    next_id: u32,
    cache: Option<(String, u64, Arc<Vec<DrawingInstruction>>)>,
    /// (cache revision, render context geometry revision) last installed.
    pub installed: Option<(u64, u64)>,
    notes_logged: HashSet<String>,
    data: Arc<dyn DataSource>,
}

impl std::ops::Deref for GmlOverlays {
    type Target = OverlayCore;
    fn deref(&self) -> &OverlayCore {
        &self.core
    }
}

impl std::ops::DerefMut for GmlOverlays {
    fn deref_mut(&mut self) -> &mut OverlayCore {
        &mut self.core
    }
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
        let mut products: Vec<Box<dyn GmlOverlayProduct>> = crate::products::gml_rules()
            .into_iter()
            .map(|rules| rules.overlay())
            .collect();
        let mut errors = Vec::new();
        let mut loaded = Vec::new();
        for product in &mut products {
            match ProductCatalogues::load(
                data.as_ref(),
                catalogues,
                product.as_ref(),
                register_symbols,
            ) {
                Ok(c) => {
                    product.catalogues_loaded(&c.xslt.info, &c.pc.version);
                    loaded.push(Some(c));
                }
                Err(error) => {
                    let message = format!(
                        "{} catalogues unavailable: {error:#}",
                        product.product().identifier()
                    );
                    warn!("{message}");
                    errors.push(message);
                    loaded.push(None);
                }
            }
        }
        Self {
            core: OverlayCore::new(Arc::clone(&platform.clock)),
            products,
            catalogues: loaded,
            catalogue_errors: errors,
            next_id: 1,
            cache: None,
            installed: None,
            notes_logged: HashSet::new(),
            data,
        }
    }

    fn index_of(&self, product: Product) -> Option<usize> {
        self.products.iter().position(|p| p.product() == product)
    }

    /// The registered product modules, in registration order.
    pub fn products(&self) -> impl Iterator<Item = &dyn GmlOverlayProduct> {
        self.products.iter().map(|p| p.as_ref())
    }

    /// The registered product module of type `T`.
    pub fn product<T: GmlOverlayProduct>(&self) -> Option<&T> {
        self.products
            .iter()
            .find_map(|p| p.as_any().downcast_ref::<T>())
    }

    /// The registered product module of type `T` with the layer state its
    /// methods take.
    pub fn product_mut<T: GmlOverlayProduct>(&mut self) -> Option<(&mut T, &mut OverlayCore)> {
        let product = self
            .products
            .iter_mut()
            .find_map(|p| p.as_any_mut().downcast_mut::<T>())?;
        Some((product, &mut self.core))
    }

    /// The layer state.
    pub fn core(&self) -> &OverlayCore {
        &self.core
    }

    pub fn is_empty(&self) -> bool {
        self.core.datasets.is_empty()
    }

    /// Catalogues of a registered product, None when not installed.
    pub fn catalogues(&self, product: Product) -> Option<&ProductCatalogues> {
        self.catalogues[self.index_of(product)?].as_ref()
    }

    /// Whether any registered product has its catalogues installed.
    pub fn has_catalogues(&self) -> bool {
        self.catalogues.iter().any(Option::is_some)
    }

    /// Something visible changed: re-evaluate and re-portray.
    pub fn invalidate(&mut self) {
        self.core.bump_revision();
        for product in &mut self.products {
            product.invalidate();
        }
    }

    fn ensure_evaluated(&mut self) {
        for product in &mut self.products {
            product.ensure_evaluated(&mut self.core);
        }
    }

    pub fn dataset(&self, id: u32) -> Option<&OverlayDataset> {
        self.core.dataset(id)
    }

    /// The datasets on the system for exchange-set cancellation checks
    /// (S-158-129 19): file name and datasetFileIdentifier of every
    /// authoritative dataset (imported KML/GPX/shapefiles are excluded),
    /// except those a product withdrew (S-124 messages whose status is
    /// Cancelled, any reason). Products share one set; their file names carry
    /// the product prefix (124 / 129), so names do not collide in practice.
    pub fn on_system(&mut self) -> HashSet<String> {
        let mut withdrawn: HashSet<u32> = HashSet::new();
        for product in &mut self.products {
            withdrawn.extend(product.withdrawn(&mut self.core));
        }
        let mut names = HashSet::new();
        for d in &self.core.datasets {
            if d.dataset.imported || withdrawn.contains(&d.id) {
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

    /// The registered product importing a non-S-100 source (ZIP: the first
    /// product importing archives).
    fn importer(&self, path: &Path) -> Option<usize> {
        self.products
            .iter()
            .position(|p| {
                p.import_extensions()
                    .iter()
                    .any(|e| crate::dataset_discovery::is_dataset_file(path, e))
            })
            .or_else(|| {
                crate::dataset_discovery::is_dataset_file(path, "zip")
                    .then(|| self.products.iter().position(|p| p.imports_archives()))
                    .flatten()
            })
    }

    /// Read a GML dataset of a registered product (from `bytes_path`, the
    /// authenticated snapshot when verified) or import a non-S-100 source
    /// (S-124: KML/GPX/shapefile/ZIP).
    pub fn read(&self, path: &Path, bytes_path: &Path) -> Result<(Dataset, Option<&'static str>)> {
        if is_import_file(path, true) {
            let index = self
                .importer(path)
                .context("No registered product imports this format")?;
            let product = &self.products[index];
            let fc = self.catalogues[index].as_ref().map(|c| c.fc.as_ref());
            let bytes = self
                .data
                .read(path)
                .with_context(|| path.display().to_string())?;
            let (dataset, format) =
                product.import(path, &bytes, fc, self.core.now(), self.data.local_paths())?;
            return Ok((dataset, Some(format)));
        }
        let product = sniff_product_in(self.data.as_ref(), path)
            .context("Not a GML dataset of a registered product")?;
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

    /// Read a GML dataset supplied as bytes; `path` names it.
    pub fn read_bytes(&self, path: &Path, bytes: &[u8]) -> Result<Dataset> {
        let product = sniff_product_bytes(path, bytes)
            .context("Not a GML dataset of a registered product")?;
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
            .core
            .datasets
            .iter()
            .any(|d| d.dataset.sha256 == dataset.sha256)
        {
            return Ok(Added::AlreadyOpen);
        }
        let not_installed = || {
            format!(
                "{} catalogues are not installed",
                dataset.product.identifier()
            )
        };
        let index = self.index_of(dataset.product).with_context(not_installed)?;
        let catalogues = self.catalogues[index]
            .as_ref()
            .with_context(not_installed)?;
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
        issues.extend(validator.validate(&dataset, size, self.products[index].rules()));
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
        let received = self.products[index].received(&mut self.core, &dataset);
        self.core.datasets.push(OverlayDataset {
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
        let before = self.core.datasets.len();
        self.core.datasets.retain(|d| d.id != id);
        if self
            .core
            .list_selection
            .as_ref()
            .is_some_and(|(d, _)| *d == id)
        {
            self.core.list_selection = None;
        }
        let removed = self.core.datasets.len() != before;
        if removed {
            self.invalidate();
        }
        removed
    }

    pub fn set_visible(&mut self, id: u32, visible: bool) -> bool {
        match self.core.datasets.iter_mut().find(|d| d.id == id) {
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
            .core
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

    // ── Lifecycles ───────────────────────────────────────────────────────

    /// Evaluate the products' time-dependent state now, or reuse the
    /// evaluation of the last minute. Returns true when a result differs
    /// from the previous one.
    pub fn evaluate(&mut self, force: bool) -> bool {
        let mut changed = false;
        for product in &mut self.products {
            changed |= product.evaluate(&mut self.core, force);
        }
        changed
    }

    /// When the periodic lifecycle re-check is due (None without datasets).
    pub fn next_check(&self) -> Option<ferrite_kernel::time::Instant> {
        if self.core.datasets.is_empty() {
            return None;
        }
        self.products.iter().filter_map(|p| p.next_check()).min()
    }

    /// Follow the viewing time setting (None: system clock). Returns true
    /// when it changed; lifecycles are then re-evaluated and re-portrayed.
    pub fn set_viewing_time(&mut self, time: Option<DateTime<Utc>>) -> bool {
        if self.core.viewing_time == time {
            return false;
        }
        self.core.viewing_time = time;
        self.invalidate();
        self.core.ui_revision = 0;
        true
    }

    // ── Per-user state ───────────────────────────────────────────────────

    /// Keep `product`'s per-user state (S-124 11.1.1) in `record` for OS user
    /// `user`, restoring what it holds. Never fails: an unreadable record is
    /// set aside and the product starts empty.
    pub fn attach_user_record(
        &mut self,
        product: Product,
        record: Box<dyn UserRecord>,
        user: String,
    ) {
        let Some(index) = self.index_of(product) else {
            return;
        };
        if let Some(errata) = self.products[index].attach_user_record(&mut self.core, record, user)
        {
            if errata != self.core.correct_pc_errata() {
                self.core.set_correct_pc_errata(errata);
                self.log_errata_mode();
            }
        }
        self.invalidate();
        self.core.ui_revision = 0;
    }

    /// The "Correct known PC errata" option.
    pub fn correct_pc_errata(&self) -> bool {
        self.core.correct_pc_errata()
    }

    /// Switch the "Correct known PC errata" option; re-portrays and lets the
    /// products save it with their per-user state.
    pub fn set_correct_pc_errata(&mut self, on: bool) {
        if on == self.core.correct_pc_errata() {
            return;
        }
        self.core.set_correct_pc_errata(on);
        self.log_errata_mode();
        self.invalidate();
        for product in &mut self.products {
            product.errata_changed(&mut self.core);
        }
    }

    fn log_errata_mode(&self) {
        if self.core.correct_pc_errata() {
            let corrections: Vec<&str> = self
                .products
                .iter()
                .filter_map(|p| p.errata_correction())
                .collect();
            info!("Correct known PC errata: on ({})", corrections.join(", "));
        } else {
            info!("Correct known PC errata: off (portrayal catalogues applied literally per S-100 Part 9)");
        }
    }

    // ── Portrayal ────────────────────────────────────────────────────────

    /// Overlay drawing instructions for the palette, or None when no
    /// overlay dataset is loaded (the host then behaves exactly as before).
    pub fn instructions(&mut self, profile: &str) -> Option<(u64, Arc<Vec<DrawingInstruction>>)> {
        if self.core.datasets.is_empty() {
            return None;
        }
        self.evaluate(false);
        let revision = self.core.revision();
        if let Some((p, r, list)) = &self.cache {
            if p == profile && *r == revision {
                return Some((*r, Arc::clone(list)));
            }
        }
        let list = Arc::new(self.portray_datasets(profile));
        let revision = self.core.revision();
        self.cache = Some((profile.to_owned(), revision, Arc::clone(&list)));
        self.core.ui_revision = 0;
        Some((revision, list))
    }

    fn portray_datasets(&mut self, profile: &str) -> Vec<DrawingInstruction> {
        let started = ferrite_kernel::time::Instant::now();
        self.ensure_evaluated();
        let mut out = Vec::new();
        let errata = self.core.correct_pc_errata();
        let mut notes = Vec::new();
        for index in 0..self.core.datasets.len() {
            let product = self.core.datasets[index].product();
            let Some(pi) = self.index_of(product) else {
                continue;
            };
            let Some(catalogues) = self.catalogues[pi].as_ref() else {
                continue;
            };
            if !self.core.datasets[index].visible {
                continue;
            }
            let module = &self.products[pi];
            let suppress = module.suppressed(&self.core, self.core.datasets[index].id);
            let settings = module.portrayal_settings(errata, &catalogues.implicit_groups);
            let entry = &mut self.core.datasets[index];
            if entry.input.is_none() {
                entry.input = Some(portrayal::input_xml(
                    &entry.dataset,
                    Some(&catalogues.fc),
                    &module.input_options(),
                ));
            }
            if entry
                .portrayal
                .as_ref()
                .is_none_or(|(k, _)| *k != settings.key)
            {
                match catalogues.xslt.portray(
                    entry.input.as_ref().expect("input"),
                    &settings.context,
                    &settings.convert,
                ) {
                    Ok(p) => {
                        for note in &p.notes {
                            notes.push(format!(
                                "{} {}: {note}",
                                product.identifier(),
                                entry.dataset.display_name()
                            ));
                        }
                        entry.portrayal = Some((settings.key, p));
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
            let Some(suppress) = suppress else {
                continue;
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
            self.core.datasets.len(),
            started.elapsed().as_secs_f64() * 1e3
        );
        out
    }

    // ── Picking ──────────────────────────────────────────────────────────

    pub fn locate(&self, key: i64) -> Option<(&OverlayDataset, &portrayal::InputFeature)> {
        let (slot, feature) = decode_key(key)?;
        let entry = self.core.dataset(slot)?;
        let feature = entry.input.as_ref()?.features.get(feature)?;
        Some((entry, feature))
    }

    /// Selected-object report of an overlay feature key.
    pub fn describe(&mut self, key: i64) -> Option<OverlayPick> {
        self.ensure_evaluated();
        let (entry, feature) = self.locate(key)?;
        let index = self.index_of(entry.product())?;
        let module = &self.products[index];
        let catalogues = self.catalogues[index].as_ref()?;
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
        attributes.extend(module.describe(&self.core, entry.id));
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
            // Information the product reports with the feature (S-124: the
            // preamble carries the message identity).
            let related = module.related_information();
            for r in object.references.iter().filter_map(|r| r.target) {
                let Some(info) = entry.dataset.objects.get(r) else {
                    continue;
                };
                if info.kind != ObjectKind::Information {
                    continue;
                }
                if let Some((_, prefix)) = related.iter().find(|(code, _)| *code == info.code) {
                    attribute_rows(fc, &info.attributes, prefix, &mut attributes);
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

    /// Selection highlight of an overlay feature key: the objects the
    /// product names (S-124: the NavwarnPart and its NavwarnAreaAffected),
    /// else nothing extra.
    pub fn highlight_geometry(&mut self, key: i64) -> Vec<Vec<WorldPoint>> {
        self.ensure_evaluated();
        let Some((entry, feature)) = self.locate(key) else {
            return Vec::new();
        };
        let Some(index) = self.index_of(entry.product()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for id in self.products[index].highlight(&self.core, entry.id, &feature.id) {
            if let Some(object) = entry.dataset.object(&id) {
                for g in &object.geometry {
                    geometry_paths(g, &mut out);
                }
            }
        }
        out
    }

    /// A feature of dataset `id` was selected or the dataset chosen in a
    /// list (S-124: the message is marked displayed).
    pub fn mark_displayed(&mut self, id: u32) {
        self.ensure_evaluated();
        let Some(index) = self
            .core
            .dataset(id)
            .and_then(|d| self.index_of(d.product()))
        else {
            return;
        };
        self.products[index].selected(&mut self.core, id);
    }

    /// The overlay feature key of a dataset object (gml:id), if portrayed input.
    pub fn key_of(&self, id: u32, object_id: &str) -> Option<i64> {
        let entry = self.core.dataset(id)?;
        let feature = entry.input.as_ref()?.feature_index(object_id)?;
        Some(overlay_key(id, feature))
    }

    /// Geographic bounds of a part of a dataset (with its associated
    /// features, S-124 message parts) or of the whole dataset.
    pub fn focus_bounds(&mut self, id: u32, part: Option<&str>) -> Option<GeoBounds> {
        self.ensure_evaluated();
        let entry = self.core.dataset(id)?;
        let ids = match (part, self.index_of(entry.product())) {
            (Some(part), Some(index)) => self.products[index].part_objects(&self.core, id, part)?,
            _ => None,
        };
        entry.bounds(ids.as_ref()).or_else(|| entry.bounds(None))
    }

    /// The part a list selection of dataset `id` focuses first (S-124: the
    /// first NavwarnPart with geometry).
    pub fn first_part(&mut self, id: u32) -> Option<String> {
        self.ensure_evaluated();
        let index = self.index_of(self.core.dataset(id)?.product())?;
        self.products[index].first_part(&self.core, id)
    }
}

impl ProductLayer for GmlOverlays {
    fn products(&self) -> Vec<ProductId> {
        self.products.iter().map(|p| p.id()).collect()
    }
    fn is_empty(&self) -> bool {
        GmlOverlays::is_empty(self)
    }
    fn open(&mut self, input: DatasetInput<'_>) -> Result<Opened> {
        input.check_policy()?;
        let dataset = self.read_bytes(input.name, input.bytes)?;
        Ok(match self.add(dataset, input.provenance)? {
            Added::New => Opened::New(self.core.datasets.last().expect("just added").id),
            Added::AlreadyOpen => Opened::AlreadyOpen,
        })
    }
    fn remove(&mut self, id: u32) -> bool {
        GmlOverlays::remove(self, id)
    }
    fn set_visible(&mut self, id: u32, visible: bool) -> bool {
        GmlOverlays::set_visible(self, id, visible)
    }
    fn portray(&mut self, request: &PortrayalRequest<'_>) -> Result<Option<SceneContribution>> {
        self.set_viewing_time(request.viewing_time);
        Ok(self
            .instructions(request.profile)
            .map(|(revision, instructions)| SceneContribution {
                revision,
                instructions,
                extensions: Vec::new(),
            }))
    }
    fn coverage(&self) -> Option<GeoBounds> {
        self.bounds()
    }
    fn describe(&mut self, key: i64) -> Option<FeaturePick> {
        GmlOverlays::describe(self, key)
    }
    fn highlight(&mut self, key: i64) -> Vec<Vec<WorldPoint>> {
        self.highlight_geometry(key)
    }
    fn open_stage(&self) -> Option<Box<dyn OpenStage>> {
        Some(Box::new(GmlStage {
            products: crate::products::gml_products(),
            catalogues: self.has_catalogues(),
            paths: Vec::new(),
        }))
    }
    fn install(
        &mut self,
        opened: Box<dyn std::any::Any + Send>,
        _context: &InstallContext<'_>,
        report: &mut InstallReport,
    ) -> Result<()> {
        let opened = opened
            .downcast::<Vec<(PathBuf, Vec<u8>, Provenance)>>()
            .map_err(|_| anyhow::anyhow!("Not a GML open result"))?;
        for (path, bytes, provenance) in *opened {
            match ProductLayer::open(self, DatasetInput::new(&path, &bytes, provenance)) {
                Ok(Opened::New(id)) => {
                    let overlay = self.dataset(id).expect("just added");
                    report.datasets.push(overlay_summary(overlay));
                    report.issues.extend(overlay_issues(overlay));
                }
                Ok(Opened::AlreadyOpen) => {
                    report
                        .notices
                        .push(format!("Already open: {}", path.display()));
                }
                Err(error) => report.issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: path.display().to_string(),
                    check: "GML load".into(),
                    message: format!("{error:#}"),
                }),
            }
        }
        Ok(())
    }
    fn datasets(&self) -> Vec<DatasetSummary> {
        self.datasets.iter().map(overlay_summary).collect()
    }
    fn feature_types(&self, id: DatasetId) -> Option<BTreeMap<String, usize>> {
        let DatasetId::Overlay(id) = id else {
            return None;
        };
        let mut counts = BTreeMap::new();
        if let Some(overlay) = self.dataset(id) {
            for object in &overlay.dataset.objects {
                *counts.entry(object.code.clone()).or_default() += 1;
            }
        }
        Some(counts)
    }
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        self.datasets.iter().flat_map(overlay_issues).collect()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// An opened overlay dataset in the common report model.
fn overlay_summary(overlay: &OverlayDataset) -> DatasetSummary {
    DatasetSummary {
        id: DatasetId::Overlay(overlay.id),
        product: overlay.product().identifier().to_string(),
        name: overlay.name(),
        source: overlay.dataset.source.clone(),
        edition: overlay
            .dataset
            .identification
            .product_edition()
            .unwrap_or_default()
            .to_string(),
        features: overlay.dataset.objects.len(),
        provenance: overlay.provenance.label(),
    }
}

/// The validation findings kept with an opened overlay dataset.
fn overlay_issues(overlay: &OverlayDataset) -> Vec<ValidationIssue> {
    overlay
        .issues
        .iter()
        .map(|issue| ValidationIssue {
            severity: if issue.severity == Severity::Error {
                IssueSeverity::Error
            } else {
                IssueSeverity::Warning
            },
            subject: match &issue.object {
                Some(object) => format!("{} {object}", overlay.name()),
                None => overlay.name(),
            },
            check: issue.check.clone(),
            message: issue.message.clone(),
        })
        .collect()
}

/// The overlay stage of a host open job: GML datasets of the registered
/// products by name and head, read (the authenticated snapshot when
/// verified) on the worker and opened into the layer on install.
struct GmlStage {
    products: Vec<Product>,
    /// The products' catalogues are installed.
    catalogues: bool,
    paths: Vec<PathBuf>,
}

impl OpenStage for GmlStage {
    fn sniffs_head(&self, name: &Path) -> bool {
        overlay::sniffable_name(name)
    }
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<ferrite_product_api::Recognised> {
        if head.is_empty() {
            return None;
        }
        overlay::sniff(name, head, &self.products).map(|_| ferrite_product_api::Recognised::Dataset)
    }
    fn accept(&mut self, path: &Path, _: &OpenContext<'_>, _: &mut OpenNotes) -> bool {
        self.paths.push(path.to_path_buf());
        true
    }
    fn counted_inputs(&self) -> usize {
        self.paths.len()
    }
    fn after_claims(&mut self, notes: &mut OpenNotes) {
        if !self.catalogues && !self.paths.is_empty() {
            notes.notices.push(format!(
                "{} S-124/S-129 dataset(s) skipped: overlay catalogues are not installed",
                self.paths.len()
            ));
            self.paths.clear();
        }
    }
    fn load(self: Box<Self>, context: &OpenContext<'_>, notes: &mut OpenNotes) -> Result<Loaded> {
        let mut loaded = Vec::new();
        for path in self.paths {
            context.check_cancelled()?;
            match read_overlay(context, &path) {
                Ok((bytes, provenance)) => loaded.push((path, bytes, provenance)),
                Err(error) => notes.issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: path.display().to_string(),
                    check: "GML load".into(),
                    message: format!("{error:#}"),
                }),
            }
        }
        let counts = vec![("overlays", loaded.len())];
        Ok(Loaded {
            payload: Box::new(loaded),
            counts,
        })
    }
}

fn authorized(
    context: &OpenContext<'_>,
    paths: &[PathBuf],
) -> Result<ferrite_security::AuthorizedDatasets> {
    (context.authorize)(paths)?
        .downcast::<ferrite_security::AuthorizedDatasets>()
        .map(|report| *report)
        .map_err(|_| anyhow::anyhow!("Host authorization is not a signature report"))
}

/// Overlay bytes (the authenticated snapshot when verified).
fn read_overlay(context: &OpenContext<'_>, path: &Path) -> Result<(Vec<u8>, Provenance)> {
    use crate::dataset_signature_policy;
    let data = context.data;
    let limits = ferrite_s100_gml::ReadLimits::default();
    if !data.local_paths() && context.required() {
        // Bytes source: the authenticated in-memory snapshot.
        let report = authorized(context, &[path.to_path_buf()])?;
        let snapshot = dataset_signature_policy::dataset_snapshot_in(data, &report, path, true)?
            .context("Required authenticated dataset snapshot is missing")?;
        let bytes = snapshot
            .bytes()
            .context("Authenticated snapshot of a bytes source holds no bytes")?;
        anyhow::ensure!(
            bytes.len() <= limits.bytes,
            "{} exceeds the {} byte dataset limit",
            path.display(),
            limits.bytes
        );
        return Ok((bytes.to_vec(), Provenance::Authenticated));
    }
    let (bytes_path, provenance) = if data.local_paths() {
        let require = context.required();
        let report = authorized(context, &[path.to_path_buf()])?;
        let snapshot = dataset_signature_policy::dataset_snapshot(&report, path, require)?;
        let provenance = if snapshot.is_some() {
            Provenance::Authenticated
        } else {
            Provenance::Unverified
        };
        (
            snapshot
                .map(|s| s.path().to_path_buf())
                .unwrap_or_else(|| path.to_path_buf()),
            provenance,
        )
    } else {
        (path.to_path_buf(), Provenance::Unverified)
    };
    let length = data
        .len(&bytes_path)
        .with_context(|| format!("Cannot open {}", path.display()))?;
    anyhow::ensure!(
        length as usize <= limits.bytes,
        "{} exceeds the {} byte dataset limit",
        path.display(),
        limits.bytes
    );
    let bytes = data
        .read(&bytes_path)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    Ok((bytes, provenance))
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
    #[cfg(all(feature = "s124", feature = "s129"))]
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
    #[cfg(feature = "s124")]
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
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
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
