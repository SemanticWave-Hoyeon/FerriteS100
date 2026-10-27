//! The S-102 product layer (`docs/ENGINE_DESIGN.md`, "제품 모듈화", step 2):
//! the engine-facing store of opened S-102 files and everything the engine
//! and the viewer used to keep about them in their own state.
//!
//! * [`S102Layer`]: the catalogues and their statuses, the loaded coverages
//!   with their extents, depth policies, unsigned input snapshots, retained
//!   original authentication and cancellation-journal admission, and the
//!   S-102 2.1 trial files opened for inspection. It implements
//!   [`ProductLayer`]; hosts that compose interoperability or choose the
//!   rule source call its inherent methods ([`S102Layer::prepare`],
//!   [`S102Layer::install`]).
//! * [`S102`]: the [`ProductAdapter`] (identity and detection).
//! * The input policies moved from the engine: [`depth_policy`] (host depth
//!   adjustments), [`input_capture`] (stable receiver snapshots),
//!   [`original_inputs`] (publication-owned original authentication),
//!   [`journal_admission`] (cancellation journal gate) and [`trial_worker`]
//!   (isolated S-102 2.1 trial inspection).
//! * [`scene`]: opening captured inputs, file reports and the raster
//!   portrayal, handed to renderers as the [`BATHYMETRY_EXTENSION`]
//!   [`ferrite_product_api::RendererExtension`].
//!
//! The layer depends on the product interface only; platform services (data
//! source, cache store) are passed in.
use crate::trial_inspection::TrialInspection;
use crate::{BathymetryCoverage, ConservativeCoverage, DatumCoverage};
use anyhow::{Context, Result};
use depth_policy::DepthPolicy;
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_kernel::depth_selection::DepthReference;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_product_api::platform::CacheStore;
use ferrite_product_api::{
    DatasetInput, Encoding, FeaturePick, GeoBounds, Opened, PortrayalRequest, ProductAdapter,
    ProductId, ProductInfo, ProductLayer, Recognised, SceneContribution, SceneRole, WorldPoint,
};
use ferrite_render::host_settings::CatalogueStatus;
use ferrite_security::{AuthenticatedSnapshot, UnauthenticatedSnapshot};
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod depth_policy;
pub mod input_capture;
pub mod journal_admission;
pub mod open_job;
pub mod original_inputs;
pub mod scene;
pub mod trial_worker;
pub use scene::*;

/// S-102 product identifier.
pub const PRODUCT: ProductId = ProductId("S-102");

/// The S-102 product adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct S102;

impl S102 {
    /// Whether a path names an HDF5 dataset (S-102 candidate): `.h5` or
    /// `.hdf5`, any case, not an AppleDouble (`._`) file.
    pub fn is_hdf5_name(path: &Path) -> bool {
        let dataset = |extension: &str| {
            !path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"))
                && path
                    .extension()
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        };
        dataset("h5") || dataset("hdf5")
    }
}

/// HDF5 format signature (at offset 0 of files without a user block).
const HDF5_SIGNATURE: &[u8] = b"\x89HDF\r\n\x1a\n";

impl ProductAdapter for S102 {
    fn info(&self) -> ProductInfo {
        ProductInfo {
            id: PRODUCT,
            title: "Bathymetric Surface",
            encoding: Encoding::Hdf5,
            role: SceneRole::Grid,
        }
    }
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        (Self::is_hdf5_name(name) && (head.is_empty() || head.starts_with(HDF5_SIGNATURE)))
            .then_some(Recognised::Dataset)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// One loaded coverage instance: its file, the decoded coverage and the
/// authenticated snapshot it was read from (None when unsigned).
pub type LoadedCoverage = (
    PathBuf,
    BathymetryCoverage,
    Option<Arc<AuthenticatedSnapshot>>,
);

/// Coverage instances taken out of the layer by [`S102Layer::take_file`]
/// with their positions, for [`S102Layer::restore_file`].
pub type TakenCoverages = Vec<(usize, LoadedCoverage)>;

/// The opened S-102 files and catalogues.
#[derive(Default)]
pub struct S102Layer {
    pub fc_status: CatalogueStatus,
    pub pc_status: CatalogueStatus,
    pub fc: Option<Arc<BoundFeatureCatalogue>>,
    pub pc: Option<Arc<BoundPortrayalCatalogue>>,
    /// S-102 2.1 trial files opened for inspection only (never depth).
    pub trials: Vec<(PathBuf, TrialInspection)>,
    /// Coverage instances in load order, grouped by file.
    pub bathymetry: Vec<LoadedCoverage>,
    /// Portrayed extent of each loaded file.
    pub bathymetry_bounds: HashMap<PathBuf, GeoBounds>,
    /// Signature provenance of each file opened through [`Self::install`].
    pub provenance: HashMap<PathBuf, String>,
    pub depth_policies: HashMap<PathBuf, DepthPolicy>,
    /// Private snapshots of files loaded without signature verification.
    pub depth_inputs: HashMap<PathBuf, Arc<UnauthenticatedSnapshot>>,
    pub original_inputs: original_inputs::Registry,
    pub journal_gate: Option<journal_admission::Gate>,
    /// Options of [`ProductLayer::open`] and [`ProductLayer::portray`].
    pub options: S102Options,
    /// Cache store of [`ProductLayer::portray`] (precomputed or reused rule
    /// instructions).
    pub cache: Option<Arc<dyn CacheStore>>,
    /// Layer-local dataset ids ([`ProductLayer`]) of the opened files.
    ids: Vec<(u32, PathBuf)>,
    next_id: u32,
}

impl S102Layer {
    pub fn new(cache: Option<Arc<dyn CacheStore>>) -> Self {
        Self {
            cache,
            ..Default::default()
        }
    }

    /// Number of loaded coverage instances.
    pub fn coverage_count(&self) -> usize {
        self.bathymetry.len()
    }

    /// Whether `path` has loaded coverage instances.
    pub fn is_loaded(&self, path: &Path) -> bool {
        self.bathymetry.iter().any(|(loaded, _, _)| loaded == path)
    }

    /// The loaded files, in load order.
    pub fn files(&self) -> Vec<&Path> {
        self.bathymetry
            .chunk_by(|a, b| a.0 == b.0)
            .map(|group| group[0].0.as_path())
            .collect()
    }

    /// Install a parsed S-102 feature catalogue.
    pub fn set_fc(&mut self, fc: BoundFeatureCatalogue, message: &str) {
        self.fc_status = CatalogueStatus {
            loaded: true,
            product_id: fc.product_id.clone(),
            version: fc.version.clone(),
            path: fc.source_path.display().to_string(),
            item_count: fc.feature_types.len(),
            validation_message: Some(message.into()),
        };
        self.fc = Some(Arc::new(fc));
    }

    /// Record a feature catalogue that failed to load.
    pub fn fc_failed(&mut self, path: &Path, message: String) {
        self.fc_status = CatalogueStatus {
            product_id: PRODUCT.0.into(),
            path: path.display().to_string(),
            validation_message: Some(message),
            ..Default::default()
        };
    }

    /// Install a parsed S-102 portrayal catalogue.
    pub fn set_pc(&mut self, pc: BoundPortrayalCatalogue, message: &str) {
        self.pc_status = Self::pc_status_of(&pc, message);
        self.pc = Some(Arc::new(pc));
    }

    /// Status of a loaded portrayal catalogue.
    pub fn pc_status_of(pc: &BoundPortrayalCatalogue, message: &str) -> CatalogueStatus {
        CatalogueStatus {
            loaded: true,
            product_id: pc.product_id.clone(),
            version: pc.version.clone(),
            path: pc.root_path.display().to_string(),
            item_count: pc.symbols.symbols.len(),
            validation_message: Some(message.into()),
        }
    }

    /// Record a portrayal catalogue that failed to load.
    pub fn pc_failed(&mut self, path: &Path, message: String) {
        self.pc_status = CatalogueStatus {
            product_id: PRODUCT.0.into(),
            path: path.display().to_string(),
            validation_message: Some(message),
            ..Default::default()
        };
    }

    fn assign_id(&mut self, path: &Path) -> u32 {
        if let Some((id, _)) = self.ids.iter().find(|(_, p)| p == path) {
            return *id;
        }
        self.next_id += 1;
        self.ids.push((self.next_id, path.to_path_buf()));
        self.next_id
    }

    /// Publish the decoded coverage instances of one file with its extent,
    /// depth policy and (unsigned) private snapshot.
    pub fn insert_coverages(
        &mut self,
        path: PathBuf,
        bounds: GeoBounds,
        policy: DepthPolicy,
        unsigned: Option<Arc<UnauthenticatedSnapshot>>,
        coverages: Vec<BathymetryCoverage>,
        snapshot: Option<Arc<AuthenticatedSnapshot>>,
    ) {
        self.assign_id(&path);
        self.bathymetry_bounds.insert(path.clone(), bounds);
        if let Some(unsigned) = unsigned {
            self.depth_inputs.insert(path.clone(), unsigned);
        }
        self.depth_policies.insert(path.clone(), policy);
        self.bathymetry.extend(
            coverages
                .into_iter()
                .map(|c| (path.clone(), c, snapshot.clone())),
        );
    }

    /// Add (or replace) an inspected S-102 2.1 trial file.
    pub fn add_trial(&mut self, path: PathBuf, inspection: TrialInspection) {
        self.assign_id(&path);
        self.trials.retain(|(p, _)| *p != path);
        self.trials.push((path, inspection));
    }

    /// Remove an inspected trial file; false when it is not open.
    pub fn remove_trial(&mut self, path: &Path) -> bool {
        let before = self.trials.len();
        self.trials.retain(|(p, _)| p != path);
        let removed = self.trials.len() < before;
        if removed {
            self.ids.retain(|(_, p)| p != path);
        }
        removed
    }

    /// Take the coverage instances of `path` out of the layer (its other
    /// records stay until [`Self::forget_file`]); [`Self::restore_file`]
    /// puts them back.
    pub fn take_file(&mut self, path: &Path) -> TakenCoverages {
        let mut removed = Vec::new();
        let retained = std::mem::take(&mut self.bathymetry)
            .into_iter()
            .enumerate()
            .filter_map(|(index, value)| {
                if value.0 == path {
                    removed.push((index, value));
                    None
                } else {
                    Some(value)
                }
            })
            .collect();
        self.bathymetry = retained;
        removed
    }

    /// Undo [`Self::take_file`].
    pub fn restore_file(&mut self, removed: TakenCoverages) {
        for (index, value) in removed {
            self.bathymetry.insert(index, value);
        }
    }

    /// Drop the extent, depth policy, private snapshot and retained original
    /// authentication of a file whose instances were taken out.
    pub fn forget_file(&mut self, path: &Path) {
        self.bathymetry_bounds.remove(path);
        self.depth_policies.remove(path);
        self.depth_inputs.remove(path);
        self.original_inputs.remove(path);
        self.ids.retain(|(_, p)| p != path);
    }

    /// Unload every coverage (inspected trials stay).
    pub fn clear_coverages(&mut self) {
        self.bathymetry.clear();
        self.bathymetry_bounds.clear();
        self.depth_policies.clear();
        self.depth_inputs.clear();
        self.original_inputs.clear();
        let trials: Vec<&PathBuf> = self.trials.iter().map(|(p, _)| p).collect();
        self.ids.retain(|(_, p)| trials.contains(&p));
    }

    /// With an enforced cancellation journal: every active instance must
    /// still have its authenticated parser owner, and every loaded file must
    /// pass admission again. A failure blocks the journal for recovery.
    pub fn recheck_journal_admission(&mut self) -> Result<()> {
        if let Some(gate) = &mut self.journal_gate {
            let mut seen = std::collections::BTreeSet::new();
            for (path, _, snapshot) in &self.bathymetry {
                let record = self.original_inputs.get(path);
                if !record
                    .zip(snapshot.as_ref())
                    .is_some_and(|(r, s)| Arc::ptr_eq(&r.snapshot, s))
                {
                    gate.block_for_recovery();
                    anyhow::bail!("Enforced S102 history requires every active instance's authenticated parser owner");
                }
                if seen.insert(path) {
                    if let Err(error) = gate.check_input(record, snapshot.as_ref(), path) {
                        // Existing scene authority failed, unlike a refused new import.
                        // Rolling back a load checkpoint must not resume that scene.
                        gate.block_for_recovery();
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    /// Report of the loaded depth at a position (the viewer's depth pick):
    /// the nearest grid node of the last loaded file covering it, with its
    /// datum adjustment, metadata and quality; "No data" text when a file
    /// covers the position without a sample. `wrapping`: also look at the
    /// position one revolution east and west.
    pub fn inspect(&self, lon: f64, lat: f64, wrapping: bool) -> Result<Option<String>> {
        let mut missing = None;
        for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0).rev() {
            let path = &group[0].0;
            let policy = self
                .depth_policies
                .get(path)
                .context("Missing loaded depth policy")?;
            let mosaic = ConservativeCoverage::new(
                group
                    .iter()
                    .map(|(_, c, _)| DatumCoverage {
                        coverage: c,
                        reference: DepthReference(c.vertical_datum as u64),
                    })
                    .collect(),
                DepthReference(policy.target as u64),
                policy,
            )?;
            for shift in &([0., -360., 360.])[..if wrapping { 3 } else { 1 }] {
                let Some(selected) = mosaic.query_nearest(lon - shift, lat)? else {
                    if mosaic.covers_position(lon - shift, lat)? {
                        missing = Some(format!("S-102 depth: No data\n{}", path.display()));
                    }
                    continue;
                };
                let sample = selected.candidate;
                let c = &group[sample.source.instance].1;
                let uncertainty = sample
                    .uncertainty
                    .map(|v| format!("{v:.2} m"))
                    .unwrap_or_else(|| "No data".into());
                let coordinate_uncertainty = |value: f32| {
                    if value == -1. {
                        "Unknown".to_owned()
                    } else {
                        format!("{value:.2} m")
                    }
                };
                let mut info = format!(
                    "S-102 depth: {:.2} m\nCell depth uncertainty: {uncertainty}\nContainer horizontal coordinate uncertainty: {}\nContainer vertical coordinate uncertainty: {}\nVertical CRS: {} / datum code: {}\n{}",
                    selected.adjusted_depth,
                    coordinate_uncertainty(c.horizontal_position_uncertainty),
                    coordinate_uncertainty(c.vertical_position_uncertainty),
                    if c.vertical_cs_reinterpreted_as_depth {
                        format!("{} (declared; read as depth-down, compatibility mode)", c.vertical_crs)
                    } else {
                        c.vertical_crs.to_string()
                    },
                    c.vertical_datum,
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                info.push_str(&format!(
                    "\nProduct issue date: {}\nProduct issue time: {}\nEncoded axes: {} / {}",
                    c.issue.date,
                    c.issue.time.as_deref().unwrap_or("Not provided"),
                    c.axes.names[0],
                    c.axes.names[1]
                ));
                info.push_str(&format!(
                    "\nDeclared S-102 features: {}",
                    c.feature_metadata.declared_features.join(", ")
                ));
                for diagnostic in &c.feature_metadata.diagnostics {
                    info.push_str(&format!("\nFeature metadata diagnostic: {diagnostic:?}"));
                }
                for definition in c
                    .feature_metadata
                    .bathymetry
                    .iter()
                    .chain(c.feature_metadata.quality.iter())
                {
                    info.push_str(&format!(
                        "\nDefinition {}: unit '{}', {}, fill {}, {} [{}, {}]",
                        definition.code,
                        definition.unit,
                        definition.datatype,
                        definition.fill_value,
                        definition.closure,
                        definition.lower,
                        definition.upper
                    ));
                }
                if !c.axes.matches_generic_array_major_order()
                    || c.quality
                        .as_ref()
                        .is_some_and(|q| !q.axes.matches_generic_array_major_order())
                {
                    info.push_str("\nAxis metadata diagnostic: declared axis order differs from generic S-100 array major order. Original S-102 east-then-north samples are retained; no automatic transpose.");
                }
                info.push_str(&format!(
                    "\nRoot geographic extent assessment: grid {:?}, domain {:?}",
                    c.root_enclosure.full_grid, c.root_enclosure.declared_domain
                ));
                info.push_str(&format!("\nTarget datum code: {}\nRaw source depth: {:.8} m\nPositive-down correction: {:+.8} m\nCorrection source: {}\nHost-supplied correction; not dataset-authenticated transformation evidence.\nSource instance: {}",
                    policy.target,sample.raw_depth.unwrap(),selected.adjustment.correction_metres,policy.source(selected.adjustment.provenance),c.instance_name));
                info.push_str(&format!("\nQuery longitude / latitude: {lon:.8} / {lat:.8}\nSample longitude / latitude: {:.8} / {:.8}\nGrid column / row (zero-based): {} / {}\nNearest grid node; no spatial interpolation. Shared boundaries use the least adjusted depth.",sample.x,sample.y,sample.source.column,sample.source.row));
                if *shift != 0. {
                    info.push_str(&format!("\nDisplay longitude copy: {shift:+.0}°; sample coordinates are from the source."));
                }
                let metadata_issues = group
                    .iter()
                    .filter_map(|(_, source, _)| {
                        let issues = source.observed_range_violations();
                        issues.iter().any(|v| *v).then(|| {
                            format!(
                                "{} ({})",
                                source.instance_name,
                                if issues[0] && issues[1] {
                                    "depth and uncertainty"
                                } else if issues[0] {
                                    "depth"
                                } else {
                                    "uncertainty"
                                }
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                if !metadata_issues.is_empty() {
                    info.push_str(&format!("\nDataset metadata warning: {} values outside declared extrema. Original samples retained.",metadata_issues.join(", ")));
                }
                let domain_issues = group
                    .iter()
                    .filter(|(_, source, _)| source.observed_depth_centroids_outside_domain())
                    .map(|(_, source, _)| source.instance_name.as_str())
                    .collect::<Vec<_>>();
                if !domain_issues.is_empty() {
                    info.push_str(&format!("\nValidity diagnostic: {} populated depth sample positions outside their own domain. Raw samples retained; this is not an entire-cell intersection test.",domain_issues.join(", ")));
                }
                let quality = match &c.quality {
                    Some(q) => {
                        let mut description=match q.sample_nearest(lon-shift,lat)? {
                            Some(record)=>record.description(),None=>"Quality: no record at this location (fill ID 0 or outside its own validity domain)".into(),
                        };
                        if q.observed_id_centroids_outside_domain() {
                            description.push_str("\nQuality validity diagnostic: nonzero original ID sample positions outside its own domain; IDs retained.");
                        }
                        description
                    }
                    None => "Quality coverage: not supplied".into(),
                };
                info.push('\n');
                info.push_str(&quality);
                return Ok(Some(info));
            }
        }
        Ok(missing)
    }
}

impl ProductLayer for S102Layer {
    fn products(&self) -> Vec<ProductId> {
        vec![PRODUCT]
    }
    fn portrayal_catalogue(&self) -> Option<&ferrite_portrayal_catalog::BoundPortrayalCatalogue> {
        self.pc.as_deref()
    }
    fn is_empty(&self) -> bool {
        self.bathymetry.is_empty() && self.trials.is_empty()
    }
    /// Decode a file from its bytes (the authenticated snapshot when the
    /// policy requires signatures; an unauthenticated input is refused then;
    /// no signature snapshot is retained) and install it as
    /// [`Self::install`] does.
    fn open(&mut self, input: DatasetInput<'_>) -> Result<Opened> {
        input.check_policy()?;
        let path = input.name.to_path_buf();
        if self.is_loaded(&path) || self.trials.iter().any(|(p, _)| *p == path) {
            return Ok(Opened::AlreadyOpen);
        }
        let mut budget = MAX_INPUT_BYTES;
        let capture = input_capture::CapturedInput::from_bytes(input.bytes, &mut budget)?;
        let decoded = open_captured(&path, capture, self.options.depth_adjustments.as_deref())?;
        let installed = self.install(OpenedS102 {
            inputs: vec![(path.clone(), decoded, input.provenance.label())],
        });
        if let Some((_, message)) = installed.errors.into_iter().next() {
            anyhow::bail!(message);
        }
        Ok(Opened::New(self.assign_id(&path)))
    }
    fn remove(&mut self, id: u32) -> bool {
        let Some(path) = self
            .ids
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, p)| p.clone())
        else {
            return false;
        };
        if self.remove_trial(&path) {
            self.provenance.remove(&path);
            return true;
        }
        let removed = self.take_file(&path);
        self.forget_file(&path);
        self.provenance.remove(&path);
        !removed.is_empty()
    }
    /// S-102 files have no visibility of their own (always shown).
    fn set_visible(&mut self, _id: u32, _visible: bool) -> bool {
        false
    }
    /// The raster tiles as a [`BATHYMETRY_EXTENSION`] (no drawing
    /// instructions) with the layer's options and the request's host
    /// services: the rule evaluated with Lua or read from the cache store
    /// (the host's rule source, written there on export), the host's cache
    /// store (else the layer's) and interoperability composition with the
    /// host's selection for S-102; None without bathymetry.
    fn portray(&mut self, request: &PortrayalRequest<'_>) -> Result<Option<SceneContribution>> {
        let defaults = ferrite_render::host_settings::SettingsState::default();
        let layer_cache = self.cache.clone();
        let none = NoStore;
        let cache: &dyn CacheStore = match request.host.cache {
            Some(cache) => cache,
            None => layer_cache.as_deref().unwrap_or(&none),
        };
        let lua = match request.host.source {
            Some(source) => source == ferrite_product_api::PortrayalSource::Lua,
            None => cfg!(feature = "lua-native"),
        };
        let host_selection = |catalogue: &ferrite_interoperability::AuthenticatedCatalogue| {
            let _ = catalogue;
            let interoperability = request
                .host
                .interoperability
                .context("No interoperability host")?;
            (interoperability.selection)(PRODUCT)?
                .downcast::<ferrite_interoperability::Selection>()
                .map(|selection| *selection)
                .map_err(|_| anyhow::anyhow!("Host selection is not an S-98 selection"))
        };
        let interoperability = match request.host.interoperability {
            Some(host) => Some(Interoperability {
                catalogue: host
                    .catalogue
                    .downcast_ref::<ferrite_interoperability::AuthenticatedCatalogue>()
                    .context("Host catalogue is not an S-98 interoperability catalogue")?,
                selection: &host_selection,
            }),
            None => None,
        };
        let prepared = self.prepare(&S102Portrayal {
            profile: request.profile,
            settings: request.settings.unwrap_or(&defaults),
            tile_edge: self.options.tile_edge,
            reuse_rule_instructions: self.options.reuse_rule_instructions,
            lua,
            export: request.host.precomputed_export,
            cache,
            interoperability,
        })?;
        Ok(prepared.map(|scene| SceneContribution {
            revision: 0,
            instructions: Arc::new(Vec::new()),
            extensions: vec![scene.into_extension()],
        }))
    }
    fn coverage(&self) -> Option<GeoBounds> {
        let mut bounds = self.bathymetry_bounds.values().copied();
        let first = bounds.next()?;
        Some(bounds.fold(first, |a, b| {
            GeoBounds::new(
                a.min_x.min(b.min_x),
                a.min_y.min(b.min_y),
                a.max_x.max(b.max_x),
                a.max_y.max(b.max_y),
            )
        }))
    }
    /// Grid values are reported by position ([`Self::inspect`]), not by
    /// drawing-instruction feature key.
    fn describe(&mut self, _key: i64) -> Option<FeaturePick> {
        None
    }
    fn highlight(&mut self, _key: i64) -> Vec<Vec<WorldPoint>> {
        Vec::new()
    }
    fn open_stage(&self) -> Option<Box<dyn ferrite_product_api::open::OpenStage>> {
        Some(Box::new(open_job::S102Stage::new(self)))
    }
    /// The depth coverages (S-98 Level 2 inputs).
    fn numeric_coverages(&self) -> Vec<&dyn ferrite_kernel::NumericCoverageSource> {
        self.bathymetry
            .iter()
            .map(|(_, c, _)| c as &dyn ferrite_kernel::NumericCoverageSource)
            .collect()
    }
    fn install(
        &mut self,
        opened: Box<dyn Any + Send>,
        context: &ferrite_product_api::open::InstallContext<'_>,
        report: &mut ferrite_product_api::report::InstallReport,
    ) -> Result<()> {
        open_job::install(self, opened, context, report)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// No cache store (every read misses, writes are dropped).
struct NoStore;
impl CacheStore for NoStore {
    fn root(&self) -> Option<PathBuf> {
        None
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("No cache store: {}", path.display()),
        ))
    }
    fn modified(&self, _path: &Path) -> Option<std::time::SystemTime> {
        None
    }
    fn write(&self, _path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn mark_used(&self, _path: &Path) {}
    fn prune(&self) {}
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Inputs of the moved engine tests without the engine's platform.
    use ferrite_product_api::platform::{DataSource, EntryKind, SourceEntry};
    use ferrite_security::AuthorizedDatasets;
    use std::io;
    use std::path::{Path, PathBuf};

    /// Signature verification off for local files (canonical keys).
    pub fn unchecked(paths: &[PathBuf]) -> AuthorizedDatasets {
        AuthorizedDatasets::unchecked(paths.iter().map(|p| p.canonicalize().unwrap()))
    }

    /// One file in memory.
    pub struct MemorySource {
        path: PathBuf,
        bytes: Vec<u8>,
    }
    impl MemorySource {
        pub fn new(path: &Path, bytes: Vec<u8>) -> Self {
            Self {
                path: path.to_path_buf(),
                bytes,
            }
        }
        fn file(&self, path: &Path) -> io::Result<&[u8]> {
            if path == self.path {
                Ok(&self.bytes)
            } else {
                Err(io::ErrorKind::NotFound.into())
            }
        }
    }
    impl DataSource for MemorySource {
        fn kind(&self, path: &Path) -> io::Result<EntryKind> {
            self.file(path).map(|_| EntryKind::File)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.file(path).is_ok()
        }
        fn is_dir(&self, _path: &Path) -> bool {
            false
        }
        fn len(&self, path: &Path) -> io::Result<u64> {
            self.file(path).map(|b| b.len() as u64)
        }
        fn modified(&self, _path: &Path) -> Option<std::time::SystemTime> {
            None
        }
        fn read_dir(&self, _path: &Path) -> io::Result<Vec<SourceEntry>> {
            Ok(Vec::new())
        }
        fn walk<'a>(
            &'a self,
            _root: &Path,
        ) -> Box<dyn Iterator<Item = io::Result<SourceEntry>> + 'a> {
            Box::new(std::iter::empty())
        }
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.file(path).map(<[u8]>::to_vec)
        }
        fn read_limited(&self, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
            self.file(path)
                .map(|b| b[..b.len().min(limit as usize + 1)].to_vec())
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.file(path).map(|_| path.to_path_buf())
        }
    }
}
