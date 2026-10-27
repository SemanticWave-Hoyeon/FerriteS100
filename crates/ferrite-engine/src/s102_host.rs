//! S-102 in the engine: the facade's calls into the S-102 product layer
//! (`ferrite_s102::layer`, held as [`EngineState::s102`] with the
//! `s102-pure` feature) and the facade types every build has.
//!
//! The product layer owns the opened files, catalogues, input policies and
//! the raster portrayal; this module only maps its results to the facade's
//! reports ([`OpenReport`], [`ValidationIssue`]) and supplies the engine's
//! services (data source, signature policy, cache store, rule source,
//! interoperability selection). Without `s102-pure`, S-102 inputs are
//! reported as not built and nothing is portrayed.
use crate::chart_engine::{OpenReport, ValidationIssue};
use crate::platform::DataSource;
use crate::state::EngineState;
use anyhow::Result;
use ferrite_product_api::RendererExtension;
use ferrite_render::host_settings::SettingsState;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// What this build serves for S-102 through the facade.
pub const SUPPORT: &str = if cfg!(feature = "s102-pure") {
    "S-102 3.0.0 bathymetric coverage (regular grid, EPSG:4326, bounding-box \
     domains) read with the pure-Rust HDF5 reader and portrayed as the viewer \
     does; S-102 2.1 trial files are inspected only (never depth)"
} else {
    "S-102 is not built (features s102-pure / s102-hdf5)"
};

#[cfg(not(feature = "s102-pure"))]
pub use absent::*;
#[cfg(feature = "s102-pure")]
pub use ferrite_s102::layer::{
    BathymetrySummary, OpenedS102, S102Options, DEFAULT_TILE_EDGE, MAX_INPUT_BYTES,
};

/// The facade types without the product (the same plain data as
/// `ferrite_s102::layer`), so hosts build without S-102.
#[cfg(not(feature = "s102-pure"))]
mod absent {
    use ferrite_render::GeoBounds;
    use std::path::PathBuf;

    /// Default raster tile edge (the viewer's default before its GPU limit).
    pub const DEFAULT_TILE_EDGE: usize = 2048;
    /// Largest S-102 input read into memory.
    pub const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;

    /// Host options for opening and portraying S-102.
    #[derive(Clone, Debug)]
    pub struct S102Options {
        pub depth_adjustments: Option<std::sync::Arc<[u8]>>,
        pub tile_edge: usize,
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
        pub product_specification: String,
        pub instances: usize,
        pub grid: (usize, usize),
        pub issue_date: String,
        pub vertical_datums: Vec<u32>,
        pub quality_records: usize,
        pub bounds: Option<GeoBounds>,
        pub inspection_only: bool,
        pub provenance: String,
    }

    /// S-102 inputs decoded by an open job (none in this build).
    #[derive(Default)]
    pub struct OpenedS102;
    impl OpenedS102 {
        pub fn len(&self) -> usize {
            0
        }
        pub fn is_empty(&self) -> bool {
            true
        }
    }
}

/// Whether a path names an HDF5 dataset (S-102 candidate).
pub fn is_hdf5_input(path: &Path) -> bool {
    crate::dataset_discovery::is_dataset_file(path, "h5")
        || crate::dataset_discovery::is_dataset_file(path, "hdf5")
}

/// Load the optional S-102 catalogues (`PC/S-102`) through the data source;
/// failures are recorded in the catalogue statuses.
pub fn load_catalogues(state: &mut EngineState, data: &dyn DataSource, catalogues: &Path) {
    #[cfg(feature = "s102-pure")]
    ferrite_s102::layer::load_catalogues(&mut state.s102, data, catalogues);
    #[cfg(not(feature = "s102-pure"))]
    let _ = (state, data, catalogues);
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
        Ok(OpenedS102)
    }
    #[cfg(feature = "s102-pure")]
    {
        let _ = &notices;
        let mut failures = Vec::new();
        let opened = ferrite_s102::layer::open_inputs(
            paths,
            data,
            authorize,
            signatures_required,
            options,
            cancel,
            &mut failures,
        )?;
        issues.extend(failures.into_iter().map(|(path, error)| ValidationIssue {
            severity: crate::chart_engine::IssueSeverity::Error,
            subject: path.display().to_string(),
            check: "S-102 load".into(),
            message: format!("{error:#}"),
        }));
        Ok(opened)
    }
}

/// Publish decoded S-102 inputs into the S-102 layer and report them.
pub fn install(state: &mut EngineState, opened: OpenedS102, report: &mut OpenReport) {
    #[cfg(not(feature = "s102-pure"))]
    let _ = (state, opened, report);
    #[cfg(feature = "s102-pure")]
    {
        let installed = state.s102.install(opened);
        report.notices.extend(installed.notices);
        report
            .issues
            .extend(
                installed
                    .errors
                    .into_iter()
                    .map(|(subject, message)| ValidationIssue {
                        severity: crate::chart_engine::IssueSeverity::Error,
                        subject,
                        check: "S-102 load".into(),
                        message,
                    }),
            );
        report.bathymetry.extend(installed.summaries);
    }
}

/// Every opened S-102 file.
pub fn summaries(state: &EngineState) -> Vec<BathymetrySummary> {
    #[cfg(feature = "s102-pure")]
    return state.s102.summaries();
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = state;
        Vec::new()
    }
}

/// The S-102 part of a scene for the display: the raster tiles as a
/// renderer extension (`ferrite_s102::layer::BATHYMETRY_EXTENSION`); None
/// without bathymetry.
pub fn prepare(
    state: &EngineState,
    profile: &str,
    settings: &SettingsState,
    options: &S102Options,
) -> Result<Option<RendererExtension>> {
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = (state, profile, settings, options);
        Ok(None)
    }
    #[cfg(feature = "s102-pure")]
    {
        use ferrite_s102::layer::{Interoperability, S102Portrayal};
        let active_ic = settings
            .interoperability_enabled
            .then(|| state.ic.clone())
            .flatten();
        let selection =
            |ic: &ferrite_interoperability::AuthenticatedCatalogue| state.ic_s102_selection(ic);
        let scene = state.s102.prepare(&S102Portrayal {
            profile,
            settings,
            tile_edge: options.tile_edge,
            reuse_rule_instructions: options.reuse_rule_instructions,
            lua: state.portrayal_source == crate::PortrayalSource::Lua,
            export: state.precomputed_export,
            cache: state.platform.cache.as_ref(),
            interoperability: active_ic.as_deref().map(|catalogue| Interoperability {
                catalogue,
                selection: &selection,
            }),
        })?;
        Ok(scene.map(ferrite_s102::layer::BathymetryScene::into_extension))
    }
}

/// Cache-store entry of the S-102 PC rule's instruction string for the
/// contour settings of `settings` (read by preparations of loaded
/// bathymetry with [`crate::PortrayalSource::Precomputed`] or
/// [`S102Options::reuse_rule_instructions`]); None without loaded S-102,
/// S-102 PC or cache store.
pub fn instruction_entry(state: &EngineState, settings: &SettingsState) -> Option<PathBuf> {
    #[cfg(feature = "s102-pure")]
    return state
        .s102
        .instruction_entry(state.platform.cache.as_ref(), settings);
    #[cfg(not(feature = "s102-pure"))]
    {
        let _ = (state, settings);
        None
    }
}
