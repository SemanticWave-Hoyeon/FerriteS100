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
//! `ferrite-product-api`. The GML products S-124 (`ferrite-s124`) and S-129
//! (`ferrite-s129`) are registered in [`products`] behind the features `s124`
//! and `s129` (on by default); the overlay layer ([`gml_overlay`]) is
//! product-agnostic. S-102 is the product layer `ferrite_s102::layer`
//! (input policies, opened files, raster portrayal), held as
//! [`EngineState::s102`] with `s102-pure` and called through [`s102_host`];
//! its raster tiles reach renderers as a renderer extension of the
//! prepared scene. S-421 is the product layer `ferrite_s421::layer` behind
//! the feature `s421` (on by default): route discovery and capture
//! ([`dataset_open_plan`], [`ChartEngine`]) and the route store of
//! [`ChartEngine`] go through it ([`products`]). S-101 is the product layer
//! `ferrite_s101::layer` (update planning, lifecycle and exchange-set
//! checks, catalogue selection, publication, caches and cell portrayal),
//! held as [`EngineState::s101`]; the engine composes interoperability and
//! the published scene around it. The stage descriptions above name the
//! modules where they were before the move.

pub mod catalogue;
pub mod chart_engine;
pub mod dataset_discovery;
pub mod dataset_open_plan;
pub mod dataset_pick_fields;
pub mod dataset_signature_policy;
pub mod gml_overlay;
pub mod pick;
pub mod platform;
pub mod products;
pub mod s102_host;
pub mod scene;
pub mod secure_instruction_cache;

mod state;
pub use chart_engine::{
    Camera, ChartEngine, DatasetSource, EngineConfig, SignaturePolicy, View, Zoom,
};
pub use ferrite_s101::layer::{CellHydrationSource, ChartLoadResult, PortrayalSource};
pub use state::{CellHydration, CellScene, ChangeCellScene, EngineState, ViewportPixels};
