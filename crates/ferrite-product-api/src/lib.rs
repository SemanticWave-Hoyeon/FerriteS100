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
//!   overlay layer serves S-124 and S-129; S-101 cells, S-102 grids and
//!   S-421 routes follow in later steps). It opens datasets from bytes
//!   ([`DatasetInput`]), portrays them into drawing instructions and
//!   renderer extensions ([`SceneContribution`]), reports coverage and
//!   describes and highlights picked features ([`FeaturePick`]).
//!
//! Platform services the product modules need themselves (the clock and
//! per-user records) are defined in [`platform`]; the engine's `Platform`
//! implements them.
use std::any::Any;
use std::path::Path;
use std::sync::Arc;

pub mod platform;

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

/// How a dataset entered the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// Exchange set signature verified.
    Authenticated,
    /// Signature verification switched off by the user.
    Unverified,
    /// Converted from a non-S-100 source; never authoritative.
    Imported(&'static str),
}

impl Provenance {
    pub fn label(&self) -> String {
        match self {
            Self::Authenticated => "signature verified".into(),
            Self::Unverified => "signature not verified".into(),
            Self::Imported(format) => {
                format!("imported from {format}; not an official S-124 dataset, not for navigation")
            }
        }
    }
}

/// A dataset handed to a layer: the name it is known by, its bytes (the
/// authenticated snapshot when signatures are verified) and its provenance.
pub struct DatasetInput<'a> {
    pub name: &'a Path,
    pub bytes: &'a [u8],
    pub provenance: Provenance,
}

/// Outcome of [`ProductLayer::open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    /// Added under this layer-local dataset id.
    New(u32),
    /// The same content is already open.
    AlreadyOpen,
}

/// What a layer portrays for.
#[derive(Clone, Copy, Debug)]
pub struct PortrayalRequest<'a> {
    /// Colour profile (palette) name.
    pub profile: &'a str,
    /// Viewing time selected by the user; None follows the platform clock.
    pub viewing_time: Option<chrono::DateTime<chrono::Utc>>,
}

/// Renderer-specific output expressed as data the renderer understands.
/// The kind names the renderer path; the payload type belongs to it (raster
/// tiles of S-102, the S-421 route overlay in later steps).
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
pub trait ProductLayer {
    /// Products served by this layer, in registration order.
    fn products(&self) -> Vec<ProductId>;
    fn is_empty(&self) -> bool;
    /// Read, validate and add a dataset.
    fn open(&mut self, input: DatasetInput<'_>) -> anyhow::Result<Opened>;
    fn remove(&mut self, id: u32) -> bool;
    fn set_visible(&mut self, id: u32, visible: bool) -> bool;
    /// Drawing instructions of every visible dataset; None when the layer
    /// holds no dataset (the host then behaves as without the layer).
    fn portray(&mut self, request: &PortrayalRequest<'_>) -> Option<SceneContribution>;
    /// Geographic extent of the visible datasets.
    fn coverage(&self) -> Option<GeoBounds>;
    /// Report of the feature behind a drawing-instruction feature key.
    fn describe(&mut self, key: i64) -> Option<FeaturePick>;
    /// Extra geometry to highlight when the feature is selected.
    fn highlight(&mut self, key: i64) -> Vec<Vec<WorldPoint>>;
}
