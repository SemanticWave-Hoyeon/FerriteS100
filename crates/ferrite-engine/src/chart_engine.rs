//! [`ChartEngine`]: the public facade of the S-100 chart kernel
//! (`docs/ENGINE_DESIGN.md` 3 and 4a).
//!
//! ```no_run
//! use ferrite_engine::chart_engine::{ChartEngine, DatasetSource, EngineConfig, View};
//! use ferrite_engine::platform::Platform;
//! # fn main() -> anyhow::Result<()> {
//! let mut engine = ChartEngine::new(EngineConfig::new(Platform::native(), "Catalogues"))?;
//! let job = engine.open(DatasetSource::Folder("TestData/SHOM-S101".into()));
//! let report = engine.install(job.wait()?)?;
//! println!("{} datasets", report.datasets.len());
//! let scene = engine.prepare(&View::fit(1280, 800))?;
//! let hits = engine.pick(&scene, [640., 400.], 10., None);
//! if let Some(hit) = hits.first() {
//!     println!("{:?}", engine.feature(hit.into()));
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Everything is read through the configured [`Platform`]: a native platform
//! opens folders on disk exactly as the viewer does (signature verification,
//! the S-101 update planner, decoded-chart memo, portrayal cell cache); an
//! in-memory platform opens the same products from bytes with no file
//! system. Opening runs as a background job on the platform executor; its
//! result is installed on the owner thread, like the viewer's publication.
use crate::catalogue::{
    load_feature_catalogue_from, load_portrayal_catalogue_from, validate_fc, validate_pc,
};
use crate::chart_cells::chart_data_bounds;
use crate::gml_overlay::{self, GmlOverlays, Provenance, SymbolRegistrar};
use crate::pick::{find_features_at, FeatureReport, GeometryPicker, PickCandidate, PickSurface};
use crate::platform::{parallel_map, EntryKind, Platform};
use crate::portrayal::{AreaTriangulator, PortrayalChangeRequest};
use crate::scene::{prepare_flat_coverage, PreparedScene};
use crate::state::{EngineState, ViewportPixels};
use crate::{dataset_signature_policy, s101_update_plan, CellHydrationSource, ChartLoadResult};
use anyhow::{bail, ensure, Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::SettingsState;
use ferrite_render::GeoBounds;
use ferrite_s100_core::{DatasetIdentification, S101Cell};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Whether dataset signatures must verify before data is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Open without verification (datasets are marked unverified).
    Off,
    /// Reject unsigned or invalid datasets (S-100 Part 15). Needs a source of
    /// local files until the signature backend reads bytes (stage 5).
    Required,
}

/// How an engine is set up.
pub struct EngineConfig {
    pub platform: Platform,
    /// Catalogue folder with `FC/<product>` and `PC/<product>` (S-101,
    /// S-124, S-129), read through the platform data source.
    pub catalogues: PathBuf,
    /// Installed S-101 FC/PC inventory for datasets of other editions.
    pub inventory: Option<PathBuf>,
    pub signatures: SignaturePolicy,
    /// Trust anchors (scheme administrator, PEM certificate).
    pub trust_anchors: Vec<(String, Vec<u8>)>,
    /// Initial colour profile.
    pub profile: String,
    /// Stored area triangulation (the renderer's; none by default, the
    /// renderer then triangulates when drawing).
    pub triangulate: AreaTriangulator,
    /// Overlay symbol registration with the renderer (accept by default).
    pub register_overlay_symbols: SymbolRegistrar,
}

fn no_triangulation(
    _: &ferrite_render::AreaInstruction,
    _: ferrite_render::FlatProjection,
    _: usize,
) -> Option<ferrite_render::AreaTriangulation> {
    None
}

impl EngineConfig {
    /// Defaults: no signature verification, Day palette, no inventory.
    pub fn new(platform: Platform, catalogues: impl Into<PathBuf>) -> Self {
        Self {
            platform,
            catalogues: catalogues.into(),
            inventory: None,
            signatures: SignaturePolicy::Off,
            trust_anchors: Vec::new(),
            profile: "Day".into(),
            triangulate: no_triangulation,
            register_overlay_symbols: |_, _| true,
        }
    }
}

/// What to open.
#[derive(Clone, Debug)]
pub enum DatasetSource {
    /// Every dataset under a folder (exchange sets included).
    Folder(PathBuf),
    /// Explicit dataset files.
    Files(Vec<PathBuf>),
}

/// Colour profile and portrayal settings for the next [`ChartEngine::prepare`].
#[derive(Clone, Debug)]
pub struct DisplayOptions {
    pub profile: String,
    pub settings: SettingsState,
    /// Viewing time and date-dependent display of the scene.
    pub display: ferrite_render::DisplaySettings,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self {
            profile: "Day".into(),
            settings: SettingsState::default(),
            display: ferrite_render::DisplaySettings::default(),
        }
    }
}

/// The view a scene is prepared for.
#[derive(Clone, Copy, Debug)]
pub struct View {
    /// Geographic extent to fit (None: all loaded data).
    pub fit: Option<GeoBounds>,
    pub viewport: ViewportPixels,
}

impl View {
    /// All loaded data in a `width` x `height` pixel view at density 1.
    pub fn fit(width: u32, height: u32) -> Self {
        Self {
            fit: None,
            viewport: ViewportPixels {
                width,
                height,
                scale_factor: 1.0,
            },
        }
    }
}

/// Identity of an opened dataset in an engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DatasetId {
    /// Loaded S-101 cell (index in load order).
    Cell(usize),
    /// S-124/S-129 overlay dataset.
    Overlay(u32),
}

/// One opened dataset.
#[derive(Clone, Debug)]
pub struct DatasetSummary {
    pub id: DatasetId,
    /// Product identifier ("S-101", "S-124", ...).
    pub product: String,
    pub name: String,
    pub source: Option<PathBuf>,
    /// S-101: dataset edition and update ("2.0" for edition 2 update 0);
    /// S-124/S-129: the product specification edition.
    pub edition: String,
    pub features: usize,
    /// How its authenticity was established.
    pub provenance: String,
}

/// Severity of a [`ValidationIssue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IssueSeverity {
    Warning,
    Error,
}

/// A validation finding or a rejected input.
#[derive(Clone, Debug)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    /// Dataset or input the finding is about.
    pub subject: String,
    /// Check identifier ("S-158-124/12", "S-101 load", ...).
    pub check: String,
    pub message: String,
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let severity = match self.severity {
            IssueSeverity::Warning => "warning",
            IssueSeverity::Error => "error",
        };
        write!(
            f,
            "{severity} [{}] {}: {}",
            self.check, self.subject, self.message
        )
    }
}

/// Outcome of installing an open job.
#[derive(Clone, Debug, Default)]
pub struct OpenReport {
    /// Datasets newly opened by this job.
    pub datasets: Vec<DatasetSummary>,
    pub issues: Vec<ValidationIssue>,
    /// Inputs not opened and other information.
    pub notices: Vec<String>,
}

/// Datasets loaded by an [`OpenJob`], not yet installed.
pub struct OpenedDatasets {
    cells: Vec<(ChartLoadResult, String)>,
    overlays: Vec<(PathBuf, Vec<u8>, Provenance)>,
    issues: Vec<ValidationIssue>,
    notices: Vec<String>,
    catalogues: ([u8; 32], [u8; 32]),
}

impl OpenedDatasets {
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
    pub fn overlay_count(&self) -> usize {
        self.overlays.len()
    }
}

/// State of an [`OpenJob`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Finished,
}

enum JobSlot {
    Running,
    Done(Result<OpenedDatasets>),
    Taken,
}

struct JobShared {
    slot: Mutex<JobSlot>,
    done: Condvar,
    cancel: AtomicBool,
}

/// Handle of a background open.
pub struct OpenJob {
    shared: Arc<JobShared>,
}

impl OpenJob {
    pub fn poll(&self) -> JobStatus {
        match self.shared.slot.lock().as_deref() {
            Ok(JobSlot::Running) => JobStatus::Running,
            _ => JobStatus::Finished,
        }
    }
    /// Ask the job to stop at the next dataset; it then fails with
    /// "Open cancelled".
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
    /// The result once finished (taken once).
    pub fn try_take(&self) -> Option<Result<OpenedDatasets>> {
        let mut slot = self.shared.slot.lock().ok()?;
        match std::mem::replace(&mut *slot, JobSlot::Taken) {
            JobSlot::Running => {
                *slot = JobSlot::Running;
                None
            }
            JobSlot::Done(result) => Some(result),
            JobSlot::Taken => Some(Err(anyhow::anyhow!("Open result already taken"))),
        }
    }
    /// Block until the job finished.
    pub fn wait(self) -> Result<OpenedDatasets> {
        let mut slot = self
            .shared
            .slot
            .lock()
            .map_err(|_| anyhow::anyhow!("Open job poisoned"))?;
        while matches!(*slot, JobSlot::Running) {
            slot = self
                .shared
                .done
                .wait(slot)
                .map_err(|_| anyhow::anyhow!("Open job poisoned"))?;
        }
        match std::mem::replace(&mut *slot, JobSlot::Taken) {
            JobSlot::Done(result) => result,
            _ => bail!("Open result already taken"),
        }
    }
}

/// Identity of a feature for [`ChartEngine::feature`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureId {
    /// Loaded cell of an ENC feature; None for overlay features.
    pub cell_index: Option<u32>,
    pub feature_id: i64,
}

impl From<&PickCandidate> for FeatureId {
    fn from(hit: &PickCandidate) -> Self {
        Self {
            cell_index: hit.cell_index,
            feature_id: hit.feature_id,
        }
    }
}

/// The S-100 chart engine.
pub struct ChartEngine {
    state: EngineState,
    overlays: GmlOverlays,
    signatures: SignaturePolicy,
    trust_anchors: Arc<Vec<(String, Vec<u8>)>>,
    triangulate: AreaTriangulator,
    display: DisplayOptions,
    provenance: Vec<String>,
}

impl ChartEngine {
    /// Load the S-101 catalogue pair (required) and the S-124/S-129
    /// catalogues (optional) from `config.catalogues`.
    pub fn new(config: EngineConfig) -> Result<Self> {
        let data = config.platform.data.as_ref();
        let fc_path = config.catalogues.join("FC/S-101");
        let pc_path = config.catalogues.join("PC/S-101");
        let fc = load_feature_catalogue_from(data, &fc_path)?;
        let pc = load_portrayal_catalogue_from(data, &pc_path)?;
        ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
        let fc_status = validate_fc(&fc, &fc.source_path);
        let pc_status = validate_pc(&pc, &pc.root_path);
        ensure!(
            pc.color_profiles.profiles.contains_key(&config.profile),
            "Portrayal catalogue has no colour profile {}",
            config.profile
        );
        let overlays = GmlOverlays::load_catalogues(
            &config.platform,
            &config.catalogues,
            config.register_overlay_symbols,
        );
        let state = EngineState::new(
            config.platform,
            Arc::new(fc),
            Arc::new(pc),
            fc_status,
            pc_status,
            config.profile.clone(),
            config.inventory.unwrap_or_default(),
        );
        Ok(Self {
            state,
            overlays,
            signatures: config.signatures,
            trust_anchors: Arc::new(config.trust_anchors),
            triangulate: config.triangulate,
            display: DisplayOptions {
                profile: config.profile,
                ..DisplayOptions::default()
            },
            provenance: Vec::new(),
        })
    }

    /// The chart state (cells, catalogues, coverage).
    pub fn state(&self) -> &EngineState {
        &self.state
    }
    /// The S-124/S-129 overlay registry.
    pub fn overlays(&self) -> &GmlOverlays {
        &self.overlays
    }
    pub fn platform(&self) -> &Platform {
        &self.state.platform
    }
    /// Catalogue errors of the optional overlay products.
    pub fn catalogue_errors(&self) -> &[String] {
        &self.overlays.catalogue_errors
    }

    /// Start opening `source` in the background.
    pub fn open(&mut self, source: DatasetSource) -> OpenJob {
        let shared = Arc::new(JobShared {
            slot: Mutex::new(JobSlot::Running),
            done: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let task = OpenTask {
            platform: self.state.platform.clone(),
            source,
            signatures: self.signatures,
            trust_anchors: Arc::clone(&self.trust_anchors),
            fc: Arc::clone(&self.state.fc),
            pc: Arc::clone(&self.state.pc),
            decoded: Arc::clone(&self.state.decoded_chart_cache),
            overlay_products: self.overlays.s124.is_some() || self.overlays.s129.is_some(),
        };
        let worker = Arc::clone(&shared);
        self.state.platform.executor.spawn(Box::new(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task.run(&worker.cancel)))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Open job panicked")));
            if let Ok(mut slot) = worker.slot.lock() {
                *slot = JobSlot::Done(result);
            }
            worker.done.notify_all();
        }));
        OpenJob { shared }
    }

    /// Open and install in one call.
    pub fn open_blocking(&mut self, source: DatasetSource) -> Result<OpenReport> {
        let job = self.open(source);
        self.install(job.wait()?)
    }

    /// Publish the datasets of a finished job into the engine.
    pub fn install(&mut self, opened: OpenedDatasets) -> Result<OpenReport> {
        ensure!(
            opened.catalogues
                == (
                    *self.state.fc.source_digest(),
                    *self.state.pc.source_digest()
                ),
            "Bound catalogues changed while loading; reopen the datasets"
        );
        let mut report = OpenReport {
            datasets: Vec::new(),
            issues: opened.issues,
            notices: opened.notices,
        };
        let mut added = 0usize;
        let mut loaded: HashSet<[u8; 32]> = (0..self.state.cells.len())
            .filter_map(|i| self.state.cell_identity_sha(i))
            .collect();
        let mut keys: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (index, cell) in self.state.cells.iter().enumerate() {
            if let Ok(key) = s101_update_plan::dataset_key(&cell.dsid) {
                keys.insert(key, index);
            }
        }
        for (result, provenance) in opened.cells {
            let ChartLoadResult {
                metadata,
                input_paths,
                cell,
                source_identity,
                hydration,
                ..
            } = result;
            if !loaded.insert(*source_identity.sha256()) {
                report
                    .notices
                    .push(format!("Already open: {}", cell.file_path.display()));
                continue;
            }
            let key = s101_update_plan::dataset_key(&cell.dsid)?;
            if let Some(&index) = keys.get(&key) {
                let loaded = &self.state.cells[index].dsid;
                ensure!(
                    (cell.dsid.edition_number, cell.dsid.update_number)
                        >= (loaded.edition_number, loaded.update_number),
                    "{} would downgrade a loaded edition/update",
                    cell.file_path.display()
                );
                // A newer edition/update replaces the loaded dataset.
                self.state.cells.remove(index);
                self.state.loaded_source_identities.remove(index);
                self.state.loaded_chain_paths.remove(index);
                self.provenance.remove(index);
                keys = self
                    .state
                    .cells
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| Some((s101_update_plan::dataset_key(&c.dsid).ok()?, i)))
                    .collect();
            }
            if let Some(source) = hydration {
                self.state.cell_hydration.insert(
                    *source_identity.sha256(),
                    crate::CellHydration {
                        source,
                        released: None,
                        verify: None,
                    },
                );
            }
            self.state.loaded_discovery.insert(key.clone(), metadata);
            self.state.loaded_chain_paths.push(input_paths);
            self.state.loaded_source_identities.push(source_identity);
            keys.insert(key, self.state.cells.len());
            self.state.cells.push(cell);
            self.provenance.push(provenance);
            added += 1;
        }
        self.state.prune_cell_hydration();
        if !self.state.cells.is_empty() {
            self.state.catalogue_registry =
                self.state
                    .prepare_catalogue_registry(&self.state.fc, &self.state.pc, true)?;
        }
        let first_new = self.state.cells.len() - added;
        for (index, cell) in self.state.cells.iter().enumerate().skip(first_new) {
            report.datasets.push(self.cell_summary(index, cell));
        }
        for (path, bytes, provenance) in opened.overlays {
            let added = self
                .overlays
                .read_bytes(&path, &bytes)
                .and_then(|dataset| self.overlays.add(dataset, provenance));
            match added {
                Ok(gml_overlay::Added::New) => {
                    let overlay = self.overlays.datasets.last().expect("just added");
                    report.datasets.push(overlay_summary(overlay));
                    report.issues.extend(overlay_issues(overlay));
                }
                Ok(gml_overlay::Added::AlreadyOpen) => {
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
        self.state.bounds = self.data_bounds();
        Ok(report)
    }

    fn data_bounds(&self) -> GeoBounds {
        if !self.state.cells.is_empty() {
            return chart_data_bounds(&self.state.cells, std::iter::empty());
        }
        self.overlays
            .bounds()
            .unwrap_or_else(|| GeoBounds::new(-180.0, -90.0, 180.0, 90.0))
    }

    fn cell_summary(&self, index: usize, cell: &S101Cell) -> DatasetSummary {
        DatasetSummary {
            id: DatasetId::Cell(index),
            product: "S-101".into(),
            name: cell.dsid.dataset_name.clone(),
            source: Some(cell.file_path.clone()),
            edition: format!("{}.{}", cell.dsid.edition_number, cell.dsid.update_number),
            features: self.state.cell_feature_count(index),
            provenance: self.provenance.get(index).cloned().unwrap_or_default(),
        }
    }

    /// Every opened dataset.
    pub fn datasets(&self) -> Vec<DatasetSummary> {
        let mut out: Vec<_> = self
            .state
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| self.cell_summary(index, cell))
            .collect();
        out.extend(self.overlays.datasets.iter().map(overlay_summary));
        out
    }

    /// Feature counts by feature type of a dataset.
    pub fn feature_types(&self, id: DatasetId) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        match id {
            DatasetId::Cell(index) => {
                if let Some(cell) = self.state.cells.get(index) {
                    for feature in cell.features.values() {
                        let code = feature.feature_code.clone().unwrap_or_else(|| "?".into());
                        *counts.entry(code).or_default() += 1;
                    }
                }
            }
            DatasetId::Overlay(id) => {
                if let Some(overlay) = self.overlays.dataset(id) {
                    for object in &overlay.dataset.objects {
                        *counts.entry(object.code.clone()).or_default() += 1;
                    }
                }
            }
        }
        counts
    }

    /// Validation findings of the opened overlay datasets (S-101 cells are
    /// validated when opened; failures are reported by [`ChartEngine::install`]).
    pub fn validation_issues(&self) -> Vec<ValidationIssue> {
        self.overlays
            .datasets
            .iter()
            .flat_map(overlay_issues)
            .collect()
    }

    pub fn display(&self) -> &DisplayOptions {
        &self.display
    }

    /// Colour profile and portrayal settings of the next scene.
    pub fn set_display(&mut self, display: DisplayOptions) -> Result<()> {
        ensure!(
            self.state
                .pc
                .color_profiles
                .profiles
                .contains_key(&display.profile),
            "Portrayal catalogue has no colour profile {}",
            display.profile
        );
        crate::portrayal::validated_lua_context(&self.state.pc, Some(&display.settings))?;
        self.display = display;
        Ok(())
    }

    /// Portray the opened datasets for `view`.
    pub fn prepare(&mut self, view: &View) -> Result<Arc<PreparedScene>> {
        let ViewportPixels {
            width,
            height,
            scale_factor,
        } = view.viewport;
        ensure!(
            width > 0 && height > 0 && scale_factor.is_finite() && scale_factor > 0.,
            "Empty view"
        );
        let settings = self.display.settings.clone();
        self.state.current_profile_name = self.display.profile.clone();
        self.state.render_context.settings = self.display.display.clone();
        self.state.render_context.settings.color_profile = self.display.profile.clone();
        // The camera of this view: residency selects cells near it.
        let fit = view.fit.unwrap_or(self.state.bounds);
        self.state.render_context.set_bounds(self.state.bounds);
        self.state
            .render_context
            .set_viewport(width as f32, height as f32);
        self.state.render_context.zoom_to_fit(fit);
        self.state
            .render_context
            .scaler
            .set_pixel_ratio(scale_factor);

        let (fc, pc) = (Arc::clone(&self.state.fc), Arc::clone(&self.state.pc));
        let (mut context, coverage, registry, ic_changed) = if self.state.cells.is_empty() {
            let mut context = self.state.render_context.empty_for_rebuild();
            context.set_bounds(self.state.bounds);
            (
                context,
                Arc::new(
                    ferrite_s101::coverage_projection::GeographicCoverageInventory::from_cells(&[])?,
                ),
                Default::default(),
                0,
            )
        } else {
            let registry = self.state.prepare_catalogue_registry(&fc, &pc, true)?;
            if self.state.requires_cell_resources(&registry, &fc, &pc)? {
                // Cells owned by other installed catalogues: per-owner portrayal.
                let coverage = self
                    .state
                    .prepare_view_residency(
                        true,
                        Some(view.viewport),
                        settings.interoperability_enabled,
                    )?
                    .context("Coverage of loaded cells")?;
                let request = PortrayalChangeRequest {
                    profile: self.display.profile.clone(),
                    settings: settings.clone(),
                };
                let change = self.state.portray_change_cells(
                    &request,
                    &fc,
                    &pc,
                    &registry,
                    true,
                    true,
                    self.triangulate,
                )?;
                let mut context = change.context;
                context.set_bounds(self.state.bounds);
                (context, coverage, registry, change.ic_changed)
            } else {
                let active_ic = settings
                    .interoperability_enabled
                    .then(|| self.state.ic.clone())
                    .flatten();
                let scene = self.state.portray_loaded_cells(
                    true,
                    Some(&settings),
                    Some(view.viewport),
                    active_ic,
                    self.triangulate,
                )?;
                (scene.context, scene.coverage, registry, scene.ic_changed)
            }
        };
        // S-124/S-129 overlays: cell-less, coverage-exempt instructions.
        self.overlays.set_viewing_time(gml_overlay::viewing_time(
            &self.state.render_context.settings,
        ));
        if let Some((_, list)) = self.overlays.instructions(&self.display.profile) {
            context.append_converted(list.to_vec());
        }
        context.set_viewport(width as f32, height as f32);
        context.scaler = self.state.render_context.scaler.clone();
        prepare_flat_coverage(
            Some(coverage.as_ref()),
            &mut context,
            [width, height],
            scale_factor,
        )?;
        self.state.catalogue_registry = registry.clone();
        self.state.coverage_inventory = Some(Arc::clone(&coverage));
        self.state.ic_assigned_vectors = ic_changed;
        Ok(Arc::new(PreparedScene {
            context,
            coverage,
            catalogue_registry: registry,
            ic_changed,
        }))
    }

    /// Features under screen point `point` of `scene` within `radius`
    /// pixels, best first. `surface` is the renderer that drew the scene;
    /// without one every instruction counts as drawn ([`GeometryPicker`]).
    pub fn pick(
        &self,
        scene: &PreparedScene,
        point: [f64; 2],
        radius: f32,
        surface: Option<&dyn PickSurface>,
    ) -> Vec<PickCandidate> {
        match surface {
            Some(surface) => find_features_at(
                &scene.context,
                surface.point_symbols(),
                Some(surface),
                point[0],
                point[1],
                radius,
                true,
            ),
            None => {
                let picker = GeometryPicker::new(&scene.context);
                find_features_at(
                    &scene.context,
                    picker.point_symbols(),
                    Some(&picker),
                    point[0],
                    point[1],
                    radius,
                    true,
                )
            }
        }
    }

    /// Plain-data report of a feature (ENC feature or overlay object).
    pub fn feature(&mut self, id: FeatureId) -> Option<FeatureReport> {
        if id.cell_index.is_none() {
            let pick = self.overlays.describe(id.feature_id)?;
            return Some(FeatureReport {
                feature_type: pick.feature_type,
                feature_id: id.feature_id,
                foid: pick.foid,
                cell_index: None,
                primitive_type: pick.primitive,
                source: Some(pick.source),
                attributes: pick.attributes,
                definition: pick.definition,
            });
        }
        let index = id.cell_index? as usize;
        self.state.cells.get(index)?.features.get(&id.feature_id)?;
        Some(
            self.state
                .describe_feature(id.cell_index, id.feature_id, ""),
        )
    }
}

fn overlay_summary(overlay: &gml_overlay::OverlayDataset) -> DatasetSummary {
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

fn overlay_issues(overlay: &gml_overlay::OverlayDataset) -> Vec<ValidationIssue> {
    overlay
        .issues
        .iter()
        .map(|issue| ValidationIssue {
            severity: if issue.severity == ferrite_s100_gml::Severity::Error {
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

/// Everything an open job needs, moved to the worker.
struct OpenTask {
    platform: Platform,
    source: DatasetSource,
    signatures: SignaturePolicy,
    trust_anchors: Arc<Vec<(String, Vec<u8>)>>,
    fc: Arc<BoundFeatureCatalogue>,
    pc: Arc<BoundPortrayalCatalogue>,
    decoded: crate::decoded_chart_cache::SharedDecodedChartCache,
    overlay_products: bool,
}

/// One S-101 chain read into memory.
struct ByteChain {
    base: (PathBuf, Vec<u8>),
    updates: Vec<Vec<u8>>,
}

impl OpenTask {
    fn run(self, cancel: &AtomicBool) -> Result<OpenedDatasets> {
        let data = self.platform.data.as_ref();
        let mut notices = Vec::new();
        let mut issues = Vec::new();
        let inputs = match &self.source {
            DatasetSource::Folder(root) => {
                ensure!(
                    data.kind(root)? == EntryKind::Dir,
                    "Dataset folder must be a real directory: {}",
                    root.display()
                );
                let mut files = Vec::new();
                for (count, entry) in data.walk(root).enumerate() {
                    ensure!(count < 100_000, "Dataset discovery exceeds entry limit");
                    let entry = entry?;
                    if entry.kind == EntryKind::File {
                        files.push(entry.path);
                    }
                }
                files.sort();
                files
            }
            DatasetSource::Files(files) => {
                let mut files = files.clone();
                files.sort();
                files.dedup();
                files
            }
        };
        let explicit = matches!(self.source, DatasetSource::Files(_));
        let mut charts = Vec::new();
        let mut overlays = Vec::new();
        for path in inputs {
            if s101_update_plan::is_chart_file(&path) {
                charts.push(path);
            } else if gml_overlay::sniff_product_in(data, &path).is_some() {
                overlays.push(path);
            } else if crate::dataset_discovery::is_dataset_file(&path, "h5")
                || crate::dataset_discovery::is_dataset_file(&path, "hdf5")
            {
                notices.push(format!(
                    "Not opened (S-102 is not yet served by ChartEngine): {}",
                    path.display()
                ));
            } else if explicit {
                notices.push(format!("Not a supported dataset: {}", path.display()));
            }
        }
        ensure!(
            charts.len() + overlays.len() <= 4096,
            "Dataset inputs exceed 4096"
        );
        if !self.overlay_products && !overlays.is_empty() {
            notices.push(format!(
                "{} S-124/S-129 dataset(s) skipped: overlay catalogues are not installed",
                overlays.len()
            ));
            overlays.clear();
        }
        let require = self.signatures == SignaturePolicy::Required;
        ensure!(
            !require || data.local_paths(),
            "Signature verification needs a source of local files until the byte-based signature backend (stage 5)"
        );
        let cells = if charts.is_empty() {
            Vec::new()
        } else if data.local_paths() {
            self.load_local_charts(charts, cancel, &mut notices, &mut issues)?
        } else {
            self.load_byte_charts(charts, cancel, &mut notices, &mut issues)?
        };
        let mut loaded_overlays = Vec::new();
        for path in overlays {
            ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
            match self.read_overlay(&path) {
                Ok((bytes, provenance)) => loaded_overlays.push((path, bytes, provenance)),
                Err(error) => issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: path.display().to_string(),
                    check: "GML load".into(),
                    message: format!("{error:#}"),
                }),
            }
        }
        Ok(OpenedDatasets {
            cells,
            overlays: loaded_overlays,
            issues,
            notices,
            catalogues: (*self.fc.source_digest(), *self.pc.source_digest()),
        })
    }

    fn trust_anchors(&self) -> Result<ferrite_security::TrustAnchors> {
        let mut anchors = ferrite_security::TrustAnchors::default();
        for (administrator, pem) in self.trust_anchors.iter() {
            anchors.install_pem(administrator, pem)?;
        }
        Ok(anchors)
    }

    fn authorize(&self, paths: &[PathBuf]) -> Result<ferrite_security::AuthorizedDatasets> {
        match self.signatures {
            SignaturePolicy::Off => dataset_signature_policy::unchecked_datasets(paths),
            SignaturePolicy::Required => ferrite_security::authorize_datasets(
                paths,
                &self.trust_anchors()?,
                self.platform.clock.unix_seconds(),
                ferrite_security::UnsignedPolicy::Reject,
            ),
        }
    }

    /// Overlay bytes (the authenticated snapshot when verified).
    fn read_overlay(&self, path: &Path) -> Result<(Vec<u8>, Provenance)> {
        let data = self.platform.data.as_ref();
        let limits = ferrite_s100_gml::ReadLimits::default();
        let (bytes_path, provenance) = if data.local_paths() {
            let require = self.signatures == SignaturePolicy::Required;
            let report = self.authorize(&[path.to_path_buf()])?;
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
        ensure!(
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

    /// Validate and normalize a materialized cell as the viewer does.
    fn admit(&self, cell: &mut S101Cell) -> Result<()> {
        ferrite_s101::validate_dataset_catalogues(
            &cell.dsid,
            &self.fc,
            &self.pc.product_id,
            &self.pc.version,
        )
        .with_context(|| format!("Incompatible S-101 dataset {}", cell.file_path.display()))?;
        ferrite_s101::validate_spatial_scale_properties(&cell.dsid, cell.features.values())
            .with_context(|| {
                format!(
                    "Nonconformant S-101 spatial scales {}",
                    cell.file_path.display()
                )
            })?;
        Ok(())
    }

    /// Local files: the viewer's authorized planner and loader.
    fn load_local_charts(
        &self,
        charts: Vec<PathBuf>,
        cancel: &AtomicBool,
        notices: &mut Vec<String>,
        issues: &mut Vec<ValidationIssue>,
    ) -> Result<Vec<(ChartLoadResult, String)>> {
        let require = self.signatures == SignaturePolicy::Required;
        let authorization = self.authorize(&charts)?;
        let provenance = if require {
            "signature verified"
        } else {
            "signature not verified"
        };
        let (plans, plan_notices) =
            s101_update_plan::authorized_folder_plans(charts, &authorization, require)?;
        notices.extend(plan_notices);
        let codes = Arc::new(self.fc.feature_type_codes());
        let plans: Vec<_> = plans.into_iter().map(Arc::new).collect();
        let results = parallel_map(self.platform.executor.as_ref(), &plans, |plan| {
            if cancel.load(Ordering::Relaxed) {
                bail!("Open cancelled");
            }
            let (mut cell, source_identity) = plan.load_with_decoded_cache(&self.decoded)?;
            self.admit(&mut cell)?;
            cell.normalize_feature_codes(&codes);
            Ok(ChartLoadResult {
                base_metadata: plan.base.metadata.clone(),
                metadata: plan.ending_metadata().cloned(),
                input_paths: plan.input_paths(),
                cell,
                source_identity,
                hydration: Some(CellHydrationSource {
                    plan: Arc::clone(plan),
                    codes: vec![Arc::clone(&codes)],
                }),
            })
        });
        // Retain authenticated snapshots until every plan was parsed.
        drop(authorization);
        let mut cells = Vec::new();
        for (plan, result) in plans.iter().zip(results) {
            match result {
                Ok(result) => cells.push((result, provenance.to_string())),
                Err(error) => {
                    ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
                    issues.push(load_issue(&plan.base.original, error));
                }
            }
        }
        Ok(cells)
    }

    /// Any source: chains planned from the dataset identifications and
    /// materialized from bytes (no signature verification).
    fn load_byte_charts(
        &self,
        charts: Vec<PathBuf>,
        cancel: &AtomicBool,
        notices: &mut Vec<String>,
        issues: &mut Vec<ValidationIssue>,
    ) -> Result<Vec<(ChartLoadResult, String)>> {
        let data = self.platform.data.as_ref();
        let max = ferrite_s100_core::updates::UpdateLimits::default().max_dataset_bytes as u64;
        let mut inputs = Vec::new();
        let mut total = 0u64;
        for path in charts {
            ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
            let read = (|| -> Result<(DatasetIdentification, Vec<u8>)> {
                let bytes = data.read_limited(&path, max)?;
                ensure!(
                    bytes.len() as u64 <= max,
                    "S-101 input byte budget exceeded"
                );
                let id = ferrite_s100_core::inspect_dataset_identification_bytes(&bytes)?;
                Ok((id, bytes))
            })();
            match read {
                Ok((id, bytes)) => {
                    total += bytes.len() as u64;
                    ensure!(total <= max, "S-101 aggregate input budget exceeded");
                    inputs.push((path, id, bytes));
                }
                Err(error) => {
                    notices.push(format!("Skipped S-101 input {}: {error:#}", path.display()))
                }
            }
        }
        let chains = plan_byte_chains(inputs, notices);
        let codes = Arc::new(self.fc.feature_type_codes());
        let chains: Vec<Mutex<Option<ByteChain>>> =
            chains.into_iter().map(|c| Mutex::new(Some(c))).collect();
        let results = parallel_map(self.platform.executor.as_ref(), &chains, |chain| {
            let chain = chain
                .lock()
                .ok()
                .and_then(|mut c| c.take())
                .context("Chain taken")?;
            let path = chain.base.0.clone();
            let run = || -> Result<ChartLoadResult> {
                if cancel.load(Ordering::Relaxed) {
                    bail!("Open cancelled");
                }
                let (mut cell, source_identity) = S101Cell::load_update_chain_bytes_with_identity(
                    &chain.base.0,
                    chain.base.1,
                    chain.updates,
                )?;
                self.admit(&mut cell)?;
                cell.normalize_feature_codes(&codes);
                Ok(ChartLoadResult {
                    base_metadata: None,
                    metadata: None,
                    input_paths: vec![path.clone()],
                    cell,
                    source_identity,
                    hydration: None,
                })
            };
            Ok::<_, anyhow::Error>((path.clone(), run()))
        });
        let mut cells = Vec::new();
        for result in results {
            let (path, result) = result?;
            match result {
                Ok(result) => cells.push((result, "signature not verified".to_string())),
                Err(error) => {
                    ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
                    issues.push(load_issue(&path, error));
                }
            }
        }
        Ok(cells)
    }
}

fn load_issue(path: &Path, error: anyhow::Error) -> ValidationIssue {
    ValidationIssue {
        severity: IssueSeverity::Error,
        subject: path.display().to_string(),
        check: "S-101 load".into(),
        message: format!("{error:#}"),
    }
}

/// Group inputs by logical dataset and select the latest edition's base
/// (or reissue) and its consecutive updates, as the S-101 update planner
/// does for authorized inputs. Rejected groups become notices.
/// An S-101 input read into memory with its identification.
type ByteInput = (PathBuf, DatasetIdentification, Vec<u8>);

fn plan_byte_chains(inputs: Vec<ByteInput>, notices: &mut Vec<String>) -> Vec<ByteChain> {
    let mut groups: BTreeMap<(String, String), Vec<ByteInput>> = BTreeMap::new();
    for input in inputs {
        match s101_update_plan::dataset_key(&input.1) {
            Ok(key) => groups.entry(key).or_default().push(input),
            Err(error) => notices.push(format!(
                "Skipped invalid S-101 dataset identity {}: {error:#}",
                input.0.display()
            )),
        }
    }
    let counter = |id: &DatasetIdentification| -> Option<u16> {
        Path::new(&id.dataset_name)
            .extension()?
            .to_str()?
            .parse()
            .ok()
    };
    let mut chains = Vec::new();
    for ((product, name), mut group) in groups {
        let planned = (|| -> Result<ByteChain> {
            ensure!(
                group.iter().all(|(_, id, _)| id.edition_number > 0),
                "Cancellation datasets need an explicit removal operation"
            );
            let edition = group
                .iter()
                .map(|(_, id, _)| id.edition_number)
                .max()
                .context("Empty group")?;
            group.retain(|(_, id, _)| id.edition_number == edition);
            let base_counter = group
                .iter()
                .filter(|(_, id, _)| id.application_profile == "1")
                .filter_map(|(_, id, _)| counter(id))
                .max()
                .context("Update chain has no profile-1 base in its latest edition")?;
            let mut bases = Vec::new();
            let mut updates = BTreeMap::new();
            for (path, id, bytes) in group {
                let Some(n) = counter(&id) else {
                    bail!("Missing dataset counter: {}", path.display());
                };
                if id.application_profile == "1" && n == base_counter {
                    bases.push((path, bytes));
                } else if id.application_profile == "2" && n > base_counter {
                    ensure!(updates.insert(n, bytes).is_none(), "Duplicate update {n}");
                }
            }
            ensure!(bases.len() == 1, "Ambiguous duplicate base/reissue");
            let mut expected = base_counter;
            for n in updates.keys() {
                expected = expected.checked_add(1).context("Update counter overflow")?;
                ensure!(*n == expected, "Missing update {expected} before {n}");
            }
            Ok(ByteChain {
                base: bases.pop().expect("one base"),
                updates: updates.into_values().collect(),
            })
        })();
        match planned {
            Ok(chain) => chains.push(chain),
            Err(error) => {
                notices.push(format!("Skipped S-101 dataset {product}/{name}: {error:#}"))
            }
        }
    }
    chains
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_handles_report_completion_and_results_once() {
        let shared = Arc::new(JobShared {
            slot: Mutex::new(JobSlot::Running),
            done: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let job = OpenJob {
            shared: Arc::clone(&shared),
        };
        assert_eq!(job.poll(), JobStatus::Running);
        assert!(job.try_take().is_none());
        job.cancel();
        assert!(shared.cancel.load(Ordering::Relaxed));
        *shared.slot.lock().unwrap() = JobSlot::Done(Err(anyhow::anyhow!("x")));
        assert_eq!(job.poll(), JobStatus::Finished);
        assert!(job.try_take().unwrap().is_err());
        assert!(job.try_take().unwrap().is_err());
    }
}
