//! The common report model of opened datasets: summaries, validation
//! findings, feature reports and product-specific opening reports. Layers
//! fill it; hosts (the engine facade, the CLI, the C ABI, the web) print it.
use crate::ProductId;
use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

/// Identity of an opened dataset in a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DatasetId {
    /// Dataset of the base chart layer (cell index in load order).
    Cell(usize),
    /// Dataset of the overlay layer (layer-local id).
    Overlay(u32),
}

/// One opened dataset.
#[derive(Clone, Debug)]
pub struct DatasetSummary {
    pub id: DatasetId,
    /// Product identifier ("S-101", "S-124", ...).
    pub product: String,
    pub name: String,
    pub source: Option<PathBuf>,
    /// S-101: dataset edition and update ("2.0" for edition 2 update 0);
    /// S-124/S-129: the product specification edition.
    pub edition: String,
    pub features: usize,
    /// How its authenticity was established.
    pub provenance: String,
}

/// Severity of a [`ValidationIssue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IssueSeverity {
    Warning,
    Error,
}

/// A validation finding or a rejected input.
#[derive(Clone, Debug)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    /// Dataset or input the finding is about.
    pub subject: String,
    /// Check identifier ("S-158-124/12", "S-101 load", ...).
    pub check: String,
    pub message: String,
}

impl ValidationIssue {
    /// An error about `subject` from `check`, with the error chain.
    pub fn error(subject: impl Into<String>, check: &str, error: &anyhow::Error) -> Self {
        Self {
            severity: IssueSeverity::Error,
            subject: subject.into(),
            check: check.into(),
            message: format!("{error:#}"),
        }
    }
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let severity = match self.severity {
            IssueSeverity::Warning => "warning",
            IssueSeverity::Error => "error",
        };
        write!(
            f,
            "{severity} [{}] {}: {}",
            self.check, self.subject, self.message
        )
    }
}

/// A product-specific report of an opening (S-102 file summaries, S-101
/// cancellations). The payload type belongs to the product
/// (`ferrite_s102::layer::BathymetrySummary`,
/// `ferrite_s101::layer::publication::CancellationRecord`); hosts that know
/// the product read it with [`ProductReport::get`].
#[derive(Clone)]
pub struct ProductReport {
    pub product: ProductId,
    /// What the report is (`"bathymetry"`, `"cancellation"`).
    pub kind: &'static str,
    pub payload: Arc<dyn Any + Send + Sync>,
    debug: Arc<str>,
}

impl ProductReport {
    pub fn new<T: Any + Send + Sync + std::fmt::Debug>(
        product: ProductId,
        kind: &'static str,
        payload: T,
    ) -> Self {
        let debug = format!("{payload:?}").into();
        Self {
            product,
            kind,
            payload: Arc::new(payload),
            debug,
        }
    }
    /// The payload when it is a `T`.
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.payload.downcast_ref()
    }
}

impl std::fmt::Debug for ProductReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.product, self.kind, self.debug)
    }
}

/// What installing an open job's results reports.
#[derive(Clone, Debug, Default)]
pub struct InstallReport {
    /// Datasets newly opened.
    pub datasets: Vec<DatasetSummary>,
    pub issues: Vec<ValidationIssue>,
    /// Inputs not opened and other information.
    pub notices: Vec<String>,
    /// Product-specific reports, in installation order.
    pub products: Vec<ProductReport>,
}

/// Plain-data report of a feature of an opened dataset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeatureReport {
    /// Feature type code (or the symbol name when the feature is unknown).
    pub feature_type: String,
    pub feature_id: i64,
    pub foid: Option<String>,
    /// Dataset index of base chart features (S-101 cells); None for overlay
    /// objects.
    pub cell_index: Option<u32>,
    pub primitive_type: String,
    /// Source dataset path.
    pub source: Option<String>,
    /// Attribute rows (name, value) resolved through the dataset's own FC.
    pub attributes: Vec<(String, String)>,
    /// Feature Catalogue definition, when the type was resolved.
    pub definition: Option<String>,
}

impl FeatureReport {
    /// The report of a layer pick without a dataset index.
    pub fn from_pick(pick: crate::FeaturePick, feature_id: i64) -> Self {
        Self {
            feature_type: pick.feature_type,
            feature_id,
            foid: pick.foid,
            cell_index: None,
            primitive_type: pick.primitive,
            source: Some(pick.source),
            attributes: pick.attributes,
            definition: pick.definition,
        }
    }
}
