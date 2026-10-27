//! The S-101 layer state: the loaded cells with their identities, chains,
//! discovery metadata and catalogue owners, the bound catalogue pair,
//! coverage, view residency and re-materialization of released cells, and
//! the renderer-independent part of cell portrayal (validation, Lua rules or
//! precomputed payloads, per-cell caches). Moved from the engine's
//! `EngineState` (step 4); the engine composes interoperability and the
//! published scene around it.
use crate::coverage_projection::GeographicCoverageInventory;
use crate::layer::catalogue::hash_bound_catalogue_inputs;
use crate::layer::cells::{canonical_cell_digest, CANONICAL_CELL_PARTS};
use crate::layer::portrayal::{
    try_lua_portrayal, try_lua_portrayal_for_cells, validated_lua_context, AreaTriangulator,
    CellCaches, CellPortrayalSelection, PortrayalChangeRequest, PortrayalRun, PortrayalSource,
    PrecomputedCells, RuleSource,
};
use crate::layer::{
    catalogue_selection as s101_catalogue_selection, decoded_cache as decoded_chart_cache,
    lifecycle_metadata as s101_lifecycle_metadata, primary_viewing_layers,
    source_identity as cell_source_identity, update_plan as s101_update_plan, CellHydrationSource,
    ChartLoadResult,
};
use anyhow::{Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_product_api::platform::Services;
use ferrite_render::host_settings::{CatalogueStatus, SettingsState};
use ferrite_render::{DrawingInstruction, GeoBounds, RenderContext};
use ferrite_s100_core::S101Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// Physical size and scale factor of the view a portrayal is prepared for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportPixels {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

/// How a loaded cell is re-materialized after its records were released.
pub struct CellHydration {
    pub source: CellHydrationSource,
    /// Statistics of a released cell (None while its records are resident).
    pub released: Option<ferrite_s100_core::CellStatistics>,
    /// `FERRITE_HYDRATION_VERIFY=1`: canonical digest taken before release,
    /// compared after re-materialization.
    pub verify: Option<Vec<[u8; 32]>>,
}

/// The loaded logical datasets (dataset key → identification, source
/// identity and lifecycle metadata) explicit cancellations are planned
/// against.
pub type LoadedDatasets = s101_update_plan::LoadedDatasets;

/// Duplicate and replacement tracking of one batch of loaded cells
/// ([`S101Layer::begin_batch`], [`S101Layer::insert_loaded`]).
pub struct CellBatch {
    /// Source identities seen: loaded before the batch or inserted by it.
    loaded: HashSet<[u8; 32]>,
    /// Index of each loaded logical dataset.
    keys: BTreeMap<(String, String), usize>,
}

/// A cell taken out of the layer by [`S101Layer::take_cell`].
pub struct TakenCell {
    pub cell: S101Cell,
    pub identity: ferrite_s100_core::CellSourceIdentity,
    pub chain_paths: Vec<PathBuf>,
}

/// Outcome of [`S101Layer::insert_loaded`].
#[derive(Debug, PartialEq, Eq)]
pub enum Inserted {
    /// The same source identity is already open (or was in this batch).
    AlreadyOpen(PathBuf),
    /// Appended as the last cell; `replaced`: the index of the older edition
    /// or update of the same dataset removed first.
    Added { replaced: Option<usize> },
}

/// The S-101 dataset store of a chart state.
pub struct S101Layer {
    /// Feature Catalogue reference for attribute lookup
    pub fc: Arc<BoundFeatureCatalogue>,
    /// Portrayal Catalogue reference
    pub pc: Arc<BoundPortrayalCatalogue>,
    pub catalogue_inventory: PathBuf,
    pub catalogue_registry: s101_catalogue_selection::DatasetCatalogueRegistry,
    /// Feature Catalogue status (for UI display)
    pub fc_status: CatalogueStatus,
    /// Portrayal Catalogue status (for UI display)
    pub pc_status: CatalogueStatus,
    /// All loaded S101 cells
    pub cells: Vec<S101Cell>,
    pub loaded_source_identities: cell_source_identity::LoadedSourceIdentities,
    pub loaded_chain_paths: Vec<Vec<PathBuf>>,
    pub loaded_discovery:
        BTreeMap<(String, String), Option<s101_lifecycle_metadata::MetadataEvidence>>,
    pub cancellation_history_path: PathBuf,
    pub coverage_inventory: Option<Arc<GeographicCoverageInventory>>,
    /// Coverage inventory keyed by the loaded cells' identities, reused by
    /// re-portrayals (residency, palette) instead of re-extracting every cell.
    pub coverage_memo: Option<(u64, Arc<GeographicCoverageInventory>)>,
    /// Re-materialization sources by loaded source identity (SHA-256).
    pub cell_hydration: HashMap<[u8; 32], CellHydration>,
    /// Set by each publication; non-resident records are released once the
    /// app is idle, after the publishing operation has finished reading cells.
    pub cell_release_pending: bool,
    /// Cells whose instructions are resident (None: all, residency inactive).
    pub resident_cells: Option<Vec<bool>>,
    /// Camera bits of the last residency check (checked again once settled).
    pub residency_camera: Option<[u64; 12]>,
    pub decoded_chart_cache: decoded_chart_cache::SharedDecodedChartCache,
    /// Provenance label of each loaded cell (by cell index) for hosts that
    /// open through [`ferrite_product_api::ProductLayer::install`].
    pub provenance: Vec<String>,
    /// Residency estimate threshold of this layer; None: the process-wide
    /// [`Self::residency_threshold`] (hosts without environment variables,
    /// such as the web, set it here).
    pub residency_threshold_override: Option<usize>,
    /// Services the layer reads inputs, caches and parallel work through
    /// (the host platform's, taken at creation).
    pub services: Services,
    /// Layer-local dataset ids ([`ferrite_product_api::ProductLayer`]) by
    /// source identity, for cells opened through the trait.
    pub(crate) ids: Vec<(u32, [u8; 32])>,
    pub(crate) next_id: u32,
}

impl S101Layer {
    /// An empty layer with the bound catalogue pair; the cancellation
    /// history location is the host's (empty: none).
    pub fn new(
        services: Services,
        fc: Arc<BoundFeatureCatalogue>,
        pc: Arc<BoundPortrayalCatalogue>,
        fc_status: CatalogueStatus,
        pc_status: CatalogueStatus,
        catalogue_inventory: PathBuf,
        cancellation_history_path: PathBuf,
    ) -> Self {
        Self {
            fc,
            pc,
            catalogue_inventory,
            catalogue_registry: Default::default(),
            fc_status,
            pc_status,
            cells: Vec::new(),
            loaded_source_identities: Default::default(),
            loaded_chain_paths: Vec::new(),
            loaded_discovery: Default::default(),
            cancellation_history_path,
            coverage_inventory: None,
            coverage_memo: None,
            cell_hydration: HashMap::new(),
            cell_release_pending: false,
            resident_cells: None,
            residency_camera: None,
            decoded_chart_cache: Arc::new(std::sync::Mutex::new(
                decoded_chart_cache::DecodedChartCache::new(),
            )),
            provenance: Vec::new(),
            residency_threshold_override: None,
            services,
            ids: Vec::new(),
            next_id: 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Portrayal inputs of precomputed payload keys other than the
    /// catalogues and the cells (see [`PrecomputedCells`]).
    pub fn precomputed_common(settings: Option<&SettingsState>, profile: &str) -> [u8; 32] {
        use sha2::Digest;
        use std::hash::Hash;
        let mut hasher = crate::layer::portrayal::PortableHasher(sha2::Sha256::new());
        crate::layer::portrayal_cache::CACHE_SCHEMA_VERSION.hash(&mut hasher);
        if let Some(settings) = settings {
            let mut pc_settings = settings.clone();
            pc_settings.interoperability_enabled = true; // IC composition is outside the cache.
            primary_viewing_layers::hash_identity(&pc_settings, &mut hasher);
            format!("{:?}", pc_settings).hash(&mut hasher);
        }
        profile.hash(&mut hasher);
        hasher.0.finalize().into()
    }

    /// Source identities of the loaded cells, by cell index.
    pub fn cell_identities(&self) -> Vec<Option<[u8; 32]>> {
        (0..self.cells.len())
            .map(|index| self.cell_identity_sha(index))
            .collect()
    }

    /// How a portrayal with `common` inputs and the host's rule source runs
    /// on this layer's platform services.
    pub fn portrayal_run<'a>(
        &'a self,
        common: [u8; 32],
        identities: &'a [Option<[u8; 32]>],
        rules: RuleSource,
    ) -> PortrayalRun<'a> {
        let precomputed =
            (rules.source == PortrayalSource::Precomputed || rules.export).then(|| {
                PrecomputedCells {
                    store: self.services.cache.as_ref(),
                    common,
                    identities,
                    export: rules.export,
                }
            });
        PortrayalRun {
            executor: self.services.executor.as_ref(),
            source: rules.source,
            precomputed,
        }
    }

    pub fn catalogue_dataset_key(id: &ferrite_s100_core::DatasetIdentification) -> Result<String> {
        Ok(serde_json::to_string(&s101_update_plan::dataset_key(id)?)?)
    }
    pub fn catalogue_owner_for_cell(
        &self,
        index: usize,
    ) -> Option<&Arc<s101_catalogue_selection::DatasetCatalogueOwner>> {
        let cell = self.cells.get(index)?;
        let key = Self::catalogue_dataset_key(&cell.dsid).ok()?;
        let identity = self.loaded_source_identities.get(index)?;
        if self.catalogue_registry.binding_identity(&key)?.0 != *identity.sha256() {
            return None;
        }
        self.catalogue_registry.owner_for(&key)
    }
    pub fn prepare_catalogue_registry(
        &self,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        retain_existing: bool,
    ) -> Result<s101_catalogue_selection::DatasetCatalogueRegistry> {
        use s101_catalogue_selection::{
            DatasetCatalogueOwner, DatasetCatalogueRegistry, DatasetCatalogueRequest,
        };
        let fallback = Arc::new(DatasetCatalogueOwner::new(Arc::clone(fc), Arc::clone(pc))?);
        let requests = self
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                Ok(DatasetCatalogueRequest {
                    dataset_key: Self::catalogue_dataset_key(&cell.dsid)?,
                    source_identity: *self
                        .loaded_source_identities
                        .get(index)
                        .context("Missing parsed cell identity for catalogue owner")?
                        .sha256(),
                    identification: cell.dsid.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let base = if retain_existing {
            self.catalogue_registry.clone()
        } else {
            DatasetCatalogueRegistry::default()
        };
        let mut next = base.stage_in(
            self.services.data.as_ref(),
            &requests,
            fallback,
            &self.catalogue_inventory,
        )?;
        next.retain_keys(&requests.iter().map(|r| r.dataset_key.clone()).collect());
        for request in &requests {
            let binding = next
                .binding_identity(&request.dataset_key)
                .context("Missing staged catalogue owner")?;
            anyhow::ensure!(
                binding.0 == request.source_identity,
                "Catalogue owner belongs to a different parsed dataset"
            );
            let owner = next
                .owner_for(&request.dataset_key)
                .context("Missing staged catalogue pair")?;
            crate::validate_dataset_catalogues(
                &request.identification,
                owner.fc(),
                &owner.pc().product_id,
                &owner.pc().version,
            )?;
        }
        Ok(next)
    }
    pub fn reconcile_catalogue_registry(
        &self,
        registry: s101_catalogue_selection::DatasetCatalogueRegistry,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
    ) -> Result<s101_catalogue_selection::DatasetCatalogueRegistry> {
        let requests = self
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                Ok(s101_catalogue_selection::DatasetCatalogueRequest {
                    dataset_key: Self::catalogue_dataset_key(&cell.dsid)?,
                    source_identity: *self
                        .loaded_source_identities
                        .get(index)
                        .context("Missing actual parsed source identity")?
                        .sha256(),
                    identification: cell.dsid.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let fallback = Arc::new(s101_catalogue_selection::DatasetCatalogueOwner::new(
            Arc::clone(fc),
            Arc::clone(pc),
        )?);
        let mut next = registry.stage_in(
            self.services.data.as_ref(),
            &requests,
            fallback,
            &self.catalogue_inventory,
        )?;
        next.retain_keys(&requests.iter().map(|r| r.dataset_key.clone()).collect());
        for request in &requests {
            anyhow::ensure!(
                next.binding_identity(&request.dataset_key)
                    .is_some_and(|b| b.0 == request.source_identity),
                "Staged catalogue binding does not match published parsed source"
            );
        }
        Ok(next)
    }
    pub fn requires_cell_resources(
        &self,
        registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        fc: &BoundFeatureCatalogue,
        pc: &BoundPortrayalCatalogue,
    ) -> Result<bool> {
        for cell in &self.cells {
            let owner = registry
                .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                .context("Missing staged catalogue owner")?;
            if owner.fc().source_digest() != fc.source_digest()
                || owner.pc().source_digest() != pc.source_digest()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn portray_cells_with_owners(
        &self,
        registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        next: &mut RenderContext,
        request: &PortrayalChangeRequest,
        triangulate: AreaTriangulator,
        rules: RuleSource,
    ) -> Result<()> {
        let mut owners = std::collections::BTreeMap::new();
        for cell in &self.cells {
            let owner = registry
                .owner_for(&Self::catalogue_dataset_key(&cell.dsid)?)
                .context("Missing Lua cell catalogue owner")?;
            owners
                .entry((*owner.fc().source_digest(), *owner.pc().source_digest()))
                .or_insert_with(|| Arc::clone(owner));
        }
        let mut ordered = std::collections::BTreeMap::<u32, Vec<DrawingInstruction>>::new();
        let common = Self::precomputed_common(Some(&request.settings), &request.profile);
        let identities = self.cell_identities();
        for (digests, owner) in owners {
            let resident = self.resident_cells.as_deref();
            let indexed = self.cells.iter().enumerate().filter(|(index, cell)| {
                resident.is_none_or(|r| r.get(*index).copied().unwrap_or(true))
                    && Self::catalogue_dataset_key(&cell.dsid)
                        .ok()
                        .and_then(|key| registry.binding_identity(&key))
                        .is_some_and(|b| (b.1, b.2) == digests)
            });
            let mut staged = next.empty_for_rebuild();
            try_lua_portrayal_for_cells(
                indexed,
                owner.fc(),
                owner.pc(),
                &mut staged,
                &request.profile,
                Some(&request.settings),
                None,
                triangulate,
                self.portrayal_run(common, &identities, rules),
            )?;
            for instruction in staged.raw_instructions() {
                let index = instruction
                    .cell_index()
                    .context("Lua instruction lacks original source cell")?;
                ordered.entry(index).or_default().push(instruction.clone());
            }
        }
        next.set_instructions_from_cache(ordered.into_values().flatten().collect());
        Ok(())
    }
    /// Regenerate drawing instructions from loaded cells
    /// Compute instruction cache file path for the current chart set.
    ///
    /// The cache key is built from the canonical (absolute, symlink-resolved)
    /// path of each chart so the same chart loaded via different working
    /// directories — e.g. `cargo run` vs running the exe from `target/release/`
    /// — yields the same cache hash. Without canonicalization, every distinct
    /// CWD spawned a parallel cache file for identical chart content.
    /// Everything except the charts that determines portrayal output.
    pub fn hash_portrayal_cache_inputs(
        &self,
        settings: Option<&SettingsState>,
        profile: &str,
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Option<()> {
        use std::hash::Hash;
        // Catalogue rules and context parameters determine the generated output.
        // Hash their bytes and current settings rather than only chart paths.
        crate::layer::portrayal_cache::CACHE_SCHEMA_VERSION.hash(&mut *hasher);
        // Fail closed if the linked runtime cannot identify itself. A major/minor
        // label alone cannot invalidate output from another Lua patch or build.
        // Without a linked runtime there is no Lua output to cache.
        #[cfg(feature = "lua-native")]
        ferrite_lua::runtime_identity()?.hash(&mut *hasher);
        #[cfg(not(feature = "lua-native"))]
        None::<()>?;
        hash_bound_catalogue_inputs(&self.fc, &self.pc, &mut *hasher);
        if let Some(settings) = settings {
            let mut pc_settings = settings.clone();
            pc_settings.interoperability_enabled = true; // IC composition is outside the ordinary PC cache.
            primary_viewing_layers::hash_identity(&pc_settings, &mut *hasher);
            format!("{:?}", pc_settings).hash(&mut *hasher);
        }
        let mut group_identity = self
            .pc
            .viewing_groups
            .identifiers
            .iter()
            .collect::<Vec<_>>();
        group_identity.sort_unstable_by(|a, b| a.0.cmp(b.0));
        group_identity.hash(&mut *hasher);
        profile.hash(&mut *hasher);
        Some(())
    }
    /// Per-cell portrayal cache files, by global cell index. A cell's output
    /// depends only on its own bytes plus the shared portrayal inputs, so a
    /// changed scene reuses every unchanged cell instead of re-running Lua.
    pub fn cell_instruction_cache_paths(
        &self,
        settings: Option<&SettingsState>,
        profile: &str,
    ) -> Option<Vec<Option<PathBuf>>> {
        if self.cells.is_empty() || std::env::var_os("FERRITE_NO_CACHE").is_some() {
            return None;
        }
        let cache_dir = self.services.cache.root()?;
        use std::hash::{Hash, Hasher};
        let mut common = std::collections::hash_map::DefaultHasher::new();
        "cell-portrayal-v1".hash(&mut common);
        self.hash_portrayal_cache_inputs(settings, profile, &mut common)?;
        Some(
            self.cells
                .iter()
                .enumerate()
                .map(|(cell_index, cell)| {
                    let mut hasher = common.clone();
                    let canonical = self
                        .services
                        .data
                        .canonicalize(&cell.file_path)
                        .unwrap_or_else(|_| cell.file_path.clone());
                    #[cfg(windows)]
                    let key = canonical.to_string_lossy().to_lowercase();
                    #[cfg(not(windows))]
                    let key = canonical.to_string_lossy().into_owned();
                    key.hash(&mut hasher);
                    self.loaded_source_identities
                        .hash_for_cell(cell_index, &mut hasher)?;
                    Some(cache_dir.join(format!(".ferrite_cell_{:016x}.bin", hasher.finish())))
                })
                .collect(),
        )
    }
    pub fn cell_identity_sha(&self, index: usize) -> Option<[u8; 32]> {
        self.loaded_source_identities
            .get(index)
            .map(|identity| *identity.sha256())
    }
    pub fn cell_is_released(&self, index: usize) -> bool {
        self.cell_identity_sha(index)
            .and_then(|key| self.cell_hydration.get(&key))
            .is_some_and(|h| h.released.is_some())
    }
    /// Feature count, also for a released cell.
    pub fn cell_feature_count(&self, index: usize) -> usize {
        self.cell_identity_sha(index)
            .and_then(|key| self.cell_hydration.get(&key))
            .and_then(|h| h.released.as_ref())
            .map_or_else(
                || self.cells.get(index).map_or(0, |c| c.features.len()),
                |stats| stats.features,
            )
    }
    /// Release the records of cells that are not resident.
    pub fn release_nonresident_cells(&mut self) {
        let Some(resident) = self.resident_cells.clone() else {
            return;
        };
        let mut released = 0;
        for (index, keep) in resident.iter().enumerate() {
            if *keep || self.cell_is_released(index) {
                continue;
            }
            let Some(key) = self.cell_identity_sha(index) else {
                continue;
            };
            if !self.cell_hydration.contains_key(&key) {
                continue;
            }
            let verify = std::env::var("FERRITE_HYDRATION_VERIFY")
                .is_ok_and(|v| v == "1")
                .then(|| canonical_cell_digest(&self.cells[index]));
            let stats = self.cells[index].release_records();
            if let Some(h) = self.cell_hydration.get_mut(&key) {
                h.released = Some(stats);
                h.verify = verify;
            }
            released += 1;
        }
        if released > 0 {
            info!("Released records of {released} non-resident cell(s)");
        }
    }
    /// Re-materialize released cells from their retained load plans, in
    /// parallel. The parsed bytes must match the identity recorded at load;
    /// the cell is then normalized with the codes used at load.
    pub fn hydrate_cells(&mut self, indices: &[usize]) -> Result<()> {
        let jobs: Vec<(usize, [u8; 32], CellHydrationSource)> = indices
            .iter()
            .filter(|i| self.cell_is_released(**i))
            .filter_map(|i| {
                let key = self.cell_identity_sha(*i)?;
                Some((*i, key, self.cell_hydration.get(&key)?.source.clone()))
            })
            .collect();
        if jobs.is_empty() {
            return Ok(());
        }
        let started = ferrite_kernel::time::Instant::now();
        let results: Vec<Result<S101Cell>> = ferrite_product_api::platform::parallel_map(
            self.services.executor.as_ref(),
            &jobs,
            |(_, key, source)| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<S101Cell> {
                    let (mut cell, identity) = source.plan.load()?;
                    anyhow::ensure!(
                        identity.sha256() == key,
                        "Released S-101 cell source changed; reopen it"
                    );
                    source.plan.validate_materialized(&cell)?;
                    for codes in &source.codes {
                        cell.normalize_feature_codes(codes);
                    }
                    Ok(cell)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Hydration panicked")))
            },
        );
        for ((index, key, _), result) in jobs.iter().zip(results) {
            let mut cell = result?;
            cell.file_path = std::mem::take(&mut self.cells[*index].file_path);
            if let Some(expected) = self.cell_hydration.get(key).and_then(|h| h.verify.as_ref()) {
                let actual = canonical_cell_digest(&cell);
                let differing: Vec<_> = CANONICAL_CELL_PARTS
                    .iter()
                    .zip(actual.iter().zip(expected))
                    .filter(|(_, (a, e))| a != e)
                    .map(|(name, _)| *name)
                    .collect();
                anyhow::ensure!(
                    differing.is_empty(),
                    "Re-materialized cell differs from the released one in {differing:?}: {}",
                    cell.file_path.display()
                );
                info!("Hydration verified: {}", cell.file_path.display());
            }
            self.cells[*index] = cell;
            if let Some(h) = self.cell_hydration.get_mut(key) {
                h.released = None;
                h.verify = None;
            }
        }
        info!(
            "Re-materialized {} released cell(s) in {:.1} ms",
            jobs.len(),
            started.elapsed().as_secs_f64() * 1000.
        );
        Ok(())
    }
    /// Bring back the released cells a portrayal reads: the resident ones, or
    /// all of them without residency or under interoperability.
    pub fn hydrate_for_portrayal(&mut self, resident: Option<&[bool]>, all: bool) -> Result<()> {
        match resident {
            Some(resident) if !all => {
                let wanted: Vec<usize> = (0..self.cells.len())
                    .filter(|i| resident.get(*i).copied().unwrap_or(true))
                    .collect();
                self.hydrate_cells(&wanted)
            }
            _ => self.hydrate_all_cells(),
        }
    }
    pub fn hydrate_all_cells(&mut self) -> Result<()> {
        let all: Vec<usize> = (0..self.cells.len()).collect();
        self.hydrate_cells(&all)
    }
    /// Drop sources of cells that are no longer loaded.
    pub fn prune_cell_hydration(&mut self) {
        let loaded: std::collections::HashSet<[u8; 32]> = (0..self.loaded_source_identities.len())
            .filter_map(|i| self.cell_identity_sha(i))
            .collect();
        self.cell_hydration.retain(|key, _| loaded.contains(key));
    }
    /// The coverage inventory of the loaded cells, extracted once per exact
    /// cell set (order, paths and source bytes).
    pub fn memo_coverage_inventory(
        &mut self,
    ) -> Result<Arc<crate::coverage_projection::GeographicCoverageInventory>> {
        use std::hash::{Hash, Hasher};
        let key = (|| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            self.cells.len().hash(&mut hasher);
            for (index, cell) in self.cells.iter().enumerate() {
                cell.file_path.hash(&mut hasher);
                self.loaded_source_identities
                    .hash_for_cell(index, &mut hasher)?;
            }
            Some(hasher.finish())
        })();
        if let (Some(key), Some((memo_key, inventory))) = (key, &self.coverage_memo) {
            if key == *memo_key {
                return Ok(Arc::clone(inventory));
            }
        }
        // Coverage is extracted from every cell's records.
        self.hydrate_all_cells()?;
        let inventory = Arc::new(
            crate::coverage_projection::GeographicCoverageInventory::from_cells(&self.cells)?,
        );
        self.coverage_memo = key.map(|key| (key, Arc::clone(&inventory)));
        Ok(inventory)
    }
    /// Residency activates only for chart sets whose instruction lists would be
    /// large (estimated at ~5 instructions per feature). `FERRITE_RESIDENCY_THRESHOLD`
    /// overrides the estimate threshold (0 always activates it).
    pub fn residency_threshold() -> usize {
        std::env::var("FERRITE_RESIDENCY_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_500_000)
    }
    /// Cells to portray for the view this preparation will show: the S-98
    /// selected datasets within one extra viewport on every side. Cells left
    /// out are fully hidden by coverage there, so the drawn scene is unchanged.
    pub fn residency_for_view(
        &self,
        inventory: &crate::coverage_projection::GeographicCoverageInventory,
        view: &RenderContext,
        bounds: GeoBounds,
        preserve_view: bool,
        viewport: Option<ViewportPixels>,
    ) -> Option<Vec<bool>> {
        let estimate: usize = (0..self.cells.len())
            .map(|i| self.cell_feature_count(i) * 5)
            .sum();
        let threshold = self
            .residency_threshold_override
            .unwrap_or_else(Self::residency_threshold);
        if estimate < threshold {
            return None;
        }
        let size = viewport?;
        let mut probe = view.empty_for_rebuild();
        probe.set_bounds(bounds);
        probe.set_viewport(size.width as f32, size.height as f32);
        if preserve_view {
            probe.scaler = view.scaler.clone();
        } else {
            probe.zoom_to_fit(bounds);
        }
        probe.scaler.set_pixel_ratio(size.scale_factor);
        match inventory.visible_datasets(&probe.scaler, true, 1.0) {
            Ok(visible) => Some(visible),
            Err(error) => {
                warn!("Residency unavailable, portraying every cell: {error:#}");
                None
            }
        }
    }

    /// The renderer-independent part of portraying the loaded cells with the
    /// bound catalogues and `profile` into a rebuild of the published `view`
    /// (data `bounds`): validation, view residency, re-materialization and
    /// Lua portrayal (per-cell caches). `interoperating`: the host composes
    /// interoperability afterwards, which reads every cell. Returns the
    /// portrayed context and the coverage inventory.
    #[allow(clippy::too_many_arguments)]
    pub fn portray_loaded_cells(
        &mut self,
        view: &RenderContext,
        bounds: GeoBounds,
        profile: &str,
        preserve_view: bool,
        settings: Option<&SettingsState>,
        viewport: Option<ViewportPixels>,
        interoperating: bool,
        triangulate: AreaTriangulator,
        rules: RuleSource,
    ) -> Result<(RenderContext, Arc<GeographicCoverageInventory>)> {
        crate::validate_catalogue_pair(&self.fc, &self.pc)?;
        let coverage_inventory = self.memo_coverage_inventory()?;
        for (index, cell) in self.cells.iter().enumerate() {
            crate::validate_dataset_catalogues(
                &cell.dsid,
                &self.fc,
                &self.pc.product_id,
                &self.pc.version,
            )?;
            // Validate mandatory producer scale metadata even on portrayal-cache
            // hits. A released cell was validated before release.
            if self.cell_is_released(index) {
                continue;
            }
            crate::coverage_scale::dataset_reference_scale(cell).with_context(|| {
                format!(
                    "Invalid DataCoverage scale metadata: {}",
                    cell.file_path.display()
                )
            })?;
        }

        validated_lua_context(&self.pc, settings)?; // Also validate cache hits.
        let mut next = view.empty_for_rebuild();
        next.set_bounds(bounds);
        // Large chart sets keep instructions only for cells that can be drawn
        // near the view at this display scale (see `residency_for_view`).
        let resident =
            self.residency_for_view(&coverage_inventory, view, bounds, preserve_view, viewport);
        self.hydrate_for_portrayal(resident.as_deref(), interoperating)?;
        self.resident_cells = resident;
        if let Some(resident) = &self.resident_cells {
            info!(
                "Residency: portraying {} of {} cells",
                resident.iter().filter(|r| **r).count(),
                resident.len()
            );
        }

        // Portray every cell; unchanged cells are reused from their per-cell
        // caches. (A whole-scene cache keyed by every cell was removed: it
        // missed on any change of the open set and accumulated without bound.)
        // Exports and precomputed scenes never read the native cell cache.
        let cell_caches = if rules.export || rules.source == PortrayalSource::Precomputed {
            None
        } else {
            self.cell_instruction_cache_paths(settings, profile)
        };
        let common = Self::precomputed_common(settings, profile);
        let identities = self.cell_identities();
        try_lua_portrayal(
            &self.cells,
            &self.fc,
            &self.pc,
            &mut next,
            profile,
            settings,
            CellPortrayalSelection {
                caches: cell_caches.as_deref().map(|paths| CellCaches {
                    paths,
                    store: self.services.cache.as_ref(),
                    data: self.services.data.as_ref(),
                }),
                resident: self.resident_cells.as_deref(),
            },
            triangulate,
            self.portrayal_run(common, &identities, rules),
        )?;
        Ok((next, coverage_inventory))
    }

    /// View residency for a portrayal change of the published `view` (data
    /// `bounds`): the coverage inventory (when cells are loaded), the
    /// resident cells for the view, and the released cells the portrayal
    /// reads brought back (`interoperating`: all of them).
    pub fn prepare_view_residency(
        &mut self,
        view: &RenderContext,
        bounds: GeoBounds,
        preserve_view: bool,
        viewport: Option<ViewportPixels>,
        interoperating: bool,
    ) -> Result<Option<Arc<GeographicCoverageInventory>>> {
        let residency_inventory = if self.cells.is_empty() {
            None
        } else {
            Some(self.memo_coverage_inventory()?)
        };
        // Portrayal changes and per-catalogue owners honour view residency.
        let resident = residency_inventory.as_deref().and_then(|inventory| {
            self.residency_for_view(inventory, view, bounds, preserve_view, viewport)
        });
        self.hydrate_for_portrayal(resident.as_deref(), interoperating)?;
        self.resident_cells = resident;
        Ok(residency_inventory)
    }

    /// The renderer-independent part of a portrayal change into a rebuild of
    /// the published `view`: validation and Lua portrayal of the resident
    /// cells with their catalogue owners (`owned`) or the given pair. The
    /// host composes interoperability afterwards.
    #[allow(clippy::too_many_arguments)]
    pub fn portray_change_cells(
        &self,
        view: &RenderContext,
        request: &PortrayalChangeRequest,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        catalogue_registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        owned: bool,
        preflight_resources: bool,
        triangulate: AreaTriangulator,
        rules: RuleSource,
    ) -> Result<RenderContext> {
        validated_lua_context(pc, Some(&request.settings))?;
        crate::validate_catalogue_pair(fc, pc)?;
        if preflight_resources {
            for (index, cell) in self.cells.iter().enumerate() {
                let owner = catalogue_registry
                    .owner_for(&S101Layer::catalogue_dataset_key(&cell.dsid)?)
                    .context("Missing validated cell catalogue owner")?;
                crate::validate_dataset_catalogues(
                    &cell.dsid,
                    owner.fc(),
                    &owner.pc().product_id,
                    &owner.pc().version,
                )?;
                // A released cell was validated before release, and
                // re-materialization checks its source bytes are unchanged.
                if !self.cell_is_released(index) {
                    crate::coverage_scale::dataset_reference_scale(cell)?;
                }
            }
        }
        let mut next = view.empty_for_rebuild();
        if !self.cells.is_empty() {
            if let Some(resident) = &self.resident_cells {
                info!(
                    "Residency: portraying {} of {} cells",
                    resident.iter().filter(|r| **r).count(),
                    resident.len()
                );
            }
            if owned {
                self.portray_cells_with_owners(
                    catalogue_registry,
                    &mut next,
                    request,
                    triangulate,
                    rules,
                )?;
            } else {
                let common = Self::precomputed_common(Some(&request.settings), &request.profile);
                let identities = self.cell_identities();
                try_lua_portrayal(
                    &self.cells,
                    fc,
                    pc,
                    &mut next,
                    &request.profile,
                    Some(&request.settings),
                    CellPortrayalSelection {
                        caches: None,
                        resident: self.resident_cells.as_deref(),
                    },
                    triangulate,
                    self.portrayal_run(common, &identities, rules),
                )?;
            }
        }
        Ok(next)
    }

    /// The feature and portrayal catalogues owning each loaded cell, in cell
    /// order (interoperability composition with per-dataset owners).
    #[allow(clippy::type_complexity)]
    pub fn cell_owner_catalogues<'a>(
        &self,
        catalogue_registry: &'a s101_catalogue_selection::DatasetCatalogueRegistry,
    ) -> Result<(
        Vec<&'a ferrite_feature_catalog::BoundFeatureCatalogue>,
        Vec<&'a ferrite_portrayal_catalog::PortrayalCatalogue>,
    )> {
        let fcs = self
            .cells
            .iter()
            .map(|cell| {
                catalogue_registry
                    .owner_for(&S101Layer::catalogue_dataset_key(&cell.dsid)?)
                    .map(|owner| owner.fc().as_ref())
                    .context("Missing IC cell FC owner")
            })
            .collect::<Result<Vec<_>>>()?;
        let pcs = self
            .cells
            .iter()
            .map(|cell| {
                catalogue_registry
                    .owner_for(&S101Layer::catalogue_dataset_key(&cell.dsid)?)
                    .map(|owner| &***owner.pc())
                    .context("Missing IC cell PC owner")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((fcs, pcs))
    }

    // ── Loaded datasets ─────────────────────────────────────────────────

    /// The loaded logical datasets explicit cancellations are planned against.
    pub fn loaded_datasets(&self) -> LoadedDatasets {
        let mut loaded = BTreeMap::new();
        for (index, cell) in self.cells.iter().enumerate() {
            let (Ok(key), Some(identity)) = (
                s101_update_plan::dataset_key(&cell.dsid),
                self.loaded_source_identities.get(index),
            ) else {
                continue;
            };
            let metadata = self.loaded_discovery.get(&key).cloned().flatten();
            loaded.insert(key, (cell.dsid.clone(), identity, metadata));
        }
        loaded
    }

    /// Remove cell `index` with its source identity and chain paths (its
    /// discovery metadata stays; see [`Self::remove_dataset`]).
    pub fn remove_cell(&mut self, index: usize) -> S101Cell {
        let cell = self.cells.remove(index);
        self.loaded_source_identities.remove(index);
        self.loaded_chain_paths.remove(index);
        cell
    }

    /// Take cell `index` out with its source identity and chain paths, for
    /// a change that may be rolled back ([`Self::restore_cell`]); the
    /// discovery metadata stays until the change commits.
    pub fn take_cell(&mut self, index: usize) -> TakenCell {
        TakenCell {
            cell: self.cells.remove(index),
            identity: self.loaded_source_identities.remove(index),
            chain_paths: self.loaded_chain_paths.remove(index),
        }
    }

    /// Put a cell taken with [`Self::take_cell`] back at `index`.
    pub fn restore_cell(&mut self, index: usize, taken: TakenCell) {
        self.cells.insert(index, taken.cell);
        self.loaded_source_identities.insert(index, taken.identity);
        self.loaded_chain_paths.insert(index, taken.chain_paths);
    }

    /// Remove every cell with its identity, chain paths and discovery
    /// metadata (catalogues, caches and hydration sources stay).
    pub fn clear_cells(&mut self) {
        self.cells.clear();
        self.loaded_source_identities.clear();
        self.loaded_chain_paths.clear();
        self.loaded_discovery.clear();
    }

    /// Remove cell `index` and its logical dataset's discovery metadata.
    pub fn remove_dataset(&mut self, index: usize) -> S101Cell {
        let cell = self.remove_cell(index);
        if let Ok(key) = s101_update_plan::dataset_key(&cell.dsid) {
            self.loaded_discovery.remove(&key);
        }
        cell
    }

    /// Start installing a batch of loaded cells.
    pub fn begin_batch(&self) -> CellBatch {
        let loaded = (0..self.cells.len())
            .filter_map(|i| self.cell_identity_sha(i))
            .collect();
        let mut keys = BTreeMap::new();
        for (index, cell) in self.cells.iter().enumerate() {
            if let Ok(key) = s101_update_plan::dataset_key(&cell.dsid) {
                keys.insert(key, index);
            }
        }
        CellBatch { loaded, keys }
    }

    /// Install one loaded cell of a batch: an identity already seen is
    /// skipped, a newer edition or update replaces the loaded dataset (an
    /// older one is refused), then the cell, its identity, chain paths,
    /// discovery metadata and re-materialization source are appended.
    pub fn insert_loaded(
        &mut self,
        batch: &mut CellBatch,
        result: ChartLoadResult,
    ) -> Result<Inserted> {
        let ChartLoadResult {
            metadata,
            input_paths,
            cell,
            source_identity,
            hydration,
            ..
        } = result;
        if !batch.loaded.insert(*source_identity.sha256()) {
            return Ok(Inserted::AlreadyOpen(cell.file_path.clone()));
        }
        let key = s101_update_plan::dataset_key(&cell.dsid)?;
        let mut replaced = None;
        if let Some(&index) = batch.keys.get(&key) {
            let loaded = &self.cells[index].dsid;
            anyhow::ensure!(
                (cell.dsid.edition_number, cell.dsid.update_number)
                    >= (loaded.edition_number, loaded.update_number),
                "{} would downgrade a loaded edition/update",
                cell.file_path.display()
            );
            // A newer edition/update replaces the loaded dataset.
            self.remove_cell(index);
            replaced = Some(index);
            batch.keys = self
                .cells
                .iter()
                .enumerate()
                .filter_map(|(i, c)| Some((s101_update_plan::dataset_key(&c.dsid).ok()?, i)))
                .collect();
        }
        if let Some(source) = hydration {
            self.cell_hydration.insert(
                *source_identity.sha256(),
                CellHydration {
                    source,
                    released: None,
                    verify: None,
                },
            );
        }
        self.loaded_discovery.insert(key.clone(), metadata);
        self.loaded_chain_paths.push(input_paths);
        self.loaded_source_identities.push(source_identity);
        batch.keys.insert(key, self.cells.len());
        self.cells.push(cell);
        Ok(Inserted::Added { replaced })
    }

    /// Finish a batch: drop re-materialization sources of cells no longer
    /// loaded and stage the per-dataset catalogue owners.
    pub fn finish_batch(&mut self) -> Result<()> {
        self.prune_cell_hydration();
        if !self.cells.is_empty() {
            self.catalogue_registry = self.prepare_catalogue_registry(&self.fc, &self.pc, true)?;
        } else {
            self.catalogue_registry = Default::default();
        }
        Ok(())
    }

    /// Validate a materialized cell against a catalogue pair and normalize
    /// it as loading does.
    pub fn admit_with(
        cell: &mut S101Cell,
        fc: &BoundFeatureCatalogue,
        pc: &BoundPortrayalCatalogue,
    ) -> Result<()> {
        crate::validate_dataset_catalogues(&cell.dsid, fc, &pc.product_id, &pc.version)
            .with_context(|| format!("Incompatible S-101 dataset {}", cell.file_path.display()))?;
        crate::validate_spatial_scale_properties(&cell.dsid, cell.features.values()).with_context(
            || {
                format!(
                    "Nonconformant S-101 spatial scales {}",
                    cell.file_path.display()
                )
            },
        )?;
        Ok(())
    }
}
