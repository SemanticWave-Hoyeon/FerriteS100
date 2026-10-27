//! S-100 chart engine: the domain logic of FerriteS100 that needs no window,
//! GUI or GPU (see `docs/ENGINE_DESIGN.md`).
//!
//! Stage 1 holds the pure domain modules moved out of the application:
//! dataset discovery and open planning, signature policy, the S-101 update
//! chain and lifecycle metadata, exchange-set checks, cell publication,
//! catalogue selection, decoded/instruction caches, S-421 route input (now
//! `ferrite_s421::layer::input`) and the non-UI S-124/S-129 overlay parts. Stage 2 adds the chart state ([`EngineState`]) and the S-101
//! portrayal pipeline ([`portrayal`], [`portrayal_cache`], [`catalogue`],
//! [`chart_cells`]) and the S-124/S-129 overlay registry; renderer-specific
//! pieces (area triangulation, overlay symbol sources) are supplied by the
//! host. Behaviour is unchanged from the application modules.
//!
//! Stage 4 adds the platform services ([`platform`]: data source, cache
//! store, user store, clock, executor) the engine reads through, and the
//! public facade [`ChartEngine`] ([`chart_engine`]) with renderer-agnostic
//! prepared scenes ([`scene`]) and picking ([`pick`]).
//!
//! Stage 5 puts the native-only dependencies behind features (all on by
//! default): `parallel` (threads and rayon; off: sequential executor and
//! iteration with identical results), `lua-native` (mlua portrayal, selected
//! by `lua54`/`lua55`; off: S-101 cells come from precomputed payloads,
//! [`PortrayalSource::Precomputed`]) and `s102-hdf5` (S-102 with the HDF5 C
//! library for local paths). `s102-pure` serves S-102 with the pure-Rust HDF5
//! reader alone (wasm); with neither, S-102 inputs are reported as unsupported.
//!
//! Product modularization (`docs/ENGINE_DESIGN.md`, "제품 모듈화") moves each
//! product into its own crate behind the common interface of
//! `ferrite-product-api`. The engine is product-agnostic: [`products`] (the
//! only module naming products) registers the product crates built into
//! this engine (features `s101`, `s124`, `s129`, `s421`, `s102-pure`; all on
//! by default) as layers of [`EngineState::layers`] ([`ProductLayers`]).
//! [`ChartEngine`] opens datasets through the layers' open-job stages (their
//! adapters recognise the inputs), installs and reports through the layer
//! interface and composes the scene by scene role: the base chart (S-101
//! cells with residency, caches and interoperability), the overlay layers'
//! instructions ([`gml_overlay`], S-124/S-129) and the grid layers'
//! renderer extensions (S-102 raster tiles). S-421 routes are held but not
//! portrayed by the engine. The stage descriptions above name the modules
//! where they were before the moves.

pub mod catalogue;
pub mod chart_engine;
pub mod dataset_discovery;
pub mod dataset_signature_policy;
pub mod gml_overlay;
pub mod layers;
pub mod pick;
pub mod platform;
pub mod products;
pub mod scene;
pub mod secure_instruction_cache;

mod state;
pub use chart_engine::{
    Camera, ChartEngine, DatasetSource, EngineConfig, SignaturePolicy, View, Zoom,
};
pub use ferrite_product_api::PortrayalSource;
#[cfg(feature = "s101")]
pub use ferrite_s101::layer::{CellHydrationSource, ChartLoadResult};
pub use layers::ProductLayers;
#[cfg(feature = "s101")]
pub use state::{CellHydration, CellScene, ChangeCellScene};
pub use state::{EngineState, ViewportPixels};
