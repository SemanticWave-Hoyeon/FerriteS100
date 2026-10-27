//! The S-102 stage of a host open job ([`ferrite_product_api::open`]):
//! HDF5 inputs by name ([`super::S102`]), captured and decoded under the
//! job's signature policy ([`super::open_inputs`]) and installed with the
//! layer's options (moved from the engine's `s102_host`).
use super::{open_inputs, OpenedS102, S102Layer, S102Options, PRODUCT, S102};
use anyhow::Result;
use ferrite_product_api::open::{InstallContext, Loaded, OpenContext, OpenNotes, OpenStage};
use ferrite_product_api::report::{InstallReport, IssueSeverity, ProductReport, ValidationIssue};
use ferrite_product_api::{ProductAdapter, Recognised};
use std::any::Any;
use std::path::{Path, PathBuf};

/// The S-102 stage: the layer's options and the taken inputs.
pub struct S102Stage {
    options: S102Options,
    paths: Vec<PathBuf>,
}

impl S102Stage {
    pub fn new(layer: &S102Layer) -> Self {
        Self {
            options: layer.options.clone(),
            paths: Vec::new(),
        }
    }
}

impl OpenStage for S102Stage {
    /// HDF5 files by name (the head is not read: a damaged file is reported
    /// by loading).
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised> {
        S102.sniff(name, head)
    }
    fn accept(&mut self, path: &Path, _: &OpenContext<'_>, _: &mut OpenNotes) -> bool {
        self.paths.push(path.to_path_buf());
        true
    }
    fn load(self: Box<Self>, context: &OpenContext<'_>, notes: &mut OpenNotes) -> Result<Loaded> {
        let mut failures = Vec::new();
        let authorize = |paths: &[PathBuf]| -> Result<ferrite_security::AuthorizedDatasets> {
            (context.authorize)(paths)?
                .downcast::<ferrite_security::AuthorizedDatasets>()
                .map(|report| *report)
                .map_err(|_| anyhow::anyhow!("Host authorization is not a signature report"))
        };
        let opened = open_inputs(
            self.paths,
            context.data,
            &authorize,
            context.required(),
            &self.options,
            context.cancel,
            &mut failures,
        )?;
        notes
            .issues
            .extend(failures.into_iter().map(|(path, error)| ValidationIssue {
                severity: IssueSeverity::Error,
                subject: path.display().to_string(),
                check: "S-102 load".into(),
                message: format!("{error:#}"),
            }));
        let counts = vec![("bathymetry", opened.len())];
        Ok(Loaded {
            payload: Box::new(opened),
            counts,
        })
    }
}

/// Publish decoded inputs into the layer and report them: notices, load
/// errors and a `"bathymetry"` report
/// ([`super::BathymetrySummary`]) per newly opened file.
pub(crate) fn install(
    layer: &mut S102Layer,
    opened: Box<dyn Any + Send>,
    _context: &InstallContext<'_>,
    report: &mut InstallReport,
) -> Result<()> {
    let opened = opened
        .downcast::<OpenedS102>()
        .map_err(|_| anyhow::anyhow!("Not an S-102 open result"))?;
    let installed = layer.install(*opened);
    report.notices.extend(installed.notices);
    report.issues.extend(
        installed
            .errors
            .into_iter()
            .map(|(subject, message)| ValidationIssue {
                severity: IssueSeverity::Error,
                subject,
                check: "S-102 load".into(),
                message,
            }),
    );
    report.products.extend(
        installed
            .summaries
            .into_iter()
            .map(|summary| ProductReport::new(PRODUCT, "bathymetry", summary)),
    );
    Ok(())
}
