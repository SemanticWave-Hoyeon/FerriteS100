//! Headless use of the S-100 chart engine: engine set-up with the GPU
//! renderer's helpers, plain-data reports of opened datasets, and offscreen
//! rendering of prepared scenes ([`OffscreenChartRenderer`]). The `ferrite`
//! command-line tool and the C ABI (`ferrite-capi`) are thin layers over
//! this crate and [`ChartEngine`].
use anyhow::{ensure, Context, Result};
use ferrite_engine::chart_engine::{
    ChartEngine, DatasetSource, DatasetSummary, EngineConfig, IssueSeverity, OpenReport,
    ValidationIssue,
};
use ferrite_engine::pick::FeatureReport;
use ferrite_engine::platform::Platform;
use ferrite_engine::s102_scene::BathymetrySummary;
use ferrite_engine::scene::PreparedScene;
use ferrite_gpu::headless::{HeadlessSurface, HeadlessUi};
use ferrite_gpu::{PhysicalSize, SurfaceHost, SymbolCache, UiLayer, WgpuRenderer};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use image::RgbaImage;

/// An engine configuration for `platform` and `catalogues` with the GPU
/// renderer's stored area triangulation and overlay symbol registration
/// (S-124/S-129 symbols are drawn by ferrite-gpu).
pub fn engine_config(platform: Platform, catalogues: impl Into<PathBuf>) -> EngineConfig {
    let mut config = EngineConfig::new(platform, catalogues);
    config.triangulate = ferrite_gpu::compute_area_triangulation;
    config.register_overlay_symbols = |id, bound| {
        ferrite_gpu::register_overlay_symbol_source(
            id,
            ferrite_gpu::OverlaySymbolSource::from_bound(bound),
        )
    };
    config
}

/// Dataset sources for command-line paths: each folder on its own, all
/// files together (so update chains given as files stay one chain).
pub fn sources_for_paths(paths: &[PathBuf]) -> Vec<DatasetSource> {
    let (folders, files): (Vec<_>, Vec<_>) = paths.iter().cloned().partition(|p| p.is_dir());
    let mut sources: Vec<_> = folders.into_iter().map(DatasetSource::Folder).collect();
    if !files.is_empty() {
        sources.push(DatasetSource::Files(files));
    }
    sources
}

/// Open every source and merge the reports.
pub fn open_all(engine: &mut ChartEngine, sources: Vec<DatasetSource>) -> Result<OpenReport> {
    let mut merged = OpenReport::default();
    for source in sources {
        let report = engine.open_blocking(source)?;
        merged.datasets.extend(report.datasets);
        merged.issues.extend(report.issues);
        merged.notices.extend(report.notices);
    }
    Ok(merged)
}

pub fn severity_name(severity: IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Warning => "warning",
        IssueSeverity::Error => "error",
    }
}

pub fn issue_json(issue: &ValidationIssue) -> Value {
    json!({
        "severity": severity_name(issue.severity),
        "subject": issue.subject,
        "check": issue.check,
        "message": issue.message,
    })
}

/// A dataset with its feature counts by feature type.
pub fn dataset_json(engine: &ChartEngine, dataset: &DatasetSummary) -> Value {
    let (kind, index) = match dataset.id {
        ferrite_engine::chart_engine::DatasetId::Cell(i) => ("cell", i as u64),
        ferrite_engine::chart_engine::DatasetId::Overlay(i) => ("overlay", u64::from(i)),
    };
    json!({
        "id": {"kind": kind, "index": index},
        "product": dataset.product,
        "name": dataset.name,
        "source": dataset.source.as_ref().map(|p| p.display().to_string()),
        "edition": dataset.edition,
        "features": dataset.features,
        "provenance": dataset.provenance,
        "feature_types": engine.feature_types(dataset.id),
    })
}

pub fn feature_json(report: &FeatureReport) -> Value {
    json!({
        "feature_type": report.feature_type,
        "feature_id": report.feature_id,
        "foid": report.foid,
        "cell_index": report.cell_index,
        "primitive": report.primitive_type,
        "source": report.source,
        "definition": report.definition,
        "attributes": report
            .attributes
            .iter()
            .map(|(name, value)| json!([name, value]))
            .collect::<Vec<_>>(),
    })
}

/// An opened S-102 file.
pub fn bathymetry_json(summary: &BathymetrySummary) -> Value {
    json!({
        "source": summary.source.display().to_string(),
        "product_specification": summary.product_specification,
        "instances": summary.instances,
        "grid": [summary.grid.0, summary.grid.1],
        "issue_date": summary.issue_date,
        "vertical_datums": summary.vertical_datums,
        "quality_records": summary.quality_records,
        "bounds": summary.bounds.map(|b| json!({
            "west": b.min_x, "south": b.min_y, "east": b.max_x, "north": b.max_y,
        })),
        "inspection_only": summary.inspection_only,
        "provenance": summary.provenance,
    })
}

/// Every opened dataset (S-102 files under `bathymetry`) with the open
/// report's issues and notices.
pub fn inspection_json(engine: &ChartEngine, report: &OpenReport) -> Value {
    json!({
        "datasets": engine
            .datasets()
            .iter()
            .map(|d| dataset_json(engine, d))
            .collect::<Vec<_>>(),
        "bathymetry": engine.bathymetry().iter().map(bathymetry_json).collect::<Vec<_>>(),
        "issues": report.issues.iter().map(issue_json).collect::<Vec<_>>(),
        "notices": report.notices,
        "catalogue_notes": engine.catalogue_errors(),
    })
}

/// Colour profile names of the bound portrayal catalogue, sorted.
pub fn palettes(engine: &ChartEngine) -> Vec<String> {
    let mut names: Vec<_> = engine
        .state()
        .pc
        .color_profiles
        .profiles
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

/// Emits prepared [`ChartEngine`] scenes into a ferrite-gpu renderer: the
/// same emission the viewer performs for a frame (palette, display settings,
/// viewing groups, per-catalogue cell resources, chart text, S-102
/// bathymetry rasters). The renderer then draws offscreen
/// ([`OffscreenChartRenderer`]) or presents to a surface (the web canvas of
/// `ferrite-wasm`).
#[derive(Default)]
pub struct SceneEmitter {
    /// Symbols of one catalogue and palette (rasterized per palette).
    symbols: Option<(([u8; 32], String), SymbolCache)>,
    /// The bathymetry scene whose tiles the renderer holds
    /// ([`ferrite_engine::s102_scene::BathymetryScene::id`]).
    rasters: Option<Arc<()>>,
}

impl SceneEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit `scene` (prepared by `engine` for a view of the renderer's size)
    /// at camera zoom `zoom` (1: all data; the viewer's zoom level, used for
    /// symbol decluttering) into `renderer`'s next frame.
    pub fn emit<U: UiLayer>(
        &mut self,
        renderer: &mut WgpuRenderer<U>,
        engine: &ChartEngine,
        scene: &mut PreparedScene,
        zoom: f64,
    ) -> Result<()> {
        let state = engine.state();
        let display = engine.display();
        let profile = display.profile.as_str();
        let pc = &state.pc;
        renderer.set_settings(display.settings.clone());
        renderer.set_color_profile(profile);
        renderer.background_color = ferrite_s101::lookup_pc_color(pc, "DEPDW", profile);
        // Producer reference scales; zero denotes unknown (as the viewer).
        let reference_scale = state
            .cells
            .iter()
            .filter_map(|cell| {
                ferrite_s101::coverage_scale::dataset_reference_scale(cell)
                    .ok()
                    .flatten()
            })
            .min();
        renderer.set_compilation_scale(reference_scale.unwrap_or(0));
        let key = (*pc.source_digest(), profile.to_string());
        if self.symbols.as_ref().is_none_or(|(k, _)| *k != key) {
            renderer.clear_symbol_textures();
            self.symbols = Some((
                key,
                SymbolCache::new_with_pattern_contract(
                    pc.root_path.join("Symbols"),
                    pc.sources(),
                    ferrite_s101::shallow_pattern_contract(pc),
                ),
            ));
        }
        let symbols = &mut self.symbols.as_mut().expect("set above").1;
        let visible = state.viewing_groups(&display.settings)?;
        let color_profile = pc
            .color_profiles
            .profiles
            .get(profile)
            .with_context(|| format!("Portrayal catalogue has no colour profile {profile}"))?;
        let context = &mut scene.context;
        renderer.set_zoom_level(zoom);
        renderer.prefill_area_triangulations(context);
        renderer.begin_frame();
        Self::emit_bathymetry(
            &mut self.rasters,
            renderer,
            engine,
            &mut scene.bathymetry,
            &context.scaler,
        )?;
        renderer.set_lon_wrap_pixels(360.0 * context.scaler.scale_x() as f32);
        if state.requires_cell_resources(&scene.catalogue_registry, &state.fc, pc)? {
            // Cells of other catalogue editions draw with their own owners.
            let bindings = state
                .cell_resource_bindings(&scene.catalogue_registry, &display.settings)?
                .into_iter()
                .map(|binding| ferrite_gpu::CellPortrayalResourceBinding {
                    cell_index: binding.cell_index,
                    catalogue: binding.catalogue,
                    shallow_pattern: binding.shallow_pattern,
                })
                .collect::<Vec<_>>();
            let mut resources =
                ferrite_gpu::CellPortrayalResources::prepare(&bindings, profile, 4096)?;
            for index in 0..state.cells.len() {
                let groups = state.cell_viewing_groups(
                    &scene.catalogue_registry,
                    index,
                    &display.settings,
                )?;
                resources.capture_viewing_groups_for_cell(index, Some(groups))?;
            }
            renderer.try_add_instructions_with_resource_owners(
                context,
                &mut resources,
                Some(&visible),
            )?;
        } else {
            symbols.prefetch_symbols(
                context.raw_instructions().iter().filter_map(|i| match i {
                    ferrite_render::DrawingInstruction::Point(point) => {
                        Some(point.symbol_ref.as_str())
                    }
                    _ => None,
                }),
                color_profile,
            );
            renderer.try_add_instructions_with_symbols(
                context,
                Some(symbols),
                Some(color_profile),
                Some(&visible),
            )?;
        }
        Ok(())
    }
}

impl SceneEmitter {
    /// Publish the S-102 tiles of a scene as the viewer's raster scene
    /// publication (staged, prepared for the scene's camera with the display
    /// mode's viewing groups, committed). The tiles move to the GPU (CPU
    /// pixels are released, as in the viewer), so a scene with bathymetry is
    /// drawn by one renderer: later frames of the same scene (re-projected
    /// views) re-project the held rasters, and another renderer refuses it.
    /// A scene without bathymetry clears them.
    fn emit_bathymetry<U: UiLayer>(
        rasters: &mut Option<Arc<()>>,
        renderer: &mut WgpuRenderer<U>,
        engine: &ChartEngine,
        bathymetry: &mut ferrite_engine::s102_scene::BathymetryScene,
        scaler: &ferrite_render::Scaler,
    ) -> Result<()> {
        // S-98 Level 2: an active combination suppressing S-102 clips the
        // rasters to the area outside the displayed S-101 coverage.
        let state = engine.state();
        let suppress = engine.display().settings.interoperability_enabled
            && state.ic.as_ref().is_some_and(|ic| {
                let mut displayed = Vec::new();
                if !state.cells.is_empty() {
                    displayed.push("S-101");
                }
                if !state.bathymetry_bounds.is_empty() {
                    displayed.push("S-102");
                }
                ic.catalogue
                    .select(state.ic_combination.as_deref(), &displayed)
                    .ok()
                    .and_then(|selection| ic.catalogue.active_combination(selection))
                    .is_some_and(|c| c.suppressed.iter().any(|l| l.product == "S-102"))
            });
        renderer.set_raster_layer_suppression(suppress);
        if bathymetry.tiles.is_empty() {
            if rasters
                .as_ref()
                .is_some_and(|id| Arc::ptr_eq(id, &bathymetry.id))
            {
                renderer.update_raster_view(scaler);
                return Ok(());
            }
            if rasters.take().is_some() {
                renderer.clear_raster_layers();
            }
            // Bathymetry whose tiles another renderer took: not silently
            // drawn without it.
            ensure!(
                bathymetry.viewing_groups.is_none(),
                "The S-102 tiles of this scene went to another renderer; prepare the scene again"
            );
            return Ok(());
        }
        *rasters = None;
        Self::publish_bathymetry(renderer, bathymetry, scaler)?;
        *rasters = Some(Arc::clone(&bathymetry.id));
        Ok(())
    }

    #[cfg(feature = "s102-pure")]
    fn publish_bathymetry<U: UiLayer>(
        renderer: &mut WgpuRenderer<U>,
        bathymetry: &mut ferrite_engine::s102_scene::BathymetryScene,
        scaler: &ferrite_render::Scaler,
    ) -> Result<()> {
        use ferrite_engine::s102_scene::BathymetryTile;
        let tiles = std::mem::take(&mut bathymetry.tiles);
        let batch = renderer.stage_s102_raster_batch(scaler, |upload| {
            for tile in tiles {
                upload(match tile {
                    BathymetryTile::Bound(material) => {
                        ferrite_gpu::S102RasterMaterial::Bound(material)
                    }
                    BathymetryTile::Composed(layer) => {
                        ferrite_gpu::S102RasterMaterial::LegacyIc(layer)
                    }
                })?;
            }
            Ok::<_, anyhow::Error>(())
        })?;
        let raster = renderer.prepare_raster_material_publication(batch, true, None)?;
        let scene = renderer.prepare_raster_scene_publication_with_groups(
            raster,
            Some(bathymetry.viewing_groups.clone().unwrap_or_default()),
        )?;
        renderer.validate_raster_scene_publication(&scene)?;
        renderer.commit_raster_scene_publication(scene);
        Ok(())
    }

    #[cfg(not(feature = "s102-pure"))]
    fn publish_bathymetry<U: UiLayer>(
        _: &mut WgpuRenderer<U>,
        bathymetry: &mut ferrite_engine::s102_scene::BathymetryScene,
        _: &ferrite_render::Scaler,
    ) -> Result<()> {
        // Without s102-pure the engine builds only composed tiles, and only
        // with S-102 support; nothing reaches here.
        anyhow::bail!(
            "S-102 tiles ({}) need ferrite-cli's s102-pure feature",
            bathymetry.tiles.len()
        )
    }
}

/// Renders prepared [`ChartEngine`] scenes into RGBA images with
/// ferrite-gpu on an offscreen target (no window, no GUI toolkit): the same
/// emission the viewer performs for a frame ([`SceneEmitter`]), drawn as its
/// chart image export draws.
pub struct OffscreenChartRenderer {
    renderer: WgpuRenderer<HeadlessUi>,
    surface: Arc<HeadlessSurface>,
    emitter: SceneEmitter,
}

impl OffscreenChartRenderer {
    /// A renderer for `width` x `height` physical pixels at `scale_factor`
    /// physical pixels per logical pixel.
    pub fn new(width: u32, height: u32, scale_factor: f64) -> Result<Self> {
        ensure!(width > 0 && height > 0, "Empty render size");
        ensure!(
            scale_factor.is_finite() && scale_factor > 0.,
            "Invalid scale factor"
        );
        let (renderer, surface) = pollster::block_on(WgpuRenderer::new_headless_with(
            PhysicalSize::new(width, height),
            scale_factor,
        ))
        .context("No GPU adapter for offscreen rendering")?;
        Ok(Self {
            renderer,
            surface,
            emitter: SceneEmitter::new(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        let size = self.surface.inner_size();
        (size.width, size.height)
    }

    /// Physical pixels per logical pixel.
    pub fn scale_factor(&self) -> f64 {
        self.surface.scale_factor()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let size = PhysicalSize::new(width.max(1), height.max(1));
        self.surface.set_size(size);
        self.renderer.resize(size);
    }

    /// GPU adapter name.
    pub fn adapter(&self) -> &str {
        &self.renderer.state.gpu_name
    }

    /// Draw `scene` (prepared by `engine` for a view of this renderer's
    /// size) at camera zoom `zoom` (1: all data; the viewer's zoom level,
    /// used for symbol decluttering) and read the image back.
    pub fn render(
        &mut self,
        engine: &ChartEngine,
        scene: &mut PreparedScene,
        zoom: f64,
    ) -> Result<RgbaImage> {
        self.emitter.emit(&mut self.renderer, engine, scene, zoom)?;
        Ok(self.renderer.render_offscreen()?)
    }
}

/// Write `image` as PNG.
pub fn save_png(image: &RgbaImage, path: &Path) -> Result<()> {
    image
        .save_with_format(path, image::ImageFormat::Png)
        .with_context(|| format!("Cannot write {}", path.display()))
}
