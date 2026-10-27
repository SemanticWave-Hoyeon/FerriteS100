//! Common interface of the S-100 product modules (`docs/ENGINE_DESIGN.md`,
//! "제품 모듈화").
//!
//! Each product (S-101, S-102, S-124, S-129, S-421) lives in its own crate
//! and owns everything product-specific: detection, reading, validation
//! rules, portrayal parameters, lifecycle state, feature reports and the data
//! model of its user-interface panels. The engine is product-agnostic: it
//! holds the registered adapters and composes their output.
//!
//! The interface has two levels:
//!
//! * [`ProductAdapter`]: one product. Identity ([`ProductInfo`]), detection
//!   of its datasets from a file name and the first bytes ([`Recognised`]),
//!   import formats it converts, and the panels it publishes
//!   ([`PanelDescriptor`]). Product-specific operations are reached through
//!   [`ProductAdapter::as_any`] by hosts that know the product (for example
//!   the viewer's S-124 warning list).
//! * [`ProductLayer`]: a store of opened datasets of one or more products
//!   that share an encoding and a portrayal pipeline (the S-100 Part 10b GML
//!   overlay layer serves S-124 and S-129, `ferrite_s102::layer` S-102
//!   grids, `ferrite_s421::layer` S-421 routes, `ferrite_s101::layer`
//!   S-101 cells). It opens datasets from bytes
//!   ([`DatasetInput`]), portrays them into drawing instructions and
//!   renderer extensions ([`SceneContribution`]), reports coverage and
//!   describes and highlights picked features ([`FeaturePick`]).
//!
//! Platform services the product modules need themselves (the clock,
//! per-user records, data sources, the cache store and the executor) are
//! defined in [`platform`]; the engine's `Platform`
//! implements them. [`route_overlay`] is the renderer extension a route
//! product (S-421) portrays into and renderers draw.
use std::any::Any;
use std::path::Path;
use std::sync::Arc;

pub mod open;
pub mod platform;
pub mod report;
pub mod route_overlay;

pub use ferrite_render::host_settings::CatalogueStatus;
pub use ferrite_render::{DrawingInstruction, GeoBounds, WorldPoint};

/// Product specification identifier as written in datasets and catalogues
/// (`"S-124"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProductId(pub &'static str);

impl ProductId {
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for ProductId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Dataset encoding of a product (S-100 Part 10a/10b/10c).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// ISO 8211 (S-101).
    Iso8211,
    /// HDF5 (S-102).
    Hdf5,
    /// GML (S-124, S-129, S-421).
    Gml,
}

/// Where the product's portrayal goes in the composed scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneRole {
    /// The base chart (S-101): cells with coverage, display scale selection.
    BaseChart,
    /// Gridded data drawn with the chart (S-102 raster tiles).
    Grid,
    /// Cell-less, coverage-exempt overlay instructions (S-124, S-129).
    Overlay,
    /// A route with its own interaction (S-421).
    Route,
}

/// What a product is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProductInfo {
    pub id: ProductId,
    /// Human-readable product name.
    pub title: &'static str,
    pub encoding: Encoding,
    pub role: SceneRole,
}

/// A file a product recognised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recognised {
    /// A dataset of the product.
    Dataset,
    /// A non-S-100 source the product converts (format label).
    Import(&'static str),
}

/// A user-interface panel a product publishes. The product defines the
/// panel's data model (plain rows and options) and the requests it accepts;
/// UI crates draw it without depending on the product's internals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelDescriptor {
    /// Stable identifier (`"S-124/warnings"`).
    pub id: &'static str,
    pub title: &'static str,
}

/// One product module.
pub trait ProductAdapter: Any {
    fn info(&self) -> ProductInfo;
    fn id(&self) -> ProductId {
        self.info().id
    }
    /// Whether `name` with the leading bytes `head` (at most 64 KiB) is a
    /// dataset of this product, or a source it imports.
    fn sniff(&self, name: &Path, head: &[u8]) -> Option<Recognised>;
    /// Lower-case file extensions of the non-S-100 sources the product
    /// imports (`["kml", "gpx", "shp"]` for S-124).
    fn import_extensions(&self) -> &'static [&'static str] {
        &[]
    }
    /// Whether archives (ZIP) of import sources are accepted when selected
    /// explicitly.
    fn imports_archives(&self) -> bool {
        false
    }
    /// Panels the product publishes.
    fn panels(&self) -> &'static [PanelDescriptor] {
        &[]
    }
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Where a product's catalogues are installed, as the catalogue loaders
/// name it when a catalogue folder is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogueLayout {
    /// The feature catalogue (`"<app>/Catalogues/FC/S-101/*Feature_Catalogue*.xml"`).
    pub feature: &'static str,
    /// The portrayal catalogue folder and its content.
    pub portrayal: &'static str,
}

/// How a dataset entered the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// Exchange set signature verified.
    Authenticated,
    /// Signature verification switched off by the user.
    Unverified,
    /// Converted from a non-S-100 source (format label) into a dataset of
    /// `product`; never authoritative.
    Imported {
        format: &'static str,
        product: ProductId,
    },
}

impl Provenance {
    pub fn label(&self) -> String {
        match self {
            Self::Authenticated => "signature verified".into(),
            Self::Unverified => "signature not verified".into(),
            Self::Imported { format, product } => {
                format!(
                    "imported from {format}; not an official {product} dataset, not for navigation"
                )
            }
        }
    }
}

/// A dataset handed to a layer: the name it is known by, its bytes (the
/// authenticated snapshot when signatures are verified), further members
/// of the same dataset (S-101 updates of a base cell, in order), its
/// provenance and the signature policy it was opened under.
pub struct DatasetInput<'a> {
    pub name: &'a Path,
    pub bytes: &'a [u8],
    /// Later members of the dataset (S-101 update chain), each an
    /// authenticated snapshot like `bytes` when signatures are verified.
    pub updates: &'a [&'a [u8]],
    pub provenance: Provenance,
    pub signatures: open::SignaturePolicy,
}

impl<'a> DatasetInput<'a> {
    /// A single unverified-policy input.
    pub fn new(name: &'a Path, bytes: &'a [u8], provenance: Provenance) -> Self {
        Self {
            name,
            bytes,
            updates: &[],
            provenance,
            signatures: open::SignaturePolicy::Off,
        }
    }
    /// Refuse an input that is not authenticated when the policy requires
    /// signatures (the host verifies; layers only check the outcome).
    pub fn check_policy(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.signatures == open::SignaturePolicy::Off
                || self.provenance == Provenance::Authenticated,
            "Signature verification is required: {} is not authenticated",
            self.name.display()
        );
        Ok(())
    }
}

/// Outcome of [`ProductLayer::open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    /// Added under this layer-local dataset id.
    New(u32),
    /// The same content is already open.
    AlreadyOpen,
}

/// Where rule-based products take their drawing instructions from (S-101
/// cells, the S-102 PC rule): a host setting carried by
/// [`HostServices::source`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortrayalSource {
    /// Run the portrayal catalogue's Lua rules (built with Lua), with the
    /// per-cell cache.
    Lua,
    /// Read precomputed instruction payloads from the cache store under
    /// portable keys; a missing payload is an error. Payloads are exported
    /// by a native build ([`HostServices::precomputed_export`]).
    Precomputed,
}

/// S-98 interoperability offered by the host for a portrayal. The catalogue
/// and selection types belong to `ferrite-interoperability`
/// (`AuthenticatedCatalogue`, `Selection`); they are passed as `Any` so the
/// interface does not depend on the signature backends.
#[derive(Clone, Copy)]
pub struct Interoperability<'a> {
    /// The active catalogue (`ferrite_interoperability::AuthenticatedCatalogue`).
    pub catalogue: &'a (dyn Any + Send + Sync),
    /// The selection (`ferrite_interoperability::Selection`) for the
    /// product being composed now, evaluated when the product needs it
    /// (after its own rules).
    pub selection: &'a dyn Fn(ProductId) -> anyhow::Result<Box<dyn Any>>,
}

/// Host services and settings a portrayal runs with.
#[derive(Clone, Copy, Default)]
pub struct HostServices<'a> {
    /// Rule source of rule-based products; None: the product's default
    /// (Lua when built with it).
    pub source: Option<PortrayalSource>,
    /// Also write precomputed payloads of what is portrayed with Lua to the
    /// cache store, for hosts without Lua.
    pub precomputed_export: bool,
    /// The cache store; None: the layer's own (or none).
    pub cache: Option<&'a dyn platform::CacheStore>,
    /// Active interoperability catalogue (settings permitting).
    pub interoperability: Option<Interoperability<'a>>,
}

/// What a layer portrays for.
#[derive(Clone, Copy)]
pub struct PortrayalRequest<'a> {
    /// Colour profile (palette) name.
    pub profile: &'a str,
    /// Viewing time selected by the user; None follows the platform clock.
    pub viewing_time: Option<chrono::DateTime<chrono::Utc>>,
    /// Display settings (contours, display mode); None: the defaults.
    pub settings: Option<&'a ferrite_render::host_settings::SettingsState>,
    /// Host services (rule source, precomputed export, cache store,
    /// interoperability).
    pub host: HostServices<'a>,
}

impl<'a> PortrayalRequest<'a> {
    /// A request for `profile` with default settings and host services.
    pub fn new(profile: &'a str) -> Self {
        Self {
            profile,
            viewing_time: None,
            settings: None,
            host: HostServices::default(),
        }
    }
}

/// Renderer-specific output expressed as data the renderer understands.
/// The kind names the renderer path; the payload type belongs to it (raster
/// tiles of S-102, the S-421 route overlay of [`route_overlay`]).
#[derive(Clone)]
pub struct RendererExtension {
    pub kind: &'static str,
    pub payload: Arc<dyn Any + Send + Sync>,
}

/// A layer's part of the scene.
#[derive(Clone)]
pub struct SceneContribution {
    /// Changes whenever the instructions change (cache and reinstall key).
    pub revision: u64,
    pub instructions: Arc<Vec<DrawingInstruction>>,
    pub extensions: Vec<RendererExtension>,
}

/// Plain-data report of a picked feature.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeaturePick {
    pub feature_type: String,
    pub primitive: String,
    pub source: String,
    pub attributes: Vec<(String, String)>,
    pub definition: Option<String>,
    pub foid: Option<String>,
}

/// A store of opened datasets of one or more products sharing a pipeline.
///
/// Beyond the single-dataset operations, a layer can take part in a host's
/// background open job ([`ProductLayer::open_stage`], then
/// [`ProductLayer::install`] on the owner thread) and report its datasets in
/// the common report model ([`report`]).
pub trait ProductLayer: Any {
    /// Products served by this layer, in registration order.
    fn products(&self) -> Vec<ProductId>;
    fn is_empty(&self) -> bool;
    /// Read, validate and add a dataset.
    fn open(&mut self, input: DatasetInput<'_>) -> anyhow::Result<Opened>;
    fn remove(&mut self, id: u32) -> bool;
    fn set_visible(&mut self, id: u32, visible: bool) -> bool;
    /// Drawing instructions and renderer extensions of every visible
    /// dataset; Ok(None) when the layer has nothing to portray (the host
    /// then behaves as without the layer).
    fn portray(
        &mut self,
        request: &PortrayalRequest<'_>,
    ) -> anyhow::Result<Option<SceneContribution>>;
    /// Geographic extent of the visible datasets.
    fn coverage(&self) -> Option<GeoBounds>;
    /// Report of the feature behind a drawing-instruction feature key.
    fn describe(&mut self, key: i64) -> Option<FeaturePick>;
    /// Report of feature `key` of dataset `dataset` (layers whose drawing
    /// instructions carry a dataset index, such as S-101 cells).
    fn describe_in(&mut self, _dataset: u32, _key: i64) -> Option<report::FeatureReport> {
        None
    }
    /// Extra geometry to highlight when the feature is selected.
    fn highlight(&mut self, key: i64) -> Vec<Vec<WorldPoint>>;

    /// A stage of a host open job for this layer's products, taken on the
    /// owner thread with what the job needs of the layer (catalogues,
    /// loaded datasets, options). None: the layer opens nothing in jobs.
    fn open_stage(&self) -> Option<Box<dyn open::OpenStage>> {
        None
    }
    /// Check a finished stage's result against the layer before anything
    /// of the job is installed (catalogues changed meanwhile, ...).
    fn check_opened(&self, _opened: &(dyn Any + Send)) -> anyhow::Result<()> {
        Ok(())
    }
    /// First installation pass, run for every layer of the job before any
    /// [`ProductLayer::install`]: removals the job validated (S-101
    /// cancellations) and results that depend on no other layer's datasets
    /// (S-421 routes), taken out of `opened`.
    fn install_early(
        &mut self,
        _opened: &mut (dyn Any + Send),
        _context: &open::InstallContext<'_>,
        _report: &mut report::InstallReport,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// Install a finished stage's result ([`open::OpenStage::load`]).
    fn install(
        &mut self,
        opened: Box<dyn Any + Send>,
        _context: &open::InstallContext<'_>,
        _report: &mut report::InstallReport,
    ) -> anyhow::Result<()> {
        drop(opened);
        anyhow::bail!("{:?} installs no open-job results", self.products())
    }
    /// Every opened dataset, in the common report model.
    fn datasets(&self) -> Vec<report::DatasetSummary> {
        Vec::new()
    }
    /// Feature counts by feature type of dataset `id` (None: not this
    /// layer's).
    fn feature_types(
        &self,
        _id: report::DatasetId,
    ) -> Option<std::collections::BTreeMap<String, usize>> {
        None
    }
    /// Validation findings kept with the opened datasets.
    fn validation_issues(&self) -> Vec<report::ValidationIssue> {
        Vec::new()
    }
    /// Whether the layer can portray with colour profile `profile` and
    /// `settings` (hosts check before accepting a display).
    fn check_display(
        &self,
        _profile: &str,
        _settings: &ferrite_render::host_settings::SettingsState,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// The installed portrayal catalogue a renderer draws this layer's
    /// scene with (palette, symbols): hosts take the first of the registered
    /// layers (the base chart's when it is built).
    fn portrayal_catalogue(&self) -> Option<&ferrite_portrayal_catalog::BoundPortrayalCatalogue> {
        None
    }
    /// Numeric coverages of the visible datasets (S-98 Level 2 inputs of
    /// interoperability: S-102 depths).
    fn numeric_coverages(&self) -> Vec<&dyn ferrite_kernel::NumericCoverageSource> {
        Vec::new()
    }
    /// Consistency checks of the opened datasets against their exchange
    /// sets: issues and notices.
    fn exchange_set_checks(&self) -> (Vec<report::ValidationIssue>, Vec<String>) {
        (Vec::new(), Vec::new())
    }
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
