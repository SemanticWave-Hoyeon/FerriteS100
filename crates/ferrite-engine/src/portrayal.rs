//! S-101 cell portrayal through the catalogue's Lua rules, with per-cell
//! instruction caches.
use crate::portrayal_cache::{encode_cache_payload, verify_and_deserialize_cache, wrap_cache};
use crate::{primary_viewing_layers, secure_instruction_cache};
use anyhow::{Context, Result};
use ferrite_feature_catalog::BoundFeatureCatalogue;
use ferrite_lua::{ContextParameters as LuaContextParameters, PortrayalEngine, TypeCatalogue};
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_render::host_settings::SettingsState;
use ferrite_render::{DrawingInstruction, RenderContext};
use ferrite_s100_core::S101Cell;
use ferrite_s101::convert_lua_results_for_cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
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
    )
}

/// Which cells to portray (None: all) and where their caches live (None: none).
#[derive(Default, Clone, Copy)]
pub struct CellPortrayalSelection<'a> {
    pub caches: Option<&'a [Option<PathBuf>]>,
    pub resident: Option<&'a [bool]>,
}

/// A per-cell portrayal: fresh Lua results, or instructions from the cache.
/// Smaller areas triangulate faster than their stored indices would load.
pub const STORED_TRIANGULATION_MIN_VERTICES: usize = 32;

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
pub fn read_cell_cache(path: &Path, cell: &S101Cell) -> Option<Vec<DrawingInstruction>> {
    let cache_time = fs::metadata(path).ok()?.modified().ok()?;
    if fs::metadata(&cell.file_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .is_some_and(|t| t > cache_time)
    {
        return None;
    }
    let data = secure_instruction_cache::read(path).ok()?;
    match verify_and_deserialize_cache(&data) {
        Ok(instructions) => {
            secure_instruction_cache::mark_used(path);
            Some(instructions)
        }
        Err(error) => {
            warn!("Cell cache rejected: {error}. Regenerating.");
            None
        }
    }
}

pub fn write_cell_cache(path: &Path, instructions: &[DrawingInstruction]) -> bool {
    if instructions.len() as u64 > secure_instruction_cache::MAX_INSTRUCTIONS {
        return false;
    }
    match encode_cache_payload(instructions) {
        Ok(payload) => match secure_instruction_cache::write_atomic(path, &wrap_cache(&payload)) {
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
    cell_caches: Option<&[Option<PathBuf>]>,
    triangulate: AreaTriangulator,
) -> Result<()> {
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

    // Cells are portrayed independently (each in its own engine state, as
    // before), so they run on several threads; results are converted strictly
    // in cell order, as soon as the next cell is ready, so instruction order
    // and the first reported error are unchanged and few results are held.
    // Cells are not split by feature: portrayal rules keep state across the
    // features of one run (e.g. text offsets of co-located lights), and a
    // split changes their output.
    let cells: Vec<(usize, &S101Cell)> = cells.into_iter().collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8)
        .min(cells.len())
        .max(1);
    // Largest cells start first so one big cell does not finish last on its
    // own; results are still assembled strictly in cell order.
    let mut schedule: Vec<usize> = (0..cells.len()).collect();
    schedule.sort_by_key(|position| std::cmp::Reverse(cells[*position].1.features.len()));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let abort = std::sync::atomic::AtomicBool::new(false);
    // Conversion only appends (filtered by viewing group), so each cell is
    // converted on its worker into an empty context with the same settings.
    let template = render_context.empty_for_rebuild();
    let mut total_results = 0;
    let mut cached_cells = 0;
    let mut written_cells = false;
    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = mpsc::channel::<(usize, Result<CellPortrayal>)>();
        for _ in 0..workers {
            let sender = sender.clone();
            let (cells, schedule, next, abort, context, template) =
                (&cells, &schedule, &next, &abort, &context, &template);
            let new_engine = &new_engine;
            scope.spawn(move || {
                let mut engine = None;
                loop {
                    let claimed = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if claimed >= cells.len() || abort.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let position = schedule[claimed];
                    let (cell_index, cell) = cells[position];
                    let cache = cell_caches
                        .and_then(|paths| paths.get(cell_index))
                        .and_then(Option::as_deref);
                    if let Some(mut instructions) =
                        cache.and_then(|path| read_cell_cache(path, cell))
                    {
                        // Renumber exactly as convert_lua_results_for_cell would:
                        // the cell index, and the S-101 dependency namespace.
                        let index = u32::try_from(cell_index).ok();
                        for instruction in &mut instructions {
                            instruction.set_cell_index(index);
                            if let Some(dependency) = instruction.dependency_mut() {
                                dependency.namespace = cell_index as u64;
                            }
                        }
                        // Decoding gives every line its own style allocation;
                        // sharing them here leaves the scene-wide interning
                        // only a few distinct allocations per cell.
                        ferrite_render::intern_line_styles(&mut instructions);
                        if sender
                            .send((position, Ok(CellPortrayal::Cached(instructions))))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    let result = (|| {
                        if engine.is_none() {
                            engine = Some(new_engine()?);
                        }
                        let engine = engine.as_mut().expect("created above");
                        let results = engine
                            .process_owned_cell(
                                ferrite_lua::CellData::from_cell(cell),
                                context.clone(),
                            )
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
                                area.triangulation = triangulate(
                                    area,
                                    projection,
                                    STORED_TRIANGULATION_MIN_VERTICES,
                                )
                                .map(Arc::new);
                            }
                        }
                        // Stable per-cell order by the global sort key: the
                        // scene's stable sort then only merges cell runs.
                        instructions.sort_by_cached_key(DrawingInstruction::render_order);
                        ferrite_render::intern_line_styles(&mut instructions);
                        let written =
                            cache.is_some_and(|path| write_cell_cache(path, &instructions));
                        Ok(CellPortrayal::Fresh {
                            instructions,
                            results: results.len(),
                            written,
                        })
                    })();
                    if sender.send((position, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        let mut pending = std::collections::BTreeMap::new();
        let mut converted = 0;
        for (position, result) in receiver {
            pending.insert(position, result);
            while let Some(result) = pending.remove(&converted) {
                match result {
                    Ok(CellPortrayal::Cached(instructions)) => {
                        cached_cells += 1;
                        render_context.append_converted(instructions);
                        converted += 1;
                    }
                    Ok(CellPortrayal::Fresh {
                        instructions,
                        results,
                        written,
                    }) => {
                        total_results += results;
                        written_cells |= written;
                        render_context.append_converted(instructions);
                        converted += 1;
                    }
                    Err(error) => {
                        abort.store(true, std::sync::atomic::Ordering::Relaxed);
                        return Err(error);
                    }
                }
            }
        }
        anyhow::ensure!(
            converted == cells.len(),
            "Lua portrayal worker stopped early"
        );
        Ok(())
    })?;

    info!(
        "Lua portrayal complete: {} total results, {} cell(s) from cache",
        total_results, cached_cells
    );
    if written_cells {
        secure_instruction_cache::prune_root();
    }
    Ok(())
}
