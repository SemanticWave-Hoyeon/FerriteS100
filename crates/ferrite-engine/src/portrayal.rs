//! S-101 cell portrayal through the catalogue's Lua rules, with per-cell
//! instruction caches.
use crate::platform::{CacheStore, DataSource, Executor};
use crate::portrayal_cache::{encode_cache_payload, verify_and_deserialize_cache, wrap_cache};
use crate::{primary_viewing_layers, secure_instruction_cache};
use anyhow::{Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_lua::ContextParameters as LuaContextParameters;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::SettingsState;
use ferrite_render::{DrawingInstruction, RenderContext};
use ferrite_s100_core::S101Cell;
#[cfg(feature = "lua-native")]
use ferrite_s101::convert_lua_results_for_cell;
use std::path::{Path, PathBuf};
#[cfg(feature = "lua-native")]
use std::sync::Arc;
use tracing::{info, warn};

/// A requested colour profile and settings for the next portrayal.
#[derive(Clone)]
pub struct PortrayalChangeRequest {
    pub profile: String,
    pub settings: SettingsState,
}

/// The triangulation the renderer would compute for an area in a projection,
/// for areas with at least the given number of ring vertices (the host's GPU
/// renderer supplies it); stored on the area and in the cell cache.
pub type AreaTriangulator = fn(
    &ferrite_render::AreaInstruction,
    ferrite_render::FlatProjection,
    usize,
) -> Option<ferrite_render::AreaTriangulation>;

/// Try to execute Lua portrayal rules and convert to drawing instructions
/// Context parameters are loaded dynamically from PC XML (no hardcoding)
/// Processes each cell separately to avoid feature ID collisions across cells
pub fn validated_lua_context(
    pc: &BoundPortrayalCatalogue,
    settings: Option<&SettingsState>,
) -> Result<LuaContextParameters> {
    if let Some(settings) = settings {
        primary_viewing_layers::resolve(pc, settings)?;
    }
    // Load context parameters from PC XML (dynamically, no hardcoding)
    let pc_context_params = pc.get_context_parameters();
    let mut context = LuaContextParameters::from_pc_context(pc_context_params);

    // Apply UI settings to context parameters if provided
    if let Some(s) = settings {
        context.safety_depth = s.safety_depth;
        context.safety_contour = s.safety_contour;
        context.shallow_contour = s.shallow_contour;
        context.deep_contour = s.deep_contour;
        context.two_shades = s.two_shades;
        context.simplified_symbols = s.simplified_symbols;
        // The IHO PC uses ShallowWaterDangers; IsolatedDangers is a legacy
        // host parameter and does not control UDWHAZ05 in current catalogues.
        context.shallow_water_dangers = s.isolated_dangers;
        context.isolated_dangers = s.isolated_dangers;
        context.full_sectors = s.full_light_sectors;
        context.ignore_scale_minimum = s.ignore_scale_minimum;
        context.ignore_scamin = s.ignore_scale_minimum;
        context.symbolized_boundaries = !s.plain_boundaries;
        info!(
            "  Applied UI settings: SafetyDepth={}, SafetyContour={}, TwoShades={}",
            s.safety_depth, s.safety_contour, s.two_shades
        );
    }

    info!(
        "  Context parameters loaded from PC XML: {} parameters",
        pc_context_params.len()
    );

    ferrite_s101::synchronize_legacy_context(pc, &mut context);
    let (_, diagnostics) = ferrite_s101::validate_portrayal_context(pc, &context)
        .context("PC portrayal input validation failed")?;
    for diagnostic in diagnostics {
        warn!("{diagnostic}");
    }
    Ok(context)
}

#[allow(clippy::too_many_arguments)]
pub fn try_lua_portrayal(
    cells: &[S101Cell],
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    render_context: &mut RenderContext,
    profile_name: &str,
    settings: Option<&SettingsState>,
    selection: CellPortrayalSelection<'_>,
    triangulate: AreaTriangulator,
    run: PortrayalRun<'_>,
) -> Result<()> {
    let CellPortrayalSelection {
        caches: cell_caches,
        resident,
    } = selection;
    try_lua_portrayal_for_cells(
        cells
            .iter()
            .enumerate()
            .filter(|(index, _)| resident.is_none_or(|r| r.get(*index).copied().unwrap_or(true))),
        fc,
        pc,
        render_context,
        profile_name,
        settings,
        cell_caches,
        triangulate,
        run,
    )
}

/// Which cells to portray (None: all) and where their caches live (None: none).
#[derive(Default, Clone, Copy)]
pub struct CellPortrayalSelection<'a> {
    pub caches: Option<CellCaches<'a>>,
    pub resident: Option<&'a [bool]>,
}

/// Per-cell cache entries (by global cell index) in a cache store; cell
/// source modification times come from the data source.
#[derive(Clone, Copy)]
pub struct CellCaches<'a> {
    pub paths: &'a [Option<PathBuf>],
    pub store: &'a dyn CacheStore,
    pub data: &'a dyn DataSource,
}

/// A per-cell portrayal: fresh Lua results, or instructions from the cache.
/// Smaller areas triangulate faster than their stored indices would load.
pub const STORED_TRIANGULATION_MIN_VERTICES: usize = 32;

#[cfg(feature = "lua-native")]
enum CellPortrayal {
    /// Converted on the worker; `written` reports a new cell cache file.
    Fresh {
        instructions: Vec<DrawingInstruction>,
        results: usize,
        written: bool,
    },
    Cached(Vec<DrawingInstruction>),
}

/// Read a cell cache written for the same inputs, if it is not older than
/// the cell's file; any rejection simply portrays the cell again.
pub fn read_cell_cache(
    caches: &CellCaches<'_>,
    path: &Path,
    cell: &S101Cell,
) -> Option<Vec<DrawingInstruction>> {
    let cache_time = caches.store.modified(path)?;
    if caches
        .data
        .modified(&cell.file_path)
        .is_some_and(|t| t > cache_time)
    {
        return None;
    }
    let data = caches.store.read(path).ok()?;
    match verify_and_deserialize_cache(&data) {
        Ok(instructions) => {
            caches.store.mark_used(path);
            Some(instructions)
        }
        Err(error) => {
            warn!("Cell cache rejected: {error}. Regenerating.");
            None
        }
    }
}

pub fn write_cell_cache(
    store: &dyn CacheStore,
    path: &Path,
    instructions: &[DrawingInstruction],
) -> bool {
    if instructions.len() as u64 > secure_instruction_cache::MAX_INSTRUCTIONS {
        return false;
    }
    match encode_cache_payload(instructions) {
        Ok(payload) => match store.write(path, &wrap_cache(&payload)) {
            Ok(()) => true,
            Err(error) => {
                warn!("Cell cache write failed: {error}");
                false
            }
        },
        Err(error) => {
            warn!("Cell cache not written: {error}");
            false
        }
    }
}

/// Where S-101 cell drawing instructions come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortrayalSource {
    /// Run the portrayal catalogue's Lua rules (`lua-native` feature), with
    /// the per-cell cache.
    Lua,
    /// Read precomputed per-cell instruction payloads (Part 9a portrayal
    /// output, converted) from the cache store under portable keys
    /// ([`PrecomputedCells::key`]); a missing payload is an error. Payloads
    /// are exported by a native build ([`PrecomputedCells::export`]).
    Precomputed,
}

impl Default for PortrayalSource {
    /// Lua when it is built, otherwise precomputed payloads.
    fn default() -> Self {
        if cfg!(feature = "lua-native") {
            Self::Lua
        } else {
            Self::Precomputed
        }
    }
}

/// Precomputed cell payloads in a cache store. Keys are SHA-256 digests of
/// the portrayal inputs that are not the catalogues or the cell (`common`:
/// settings, colour profile, cache schema), the bound catalogue pair and the
/// cell's source identity, so they hold across machines, paths and builds
/// with the same cache schema.
#[derive(Clone, Copy)]
pub struct PrecomputedCells<'a> {
    pub store: &'a dyn CacheStore,
    pub common: [u8; 32],
    /// Source identity (SHA-256 of the loaded chain) by global cell index.
    pub identities: &'a [Option<[u8; 32]>],
    /// Also write the payload of every cell portrayed with Lua.
    pub export: bool,
}

impl PrecomputedCells<'_> {
    /// Entry of cell `cell_index` portrayed with `fc`/`pc`.
    pub fn key(
        &self,
        fc: &BoundFeatureCatalogue,
        pc: &BoundPortrayalCatalogue,
        cell_index: usize,
    ) -> Option<PathBuf> {
        use sha2::{Digest, Sha256};
        let identity = self.identities.get(cell_index)?.as_ref()?;
        let mut sha = Sha256::new();
        sha.update(b"ferrite-precomputed-cell-v1\0");
        sha.update(self.common);
        sha.update((fc.version.len() as u64).to_le_bytes());
        sha.update(fc.version.as_bytes());
        sha.update(fc.source_digest());
        sha.update(pc.source_digest());
        sha.update(identity);
        let digest = sha.finalize();
        let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        Some(self.store.root()?.join(format!(".ferrite_pre_{name}.bin")))
    }
}

/// [`std::hash::Hasher`] into SHA-256 with platform-independent integer
/// widths, for portable keys built from `Hash` implementations.
pub struct PortableHasher(pub sha2::Sha256);
impl std::hash::Hasher for PortableHasher {
    fn finish(&self) -> u64 {
        use sha2::Digest;
        let digest = self.0.clone().finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
    }
    fn write(&mut self, bytes: &[u8]) {
        use sha2::Digest;
        self.0.update(bytes);
    }
    fn write_usize(&mut self, value: usize) {
        self.write(&(value as u64).to_le_bytes());
    }
    fn write_isize(&mut self, value: isize) {
        self.write(&(value as i64).to_le_bytes());
    }
}

/// How a portrayal run executes.
#[derive(Clone, Copy)]
pub struct PortrayalRun<'a> {
    pub executor: &'a dyn Executor,
    pub source: PortrayalSource,
    pub precomputed: Option<PrecomputedCells<'a>>,
}

/// Renumber cached or precomputed instructions exactly as
/// convert_lua_results_for_cell would: the cell index, and the S-101
/// dependency namespace; then share equal line styles.
fn renumber_stored(instructions: &mut [DrawingInstruction], cell_index: usize) {
    let index = u32::try_from(cell_index).ok();
    for instruction in instructions.iter_mut() {
        instruction.set_cell_index(index);
        if let Some(dependency) = instruction.dependency_mut() {
            dependency.namespace = cell_index as u64;
        }
    }
    // Decoding gives every line its own style allocation; sharing them here
    // leaves the scene-wide interning only a few distinct allocations per cell.
    ferrite_render::intern_line_styles(instructions);
}

/// A catalogue owner may process a subset of the scene without renumbering
/// source cells. Original global indices keep coverage, picking and dependencies
/// bound to the same chart when several owner engines feed one display list.
#[allow(clippy::too_many_arguments)]
pub fn try_lua_portrayal_for_cells<'a>(
    cells: impl IntoIterator<Item = (usize, &'a S101Cell)>,
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    render_context: &mut RenderContext,
    profile_name: &str,
    settings: Option<&SettingsState>,
    cell_caches: Option<CellCaches<'_>>,
    triangulate: AreaTriangulator,
    run: PortrayalRun<'_>,
) -> Result<()> {
    match run.source {
        PortrayalSource::Precomputed => {
            validated_lua_context(pc, settings)?;
            precomputed_portrayal_for_cells(cells, fc, pc, render_context, run)
        }
        #[cfg(feature = "lua-native")]
        PortrayalSource::Lua => lua_portrayal_for_cells(
            cells,
            fc,
            pc,
            render_context,
            profile_name,
            settings,
            cell_caches,
            triangulate,
            run,
        ),
        #[cfg(not(feature = "lua-native"))]
        PortrayalSource::Lua => {
            let _ = (
                cells,
                render_context,
                profile_name,
                cell_caches,
                triangulate,
            );
            anyhow::bail!(
                "S-101 Lua portrayal is not built (feature lua-native); \
                 use precomputed portrayal payloads"
            )
        }
    }
}

/// Cells from precomputed payloads, decoded on the executor and appended in
/// cell order; the first missing or rejected payload (in that order) fails.
fn precomputed_portrayal_for_cells<'a>(
    cells: impl IntoIterator<Item = (usize, &'a S101Cell)>,
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    render_context: &mut RenderContext,
    run: PortrayalRun<'_>,
) -> Result<()> {
    let precomputed = run.precomputed.context(
        "Precomputed S-101 portrayal needs a cache store (none is available); \
         without the lua-native feature cells are portrayed only from payloads \
         exported by a native build",
    )?;
    let cells: Vec<(usize, &S101Cell)> = cells.into_iter().collect();
    let decoded = crate::platform::parallel_map(run.executor, &cells, |&(cell_index, cell)| {
        let missing = || {
            format!(
                "No precomputed portrayal for S-101 cell {} ({}); without the \
                 lua-native feature, payloads exported by a native build \
                 (`ferrite precompute`) for the same catalogues and settings \
                 must be in the cache store",
                cell.dsid.dataset_name,
                cell.file_path.display()
            )
        };
        let key = precomputed.key(fc, pc, cell_index).with_context(missing)?;
        let data = precomputed
            .store
            .read(&key)
            .with_context(|| format!("{} [{}]", missing(), key.display()))?;
        let mut instructions = verify_and_deserialize_cache(&data).map_err(|error| {
            anyhow::anyhow!(
                "Precomputed portrayal of {} rejected: {error}",
                cell.file_path.display()
            )
        })?;
        renumber_stored(&mut instructions, cell_index);
        Ok::<_, anyhow::Error>(instructions)
    });
    for instructions in decoded {
        render_context.append_converted(instructions?);
    }
    Ok(())
}

/// Write the precomputed payload of a portrayed cell (export).
#[cfg(feature = "lua-native")]
fn export_precomputed(
    precomputed: Option<&PrecomputedCells<'_>>,
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    cell_index: usize,
    instructions: &[DrawingInstruction],
) {
    let Some(precomputed) = precomputed.filter(|p| p.export) else {
        return;
    };
    if let Some(key) = precomputed.key(fc, pc, cell_index) {
        if !write_cell_cache(precomputed.store, &key, instructions) {
            warn!("Precomputed portrayal of cell {cell_index} not written");
        }
    }
}

/// Lua portrayal: cells are portrayed independently (each in its own engine
/// state, as before) on the executor's workers; results are kept by cell and
/// converted strictly in cell order, so instruction order and the first
/// reported error are unchanged. Cells are not split by feature: portrayal
/// rules keep state across the features of one run (e.g. text offsets of
/// co-located lights), and a split changes their output.
#[cfg(feature = "lua-native")]
#[allow(clippy::too_many_arguments)]
fn lua_portrayal_for_cells<'a>(
    cells: impl IntoIterator<Item = (usize, &'a S101Cell)>,
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    render_context: &mut RenderContext,
    profile_name: &str,
    settings: Option<&SettingsState>,
    cell_caches: Option<CellCaches<'_>>,
    triangulate: AreaTriangulator,
    run: PortrayalRun<'_>,
) -> Result<()> {
    use ferrite_lua::{PortrayalEngine, TypeCatalogue};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let context = validated_lua_context(pc, settings)?;

    let rules_path = pc.root_path.join("Rules");

    if pc
        .sources()
        .read_relative(Path::new("Rules/main.lua"))
        .is_err()
    {
        return Err(anyhow::anyhow!(
            "Rules directory not found: {}",
            rules_path.display()
        ));
    }

    info!("Initializing Lua portrayal engines...");
    let type_catalogue = Arc::new(TypeCatalogue::from_feature_catalogue(fc));
    info!(
        "  Type catalogue loaded: {} feature types, {} attributes",
        fc.feature_types.len(),
        fc.simple_attributes.len()
    );
    let new_engine = || -> Result<PortrayalEngine> {
        let mut engine = PortrayalEngine::new_with_sources(pc.sources())
            .context("Failed to create portrayal engine")?;
        engine.set_shared_type_catalogue(Arc::clone(&type_catalogue));
        engine
            .initialize()
            .context("Failed to initialize portrayal engine")?;
        Ok(engine)
    };

    let cells: Vec<(usize, &S101Cell)> = cells.into_iter().collect();
    let workers = run.executor.parallelism().min(8).min(cells.len()).max(1);
    // Largest cells start first so one big cell does not finish last on its
    // own; results are still assembled strictly in cell order.
    let mut schedule: Vec<usize> = (0..cells.len()).collect();
    schedule.sort_by_key(|position| std::cmp::Reverse(cells[*position].1.features.len()));
    let next = AtomicUsize::new(0);
    // Earliest failed position: later cells are no longer needed.
    let first_error = AtomicUsize::new(usize::MAX);
    let results: Vec<Mutex<Option<Result<CellPortrayal>>>> =
        cells.iter().map(|_| Mutex::new(None)).collect();
    // Conversion only appends (filtered by viewing group), so each cell is
    // converted on its worker into an empty context with the same settings.
    let template = render_context.empty_for_rebuild();
    let precomputed = run.precomputed;
    run.executor.for_each_index(workers, &|_| {
        let mut engine = None;
        loop {
            let claimed = next.fetch_add(1, Ordering::Relaxed);
            if claimed >= cells.len() {
                break;
            }
            let position = schedule[claimed];
            if position > first_error.load(Ordering::Relaxed) {
                continue;
            }
            let (cell_index, cell) = cells[position];
            let cache = cell_caches.as_ref().and_then(|caches| {
                caches
                    .paths
                    .get(cell_index)
                    .and_then(Option::as_deref)
                    .map(|path| (caches, path))
            });
            let result = if let Some(mut instructions) =
                cache.and_then(|(caches, path)| read_cell_cache(caches, path, cell))
            {
                renumber_stored(&mut instructions, cell_index);
                export_precomputed(precomputed.as_ref(), fc, pc, cell_index, &instructions);
                Ok(CellPortrayal::Cached(instructions))
            } else {
                (|| {
                    if engine.is_none() {
                        engine = Some(new_engine()?);
                    }
                    let engine = engine.as_mut().expect("created above");
                    let results = engine
                        .process_owned_cell(ferrite_lua::CellData::from_cell(cell), context.clone())
                        .with_context(|| {
                            format!(
                                "Cell {} portrayal failed: {}",
                                cell_index,
                                cell.file_path.display()
                            )
                        })?;
                    let mut converted = template.empty_for_rebuild();
                    convert_lua_results_for_cell(
                        &results,
                        cell,
                        pc,
                        &mut converted,
                        cell_index,
                        profile_name,
                    )?;
                    let mut instructions = converted.into_instructions();
                    // Large areas carry their triangulation into the cell
                    // cache, so later loads skip the earcut run.
                    let projection = template.scaler.projection();
                    for instruction in &mut instructions {
                        if let DrawingInstruction::Area(area) = instruction {
                            area.triangulation =
                                triangulate(area, projection, STORED_TRIANGULATION_MIN_VERTICES)
                                    .map(Arc::new);
                        }
                    }
                    // Stable per-cell order by the global sort key: the
                    // scene's stable sort then only merges cell runs.
                    instructions.sort_by_cached_key(DrawingInstruction::render_order);
                    ferrite_render::intern_line_styles(&mut instructions);
                    let written = cache.is_some_and(|(caches, path)| {
                        write_cell_cache(caches.store, path, &instructions)
                    });
                    export_precomputed(precomputed.as_ref(), fc, pc, cell_index, &instructions);
                    Ok(CellPortrayal::Fresh {
                        instructions,
                        results: results.len(),
                        written,
                    })
                })()
            };
            if result.is_err() {
                first_error.fetch_min(position, Ordering::Relaxed);
            }
            if let Ok(mut slot) = results[position].lock() {
                *slot = Some(result);
            }
        }
    });

    let mut total_results = 0;
    let mut cached_cells = 0;
    let mut written_cells = false;
    for slot in results {
        let result = slot
            .into_inner()
            .ok()
            .flatten()
            .context("Lua portrayal worker stopped early")?;
        match result? {
            CellPortrayal::Cached(instructions) => {
                cached_cells += 1;
                render_context.append_converted(instructions);
            }
            CellPortrayal::Fresh {
                instructions,
                results,
                written,
            } => {
                total_results += results;
                written_cells |= written;
                render_context.append_converted(instructions);
            }
        }
    }

    info!(
        "Lua portrayal complete: {} total results, {} cell(s) from cache",
        total_results, cached_cells
    );
    if written_cells {
        if let Some(caches) = &cell_caches {
            caches.store.prune();
        }
    }
    Ok(())
}
