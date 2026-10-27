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
use crate::dataset_signature_policy;
use crate::gml_overlay::{self, GmlOverlays, SymbolRegistrar};
use crate::layers::ProductLayers;
use crate::pick::{find_features_at, FeatureReport, GeometryPicker, PickCandidate, PickSurface};
use crate::platform::{EntryKind, Platform};
#[cfg(feature = "s421")]
use crate::products::s421::{CapturedRouteInput, S421Layer};
use crate::scene::{prepare_flat_coverage, PreparedScene};
use crate::state::{EngineState, ViewportPixels};
use anyhow::{bail, ensure, Context, Result};
use ferrite_product_api::open::{InstallContext, Loaded, OpenContext, OpenNotes, OpenStage};
use ferrite_product_api::report::{InstallReport, ProductReport};
use ferrite_product_api::{
    HostServices, Interoperability, PortrayalRequest, PortrayalSource, ProductId, SceneRole,
};
use ferrite_render::host_settings::SettingsState;
use ferrite_render::GeoBounds;
use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
#[cfg(feature = "s101")]
use {
    ferrite_s101::layer::cells::chart_data_bounds,
    ferrite_s101::layer::portrayal::{AreaTriangulator, PortrayalChangeRequest},
    ferrite_s101::layer::publication::{CancellationHistory, CancellationRecord},
    ferrite_s101::layer::S101Layer,
};

/// Whether dataset signatures must verify before data is opened (defined
/// with the product interface, which layers check inputs against).
pub use ferrite_product_api::open::SignaturePolicy;
/// The common report model (defined with the product interface).
pub use ferrite_product_api::report::{DatasetId, DatasetSummary, IssueSeverity, ValidationIssue};

/// Stored area triangulation of the renderer (S-101 cell portrayal).
#[cfg(feature = "s101")]
type Triangulator = AreaTriangulator;
#[cfg(not(feature = "s101"))]
type Triangulator = fn(
    &ferrite_render::AreaInstruction,
    ferrite_render::FlatProjection,
    usize,
) -> Option<ferrite_render::AreaTriangulation>;

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
    pub triangulate: Triangulator,
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

/// Outcome of installing an open job.
#[derive(Clone, Debug, Default)]
pub struct OpenReport {
    /// Datasets newly opened by this job.
    pub datasets: Vec<DatasetSummary>,
    pub issues: Vec<ValidationIssue>,
    /// Inputs not opened and other information.
    pub notices: Vec<String>,
    /// Product-specific reports in installation order: S-101 cancellations
    /// applied and recorded in the cancellation history by this job
    /// (removals of loaded datasets and announcements), S-102 files newly
    /// opened by this job.
    pub products: Vec<ProductReport>,
}

impl From<InstallReport> for OpenReport {
    fn from(report: InstallReport) -> Self {
        Self {
            datasets: report.datasets,
            issues: report.issues,
            notices: report.notices,
            products: report.products,
        }
    }
}

impl OpenReport {
    /// The product reports of `product` of kind `kind` as `T`.
    pub fn product_reports<T: Clone + 'static>(&self, product: ProductId, kind: &str) -> Vec<T> {
        self.products
            .iter()
            .filter(|r| r.product == product && r.kind == kind)
            .filter_map(|r| r.get::<T>().cloned())
            .collect()
    }
    /// S-101 cancellations applied and recorded by this job.
    #[cfg(feature = "s101")]
    pub fn cancellations(&self) -> Vec<CancellationRecord> {
        self.product_reports(ferrite_s101::layer::PRODUCT, "cancellation")
    }
    /// S-102 files newly opened by this job.
    #[cfg(feature = "s102-pure")]
    pub fn bathymetry(&self) -> Vec<ferrite_s102::layer::BathymetrySummary> {
        self.product_reports(ferrite_s102::layer::PRODUCT, "bathymetry")
    }
}

/// Datasets loaded by an [`OpenJob`], not yet installed: what each
/// registered layer's stage loaded, in registration order.
pub struct OpenedDatasets {
    stages: Vec<(usize, Loaded)>,
    issues: Vec<ValidationIssue>,
    notices: Vec<String>,
}

impl OpenedDatasets {
    /// Loaded inputs of kind `kind` (`"cells"`, `"cancellations"`,
    /// `"overlays"`, `"bathymetry"`, `"routes"`).
    pub fn count(&self, kind: &str) -> usize {
        self.stages
            .iter()
            .flat_map(|(_, loaded)| loaded.counts.iter())
            .filter(|(k, _)| *k == kind)
            .map(|(_, n)| n)
            .sum()
    }
    /// Decoded S-102 files (coverage and trial inspections).
    pub fn bathymetry_count(&self) -> usize {
        self.count("bathymetry")
    }
    pub fn cell_count(&self) -> usize {
        self.count("cells")
    }
    pub fn overlay_count(&self) -> usize {
        self.count("overlays")
    }
    /// Validated S-101 cancellations (removals and announcements).
    pub fn cancellation_count(&self) -> usize {
        self.count("cancellations")
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

/// The S-100 chart engine: a chart state with the registered product layers
/// of this build ([`crate::products::engine_layers`]) composed generically.
pub struct ChartEngine {
    state: EngineState,
    signatures: SignaturePolicy,
    trust_anchors: Arc<Vec<(String, Vec<u8>)>>,
    #[cfg(feature = "s101")]
    triangulate: Triangulator,
    display: DisplayOptions,
}

impl ChartEngine {
    /// The registered layers with their catalogues from `config.catalogues`:
    /// the S-101 pair (required), the overlay products' and S-102
    /// catalogues (optional).
    pub fn new(config: EngineConfig) -> Result<Self> {
        let layers = crate::products::engine_layers(&config)?;
        let state = EngineState::new(config.platform, config.profile.clone(), layers);
        Ok(Self {
            state,
            signatures: config.signatures,
            trust_anchors: Arc::new(config.trust_anchors),
            #[cfg(feature = "s101")]
            triangulate: config.triangulate,
            display: DisplayOptions {
                profile: config.profile,
                ..DisplayOptions::default()
            },
        })
    }

    /// Opened S-102 files ([`crate::products::S102_SUPPORT`] says what is
    /// served).
    #[cfg(feature = "s102-pure")]
    pub fn bathymetry(&self) -> Vec<ferrite_s102::layer::BathymetrySummary> {
        self.state.layers.s102().summaries()
    }
    #[cfg(feature = "s102-pure")]
    pub fn s102_options(&self) -> &ferrite_s102::layer::S102Options {
        &self.state.layers.s102().options
    }
    /// Depth adjustments and raster tile edge for S-102 opened and prepared
    /// from now on.
    #[cfg(feature = "s102-pure")]
    pub fn set_s102_options(&mut self, options: ferrite_s102::layer::S102Options) {
        self.state.layers.s102_mut().options = options;
    }

    /// Estimated instruction count from which preparations portray only the
    /// cells near the view (residency); None restores the default
    /// (`FERRITE_RESIDENCY_THRESHOLD` or 1,500,000; 0 always activates it).
    /// Applies from the next [`Self::prepare`].
    #[cfg(feature = "s101")]
    pub fn set_residency_threshold(&mut self, threshold: Option<usize>) {
        self.state.layers.s101_mut().residency_threshold_override = threshold;
    }

    /// The chart state (layers, catalogues, coverage).
    pub fn state(&self) -> &EngineState {
        &self.state
    }
    /// The registered product layers.
    pub fn layers(&self) -> &ProductLayers {
        &self.state.layers
    }
    /// Mutable [`Self::layers`] (product-specific host operations).
    pub fn layers_mut(&mut self) -> &mut ProductLayers {
        &mut self.state.layers
    }
    /// The GML overlay layer (S-124/S-129).
    pub fn overlays(&self) -> &GmlOverlays {
        self.state
            .layers
            .overlays()
            .expect("The overlay layer is registered")
    }
    pub fn platform(&self) -> &Platform {
        &self.state.platform
    }
    /// Catalogue errors of the optional overlay products.
    pub fn catalogue_errors(&self) -> &[String] {
        &self.overlays().catalogue_errors
    }
    /// S-421 routes captured by the opened inputs, in opening order (not
    /// portrayed by the engine; under [`SignaturePolicy::Required`] none are
    /// admitted).
    #[cfg(feature = "s421")]
    pub fn routes(&self) -> Vec<&CapturedRouteInput> {
        self.state.layers.routes().inputs().collect()
    }
    /// The S-421 product layer holding the routes (coverage, feature
    /// reports, the route overlay renderer extension).
    #[cfg(feature = "s421")]
    pub fn route_layer(&mut self) -> &mut S421Layer {
        self.state.layers.routes_mut()
    }
    /// The S-101 cancellation history of the platform user store (empty when
    /// the host keeps none).
    #[cfg(feature = "s101")]
    pub fn cancellation_history(&self) -> Result<CancellationHistory> {
        match ferrite_s101::layer::publication::history_record(self.state.platform.user.as_ref()) {
            Some(record) => CancellationHistory::read_record(record.as_ref()),
            None => Ok(CancellationHistory::default()),
        }
    }

    /// Where rule-based products take their instructions from (default:
    /// Lua when the `lua-native` feature is built, otherwise precomputed
    /// payloads).
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

    /// Start opening `source` in the background: every registered layer's
    /// stage takes the inputs its products' adapters recognise.
    pub fn open(&mut self, source: DatasetSource) -> OpenJob {
        let shared = Arc::new(JobShared {
            slot: Mutex::new(JobSlot::Running),
            done: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let stages = self
            .state
            .layers
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| Some((index, entry.layer.open_stage()?)))
            .collect();
        let task = OpenTask {
            platform: self.state.platform.clone(),
            source,
            signatures: self.signatures,
            trust_anchors: Arc::clone(&self.trust_anchors),
            stages,
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

    /// Open and install in one call.
    pub fn open_blocking(&mut self, source: DatasetSource) -> Result<OpenReport> {
        let job = self.open(source);
        self.install(job.wait()?)
    }

    /// Publish the datasets of a finished job into the engine: every layer
    /// checks its result first; then the first installation pass
    /// (removals, independent results) and the installs run in registration
    /// order.
    pub fn install(&mut self, opened: OpenedDatasets) -> Result<OpenReport> {
        let OpenedDatasets {
            mut stages,
            issues,
            notices,
        } = opened;
        for (index, loaded) in &stages {
            self.layer_at(*index)?
                .layer
                .check_opened(loaded.payload.as_ref())?;
        }
        let mut report = InstallReport {
            issues,
            notices,
            ..InstallReport::default()
        };
        let user = Arc::clone(&self.state.platform.user);
        let context = InstallContext {
            user: user.as_ref(),
        };
        for (index, loaded) in &mut stages {
            self.layer_at_mut(*index)?.layer.install_early(
                loaded.payload.as_mut(),
                &context,
                &mut report,
            )?;
        }
        for (index, loaded) in stages {
            self.layer_at_mut(index)?
                .layer
                .install(loaded.payload, &context, &mut report)?;
        }
        self.state.bounds = self.data_bounds();
        Ok(report.into())
    }

    fn layer_at(&self, index: usize) -> Result<&crate::layers::RegisteredLayer> {
        self.state.layers.iter().nth(index).context("Unknown layer")
    }
    fn layer_at_mut(&mut self, index: usize) -> Result<&mut crate::layers::RegisteredLayer> {
        self.state
            .layers
            .iter_mut()
            .nth(index)
            .context("Unknown layer")
    }

    /// The all-data view: the base chart and grid data extent (S-101 cells
    /// and S-102 rasters) with a margin; else the overlays' extent; else the
    /// world.
    fn data_bounds(&self) -> GeoBounds {
        let layers = &self.state.layers;
        let grids: Vec<GeoBounds> = layers
            .with_role(SceneRole::Grid)
            .filter_map(|layer| layer.coverage())
            .collect();
        let base = layers
            .with_role(SceneRole::BaseChart)
            .any(|layer| !layer.is_empty());
        if base || !grids.is_empty() {
            #[cfg(feature = "s101")]
            let mut bounds = match layers.get::<S101Layer>() {
                Some(s101) => chart_data_bounds(&s101.cells, grids),
                None => chart_data_bounds(&[], grids),
            };
            #[cfg(not(feature = "s101"))]
            let mut bounds = {
                let mut bounds = GeoBounds::default();
                for grid in grids {
                    bounds.expand(ferrite_render::WorldPoint::new(grid.min_x, grid.min_y));
                    bounds.expand(ferrite_render::WorldPoint::new(grid.max_x, grid.max_y));
                }
                bounds
            };
            bounds.expand_by_percent(0.1);
            return bounds;
        }
        layers
            .with_role(SceneRole::Overlay)
            .filter_map(|layer| layer.coverage())
            .reduce(|a, b| {
                GeoBounds::new(
                    a.min_x.min(b.min_x),
                    a.min_y.min(b.min_y),
                    a.max_x.max(b.max_x),
                    a.max_y.max(b.max_y),
                )
            })
            .unwrap_or_else(|| GeoBounds::new(-180.0, -90.0, 180.0, 90.0))
    }

    /// Every opened dataset, in registration order of the layers.
    pub fn datasets(&self) -> Vec<DatasetSummary> {
        self.state
            .layers
            .iter()
            .flat_map(|entry| entry.layer.datasets())
            .collect()
    }

    /// Feature counts by feature type of a dataset.
    pub fn feature_types(&self, id: DatasetId) -> BTreeMap<String, usize> {
        self.state
            .layers
            .iter()
            .find_map(|entry| entry.layer.feature_types(id))
            .unwrap_or_default()
    }

    /// Validation findings kept with the opened datasets (overlay datasets;
    /// S-101 cells are validated when opened, failures are reported by
    /// [`ChartEngine::install`]).
    pub fn validation_issues(&self) -> Vec<ValidationIssue> {
        self.state
            .layers
            .iter()
            .flat_map(|entry| entry.layer.validation_issues())
            .collect()
    }

    /// Exchange-set checks of the opened datasets (S-101: the discovery
    /// metadata of each dataset's exchange catalogue against the
    /// materialized cell). Disagreements are issues; outcomes and datasets
    /// without catalogue metadata are notices.
    pub fn exchange_set_checks(&self) -> (Vec<ValidationIssue>, Vec<String>) {
        let mut issues = Vec::new();
        let mut notices = Vec::new();
        for entry in self.state.layers.iter() {
            let (i, n) = entry.layer.exchange_set_checks();
            issues.extend(i);
            notices.extend(n);
        }
        (issues, notices)
    }

    pub fn display(&self) -> &DisplayOptions {
        &self.display
    }

    /// Colour profile and portrayal settings of the next scene (every layer
    /// must accept them).
    pub fn set_display(&mut self, display: DisplayOptions) -> Result<()> {
        for entry in self.state.layers.iter() {
            entry
                .layer
                .check_display(&display.profile, &display.settings)?;
        }
        self.display = display;
        Ok(())
    }

    /// Portray the opened datasets for `view`: the base chart (S-101 cells
    /// with residency, per-cell caches and interoperability), the overlay
    /// layers' instructions (cell-less, coverage-exempt), coverage clipping
    /// and the grid layers' renderer extensions (S-102 raster tiles).
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

        #[cfg(feature = "s101")]
        let (mut context, coverage, registry, ic_changed) = self.prepare_base(view, &settings)?;
        #[cfg(not(feature = "s101"))]
        let (mut context, ic_changed) = {
            let mut context = self.state.render_context.empty_for_rebuild();
            context.set_bounds(self.state.bounds);
            (context, 0)
        };
        // Overlay layers (S-124/S-129): cell-less, coverage-exempt instructions.
        let request = PortrayalRequest {
            profile: &self.display.profile,
            viewing_time: gml_overlay::viewing_time(&self.state.render_context.settings),
            settings: Some(&settings),
            host: HostServices::default(),
        };
        for layer in self.state.layers.with_role_mut(SceneRole::Overlay) {
            if let Some(overlay) = layer.portray(&request)? {
                context.append_converted(overlay.instructions.to_vec());
            }
        }
        context.set_viewport(width as f32, height as f32);
        context.scaler = self.state.render_context.scaler.clone();
        #[cfg(feature = "s101")]
        let inventory = Some(coverage.as_ref());
        #[cfg(not(feature = "s101"))]
        let inventory = None;
        prepare_flat_coverage(inventory, &mut context, [width, height], scale_factor)?;
        // Grid layers (S-102): the viewer's raster portrayal of the loaded
        // bathymetry, handed to the renderer as extensions.
        let extensions = self.portray_grids(&settings)?;
        #[cfg(feature = "s101")]
        {
            let s101 = self.state.layers.s101_mut();
            s101.catalogue_registry = registry.clone();
            s101.coverage_inventory = Some(Arc::clone(&coverage));
        }
        self.state.ic_assigned_vectors = ic_changed;
        Ok(Arc::new(PreparedScene {
            context,
            #[cfg(feature = "s101")]
            coverage,
            #[cfg(feature = "s101")]
            catalogue_registry: registry,
            ic_changed,
            id: Arc::new(()),
            extensions,
        }))
    }

    /// The base chart part of [`Self::prepare`]: the cells portrayed with
    /// their catalogue owners or the bound pair, their coverage, the
    /// catalogue registry and the interoperability changes.
    #[cfg(feature = "s101")]
    #[allow(clippy::type_complexity)]
    fn prepare_base(
        &mut self,
        view: &View,
        settings: &SettingsState,
    ) -> Result<(
        ferrite_render::RenderContext,
        Arc<ferrite_s101::coverage_projection::GeographicCoverageInventory>,
        ferrite_s101::layer::catalogue_selection::DatasetCatalogueRegistry,
        usize,
    )> {
        let s101 = self.state.layers.s101();
        let (fc, pc) = (Arc::clone(&s101.fc), Arc::clone(&s101.pc));
        if s101.cells.is_empty() {
            let mut context = self.state.render_context.empty_for_rebuild();
            context.set_bounds(self.state.bounds);
            return Ok((
                context,
                Arc::new(
                    ferrite_s101::coverage_projection::GeographicCoverageInventory::from_cells(&[])?,
                ),
                Default::default(),
                0,
            ));
        }
        let registry = self
            .state
            .layers
            .s101_mut()
            .prepare_catalogue_registry(&fc, &pc, true)?;
        if self
            .state
            .layers
            .s101()
            .requires_cell_resources(&registry, &fc, &pc)?
        {
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
            Ok((context, coverage, registry, change.ic_changed))
        } else {
            let active_ic = settings
                .interoperability_enabled
                .then(|| self.state.ic.clone())
                .flatten();
            let scene = self.state.portray_loaded_cells(
                true,
                Some(settings),
                Some(view.viewport),
                active_ic,
                self.triangulate,
            )?;
            Ok((scene.context, scene.coverage, registry, scene.ic_changed))
        }
    }

    /// The grid layers' renderer extensions for the display, with the
    /// host's rule source, cache store and interoperability selection.
    fn portray_grids(
        &mut self,
        settings: &SettingsState,
    ) -> Result<Vec<ferrite_product_api::RendererExtension>> {
        let active_ic = settings
            .interoperability_enabled
            .then(|| self.state.ic.clone())
            .flatten();
        let base = self.state.base_products();
        let combination = self.state.ic_combination.clone();
        let selection = |product: ProductId| -> Result<Box<dyn Any>> {
            let ic = active_ic
                .as_ref()
                .context("No interoperability catalogue")?;
            let mut displayed = vec![product.as_str()];
            displayed.extend(base.iter().copied().filter(|p| *p != product.as_str()));
            Ok(Box::new(
                ic.catalogue.select(combination.as_deref(), &displayed)?,
            ))
        };
        let cache = Arc::clone(&self.state.platform.cache);
        let request = PortrayalRequest {
            profile: &self.display.profile,
            viewing_time: None,
            settings: Some(settings),
            host: HostServices {
                source: Some(self.state.portrayal_source),
                precomputed_export: self.state.precomputed_export,
                cache: Some(cache.as_ref()),
                interoperability: active_ic.as_deref().map(|catalogue| Interoperability {
                    catalogue,
                    selection: &selection,
                }),
            },
        };
        let mut extensions = Vec::new();
        for layer in self.state.layers.with_role_mut(SceneRole::Grid) {
            if let Some(grid) = layer.portray(&request)? {
                extensions.extend(grid.extensions);
            }
        }
        Ok(extensions)
    }

    /// Cache-store entries a [`Self::prepare`] with the current display reads
    /// for the opened S-101 cells when they are portrayed with Lua: the
    /// per-cell instruction caches
    /// ([`S101Layer::cell_instruction_cache_paths`]); empty without cells
    /// or without a cache. A host whose persistent store is asynchronous
    /// (the web's IndexedDB) loads these entries into the platform cache
    /// store before preparing, and saves the entries the preparation wrote.
    #[cfg(feature = "s101")]
    pub fn cell_cache_entries(&mut self) -> Vec<PathBuf> {
        // The key covers the colour profile `prepare` selects first.
        let paths = self
            .state
            .layers
            .s101()
            .cell_instruction_cache_paths(Some(&self.display.settings), &self.display.profile);
        paths.into_iter().flatten().flatten().collect()
    }

    /// Cache-store entries of precomputed payloads
    /// ([`ferrite_s101::layer::portrayal::PrecomputedCells`]) for the opened S-101 cells
    /// with the current display, i.e. what a [`Self::prepare`] with
    /// [`PortrayalSource::Precomputed`] reads; empty without cells or cache
    /// store. A host keeping payloads in an asynchronous store loads these
    /// before preparing.
    #[cfg(feature = "s101")]
    pub fn precomputed_cell_entries(&self) -> Vec<PathBuf> {
        let common =
            S101Layer::precomputed_common(Some(&self.display.settings), &self.display.profile);
        let identities = self.state.layers.s101().cell_identities();
        let cells = ferrite_s101::layer::portrayal::PrecomputedCells {
            store: self.state.platform.cache.as_ref(),
            common,
            identities: &identities,
            export: false,
        };
        (0..self.state.layers.s101().cells.len())
            .filter_map(|index| {
                cells.key(
                    &self.state.layers.s101().fc,
                    &self.state.layers.s101().pc,
                    index,
                )
            })
            .collect()
    }

    /// Cache-store entry of the S-102 portrayal rule's instructions the
    /// next [`Self::prepare`] reads for the loaded bathymetry with the
    /// current display (precomputed portrayal, or Lua with
    /// `S102Options::reuse_rule_instructions`); None without S-102.
    pub fn s102_cache_entry(&self) -> Option<PathBuf> {
        #[cfg(feature = "s102-pure")]
        return self
            .state
            .layers
            .get::<ferrite_s102::layer::S102Layer>()?
            .instruction_entry(self.state.platform.cache.as_ref(), &self.display.settings);
        #[cfg(not(feature = "s102-pure"))]
        None
    }

    /// Move `scene` to `view` without portraying again, as the viewer's
    /// pan/zoom path does (cell instructions do not depend on the view):
    /// the camera, viewport and pixel ratio of [`Self::prepare`] and the flat
    /// coverage of the new view. `scene` must be the scene this engine
    /// prepared last, with the datasets and display unchanged since. Returns
    /// false (`scene` unchanged) when the view needs other cells resident
    /// (large chart sets, `S101Layer::residency_for_view`); then
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
        #[cfg(feature = "s101")]
        {
            let s101 = self.state.layers.s101();
            if !s101.cells.is_empty()
                && s101.residency_for_view(
                    &scene.coverage,
                    &self.state.render_context,
                    self.state.bounds,
                    true,
                    Some(view.viewport),
                ) != s101.resident_cells
            {
                return Ok(false);
            }
        }
        scene.context.set_viewport(width as f32, height as f32);
        scene.context.scaler = self.state.render_context.scaler.clone();
        #[cfg(feature = "s101")]
        let inventory = Some(scene.coverage.as_ref());
        #[cfg(not(feature = "s101"))]
        let inventory = None;
        prepare_flat_coverage(inventory, &mut scene.context, [width, height], scale_factor)?;
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

    /// Plain-data report of a feature: an overlay object (no cell index)
    /// described by the overlay layers, or a base chart feature by the base
    /// chart layer with the dataset's own catalogue.
    pub fn feature(&mut self, id: FeatureId) -> Option<FeatureReport> {
        match id.cell_index {
            None => self
                .state
                .layers
                .with_role_mut(SceneRole::Overlay)
                .find_map(|layer| layer.describe(id.feature_id))
                .map(|pick| FeatureReport::from_pick(pick, id.feature_id)),
            Some(index) => self
                .state
                .layers
                .with_role_mut(SceneRole::BaseChart)
                .find_map(|layer| layer.describe_in(index, id.feature_id)),
        }
    }
}

/// Everything an open job needs, moved to the worker.
struct OpenTask {
    platform: Platform,
    source: DatasetSource,
    signatures: SignaturePolicy,
    trust_anchors: Arc<Vec<(String, Vec<u8>)>>,
    /// The registered layers' stages with their registry index.
    stages: Vec<(usize, Box<dyn OpenStage>)>,
}

/// The job's signature verification (shared by the stages).
struct Authorizer {
    platform: Platform,
    signatures: SignaturePolicy,
    trust_anchors: Arc<Vec<(String, Vec<u8>)>>,
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
        let authorizer = Authorizer {
            platform: self.platform.clone(),
            signatures: self.signatures,
            trust_anchors: Arc::clone(&self.trust_anchors),
        };
        let authorize = |paths: &[PathBuf]| -> Result<Box<dyn Any>> {
            Ok(Box::new(authorizer.authorize(paths)?))
        };
        let context = OpenContext {
            data,
            executor: self.platform.executor.as_ref(),
            signatures: self.signatures,
            explicit,
            authorize: &authorize,
            user: self.platform.user.as_ref(),
            cancel,
        };
        let mut notes = OpenNotes::default();
        let mut stages = self.stages;
        // Every input goes to the first stage whose products recognise it.
        for path in inputs {
            let mut head: Option<Vec<u8>> = None;
            let mut taken = false;
            for (_, stage) in stages.iter_mut() {
                let mut recognised = stage.sniff(&path, &[]);
                if recognised.is_none() && stage.sniffs_head(&path) {
                    let head = head.get_or_insert_with(|| {
                        data.read_limited(&path, 64 * 1024 - 1).unwrap_or_default()
                    });
                    recognised = stage.sniff(&path, head);
                }
                if recognised.is_some() && stage.accept(&path, &context, &mut notes) {
                    taken = true;
                    break;
                }
            }
            if !taken && explicit {
                notes
                    .notices
                    .push(format!("Not a supported dataset: {}", path.display()));
            }
        }
        ensure!(
            stages
                .iter()
                .map(|(_, stage)| stage.counted_inputs())
                .sum::<usize>()
                <= 4096,
            "Dataset inputs exceed 4096"
        );
        for (_, stage) in stages.iter_mut() {
            stage.after_claims(&mut notes);
        }
        let mut loaded = Vec::with_capacity(stages.len());
        for (index, stage) in stages {
            loaded.push((index, stage.load(&context, &mut notes)?));
        }
        Ok(OpenedDatasets {
            stages: loaded,
            issues: notes.issues,
            notices: notes.notices,
        })
    }
}

impl Authorizer {
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
