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
//! system, including signature verification and S-101 update planning of
//! exchange sets. Explicitly selected S-101 cancellations remove their
//! loaded target (or are recorded as announcements) and persist in the
//! platform user store's cancellation history. Opening runs as a background
//! job on the platform executor; its result is installed on the owner
//! thread, like the viewer's publication.
use crate::catalogue::{
    load_feature_catalogue_from, load_portrayal_catalogue_from, validate_fc, validate_pc,
};
use crate::chart_cells::chart_data_bounds;
use crate::chart_publication::{
    CancellationHistory, CancellationRecord, HistoryStore, ValidatedRemoval,
};
use crate::gml_overlay::{self, GmlOverlays, Provenance, SymbolRegistrar};
use crate::pick::{find_features_at, FeatureReport, GeometryPicker, PickCandidate, PickSurface};
use crate::platform::{parallel_map, EntryKind, Platform};
use crate::portrayal::{AreaTriangulator, PortrayalChangeRequest, PortrayalSource};
#[cfg(feature = "s421")]
use crate::products::s421::{CapturedRouteInput, S421Layer, S421};
/// Without the S-421 product no route is captured.
#[cfg(not(feature = "s421"))]
type CapturedRouteInput = std::convert::Infallible;
use crate::scene::{prepare_flat_coverage, PreparedScene};
use crate::state::{EngineState, ViewportPixels};
use crate::{dataset_signature_policy, s101_update_plan, CellHydrationSource, ChartLoadResult};
use anyhow::{bail, ensure, Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_product_api::{DatasetInput, Opened, PortrayalRequest, ProductLayer};
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
    /// Reject unsigned or invalid datasets (S-100 Part 15). Exchange sets are
    /// verified through the platform data source (files or bytes) and S-101
    /// chains are planned from the authenticated snapshots, as the viewer
    /// does for local folders.
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
    /// Datasets held by the host, named by paths that are only their
    /// identity (exchange-set file names, update chains and product sniffing
    /// use them). Under [`SignaturePolicy::Off`] they are opened unverified
    /// (S-101 chains are planned from the dataset identifications). Under
    /// [`SignaturePolicy::Required`] they are verified from the bytes and
    /// S-101 chains planned by the S-101 update planner (include the
    /// signature files and exchange catalogue among the inputs; paths must
    /// be absolute, they are the authorization keys: on Windows a root-relative
    /// `/x` is not, see [`crate::platform::memory_path`]).
    Bytes(Vec<(PathBuf, Arc<[u8]>)>),
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
    /// A camera on the loaded data; overrides `fit`.
    pub camera: Option<Camera>,
}

/// A camera relative to the loaded data, as the viewer's `--center` and
/// `--zoom`: zoom 1 shows all data, the centre pans the view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// Centre (latitude, longitude) in degrees; None: centre of the data.
    pub center: Option<[f64; 2]>,
    pub zoom: Zoom,
}

/// Magnification of a [`Camera`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zoom {
    /// Factor over the all-data view (the viewer's zoom level).
    Factor(f64),
    /// Display scale denominator (1:N) at the view's pixel density.
    Scale(f64),
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
            camera: None,
        }
    }
    /// A camera in a `width` x `height` pixel view at density 1.
    pub fn camera(width: u32, height: u32, camera: Camera) -> Self {
        Self {
            camera: Some(camera),
            ..Self::fit(width, height)
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
    /// S-101 cancellations applied and recorded in the cancellation history
    /// by this job (removals of loaded datasets and announcements).
    pub cancellations: Vec<CancellationRecord>,
    /// S-102 files newly opened by this job.
    pub bathymetry: Vec<crate::s102_host::BathymetrySummary>,
}

/// Datasets loaded by an [`OpenJob`], not yet installed.
pub struct OpenedDatasets {
    cells: Vec<(ChartLoadResult, String)>,
    overlays: Vec<(PathBuf, Vec<u8>, Provenance)>,
    removals: Vec<(PathBuf, ValidatedRemoval)>,
    routes: Vec<CapturedRouteInput>,
    issues: Vec<ValidationIssue>,
    notices: Vec<String>,
    catalogues: ([u8; 32], [u8; 32]),
    /// Decoded S-102 inputs.
    s102: crate::s102_host::OpenedS102,
}

impl OpenedDatasets {
    /// Decoded S-102 files (coverage and trial inspections).
    pub fn bathymetry_count(&self) -> usize {
        self.s102.len()
    }
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
    pub fn overlay_count(&self) -> usize {
        self.overlays.len()
    }
    /// Validated S-101 cancellations (removals and announcements).
    pub fn cancellation_count(&self) -> usize {
        self.removals.len()
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
    Done(Box<Result<OpenedDatasets>>),
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
            JobSlot::Done(result) => Some(*result),
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
            JobSlot::Done(result) => *result,
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
    /// S-421 routes (product layer; captured, not portrayed by the engine).
    #[cfg(feature = "s421")]
    routes: S421Layer,
    /// S-102 host options (depth adjustments, raster tile edge).
    s102: crate::s102_host::S102Options,
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
        let mut state = EngineState::new(
            config.platform,
            Arc::new(fc),
            Arc::new(pc),
            fc_status,
            pc_status,
            config.profile.clone(),
            config.inventory.unwrap_or_default(),
        );
        // S-102 catalogues are optional, like the overlay catalogues.
        let data = Arc::clone(&state.platform.data);
        crate::s102_host::load_catalogues(&mut state, data.as_ref(), &config.catalogues);
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
            #[cfg(feature = "s421")]
            routes: S421Layer::new(),
            s102: Default::default(),
        })
    }

    /// Opened S-102 files ([`crate::s102_host::SUPPORT`] says what is served).
    pub fn bathymetry(&self) -> Vec<crate::s102_host::BathymetrySummary> {
        crate::s102_host::summaries(&self.state)
    }
    pub fn s102_options(&self) -> &crate::s102_host::S102Options {
        &self.s102
    }
    /// Depth adjustments and raster tile edge for S-102 opened and prepared
    /// from now on.
    pub fn set_s102_options(&mut self, options: crate::s102_host::S102Options) {
        #[cfg(feature = "s102-pure")]
        {
            self.state.s102.options = options.clone();
        }
        self.s102 = options;
    }

    /// Estimated instruction count from which preparations portray only the
    /// cells near the view (residency); None restores the default
    /// (`FERRITE_RESIDENCY_THRESHOLD` or 1,500,000; 0 always activates it).
    /// Applies from the next [`Self::prepare`].
    pub fn set_residency_threshold(&mut self, threshold: Option<usize>) {
        self.state.residency_threshold_override = threshold;
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
    /// S-421 routes captured by the opened inputs, in opening order (not
    /// portrayed by the engine; under [`SignaturePolicy::Required`] none are
    /// admitted).
    #[cfg(feature = "s421")]
    pub fn routes(&self) -> Vec<&CapturedRouteInput> {
        self.routes.inputs().collect()
    }
    /// The S-421 product layer holding the routes (coverage, feature
    /// reports, the route overlay renderer extension).
    #[cfg(feature = "s421")]
    pub fn route_layer(&mut self) -> &mut S421Layer {
        &mut self.routes
    }
    /// The S-101 cancellation history of the platform user store (empty when
    /// the host keeps none).
    pub fn cancellation_history(&self) -> Result<CancellationHistory> {
        match self.state.platform.user.cancellation_history() {
            Some(record) => CancellationHistory::read_record(record.as_ref()),
            None => Ok(CancellationHistory::default()),
        }
    }

    /// Where S-101 cell instructions come from (default: Lua when the
    /// `lua-native` feature is built, otherwise precomputed payloads).
    pub fn portrayal_source(&self) -> PortrayalSource {
        self.state.portrayal_source
    }
    pub fn set_portrayal_source(&mut self, source: PortrayalSource) {
        self.state.portrayal_source = source;
    }
    /// Export: also write a precomputed payload of every cell the following
    /// preparations portray with Lua to the platform cache store, for hosts
    /// without Lua ([`PortrayalSource::Precomputed`]).
    pub fn set_precomputed_export(&mut self, export: bool) {
        self.state.precomputed_export = export;
    }

    /// Geographic extent of `camera` in `viewport` over the loaded data, and
    /// its zoom factor over the all-data view.
    pub fn camera_view(
        &self,
        camera: Camera,
        viewport: ViewportPixels,
    ) -> Result<(GeoBounds, f64)> {
        let projection = self.state.render_context.scaler.projection();
        let base = self.state.bounds;
        let pan = match camera.center {
            Some([lat, lon]) => projection
                .pan_to(base, ferrite_render::WorldPoint::new(lon, lat))
                .context("Invalid camera centre")?,
            None => [0., 0.],
        };
        // The viewer keeps the longitude pan within one turn.
        let pan = [pan[0] - (pan[0] / 360.).round() * 360., pan[1]];
        let bounds = |zoom: f64| {
            projection
                .view_bounds(base, zoom, pan)
                .context("Invalid camera zoom")
        };
        let zoom = match camera.zoom {
            Zoom::Factor(zoom) => zoom,
            Zoom::Scale(denominator) => {
                ensure!(
                    denominator.is_finite() && denominator > 0.,
                    "Invalid scale denominator"
                );
                // The display scale is inversely proportional to the zoom
                // at a fixed centre; two corrections absorb the latitude of
                // the actual view centre.
                let display_scale = |zoom: f64| -> Result<f64> {
                    let mut scaler = self.state.render_context.scaler.clone();
                    scaler.set_viewport(ferrite_render::Viewport::new(
                        viewport.width as f32,
                        viewport.height as f32,
                    ));
                    scaler.set_pixel_ratio(viewport.scale_factor);
                    scaler.zoom_to_fit(bounds(zoom)?);
                    Ok(scaler.display_scale)
                };
                let mut zoom = 1.0;
                for _ in 0..3 {
                    zoom *= display_scale(zoom)? / denominator;
                }
                zoom
            }
        };
        ensure!(zoom.is_finite() && zoom > 0., "Invalid camera zoom");
        Ok((bounds(zoom)?, zoom))
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
            overlay_products: self.overlays.has_catalogues(),
            inventory: self.state.catalogue_inventory.clone(),
            loaded: self.loaded_datasets(),
            loaded_inputs: self
                .state
                .loaded_chain_paths
                .iter()
                .flatten()
                .cloned()
                .collect(),
            s102_options: self.s102.clone(),
        };
        let worker = Arc::clone(&shared);
        self.state.platform.executor.spawn(Box::new(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task.run(&worker.cancel)))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Open job panicked")));
            if let Ok(mut slot) = worker.slot.lock() {
                *slot = JobSlot::Done(Box::new(result));
            }
            worker.done.notify_all();
        }));
        OpenJob { shared }
    }

    /// The loaded logical datasets explicit cancellations are planned against.
    fn loaded_datasets(&self) -> s101_update_plan::LoadedDatasets {
        let mut loaded = BTreeMap::new();
        for (index, cell) in self.state.cells.iter().enumerate() {
            let (Ok(key), Some(identity)) = (
                s101_update_plan::dataset_key(&cell.dsid),
                self.state.loaded_source_identities.get(index),
            ) else {
                continue;
            };
            let metadata = self.state.loaded_discovery.get(&key).cloned().flatten();
            loaded.insert(key, (cell.dsid.clone(), identity, metadata));
        }
        loaded
    }

    /// Validate the job's cancellations against the current cells and a
    /// fresh, locked cancellation history, persist them, then remove the
    /// cancelled cells. Nothing changes when any check or the write fails.
    fn apply_cancellations(
        &mut self,
        removals: Vec<(PathBuf, ValidatedRemoval)>,
        incoming: &[(ChartLoadResult, String)],
        report: &mut OpenReport,
    ) -> Result<()> {
        let record = self.state.platform.user.cancellation_history();
        if removals.is_empty() {
            // Reused cancelled names need a newer issueDate (fresh history).
            if !incoming.is_empty() {
                if let Some(record) = record {
                    let history = CancellationHistory::read_record(record.as_ref())?;
                    for (result, _) in incoming {
                        history.validate_reuse(
                            &result.cell.dsid,
                            result.base_metadata.as_ref().map(|m| m.issue_date),
                        )?;
                    }
                }
            }
            return Ok(());
        }
        let record = record.context("This host keeps no S-101 cancellation history")?;
        let mut store = HistoryStore::begin_record(record)?;
        let incoming_keys = incoming
            .iter()
            .map(|(result, _)| s101_update_plan::dataset_key(&result.cell.dsid))
            .collect::<Result<HashSet<_>>>()?;
        let mut targets = Vec::new();
        for (path, removal) in &removals {
            store.history.validate_cancellation(removal.record())?;
            ensure!(
                !incoming_keys.contains(removal.key()),
                "Same dataset cannot be loaded and cancelled in one batch: {}",
                path.display()
            );
            let index = self.state.cells.iter().position(|cell| {
                s101_update_plan::dataset_key(&cell.dsid).is_ok_and(|key| &key == removal.key())
            });
            if removal.removes_content() {
                let index = index.with_context(|| {
                    format!("Cancellation target is not loaded: {}", path.display())
                })?;
                ensure!(
                    removal.targets(
                        &self.state.cells[index].dsid,
                        self.state.loaded_source_identities.get(index)
                    ),
                    "Cancellation target changed after validation: {}",
                    path.display()
                );
                ensure!(!targets.contains(&index), "Duplicate cancellation target");
                targets.push(index);
            } else {
                ensure!(
                    index.is_none(),
                    "Cancellation absence changed after announcement validation: {}",
                    path.display()
                );
            }
        }
        for (result, _) in incoming {
            store.history.validate_reuse(
                &result.cell.dsid,
                result.base_metadata.as_ref().map(|m| m.issue_date),
            )?;
        }
        let records: Vec<CancellationRecord> =
            removals.iter().map(|(_, r)| r.record().clone()).collect();
        store.persist(records.clone())?;
        targets.sort_unstable();
        for index in targets.into_iter().rev() {
            let cell = self.state.cells.remove(index);
            self.state.loaded_source_identities.remove(index);
            self.state.loaded_chain_paths.remove(index);
            self.provenance.remove(index);
            if let Ok(key) = s101_update_plan::dataset_key(&cell.dsid) {
                self.state.loaded_discovery.remove(&key);
            }
            report.notices.push(format!(
                "Cancelled S-101 dataset {} (edition {}, update {})",
                cell.dsid.dataset_name, cell.dsid.edition_number, cell.dsid.update_number
            ));
        }
        for (path, removal) in &removals {
            if !removal.removes_content() {
                report.notices.push(format!(
                    "Recorded S-101 cancellation announcement {} (no loaded content)",
                    path.display()
                ));
            }
        }
        report.cancellations = records;
        Ok(())
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
            cancellations: Vec::new(),
            bathymetry: Vec::new(),
        };
        self.apply_cancellations(opened.removals, &opened.cells, &mut report)?;
        #[cfg(feature = "s421")]
        for route in opened.routes {
            let path = route.path().to_path_buf();
            match self.routes.insert(route) {
                ferrite_product_api::Opened::AlreadyOpen => report
                    .notices
                    .push(format!("Already open: {}", path.display())),
                ferrite_product_api::Opened::New(_) => report.notices.push(format!(
                    "S-421 route captured (not portrayed by ChartEngine): {}",
                    path.display()
                )),
            }
        }
        #[cfg(not(feature = "s421"))]
        for route in opened.routes {
            match route {}
        }
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
        } else {
            self.state.catalogue_registry = Default::default();
        }
        let first_new = self.state.cells.len() - added;
        for (index, cell) in self.state.cells.iter().enumerate().skip(first_new) {
            report.datasets.push(self.cell_summary(index, cell));
        }
        for (path, bytes, provenance) in opened.overlays {
            let added = self.overlays.open(DatasetInput {
                name: &path,
                bytes: &bytes,
                provenance,
            });
            match added {
                Ok(Opened::New(id)) => {
                    let overlay = self.overlays.dataset(id).expect("just added");
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
        crate::s102_host::install(&mut self.state, opened.s102, &mut report);
        self.state.bounds = self.data_bounds();
        Ok(report)
    }

    fn data_bounds(&self) -> GeoBounds {
        let bathymetry = self.state.bathymetry_extents();
        if !self.state.cells.is_empty() || !bathymetry.is_empty() {
            // The viewer's all-data view: the data extent (S-101 cells and
            // S-102 rasters) with a margin.
            let mut bounds = chart_data_bounds(&self.state.cells, bathymetry);
            bounds.expand_by_percent(0.1);
            return bounds;
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

    /// Exchange-set checks of the loaded S-101 datasets: the discovery
    /// metadata of each dataset's exchange catalogue (`CATALOG.XML`) against
    /// the materialized cell (XC profile, effective scales, regional
    /// coverage). Disagreements are issues; outcomes and datasets without
    /// catalogue metadata are notices.
    pub fn exchange_set_checks(&self) -> (Vec<ValidationIssue>, Vec<String>) {
        let mut issues = Vec::new();
        let mut notices = Vec::new();
        for cell in &self.state.cells {
            let subject = cell.file_path.display().to_string();
            let metadata = s101_update_plan::dataset_key(&cell.dsid)
                .ok()
                .and_then(|key| self.state.loaded_discovery.get(&key))
                .and_then(Option::as_ref);
            let Some(metadata) = metadata else {
                notices.push(format!(
                    "{}: no exchange catalogue metadata; exchange-set checks not applied",
                    cell.dsid.dataset_name
                ));
                continue;
            };
            let mut issue = |check: &str, error: anyhow::Error| {
                issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: subject.clone(),
                    check: check.into(),
                    message: format!("{error:#}"),
                })
            };
            if let Err(error) = metadata.validate_xc_profile(&cell.dsid) {
                issue("XC profile", error);
            }
            match metadata.validate_effective_xc_scales(cell) {
                Ok(status) => notices.push(format!(
                    "{}: exchange catalogue scales {status:?}",
                    cell.dsid.dataset_name
                )),
                Err(error) => issue("XC scales", error),
            }
            match metadata.validate_xc_regions(cell) {
                Ok(status) => notices.push(format!(
                    "{}: exchange catalogue coverage {status:?}",
                    cell.dsid.dataset_name
                )),
                Err(error) => issue("XC coverage", error),
            }
        }
        (issues, notices)
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
        let fit = match view.camera {
            Some(camera) => self.camera_view(camera, view.viewport)?.0,
            None => view.fit.unwrap_or(self.state.bounds),
        };
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
        // GML overlays (S-124/S-129): cell-less, coverage-exempt instructions.
        let request = PortrayalRequest {
            profile: &self.display.profile,
            viewing_time: gml_overlay::viewing_time(&self.state.render_context.settings),
            settings: Some(&settings),
        };
        if let Some(overlay) = self.overlays.portray(&request) {
            context.append_converted(overlay.instructions.to_vec());
        }
        context.set_viewport(width as f32, height as f32);
        context.scaler = self.state.render_context.scaler.clone();
        prepare_flat_coverage(
            Some(coverage.as_ref()),
            &mut context,
            [width, height],
            scale_factor,
        )?;
        // S-102: the viewer's raster portrayal of the loaded bathymetry,
        // handed to the renderer as an extension.
        let extensions =
            crate::s102_host::prepare(&self.state, &self.display.profile, &settings, &self.s102)?
                .into_iter()
                .collect();
        self.state.catalogue_registry = registry.clone();
        self.state.coverage_inventory = Some(Arc::clone(&coverage));
        self.state.ic_assigned_vectors = ic_changed;
        Ok(Arc::new(PreparedScene {
            context,
            coverage,
            catalogue_registry: registry,
            ic_changed,
            id: Arc::new(()),
            extensions,
        }))
    }

    /// Cache-store entries a [`Self::prepare`] with the current display reads
    /// for the opened S-101 cells when they are portrayed with Lua: the
    /// per-cell instruction caches
    /// ([`EngineState::cell_instruction_cache_paths`]); empty without cells
    /// or without a cache. A host whose persistent store is asynchronous
    /// (the web's IndexedDB) loads these entries into the platform cache
    /// store before preparing, and saves the entries the preparation wrote.
    pub fn cell_cache_entries(&mut self) -> Vec<PathBuf> {
        // The key covers the colour profile `prepare` selects first.
        let previous = std::mem::replace(
            &mut self.state.current_profile_name,
            self.display.profile.clone(),
        );
        let paths = self
            .state
            .cell_instruction_cache_paths(Some(&self.display.settings));
        self.state.current_profile_name = previous;
        paths.into_iter().flatten().flatten().collect()
    }

    /// Cache-store entries of precomputed payloads
    /// ([`crate::portrayal::PrecomputedCells`]) for the opened S-101 cells
    /// with the current display, i.e. what a [`Self::prepare`] with
    /// [`PortrayalSource::Precomputed`] reads; empty without cells or cache
    /// store. A host keeping payloads in an asynchronous store loads these
    /// before preparing.
    pub fn precomputed_cell_entries(&self) -> Vec<PathBuf> {
        let common =
            EngineState::precomputed_common(Some(&self.display.settings), &self.display.profile);
        let identities = self.state.cell_identities();
        let cells = crate::portrayal::PrecomputedCells {
            store: self.state.platform.cache.as_ref(),
            common,
            identities: &identities,
            export: false,
        };
        (0..self.state.cells.len())
            .filter_map(|index| cells.key(&self.state.fc, &self.state.pc, index))
            .collect()
    }

    /// Cache-store entry of the S-102 portrayal rule's instructions the
    /// next [`Self::prepare`] reads for the loaded bathymetry with the
    /// current display (precomputed portrayal, or Lua with
    /// [`crate::s102_host::S102Options::reuse_rule_instructions`]).
    pub fn s102_cache_entry(&self) -> Option<PathBuf> {
        crate::s102_host::instruction_entry(&self.state, &self.display.settings)
    }

    /// Move `scene` to `view` without portraying again, as the viewer's
    /// pan/zoom path does (cell instructions do not depend on the view):
    /// the camera, viewport and pixel ratio of [`Self::prepare`] and the flat
    /// coverage of the new view. `scene` must be the scene this engine
    /// prepared last, with the datasets and display unchanged since. Returns
    /// false (`scene` unchanged) when the view needs other cells resident
    /// (large chart sets, `EngineState::residency_for_view`); then
    /// [`Self::prepare`] portrays the new resident set. Otherwise the result
    /// equals what [`Self::prepare`] would build for `view`.
    pub fn reproject(&mut self, scene: &mut PreparedScene, view: &View) -> Result<bool> {
        let ViewportPixels {
            width,
            height,
            scale_factor,
        } = view.viewport;
        ensure!(
            width > 0 && height > 0 && scale_factor.is_finite() && scale_factor > 0.,
            "Empty view"
        );
        let fit = match view.camera {
            Some(camera) => self.camera_view(camera, view.viewport)?.0,
            None => view.fit.unwrap_or(self.state.bounds),
        };
        let context = &mut self.state.render_context;
        context.set_bounds(self.state.bounds);
        context.set_viewport(width as f32, height as f32);
        context.zoom_to_fit(fit);
        context.scaler.set_pixel_ratio(scale_factor);
        if !self.state.cells.is_empty()
            && self
                .state
                .residency_for_view(&scene.coverage, true, Some(view.viewport))
                != self.state.resident_cells
        {
            return Ok(false);
        }
        scene.context.set_viewport(width as f32, height as f32);
        scene.context.scaler = self.state.render_context.scaler.clone();
        prepare_flat_coverage(
            Some(scene.coverage.as_ref()),
            &mut scene.context,
            [width, height],
            scale_factor,
        )?;
        Ok(true)
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
    /// Installed S-101 catalogue inventory (empty: none).
    inventory: PathBuf,
    /// Loaded logical datasets when the job started (cancellation targets).
    loaded: s101_update_plan::LoadedDatasets,
    /// Inputs of the loaded chains (predecessors of selected updates).
    loaded_inputs: Vec<PathBuf>,
    s102_options: crate::s102_host::S102Options,
}

/// One S-101 chain read into memory.
struct ByteChain {
    base: (PathBuf, Vec<u8>),
    updates: Vec<Vec<u8>>,
}

impl OpenTask {
    fn run(self, cancel: &AtomicBool) -> Result<OpenedDatasets> {
        if let DatasetSource::Bytes(files) = &self.source {
            // Host bytes: the same opening over a memory source of them.
            let mut memory = crate::platform::MemoryDataSource::new();
            for (path, bytes) in files {
                memory.insert(path.clone(), Arc::clone(bytes));
            }
            let paths = files.iter().map(|(path, _)| path.clone()).collect();
            return OpenTask {
                platform: self.platform.clone().with_data(Arc::new(memory)),
                source: DatasetSource::Files(paths),
                ..self
            }
            .run(cancel);
        }
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
            DatasetSource::Bytes(_) => unreachable!("opened from memory above"),
        };
        let explicit = matches!(
            self.source,
            DatasetSource::Files(_) | DatasetSource::Bytes(_)
        );
        let mut charts = Vec::new();
        let mut overlays = Vec::new();
        let mut routes = Vec::new();
        let mut s102_inputs = Vec::new();
        for path in inputs {
            if s101_update_plan::is_chart_file(&path) {
                charts.push(path);
            } else if gml_overlay::sniff_product_in(data, &path).is_some() {
                overlays.push(path);
            } else if self.capture_route(&path, &mut notices, &mut routes) {
                // Captured into `routes`.
            } else if crate::s102_host::is_hdf5_input(&path) {
                s102_inputs.push(path);
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
        let (cells, removals) = if charts.is_empty() {
            (Vec::new(), Vec::new())
        } else if data.local_paths() || require {
            self.load_planned_charts(charts, explicit, cancel, &mut notices, &mut issues)?
        } else {
            (
                self.load_byte_charts(charts, cancel, &mut notices, &mut issues)?,
                Vec::new(),
            )
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
        let s102 = crate::s102_host::open_inputs(
            s102_inputs,
            data,
            &|paths| self.authorize(paths),
            require,
            &self.s102_options,
            cancel,
            &mut notices,
            &mut issues,
        )?;
        Ok(OpenedDatasets {
            cells,
            overlays: loaded_overlays,
            removals,
            routes,
            issues,
            notices,
            catalogues: (*self.fc.source_digest(), *self.pc.source_digest()),
            s102,
        })
    }

    fn trust_anchors(&self) -> Result<ferrite_security::TrustAnchors> {
        let mut anchors = ferrite_security::TrustAnchors::default();
        for (administrator, pem) in self.trust_anchors.iter() {
            anchors.install_pem(administrator, pem)?;
        }
        Ok(anchors)
    }

    /// Local sources: the native file verifier, unchanged. Other sources:
    /// the same verification reading exchange sets through the data source.
    fn authorize(&self, paths: &[PathBuf]) -> Result<ferrite_security::AuthorizedDatasets> {
        match self.signatures {
            SignaturePolicy::Off => {
                dataset_signature_policy::unchecked_datasets_in(self.platform.data.as_ref(), paths)
            }
            SignaturePolicy::Required if self.platform.data.local_paths() => {
                ferrite_security::authorize_datasets(
                    paths,
                    &self.trust_anchors()?,
                    self.platform.clock.unix_seconds(),
                    ferrite_security::UnsignedPolicy::Reject,
                )
            }
            SignaturePolicy::Required => ferrite_security::authorize_datasets_in(
                &dataset_signature_policy::exchange_source(&self.platform.data),
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
        if !data.local_paths() && self.signatures == SignaturePolicy::Required {
            // Bytes source: the authenticated in-memory snapshot.
            let report = self.authorize(&[path.to_path_buf()])?;
            let snapshot =
                dataset_signature_policy::dataset_snapshot_in(data, &report, path, true)?
                    .context("Required authenticated dataset snapshot is missing")?;
            let bytes = snapshot
                .bytes()
                .context("Authenticated snapshot of a bytes source holds no bytes")?;
            ensure!(
                bytes.len() <= limits.bytes,
                "{} exceeds the {} byte dataset limit",
                path.display(),
                limits.bytes
            );
            return Ok((bytes.to_vec(), Provenance::Authenticated));
        }
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
        self.admit_with(cell, &self.fc, &self.pc)
    }

    /// [`Self::admit`] against the catalogue pair that owns the cell.
    fn admit_with(
        &self,
        cell: &mut S101Cell,
        fc: &BoundFeatureCatalogue,
        pc: &BoundPortrayalCatalogue,
    ) -> Result<()> {
        ferrite_s101::validate_dataset_catalogues(&cell.dsid, fc, &pc.product_id, &pc.version)
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

    /// An S-421 route among the inputs (the S-421 adapter recognises the
    /// name and head), captured once into `routes` (true); capture failures
    /// and routes refused by the signature policy are notices.
    #[cfg(feature = "s421")]
    fn capture_route(
        &self,
        path: &Path,
        notices: &mut Vec<String>,
        routes: &mut Vec<CapturedRouteInput>,
    ) -> bool {
        use ferrite_product_api::ProductAdapter;
        let data = self.platform.data.as_ref();
        if !S421::is_route_name(path) {
            return false;
        }
        let Ok(head) = data.read_limited(path, 64 * 1024 - 1) else {
            return false;
        };
        if S421.sniff(path, &head).is_none() {
            return false;
        }
        match crate::products::s421::capture_if_route_in(data, path) {
            Ok(Some(route)) => {
                match route.allowed_xml(self.signatures == SignaturePolicy::Required) {
                    Ok(_) => {
                        routes.push(route);
                        true
                    }
                    Err(error) => {
                        notices.push(format!("Not opened: {}: {error}", path.display()));
                        false
                    }
                }
            }
            Ok(None) => false,
            Err(error) => {
                notices.push(format!("Not opened: {}: {error}", path.display()));
                false
            }
        }
    }
    /// Without the S-421 product no input is a route.
    #[cfg(not(feature = "s421"))]
    fn capture_route(
        &self,
        _path: &Path,
        _notices: &mut Vec<String>,
        _routes: &mut Vec<CapturedRouteInput>,
    ) -> bool {
        false
    }

    /// The viewer's authorized planner and loader, over any data source:
    /// local files (private snapshot files) or bytes (owned copies). An
    /// explicit selection also plans its physical cancellations.
    #[allow(clippy::type_complexity)]
    fn load_planned_charts(
        &self,
        charts: Vec<PathBuf>,
        explicit: bool,
        cancel: &AtomicBool,
        notices: &mut Vec<String>,
        issues: &mut Vec<ValidationIssue>,
    ) -> Result<(
        Vec<(ChartLoadResult, String)>,
        Vec<(PathBuf, ValidatedRemoval)>,
    )> {
        let data = self.platform.data.as_ref();
        let require = self.signatures == SignaturePolicy::Required;
        // An explicit selection also opens the base and predecessors of a
        // selected update (as the viewer does).
        let selected = charts.clone();
        // Planned inputs are canonical paths, the keys of their authorization
        // and lifecycle metadata.
        let charts = if explicit {
            s101_update_plan::expand_candidates_in(data, &charts, &self.loaded_inputs)?
        } else {
            let mut canonical = Vec::with_capacity(charts.len());
            for path in charts {
                match data.canonicalize(&path) {
                    Ok(path) => canonical.push(path),
                    Err(error) => {
                        notices.push(format!("Skipped S-101 input {}: {error}", path.display()))
                    }
                }
            }
            canonical
        };
        let authorization = self.authorize(&charts)?;
        let provenance = if require {
            "signature verified"
        } else {
            "signature not verified"
        };
        let (plans, cancellations, plan_notices) = if explicit {
            let record = self.platform.user.cancellation_history();
            let history = match &record {
                Some(record) => CancellationHistory::read_record(record.as_ref())?,
                None => CancellationHistory::default(),
            };
            let (plans, mut cancellations, mut plan_notices) =
                s101_update_plan::authorized_selection_plans_in(
                    data,
                    charts,
                    &selected,
                    &authorization,
                    require,
                    &self.loaded,
                    &history,
                )?;
            if record.is_none() {
                for plan in cancellations.drain(..) {
                    plan_notices.push(format!(
                        "Skipped S-101 cancellation {}: this host keeps no cancellation history",
                        plan.original().display()
                    ));
                }
            }
            (plans, cancellations, plan_notices)
        } else {
            let (plans, plan_notices) = s101_update_plan::authorized_folder_plans_in(
                data,
                charts,
                &authorization,
                require,
            )?;
            (plans, Vec::new(), plan_notices)
        };
        notices.extend(plan_notices);
        let mut removals = Vec::new();
        for plan in cancellations {
            ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
            let path = plan.original().to_path_buf();
            match plan.load() {
                Ok(removal) => removals.push((path, removal)),
                Err(error) => issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: path.display().to_string(),
                    check: "S-101 cancellation".into(),
                    message: format!("{error:#}"),
                }),
            }
        }
        let codes = Arc::new(self.fc.feature_type_codes());
        let owners = self.inventory_owners(&plans, notices);
        let plans: Vec<_> = plans.into_iter().map(Arc::new).collect();
        let results = parallel_map(self.platform.executor.as_ref(), &plans, |plan| {
            if cancel.load(Ordering::Relaxed) {
                bail!("Open cancelled");
            }
            let (mut cell, source_identity) = plan.load_with_decoded_cache(&self.decoded)?;
            // Datasets of other catalogue editions: their installed owner.
            let owner = owners.as_ref().and_then(|(registry, codes)| {
                let key = EngineState::catalogue_dataset_key(&plan.base.id).ok()?;
                let owner = registry.owner_for(&key)?;
                Some((owner, codes.get(owner.fc().source_digest())?))
            });
            let codes = match owner {
                Some((owner, owner_codes)) => {
                    self.admit_with(&mut cell, owner.fc(), owner.pc())?;
                    Arc::clone(owner_codes)
                }
                None => {
                    self.admit(&mut cell)?;
                    Arc::clone(&codes)
                }
            };
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
        Ok((cells, removals))
    }

    /// Catalogue owners from the installed inventory for planned datasets
    /// the bound pair cannot process (as the viewer's loader stages them),
    /// with each owner's feature type codes. None without an inventory or
    /// when no dataset needs one; a failed selection is a notice (those
    /// datasets are then rejected by the bound pair).
    #[allow(clippy::type_complexity)]
    fn inventory_owners(
        &self,
        plans: &[s101_update_plan::Plan],
        notices: &mut Vec<String>,
    ) -> Option<(
        crate::s101_catalogue_selection::DatasetCatalogueRegistry,
        BTreeMap<[u8; 32], Arc<Vec<String>>>,
    )> {
        use crate::s101_catalogue_selection::{
            DatasetCatalogueOwner, DatasetCatalogueRegistry, DatasetCatalogueRequest,
        };
        if self.inventory.as_os_str().is_empty()
            || plans.iter().all(|plan| {
                ferrite_s101::validate_dataset_catalogues(
                    &plan.base.id,
                    &self.fc,
                    &self.pc.product_id,
                    &self.pc.version,
                )
                .is_ok()
            })
        {
            return None;
        }
        let staged = (|| -> Result<DatasetCatalogueRegistry> {
            let requests = plans
                .iter()
                .map(|plan| {
                    Ok(DatasetCatalogueRequest {
                        dataset_key: EngineState::catalogue_dataset_key(&plan.base.id)?,
                        source_identity: plan.captured_source_identity_sha(),
                        identification: plan.base.id.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let fallback = Arc::new(DatasetCatalogueOwner::new(
                Arc::clone(&self.fc),
                Arc::clone(&self.pc),
            )?);
            DatasetCatalogueRegistry::default().stage_in(
                self.platform.data.as_ref(),
                &requests,
                fallback,
                &self.inventory,
            )
        })();
        let registry = match staged {
            Ok(registry) => registry,
            Err(error) => {
                notices.push(format!("Installed catalogue selection failed: {error:#}"));
                return None;
            }
        };
        let mut codes = BTreeMap::new();
        for plan in plans {
            let Some(owner) = EngineState::catalogue_dataset_key(&plan.base.id)
                .ok()
                .and_then(|key| registry.owner_for(&key))
            else {
                continue;
            };
            codes
                .entry(*owner.fc().source_digest())
                .or_insert_with(|| Arc::new(owner.fc().feature_type_codes()));
        }
        Some((registry, codes))
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
        *shared.slot.lock().unwrap() = JobSlot::Done(Box::new(Err(anyhow::anyhow!("x"))));
        assert_eq!(job.poll(), JobStatus::Finished);
        assert!(job.try_take().unwrap().is_err());
        assert!(job.try_take().unwrap().is_err());
    }
}
