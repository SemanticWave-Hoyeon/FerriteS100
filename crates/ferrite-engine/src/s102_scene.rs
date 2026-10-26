//! S-102 bathymetry served by [`crate::chart_engine::ChartEngine`]: opening
//! captured inputs from a data source, and the viewer's raster portrayal of
//! the loaded coverages for a prepared scene.
//!
//! Opening reads each input once through the platform data source (or the
//! authenticated in-memory snapshot) and decodes the captured bytes with the
//! pure-Rust HDF5 reader, so it behaves the same on every platform and the
//! decoded bytes cannot change underneath. Portrayal follows the viewer
//! (`load_bathymetry` / raster rebuild): per input file, the raw coverage or
//! the conservative datum mosaic under the file's depth policy, tiled in
//! `edge`-sized windows, coloured by the S-102 portrayal catalogue rule for
//! the display's contours and shades, or composed with an active
//! interoperability catalogue.
#[cfg(feature = "s102-pure")]
use crate::chart_engine::IssueSeverity;
use crate::chart_engine::{OpenReport, ValidationIssue};
use crate::platform::DataSource;
use crate::state::EngineState;
#[allow(unused_imports)]
use anyhow::{Context, Result};
use ferrite_render::host_settings::SettingsState;
use ferrite_render::{GeoBounds, RasterLayer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
#[cfg(feature = "s102-pure")]
use std::sync::atomic::Ordering;

/// What this build serves for S-102 through the facade.
pub const SUPPORT: &str = if cfg!(feature = "s102-pure") {
    "S-102 3.0.0 bathymetric coverage (regular grid, EPSG:4326, bounding-box \
     domains) read with the pure-Rust HDF5 reader and portrayed as the viewer \
     does; S-102 2.1 trial files are inspected only (never depth)"
} else {
    "S-102 is not built (features s102-pure / s102-hdf5)"
};

/// Host options for S-102 opened by the facade.
#[derive(Clone, Debug)]
pub struct S102Options {
    /// Depth adjustment configuration (JSON of `s102_depth_policy`); None:
    /// each file is shown in its own datum.
    pub depth_adjustments: Option<std::sync::Arc<[u8]>>,
    /// Raster tile edge in samples (keep within the GPU texture limit).
    pub tile_edge: usize,
    /// With Lua portrayal, use the PC rule's instruction string from the
    /// cache store when an intact entry for this PC and these contour
    /// settings exists ([`s102_instruction_entry`]), and write it after
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

/// S-102 inputs decoded by an open job.
#[derive(Default)]
pub struct OpenedS102 {
    #[cfg(feature = "s102-pure")]
    inputs: Vec<(PathBuf, S102Input, String)>,
}
impl OpenedS102 {
    pub fn len(&self) -> usize {
        #[cfg(feature = "s102-pure")]
        return self.inputs.len();
        #[cfg(not(feature = "s102-pure"))]
        0
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether a path names an HDF5 dataset (S-102 candidate).
pub fn is_hdf5_input(path: &Path) -> bool {
    crate::dataset_discovery::is_dataset_file(path, "h5")
        || crate::dataset_discovery::is_dataset_file(path, "hdf5")
}

/// Load the optional S-102 catalogues (`FC/S-102`, `PC/S-102`) through the
/// data source; failures are recorded in the catalogue statuses.
pub fn load_catalogues(state: &mut EngineState, data: &dyn DataSource, catalogues: &Path) {
    use ferrite_render::host_settings::CatalogueStatus;
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
        Ok(pc) => {
            state.s102_pc_status = CatalogueStatus {
                loaded: true,
                product_id: pc.product_id.clone(),
                version: pc.version.clone(),
                path: pc.root_path.display().to_string(),
                item_count: pc.symbols.symbols.len(),
                validation_message: Some("Parsed installed S-102 Portrayal Catalogue".into()),
            };
            state.s102_pc = Some(std::sync::Arc::new(pc));
        }
        Err(error) => {
            state.s102_pc_status = CatalogueStatus {
                product_id: "S-102".into(),
                path: pc_path.display().to_string(),
                validation_message: Some(format!("Load failed: {error:#}")),
                ..Default::default()
            };
        }
    }
}

/// Decode the S-102 inputs of an open job. `authorize` is the job's
/// signature policy over the inputs.
#[allow(clippy::too_many_arguments)]
pub fn open_inputs(
    paths: Vec<PathBuf>,
    data: &dyn DataSource,
    authorize: &dyn Fn(&[PathBuf]) -> Result<ferrite_security::AuthorizedDatasets>,
    signatures_required: bool,
    options: &S102Options,
    cancel: &AtomicBool,
    notices: &mut Vec<String>,
    issues: &mut Vec<ValidationIssue>,
) -> Result<OpenedS102> {
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = (
            data,
            authorize,
            signatures_required,
            options,
            cancel,
            issues,
        );
        for path in paths {
            notices.push(format!(
                "Not opened (S-102 support is not built: feature s102-pure): {}",
                path.display()
            ));
        }
        Ok(OpenedS102::default())
    }
    #[cfg(feature = "s102-pure")]
    {
        let _ = &notices;
        anyhow::ensure!(
            paths.len() <= 512,
            "S102 publication exceeds 512-file work budget"
        );
        let mut out = OpenedS102::default();
        // Receiver snapshot admission policy, as the viewer's.
        let mut budget = MAX_INPUT_BYTES;
        for path in paths {
            anyhow::ensure!(!cancel.load(Ordering::Relaxed), "Open cancelled");
            let opened = authorize(std::slice::from_ref(&path)).and_then(|report| {
                let capture = crate::s102_input_capture::CapturedInput::capture_in(
                    data,
                    &report,
                    &path,
                    signatures_required,
                    &mut budget,
                )?;
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
                Err(error) => issues.push(ValidationIssue {
                    severity: IssueSeverity::Error,
                    subject: path.display().to_string(),
                    check: "S-102 load".into(),
                    message: format!("{error:#}"),
                }),
            }
        }
        Ok(out)
    }
}

/// Publish decoded S-102 inputs into the chart state (as the viewer's
/// commit: coverages, depth policies and extents per file; trials apart).
pub fn install(state: &mut EngineState, opened: OpenedS102, report: &mut OpenReport) {
    #[cfg(not(feature = "s102-pure"))]
    let _ = (state, opened, report);
    #[cfg(feature = "s102-pure")]
    for (path, input, provenance) in opened.inputs {
        let loaded = state.bathymetry.iter().any(|(p, _, _)| *p == path)
            || state.s102_trials.iter().any(|(p, _)| *p == path);
        if loaded {
            report
                .notices
                .push(format!("Already open: {}", path.display()));
            continue;
        }
        if matches!(input, S102Input::Coverage { .. }) && state.s102_pc.is_none() {
            // The viewer refuses bathymetry it cannot portray.
            report.issues.push(ValidationIssue {
                severity: IssueSeverity::Error,
                subject: path.display().to_string(),
                check: "S-102 load".into(),
                message: format!(
                    "S-102 PC unavailable ({}); install PC/S-102 in the catalogue folder",
                    state
                        .s102_pc_status
                        .validation_message
                        .as_deref()
                        .unwrap_or("not installed")
                ),
            });
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
                        report.notices.push(format!(
                            "S-102 compatibility: {} declares verticalCS 6499 (height up) but is \
                             read as depth-down because {}=1",
                            path.display(),
                            ferrite_s102::VERTICAL_CS_6499_AS_DEPTH_ENV
                        ));
                    }
                    if !c.root_enclosure.encoding_compatible() {
                        report.notices.push(format!(
                            "S-102 {} {}: root geographic enclosure {:?}; raw values retained",
                            path.display(),
                            c.instance_name,
                            c.root_enclosure
                        ));
                    }
                }
                state.bathymetry_bounds.insert(path.clone(), bounds);
                state.depth_policies.insert(path.clone(), policy);
                let snapshot = capture.authenticated.clone();
                state.bathymetry.extend(
                    coverages
                        .into_iter()
                        .map(|c| (path.clone(), c, snapshot.clone())),
                );
                state.s102_provenance.insert(path.clone(), provenance);
            }
            S102Input::Trial(inspection) => {
                report.notices.push(format!(
                    "S-102 2.1 trial opened for inspection only (not used for depth): {}",
                    path.display()
                ));
                state.s102_trials.push((path.clone(), inspection));
                state.s102_provenance.insert(path.clone(), provenance);
            }
        }
        if let Some(summary) = summaries(state).into_iter().find(|s| s.source == path) {
            report.bathymetry.push(summary);
        }
    }
}

/// Every opened S-102 file.
pub fn summaries(state: &EngineState) -> Vec<BathymetrySummary> {
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = state;
        Vec::new()
    }
    #[cfg(feature = "s102-pure")]
    {
        let mut out = Vec::new();
        for group in state.bathymetry.chunk_by(|a, b| a.0 == b.0) {
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
                bounds: state.bathymetry_bounds.get(path).copied(),
                inspection_only: false,
                provenance: state.s102_provenance.get(path).cloned().unwrap_or_default(),
            });
        }
        for (path, inspection) in &state.s102_trials {
            let m = inspection.metadata();
            out.push(BathymetrySummary {
                source: path.clone(),
                product_specification: m.product_specification.clone(),
                instances: 0,
                grid: (m.width as usize, m.height as usize),
                issue_date: m.issue_date_lexical.clone(),
                vertical_datums: match m.vertical_datum {
                    ferrite_s102::legacy_trial::TrialVerticalDatum::Encoded(code) => vec![code],
                    ferrite_s102::legacy_trial::TrialVerticalDatum::Missing => Vec::new(),
                },
                quality_records: 0,
                bounds: None,
                inspection_only: true,
                provenance: state.s102_provenance.get(path).cloned().unwrap_or_default(),
            });
        }
        out
    }
}

/// The S-102 part of a scene for the display (empty without bathymetry).
pub fn prepare_scene(
    state: &EngineState,
    profile: &str,
    settings: &SettingsState,
    options: &S102Options,
) -> Result<BathymetryScene> {
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = (state, profile, settings, options);
        Ok(BathymetryScene::default())
    }
    #[cfg(feature = "s102-pure")]
    prepare(
        state,
        profile,
        settings,
        options.tile_edge,
        options.reuse_rule_instructions,
    )
}

/// Cache-store entry of the S-102 PC rule's instruction string for the
/// contour settings of `settings` (read by preparations of loaded
/// bathymetry with [`crate::PortrayalSource::Precomputed`] or
/// [`S102Options::reuse_rule_instructions`]); None without loaded S-102,
/// S-102 PC or cache store.
pub fn s102_instruction_entry(state: &EngineState, settings: &SettingsState) -> Option<PathBuf> {
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = (state, settings);
        None
    }
    #[cfg(feature = "s102-pure")]
    {
        if state.bathymetry.is_empty() {
            return None;
        }
        instructions_key(
            state.platform.cache.as_ref(),
            state.s102_pc.as_ref()?,
            depth_settings(settings),
        )
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
    #[cfg(feature = "s102-pure")]
    Bound(ferrite_s102::BoundBathymetryMaterial),
    /// Tile composed with an interoperability catalogue.
    Composed(RasterLayer),
}
impl BathymetryTile {
    pub fn bounds(&self) -> GeoBounds {
        match self {
            #[cfg(feature = "s102-pure")]
            Self::Bound(material) => material.bounds(),
            Self::Composed(layer) => layer.bounds,
        }
    }
}

/// The S-102 part of a prepared scene.
#[derive(Default)]
pub struct BathymetryScene {
    /// Tiles in input order, each input's tiles row-major by window.
    pub tiles: Vec<BathymetryTile>,
    /// Viewing groups enabled for the display mode (the renderer shows a
    /// tile only when all its groups are enabled); None without bathymetry.
    pub viewing_groups: Option<HashSet<u32>>,
    /// Identity of this preparation (unique per scene). A host that moves
    /// the tiles to the GPU (taking them out of `tiles`) compares it to know
    /// that a later frame of the same, re-projected scene still shows them.
    pub id: std::sync::Arc<()>,
}
impl BathymetryScene {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }
}

#[cfg(feature = "s102-pure")]
pub use opened::*;

#[cfg(feature = "s102-pure")]
mod opened {
    use super::*;
    use crate::platform::CacheStore;
    use crate::portrayal::PortrayalSource;
    use crate::s102_depth_policy::DepthPolicy;
    use crate::s102_input_capture::CapturedInput;
    use crate::state::EngineState;
    use anyhow::ensure;
    use ferrite_kernel::depth_selection::DepthReference;
    use ferrite_kernel::NumericCoverageSource;
    use ferrite_portrayal_catalog::BoundPortrayalCatalogue;
    use ferrite_render::host_settings::{DisplayMode, SettingsState};
    use ferrite_s102::trial_inspection::TrialInspection;
    use ferrite_s102::{
        h5, BathymetryCoverage, BathymetryDisplayPreset, BathymetryPortrayal, ConservativeCoverage,
        DatumCoverage, DepthSettings,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

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
        let spec = ferrite_s102::product_specification(&file)?;
        if ferrite_s102::is_trial_product(&spec) {
            return Ok(S102Input::Trial(crate::s102_trial_worker::inspect_bytes(
                bytes,
            )?));
        }
        ensure!(
            spec == ferrite_s102::SUPPORTED_PRODUCT,
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
    pub fn viewing_groups(pc: &BoundPortrayalCatalogue, mode: DisplayMode) -> Result<HashSet<u32>> {
        let preset = match mode {
            DisplayMode::Base => BathymetryDisplayPreset::Base,
            DisplayMode::Standard => BathymetryDisplayPreset::Standard,
            DisplayMode::All => BathymetryDisplayPreset::Other,
        };
        ferrite_s102::bathymetry_viewing_groups_for_preset(pc, preset)
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
        source: PortrayalSource,
        export: bool,
        reuse: bool,
        store: &dyn CacheStore,
    ) -> Result<BathymetryPortrayal> {
        // Reuse (a Lua build): an intact stored string stands for the rule's
        // output whatever the S-101 source; anything else (missing,
        // corrupt) evaluates the rule again.
        #[cfg(feature = "lua-native")]
        if source == PortrayalSource::Lua || reuse {
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
                    if let Err(error) =
                        store.write(&key, &encode_instructions(&portrayal.instructions))
                    {
                        tracing::warn!("Precomputed S-102 instructions not written: {error}");
                    }
                }
            }
            return Ok(portrayal);
        }
        let _ = (source, export, reuse);
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

    /// Portray the loaded bathymetry of `state` as the viewer does.
    pub fn prepare(
        state: &EngineState,
        profile: &str,
        settings: &SettingsState,
        edge: usize,
        reuse: bool,
    ) -> Result<BathymetryScene> {
        if state.bathymetry.is_empty() {
            return Ok(BathymetryScene::default());
        }
        ensure!(edge > 0, "Bathymetry tile edge must be positive");
        let pc = state
            .s102_pc
            .as_ref()
            .context("S-102 PC unavailable; install PC/S-102 in the catalogue folder")?;
        let groups = viewing_groups(pc, settings.display_mode)?;
        let portrayal = portrayal(
            pc,
            profile,
            depth_settings(settings),
            state.portrayal_source,
            state.precomputed_export,
            reuse,
            state.platform.cache.as_ref(),
        )?;
        let active_ic = settings
            .interoperability_enabled
            .then(|| state.ic.clone())
            .flatten();
        let selection = active_ic
            .as_ref()
            .map(|ic| state.ic_s102_selection(ic))
            .transpose()?
            .unwrap_or_default();
        let mut tiles = Vec::new();
        for group in state.bathymetry.chunk_by(|a, b| a.0 == b.0) {
            let path = &group[0].0;
            let first = &group[0].1;
            let policy = state
                .depth_policies
                .get(path)
                .context("Missing loaded depth policy")?;
            let coverages: Vec<&BathymetryCoverage> = group.iter().map(|(_, c, _)| c).collect();
            let assignment = active_ic
                .as_ref()
                .map(|ic| first.interoperability_assignment_in(&ic.catalogue, selection))
                .transpose()?
                .flatten();
            if let Some(ic) = active_ic.as_ref() {
                for c in &coverages {
                    ensure!(
                        c.interoperability_assignment_in(&ic.catalogue, selection)? == assignment,
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
                    BathymetryTile::Composed(crate::interoperability::compose_raster(
                        portrayal.raster_window(numeric, window, id)?,
                        assignment.as_ref(),
                    )?)
                });
            }
        }
        Ok(BathymetryScene {
            tiles,
            viewing_groups: Some(groups),
            id: Default::default(),
        })
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
}
