//! Chart (domain) state of the viewer: the loaded datasets, their bound
//! catalogues, coverage, residency, interoperability and bathymetry, and the
//! published portrayal. Window, GUI, input and navigation state stay with the
//! host application.
use crate::catalogue::hash_bound_catalogue_inputs;
use crate::chart_cells::{canonical_cell_digest, CANONICAL_CELL_PARTS};
use crate::portrayal::{
    try_lua_portrayal, try_lua_portrayal_for_cells, validated_lua_context, AreaTriangulator,
    CellPortrayalSelection, PortrayalChangeRequest,
};
use crate::{
    cell_source_identity, decoded_chart_cache, interoperability, primary_viewing_layers,
    s101_catalogue_selection, s101_lifecycle_metadata, s101_update_plan,
    s102_depth_policy::DepthPolicy, s102_journal_admission, s102_original_inputs,
    secure_instruction_cache, CellHydrationSource,
};
use anyhow::{Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::CatalogueStatus;
use ferrite_render::host_settings::SettingsState;
use ferrite_render::DrawingInstruction;
use ferrite_render::{GeoBounds, RenderContext};
use ferrite_s100_core::S101Cell;
use ferrite_s101::coverage_projection::GeographicCoverageInventory;
use ferrite_s102::BathymetryCoverage;
use ferrite_security::{AuthenticatedSnapshot, UnauthenticatedSnapshot};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use tracing::{info, warn};

/// Physical size and scale factor of the view a portrayal is prepared for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportPixels {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}
use std::sync::Arc;

/// How a loaded cell is re-materialized after its records were released.
pub struct CellHydration {
    pub source: CellHydrationSource,
    /// Statistics of a released cell (None while its records are resident).
    pub released: Option<ferrite_s100_core::CellStatistics>,
    /// `FERRITE_HYDRATION_VERIFY=1`: canonical digest taken before release,
    /// compared after re-materialization.
    pub verify: Option<Vec<[u8; 32]>>,
}

/// Cells portrayed for the current catalogues (see
/// [`EngineState::portray_loaded_cells`]).
pub struct CellScene {
    pub context: RenderContext,
    pub coverage: Arc<GeographicCoverageInventory>,
    /// Vector instructions changed by interoperability composition.
    pub ic_changed: usize,
    /// When Lua portrayal finished (post-Lua timing).
    pub after_lua: std::time::Instant,
}

/// Cells portrayed for a portrayal change (see
/// [`EngineState::portray_change_cells`]).
pub struct ChangeCellScene {
    pub context: RenderContext,
    /// The interoperability catalogue applied (settings permitting).
    pub active_ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
    /// Its selection for S-102 rasters composed with this scene.
    pub s102_selection: ferrite_interoperability::Selection,
    pub ic_changed: usize,
}

/// Chart state owned by the engine.
pub struct EngineState {
    /// Published portrayal (the engine's output scene).
    pub render_context: RenderContext,
    pub bounds: GeoBounds,
    /// Base instruction count (chart instructions only, before plugin instructions)
    pub base_instruction_count: usize,
    /// Current color profile name (Day, Dusk, Night)
    pub current_profile_name: String,
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
    pub s102_pc_status: CatalogueStatus,
    pub s102_fc_status: CatalogueStatus,
    pub s102_fc: Option<Arc<BoundFeatureCatalogue>>,
    pub s102_pc: Option<Arc<BoundPortrayalCatalogue>>,
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
    pub ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
    pub ic_combination: Option<String>,
    pub ic_assigned_vectors: usize,
    /// S-102 2.1 trial files opened for inspection only (never depth).
    pub s102_trials: Vec<(PathBuf, ferrite_s102::trial_inspection::TrialInspection)>,
    pub bathymetry: Vec<(
        PathBuf,
        BathymetryCoverage,
        Option<Arc<AuthenticatedSnapshot>>,
    )>,
    pub bathymetry_bounds: HashMap<PathBuf, GeoBounds>,
    pub depth_policies: HashMap<PathBuf, DepthPolicy>,
    pub depth_inputs: HashMap<PathBuf, Arc<UnauthenticatedSnapshot>>,
    pub s102_original_inputs: s102_original_inputs::Registry,
    pub s102_journal_gate: Option<s102_journal_admission::Gate>,
    pub require_signatures: bool,
    pub operational_mode: bool,
    pub verified_count: usize,
    pub unsigned_count: usize,
    pub decoded_chart_cache: decoded_chart_cache::SharedDecodedChartCache,
}

impl EngineState {
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
        let mut next = base.stage(&requests, fallback, &self.catalogue_inventory)?;
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
            ferrite_s101::validate_dataset_catalogues(
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
        let mut next = registry.stage(&requests, fallback, &self.catalogue_inventory)?;
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
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Option<()> {
        use std::hash::Hash;
        // Catalogue rules and context parameters determine the generated output.
        // Hash their bytes and current settings rather than only chart paths.
        crate::portrayal_cache::CACHE_SCHEMA_VERSION.hash(&mut *hasher);
        // Fail closed if the linked runtime cannot identify itself. A major/minor
        // label alone cannot invalidate output from another Lua patch or build.
        ferrite_lua::runtime_identity()?.hash(&mut *hasher);
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
        self.current_profile_name.hash(&mut *hasher);
        Some(())
    }
    /// Per-cell portrayal cache files, by global cell index. A cell's output
    /// depends only on its own bytes plus the shared portrayal inputs, so a
    /// changed scene reuses every unchanged cell instead of re-running Lua.
    pub fn cell_instruction_cache_paths(
        &self,
        settings: Option<&SettingsState>,
    ) -> Option<Vec<Option<PathBuf>>> {
        if self.cells.is_empty() || std::env::var_os("FERRITE_NO_CACHE").is_some() {
            return None;
        }
        let cache_dir = secure_instruction_cache::root()?;
        use std::hash::{Hash, Hasher};
        let mut common = std::collections::hash_map::DefaultHasher::new();
        "cell-portrayal-v1".hash(&mut common);
        self.hash_portrayal_cache_inputs(settings, &mut common)?;
        Some(
            self.cells
                .iter()
                .enumerate()
                .map(|(cell_index, cell)| {
                    let mut hasher = common.clone();
                    let canonical = std::fs::canonicalize(&cell.file_path)
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
        let started = std::time::Instant::now();
        let results: Vec<Result<S101Cell>> = std::thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .iter()
                .map(|(_, key, source)| {
                    scope.spawn(move || -> Result<S101Cell> {
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
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("Hydration panicked")))
                })
                .collect()
        });
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
    ) -> Result<Arc<ferrite_s101::coverage_projection::GeographicCoverageInventory>> {
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
            ferrite_s101::coverage_projection::GeographicCoverageInventory::from_cells(
                &self.cells,
            )?,
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
        inventory: &ferrite_s101::coverage_projection::GeographicCoverageInventory,
        preserve_view: bool,
        viewport: Option<ViewportPixels>,
    ) -> Option<Vec<bool>> {
        let estimate: usize = (0..self.cells.len())
            .map(|i| self.cell_feature_count(i) * 5)
            .sum();
        if estimate < Self::residency_threshold() {
            return None;
        }
        let size = viewport?;
        let mut probe = self.render_context.empty_for_rebuild();
        probe.set_bounds(self.bounds);
        probe.set_viewport(size.width as f32, size.height as f32);
        if preserve_view {
            probe.scaler = self.render_context.scaler.clone();
        } else {
            probe.zoom_to_fit(self.bounds);
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
    /// S-98 Level 2 context for the products currently on display.
    pub fn ic_level2(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
    ) -> Result<interoperability::Level2> {
        let mut displayed = Vec::new();
        if !self.cells.is_empty() {
            displayed.push("S-101");
        }
        if !self.bathymetry.is_empty() {
            displayed.push("S-102");
        }
        interoperability::Level2::new(
            ic,
            self.ic_combination.as_deref(),
            &displayed,
            self.bathymetry
                .iter()
                .map(|(_, c, _)| c as &dyn ferrite_kernel::NumericCoverageSource),
        )
    }
    /// Selection for S-102 rasters being composed now: S-102 is on display.
    pub fn ic_s102_selection(
        &self,
        ic: &ferrite_interoperability::AuthenticatedCatalogue,
    ) -> Result<ferrite_interoperability::Selection> {
        let mut displayed = vec!["S-102"];
        if !self.cells.is_empty() {
            displayed.push("S-101");
        }
        ic.catalogue
            .select(self.ic_combination.as_deref(), &displayed)
    }

    /// The renderer-independent part of portraying the loaded cells with the
    /// bound catalogues: validation, view residency, re-materialization, Lua
    /// portrayal (per-cell caches) and interoperability composition.
    pub fn portray_loaded_cells(
        &mut self,
        preserve_view: bool,
        settings: Option<&SettingsState>,
        viewport: Option<ViewportPixels>,
        active_ic: Option<Arc<ferrite_interoperability::AuthenticatedCatalogue>>,
        triangulate: AreaTriangulator,
    ) -> Result<CellScene> {
        ferrite_s101::validate_catalogue_pair(&self.fc, &self.pc)?;
        let coverage_inventory = self.memo_coverage_inventory()?;
        for (index, cell) in self.cells.iter().enumerate() {
            ferrite_s101::validate_dataset_catalogues(
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
            ferrite_s101::coverage_scale::dataset_reference_scale(cell).with_context(|| {
                format!(
                    "Invalid DataCoverage scale metadata: {}",
                    cell.file_path.display()
                )
            })?;
        }

        validated_lua_context(&self.pc, settings)?; // Also validate cache hits.
        let mut next = self.render_context.empty_for_rebuild();
        next.set_bounds(self.bounds);
        // Large chart sets keep instructions only for cells that can be drawn
        // near the view at this display scale (see `residency_for_view`).
        let resident = self.residency_for_view(&coverage_inventory, preserve_view, viewport);
        self.hydrate_for_portrayal(resident.as_deref(), active_ic.is_some())?;
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
        let cell_caches = self.cell_instruction_cache_paths(settings);
        try_lua_portrayal(
            &self.cells,
            &self.fc,
            &self.pc,
            &mut next,
            &self.current_profile_name,
            settings,
            CellPortrayalSelection {
                caches: cell_caches.as_deref(),
                resident: self.resident_cells.as_deref(),
            },
            triangulate,
        )?;
        let after_lua = std::time::Instant::now();

        let ic_changed = if let Some(ic) = active_ic {
            let level2 = self.ic_level2(&ic)?;
            interoperability::compose_vectors(
                &ic,
                &level2,
                &self.cells,
                &self.fc,
                (&self.pc, &self.current_profile_name),
                &mut next,
            )?
        } else {
            0
        };
        Ok(CellScene {
            context: next,
            coverage: coverage_inventory,
            ic_changed,
            after_lua,
        })
    }

    /// View residency for a portrayal change: the coverage inventory (when
    /// cells are loaded), the resident cells for the view, and the released
    /// cells the portrayal reads brought back.
    pub fn prepare_view_residency(
        &mut self,
        preserve_view: bool,
        viewport: Option<ViewportPixels>,
        interoperability_enabled: bool,
    ) -> Result<Option<Arc<GeographicCoverageInventory>>> {
        let residency_inventory = if self.cells.is_empty() {
            None
        } else {
            Some(self.memo_coverage_inventory()?)
        };
        // Portrayal changes and per-catalogue owners honour view residency.
        let resident = residency_inventory
            .as_deref()
            .and_then(|inventory| self.residency_for_view(inventory, preserve_view, viewport));
        let interoperating = interoperability_enabled && self.ic.is_some();
        self.hydrate_for_portrayal(resident.as_deref(), interoperating)?;
        self.resident_cells = resident;
        Ok(residency_inventory)
    }

    /// The renderer-independent part of a portrayal change: validation, Lua
    /// portrayal of the resident cells with their catalogue owners (`owned`)
    /// or the given pair, and interoperability composition.
    #[allow(clippy::too_many_arguments)]
    pub fn portray_change_cells(
        &self,
        request: &PortrayalChangeRequest,
        fc: &Arc<BoundFeatureCatalogue>,
        pc: &Arc<BoundPortrayalCatalogue>,
        catalogue_registry: &s101_catalogue_selection::DatasetCatalogueRegistry,
        owned: bool,
        preflight_resources: bool,
        triangulate: AreaTriangulator,
    ) -> Result<ChangeCellScene> {
        validated_lua_context(pc, Some(&request.settings))?;
        ferrite_s101::validate_catalogue_pair(fc, pc)?;
        if preflight_resources {
            for (index, cell) in self.cells.iter().enumerate() {
                let owner = catalogue_registry
                    .owner_for(&EngineState::catalogue_dataset_key(&cell.dsid)?)
                    .context("Missing validated cell catalogue owner")?;
                ferrite_s101::validate_dataset_catalogues(
                    &cell.dsid,
                    owner.fc(),
                    &owner.pc().product_id,
                    &owner.pc().version,
                )?;
                // A released cell was validated before release, and
                // re-materialization checks its source bytes are unchanged.
                if !self.cell_is_released(index) {
                    ferrite_s101::coverage_scale::dataset_reference_scale(cell)?;
                }
            }
        }
        let mut next = self.render_context.empty_for_rebuild();
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
                )?;
            } else {
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
                )?;
            }
        }
        let active_ic = if request.settings.interoperability_enabled {
            self.ic.clone()
        } else {
            None
        };
        let s102_selection = active_ic
            .as_ref()
            .map(|ic| self.ic_s102_selection(ic))
            .transpose()?
            .unwrap_or_default();
        let ic_changed = if let Some(ic) = active_ic.as_ref() {
            if owned {
                let fcs = self
                    .cells
                    .iter()
                    .map(|cell| {
                        catalogue_registry
                            .owner_for(&EngineState::catalogue_dataset_key(&cell.dsid)?)
                            .map(|owner| owner.fc().as_ref())
                            .context("Missing IC cell FC owner")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let pcs = self
                    .cells
                    .iter()
                    .map(|cell| {
                        catalogue_registry
                            .owner_for(&EngineState::catalogue_dataset_key(&cell.dsid)?)
                            .map(|owner| &***owner.pc())
                            .context("Missing IC cell PC owner")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let level2 = self.ic_level2(ic)?;
                interoperability::compose_vectors_for_cells(
                    ic,
                    &level2,
                    &self.cells,
                    &fcs,
                    &pcs,
                    &request.profile,
                    &mut next,
                )?
            } else {
                let level2 = self.ic_level2(ic)?;
                interoperability::compose_vectors(
                    ic,
                    &level2,
                    &self.cells,
                    fc,
                    (pc, &request.profile),
                    &mut next,
                )?
            }
        } else {
            0
        };
        Ok(ChangeCellScene {
            context: next,
            active_ic,
            s102_selection,
            ic_changed,
        })
    }
}
