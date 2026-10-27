//! S-102 bathymetry of the product layer: opening captured inputs, the
//! opened-file reports, and the viewer's raster portrayal of the loaded
//! coverages for a prepared scene (moved from the engine's `s102_scene`).
//!
//! Opening decodes each captured input (read once through the platform data
//! source or the authenticated in-memory snapshot) with the pure-Rust HDF5
//! reader, so it behaves the same on every platform and the decoded bytes
//! cannot change underneath. Portrayal follows the viewer
//! (`load_bathymetry` / raster rebuild): per input file, the raw coverage or
//! the conservative datum mosaic under the file's depth policy, tiled in
//! `edge`-sized windows, coloured by the S-102 portrayal catalogue rule for
//! the display's contours and shades, or composed with an active
//! interoperability catalogue.
use super::depth_policy::DepthPolicy;
use super::input_capture::CapturedInput;
use super::S102Layer;
use crate::trial_inspection::TrialInspection;
use crate::{
    h5, BathymetryCoverage, BathymetryDisplayPreset, BathymetryPortrayal, ConservativeCoverage,
    DatumCoverage, DepthSettings,
};
use anyhow::{ensure, Context, Result};
use ferrite_interoperability::{Assignment, AuthenticatedCatalogue, Selection};
use ferrite_kernel::depth_selection::DepthReference;
use ferrite_kernel::NumericCoverageSource;
use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
use ferrite_product_api::platform::{CacheStore, DataSource};
use ferrite_render::host_settings::{DisplayMode, SettingsState};
use ferrite_render::{DisplayPlane, GeoBounds, RasterDrawOrder, RasterLayer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Host options for opening and portraying S-102.
#[derive(Clone, Debug)]
pub struct S102Options {
    /// Depth adjustment configuration (JSON of [`super::depth_policy`]); None:
    /// each file is shown in its own datum.
    pub depth_adjustments: Option<Arc<[u8]>>,
    /// Raster tile edge in samples (keep within the GPU texture limit).
    pub tile_edge: usize,
    /// With Lua portrayal, use the PC rule's instruction string from the
    /// cache store when an intact entry for this PC and these contour
    /// settings exists ([`S102Layer::instruction_entry`]), and write it after
    /// evaluating the rule otherwise; the rule is deterministic in those
    /// inputs. Hosts with a persistent cache (the web's IndexedDB) then
    /// skip Lua for S-102. Off by default.
    pub reuse_rule_instructions: bool,
}
impl Default for S102Options {
    fn default() -> Self {
        Self {
            depth_adjustments: None,
            tile_edge: DEFAULT_TILE_EDGE,
            reuse_rule_instructions: false,
        }
    }
}

/// One opened S-102 file.
#[derive(Clone, Debug)]
pub struct BathymetrySummary {
    pub source: PathBuf,
    /// Product specification of the file.
    pub product_specification: String,
    /// Coverage instances (0 for an inspected trial file).
    pub instances: usize,
    /// Grid size of the first instance (columns, rows).
    pub grid: (usize, usize),
    /// Issue date (S-102 3.0) or lexical issue date (trial).
    pub issue_date: String,
    /// Vertical datum codes of the instances.
    pub vertical_datums: Vec<u32>,
    pub quality_records: usize,
    /// Portrayed extent (None for trial inspections).
    pub bounds: Option<GeoBounds>,
    /// "inspection only" for S-102 2.1 trial files.
    pub inspection_only: bool,
    pub provenance: String,
}

/// S-102 inputs decoded by an open job, not yet installed.
#[derive(Default)]
pub struct OpenedS102 {
    pub inputs: Vec<(PathBuf, S102Input, String)>,
}
impl OpenedS102 {
    pub fn len(&self) -> usize {
        self.inputs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What installing decoded inputs reported, in order: notices, load errors
/// (subject, message) and the summary of every newly opened file.
#[derive(Default)]
pub struct Installed {
    pub notices: Vec<String>,
    pub errors: Vec<(String, String)>,
    pub summaries: Vec<BathymetrySummary>,
}

/// Load the optional S-102 portrayal catalogue (`PC/S-102`) through the data
/// source; failures are recorded in the catalogue status.
/// Where the S-102 catalogues are installed (catalogue load errors).
pub const CATALOGUE_LAYOUT: ferrite_product_api::CatalogueLayout =
    ferrite_product_api::CatalogueLayout {
        feature: "<app>/Catalogues/FC/S-102/*.xml",
        portrayal: "<app>/Catalogues/PC/S-102/ with ColorProfiles/, Rules/",
    };

pub fn load_catalogues(layer: &mut S102Layer, data: &dyn DataSource, catalogues: &Path) {
    let pc_path = catalogues.join("PC/S-102");
    if !data.is_dir(&pc_path) {
        return;
    }
    // As the viewer's `PortrayalCatalogue::load_bound` (the S-102 PC has no
    // symbols, unlike the S-101 catalogue check), through the data source.
    let loaded = data
        .portrayal_sources(&pc_path)
        .and_then(|sources| {
            ferrite_portrayal_catalog::PortrayalCatalogue::bound_from_sources(&pc_path, sources)
        })
        .map_err(anyhow::Error::from)
        .and_then(|pc| {
            anyhow::ensure!(
                pc.product_id == "S-102",
                "Expected S-102 PC, found {}",
                pc.product_id
            );
            Ok(pc)
        });
    match loaded {
        Ok(pc) => layer.set_pc(pc, "Parsed installed S-102 Portrayal Catalogue"),
        Err(error) => layer.pc_failed(&pc_path, format!("Load failed: {error:#}")),
    }
}

/// Decode S-102 inputs: each is authorized (`authorize`, the open job's
/// signature policy), captured once through the data source under the
/// aggregate input budget and decoded. Per-input failures are returned with
/// their path, in input order.
pub fn open_inputs(
    paths: Vec<PathBuf>,
    data: &dyn DataSource,
    authorize: &dyn Fn(&[PathBuf]) -> Result<ferrite_security::AuthorizedDatasets>,
    signatures_required: bool,
    options: &S102Options,
    cancel: &AtomicBool,
    failures: &mut Vec<(PathBuf, anyhow::Error)>,
) -> Result<OpenedS102> {
    ensure!(
        paths.len() <= 512,
        "S102 publication exceeds 512-file work budget"
    );
    let mut out = OpenedS102::default();
    // Receiver snapshot admission policy, as the viewer's.
    let mut budget = MAX_INPUT_BYTES;
    for path in paths {
        ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
        let opened = authorize(std::slice::from_ref(&path)).and_then(|report| {
            let capture =
                CapturedInput::capture_in(data, &report, &path, signatures_required, &mut budget)?;
            let provenance = if capture.authenticated.is_some() {
                "signature verified"
            } else {
                "signature not verified"
            };
            Ok((
                open_captured(&path, capture, options.depth_adjustments.as_deref())?,
                provenance.to_string(),
            ))
        });
        match opened {
            Ok((input, provenance)) => out.inputs.push((path, input, provenance)),
            Err(error) => failures.push((path, error)),
        }
    }
    Ok(out)
}

impl S102Layer {
    /// Publish decoded S-102 inputs (as the viewer's commit: coverages,
    /// depth policies and extents per file; trials apart).
    pub fn install(&mut self, opened: OpenedS102) -> Installed {
        let mut out = Installed::default();
        for (path, input, provenance) in opened.inputs {
            let loaded = self.bathymetry.iter().any(|(p, _, _)| *p == path)
                || self.trials.iter().any(|(p, _)| *p == path);
            if loaded {
                out.notices
                    .push(format!("Already open: {}", path.display()));
                continue;
            }
            if matches!(input, S102Input::Coverage { .. }) && self.pc.is_none() {
                // The viewer refuses bathymetry it cannot portray.
                out.errors.push((
                    path.display().to_string(),
                    format!(
                        "S-102 PC unavailable ({}); install PC/S-102 in the catalogue folder",
                        self.pc_status
                            .validation_message
                            .as_deref()
                            .unwrap_or("not installed")
                    ),
                ));
                continue;
            }
            match input {
                S102Input::Coverage {
                    coverages,
                    policy,
                    capture,
                    bounds,
                } => {
                    for c in &coverages {
                        if c.vertical_cs_reinterpreted_as_depth {
                            out.notices.push(format!(
                                "S-102 compatibility: {} declares verticalCS 6499 (height up) but is \
                                 read as depth-down because {}=1",
                                path.display(),
                                crate::VERTICAL_CS_6499_AS_DEPTH_ENV
                            ));
                        }
                        if !c.root_enclosure.encoding_compatible() {
                            out.notices.push(format!(
                                "S-102 {} {}: root geographic enclosure {:?}; raw values retained",
                                path.display(),
                                c.instance_name,
                                c.root_enclosure
                            ));
                        }
                    }
                    let snapshot = capture.authenticated.clone();
                    self.insert_coverages(path.clone(), bounds, policy, None, coverages, snapshot);
                    self.provenance.insert(path.clone(), provenance);
                }
                S102Input::Trial(inspection) => {
                    out.notices.push(format!(
                        "S-102 2.1 trial opened for inspection only (not used for depth): {}",
                        path.display()
                    ));
                    self.add_trial(path.clone(), inspection);
                    self.provenance.insert(path.clone(), provenance);
                }
            }
            if let Some(summary) = self.summaries().into_iter().find(|s| s.source == path) {
                out.summaries.push(summary);
            }
        }
        out
    }

    /// Every opened S-102 file.
    pub fn summaries(&self) -> Vec<BathymetrySummary> {
        let mut out = Vec::new();
        for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0) {
            let (path, first, _) = &group[0];
            let g = ferrite_kernel::CoverageSource::geometry(first);
            out.push(BathymetrySummary {
                source: path.clone(),
                product_specification: first.product_specification.clone(),
                instances: group.len(),
                grid: (g.width, g.height),
                issue_date: first.issue.date.clone(),
                vertical_datums: group.iter().map(|(_, c, _)| c.vertical_datum).collect(),
                quality_records: first.quality.as_ref().map_or(0, |q| q.record_count()),
                bounds: self.bathymetry_bounds.get(path).copied(),
                inspection_only: false,
                provenance: self.provenance.get(path).cloned().unwrap_or_default(),
            });
        }
        for (path, inspection) in &self.trials {
            let m = inspection.metadata();
            out.push(BathymetrySummary {
                source: path.clone(),
                product_specification: m.product_specification.clone(),
                instances: 0,
                grid: (m.width as usize, m.height as usize),
                issue_date: m.issue_date_lexical.clone(),
                vertical_datums: match m.vertical_datum {
                    crate::legacy_trial::TrialVerticalDatum::Encoded(code) => vec![code],
                    crate::legacy_trial::TrialVerticalDatum::Missing => Vec::new(),
                },
                quality_records: 0,
                bounds: None,
                inspection_only: true,
                provenance: self.provenance.get(path).cloned().unwrap_or_default(),
            });
        }
        out
    }

    /// Cache-store entry of the S-102 PC rule's instruction string for the
    /// contour settings of `settings` (read by preparations of loaded
    /// bathymetry with precomputed portrayal or
    /// [`S102Options::reuse_rule_instructions`]); None without loaded S-102,
    /// S-102 PC or cache store.
    pub fn instruction_entry(
        &self,
        store: &dyn CacheStore,
        settings: &SettingsState,
    ) -> Option<PathBuf> {
        if self.bathymetry.is_empty() {
            return None;
        }
        instructions_key(store, self.pc.as_ref()?, depth_settings(settings))
    }
}

/// Default raster tile edge (the viewer's default before its GPU limit).
pub const DEFAULT_TILE_EDGE: usize = 2048;
/// Largest S-102 input read into memory.
pub const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;

/// One portrayed tile of S-102 bathymetry.
pub enum BathymetryTile {
    /// Level-0 tile coloured by the S-102 PC rule, sealed with the
    /// evaluation that produced it (the GPU's bound material).
    Bound(crate::BoundBathymetryMaterial),
    /// Tile composed with an interoperability catalogue.
    Composed(RasterLayer),
}
impl BathymetryTile {
    pub fn bounds(&self) -> GeoBounds {
        match self {
            Self::Bound(material) => material.bounds(),
            Self::Composed(layer) => layer.bounds,
        }
    }
}

/// [`ferrite_product_api::RendererExtension::kind`] of the S-102 raster
/// tiles; the payload is a [`BathymetryScene`].
pub const BATHYMETRY_EXTENSION: &str = "S-102/bathymetry-tiles";

/// The S-102 part of a prepared scene (the payload of the
/// [`BATHYMETRY_EXTENSION`] renderer extension).
pub struct BathymetryScene {
    /// Tiles in input order, each input's tiles row-major by window. A
    /// renderer moving them to the GPU takes them out (a later frame of the
    /// same, re-projected scene then finds none and keeps what it holds).
    pub tiles: Mutex<Vec<BathymetryTile>>,
    /// Viewing groups enabled for the display mode (the renderer shows a
    /// tile only when all its groups are enabled).
    pub viewing_groups: HashSet<u32>,
}
impl BathymetryScene {
    /// The tiles, left empty.
    pub fn take_tiles(&self) -> Vec<BathymetryTile> {
        std::mem::take(&mut *self.tiles.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn is_empty(&self) -> bool {
        self.tiles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
    /// As a renderer extension.
    pub fn into_extension(self) -> ferrite_product_api::RendererExtension {
        ferrite_product_api::RendererExtension {
            kind: BATHYMETRY_EXTENSION,
            payload: Arc::new(self),
        }
    }
}

/// The bathymetry among a scene's renderer extensions.
pub fn bathymetry(
    extensions: &[ferrite_product_api::RendererExtension],
) -> Option<&BathymetryScene> {
    extensions
        .iter()
        .find(|e| e.kind == BATHYMETRY_EXTENSION)
        .and_then(|e| e.payload.downcast_ref())
}

/// Interoperability composition of the S-102 rasters.
pub struct Interoperability<'a> {
    pub catalogue: &'a AuthenticatedCatalogue,
    /// The selection for S-102 rasters being composed now (S-102 on
    /// display); evaluated after the PC rule.
    pub selection: &'a dyn Fn(&AuthenticatedCatalogue) -> Result<Selection>,
}

/// What a portrayal of the loaded bathymetry needs from the host.
pub struct S102Portrayal<'a> {
    /// Colour profile.
    pub profile: &'a str,
    pub settings: &'a SettingsState,
    /// Raster tile edge in samples.
    pub tile_edge: usize,
    /// [`S102Options::reuse_rule_instructions`].
    pub reuse_rule_instructions: bool,
    /// Evaluate the PC rule with Lua (the host's Lua portrayal source);
    /// otherwise its instruction string comes from the cache store.
    pub lua: bool,
    /// Write the rule's instruction string to the cache store.
    pub export: bool,
    pub cache: &'a dyn CacheStore,
    /// Active interoperability catalogue (settings permitting).
    pub interoperability: Option<Interoperability<'a>>,
}

/// A decoded S-102 input, not yet installed.
pub enum S102Input {
    /// S-102 3.0 bathymetric coverage instances of one file.
    Coverage {
        coverages: Vec<BathymetryCoverage>,
        policy: DepthPolicy,
        capture: CapturedInput,
        bounds: GeoBounds,
    },
    /// S-102 2.1 trial: metadata inspection only, never depth.
    Trial(TrialInspection),
}

/// Extent of the portrayed rasters of a grid (the union of
/// `BathymetryPortrayal::raster_window` bounds over all windows).
pub fn raster_bounds(g: &ferrite_kernel::GridGeometry) -> GeoBounds {
    let x0 = g.origin_x - g.spacing_x * 0.5;
    let x1 = g.origin_x + (g.width as f64 - 0.5) * g.spacing_x;
    let y0 = g.origin_y - g.spacing_y * 0.5;
    let y1 = g.origin_y + (g.height as f64 - 0.5) * g.spacing_y;
    GeoBounds::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

/// The numeric source the viewer portrays for one file: the raw single
/// instance under an identity policy, otherwise the conservative mosaic.
fn with_numeric<T>(
    coverages: &[BathymetryCoverage],
    policy: &DepthPolicy,
    f: impl FnOnce(&dyn NumericCoverageSource) -> Result<T>,
) -> Result<T> {
    let mosaic = ConservativeCoverage::new(
        coverages
            .iter()
            .map(|c| DatumCoverage {
                coverage: c,
                reference: DepthReference(c.vertical_datum as u64),
            })
            .collect(),
        DepthReference(policy.target as u64),
        policy,
    )?;
    let first = coverages.first().context("No S102 instances")?;
    if coverages.len() == 1 && policy.identity(first.vertical_datum) {
        f(first)
    } else {
        let g = mosaic.numeric_geometry();
        ensure!(
            g.width
                .checked_mul(g.height)
                .is_some_and(|n| n <= 64 * 1024 * 1024),
            "Composed depth output exceeds 64M-cell application work budget"
        );
        f(&mosaic)
    }
}

/// Decode one captured input. `adjustments`: the host's depth
/// adjustment configuration (JSON, see `s102_depth_policy`).
pub fn open_captured(
    path: &Path,
    capture: CapturedInput,
    adjustments: Option<&[u8]>,
) -> Result<S102Input> {
    let bytes = Arc::clone(capture.bytes().context("S-102 capture holds no bytes")?);
    let file = h5::File::open_bytes(Arc::clone(&bytes))
        .with_context(|| format!("Cannot open HDF5 {}", path.display()))?;
    let spec = crate::product_specification(&file)?;
    if crate::is_trial_product(&spec) {
        return Ok(S102Input::Trial(super::trial_worker::inspect_bytes(bytes)?));
    }
    ensure!(
        spec == crate::SUPPORTED_PRODUCT,
        "Skipped unsupported product {spec}: {}",
        path.display()
    );
    capture.verify()?;
    let coverages = BathymetryCoverage::open_file(file, None)?;
    capture.verify()?;
    let sha256 = capture.identity().1;
    let policy = DepthPolicy::from_bytes(
        adjustments,
        || Ok(sha256),
        &coverages
            .iter()
            .map(|c| c.vertical_datum)
            .collect::<Vec<_>>(),
    )?;
    let bounds = with_numeric(&coverages, &policy, |numeric| {
        let g = numeric.numeric_geometry();
        ensure!(
            g.horizontal_crs == 4326,
            "Map portrayal currently requires EPSG:4326"
        );
        ensure!(
            !numeric.requires_spatial_mask(),
            "Geometric domain portrayal requires continuous clipping; centroid flattening is unsupported"
        );
        Ok(raster_bounds(g))
    })?;
    Ok(S102Input::Coverage {
        coverages,
        policy,
        capture,
        bounds,
    })
}

/// Viewing groups of the display mode (the viewer's preset mapping).
pub fn viewing_groups(
    pc: &ferrite_portrayal_catalog::PortrayalCatalogue,
    mode: DisplayMode,
) -> Result<HashSet<u32>> {
    let preset = match mode {
        DisplayMode::Base => BathymetryDisplayPreset::Base,
        DisplayMode::Standard => BathymetryDisplayPreset::Standard,
        DisplayMode::All => BathymetryDisplayPreset::Other,
    };
    crate::bathymetry_viewing_groups_for_preset(pc, preset)
}

pub fn depth_settings(settings: &SettingsState) -> DepthSettings {
    DepthSettings {
        safety_contour: settings.safety_contour,
        shallow_contour: settings.shallow_contour,
        deep_contour: settings.deep_contour,
        four_shades: !settings.two_shades,
    }
}

/// Cache-store entry of the PC rule's instruction string for `settings`.
pub fn instructions_key(
    store: &dyn CacheStore,
    pc: &BoundPortrayalCatalogue,
    settings: DepthSettings,
) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let mut sha = Sha256::new();
    sha.update(b"ferrite-s102-rule-instructions-v1\0");
    sha.update(pc.source_digest());
    for v in [
        settings.safety_contour,
        settings.shallow_contour,
        settings.deep_contour,
    ] {
        sha.update(v.to_bits().to_le_bytes());
    }
    sha.update([u8::from(settings.four_shades)]);
    let name: String = sha.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(store.root()?.join(format!(".ferrite_s102_{name}.bin")))
}

#[cfg(any(feature = "lua-native", test))]
fn encode_instructions(text: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{digest}\n{text}").into_bytes()
}
fn decode_instructions(bytes: &[u8]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let text = std::str::from_utf8(bytes)?;
    let (digest, body) = text
        .split_once('\n')
        .context("Malformed precomputed S-102 instructions")?;
    let actual: String = Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ensure!(
        digest == actual,
        "Precomputed S-102 instructions are corrupt"
    );
    Ok(body.to_owned())
}

/// The S-102 PC portrayal for `settings`: the rule evaluated with Lua
/// (exported to the cache store when `export`), or the precomputed
/// instruction string from the cache store.
pub fn portrayal(
    pc: &Arc<BoundPortrayalCatalogue>,
    profile: &str,
    settings: DepthSettings,
    lua: bool,
    export: bool,
    reuse: bool,
    store: &dyn CacheStore,
) -> Result<BathymetryPortrayal> {
    // Reuse (a Lua build): an intact stored string stands for the rule's
    // output whatever the S-101 source; anything else (missing,
    // corrupt) evaluates the rule again.
    #[cfg(feature = "lua-native")]
    if lua || reuse {
        if reuse {
            if let Some(key) = instructions_key(store, pc, settings) {
                if let Ok(bytes) = store.read(&key) {
                    match decode_instructions(&bytes).and_then(|text| {
                        BathymetryPortrayal::from_bound_instructions(
                            Arc::clone(pc),
                            profile,
                            settings,
                            &text,
                        )
                    }) {
                        Ok(portrayal) => {
                            store.mark_used(&key);
                            return Ok(portrayal);
                        }
                        Err(error) => tracing::warn!(
                            "Stored S-102 instructions rejected ({error:#}); evaluating the rule"
                        ),
                    }
                }
            }
        }
        let export = export || reuse;
        let portrayal =
            BathymetryPortrayal::from_bound_catalogue(Arc::clone(pc), profile, settings)?;
        if export {
            if let Some(key) = instructions_key(store, pc, settings) {
                if let Err(error) = store.write(&key, &encode_instructions(&portrayal.instructions))
                {
                    tracing::warn!("Precomputed S-102 instructions not written: {error}");
                }
            }
        }
        return Ok(portrayal);
    }
    let _ = (lua, export, reuse);
    let missing = "No precomputed S-102 portrayal instructions for this S-102 PC and \
                   contour settings in the cache store; export them with a lua-native \
                   build (ChartEngine::set_precomputed_export)";
    let key = instructions_key(store, pc, settings).context(missing)?;
    let bytes = store
        .read(&key)
        .with_context(|| format!("{missing} [{}]", key.display()))?;
    BathymetryPortrayal::from_bound_instructions(
        Arc::clone(pc),
        profile,
        settings,
        &decode_instructions(&bytes)?,
    )
}

/// Assign an interoperability catalogue's raster portrayal plane to a
/// composed S-102 tile.
pub fn compose_raster(
    mut layer: RasterLayer,
    assignment: Option<&Assignment>,
) -> Result<RasterLayer> {
    if let Some(a) = assignment {
        ensure!(
            a.plane.stage == ferrite_kernel::CompositionStage::Chart && a.priority >= 0,
            "Invalid raster IC assignment"
        );
        // A coverage raster has no symbol or line style to substitute.
        ensure!(
            a.substitution.is_none(),
            "substituteSymbolization cannot apply to a raster coverage"
        );
        layer.draw_order = RasterDrawOrder {
            stage: a.plane.stage,
            display_plane: DisplayPlane::Interoperability(a.plane.order),
            priority: a.priority,
        };
        layer.viewing_groups = vec![a.viewing_group];
    }
    Ok(layer)
}

impl S102Layer {
    /// Portray the loaded bathymetry as the viewer does; None without
    /// bathymetry.
    pub fn prepare(&self, request: &S102Portrayal<'_>) -> Result<Option<BathymetryScene>> {
        if self.bathymetry.is_empty() {
            return Ok(None);
        }
        let edge = request.tile_edge;
        let settings = request.settings;
        ensure!(edge > 0, "Bathymetry tile edge must be positive");
        let pc = self
            .pc
            .as_ref()
            .context("S-102 PC unavailable; install PC/S-102 in the catalogue folder")?;
        let groups = viewing_groups(pc, settings.display_mode)?;
        let portrayal = portrayal(
            pc,
            request.profile,
            depth_settings(settings),
            request.lua,
            request.export,
            request.reuse_rule_instructions,
            request.cache,
        )?;
        let active_ic = request.interoperability.as_ref();
        let selection = active_ic
            .map(|ic| (ic.selection)(ic.catalogue))
            .transpose()?
            .unwrap_or_default();
        let mut tiles = Vec::new();
        for group in self.bathymetry.chunk_by(|a, b| a.0 == b.0) {
            let path = &group[0].0;
            let first = &group[0].1;
            let policy = self
                .depth_policies
                .get(path)
                .context("Missing loaded depth policy")?;
            let coverages: Vec<&BathymetryCoverage> = group.iter().map(|(_, c, _)| c).collect();
            let assignment = active_ic
                .map(|ic| first.interoperability_assignment_in(&ic.catalogue.catalogue, selection))
                .transpose()?
                .flatten();
            if let Some(ic) = active_ic {
                for c in &coverages {
                    ensure!(
                        c.interoperability_assignment_in(&ic.catalogue.catalogue, selection)?
                            == assignment,
                        "Cannot compose S102 instances with different interoperability assignments"
                    );
                }
            }
            let mosaic = ConservativeCoverage::new(
                coverages
                    .iter()
                    .map(|c| DatumCoverage {
                        coverage: *c,
                        reference: DepthReference(c.vertical_datum as u64),
                    })
                    .collect(),
                DepthReference(policy.target as u64),
                policy,
            )?;
            let numeric: &dyn NumericCoverageSource =
                if coverages.len() == 1 && policy.identity(first.vertical_datum) {
                    first
                } else {
                    &mosaic
                };
            for window in numeric.numeric_geometry().windows(edge, edge)? {
                let id = format!(
                    "{}:composed:{}:{}",
                    path.display(),
                    window.column,
                    window.row
                );
                tiles.push(if active_ic.is_none() {
                    BathymetryTile::Bound(portrayal.bound_raster_window(numeric, window, id)?)
                } else {
                    BathymetryTile::Composed(compose_raster(
                        portrayal.raster_window(numeric, window, id)?,
                        assignment.as_ref(),
                    )?)
                });
            }
        }
        Ok(Some(BathymetryScene {
            tiles: Mutex::new(tiles),
            viewing_groups: groups,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precomputed_instructions_are_integrity_checked() {
        let bytes = encode_instructions("CoverageFill:depth");
        assert_eq!(decode_instructions(&bytes).unwrap(), "CoverageFill:depth");
        let mut bad = bytes.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decode_instructions(&bad).is_err());
        assert!(decode_instructions(b"no newline").is_err());
    }
}
