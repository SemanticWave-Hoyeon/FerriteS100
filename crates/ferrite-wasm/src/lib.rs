//! Web bindings of the S-100 chart engine (`docs/ENGINE_DESIGN.md` 4a,
//! stage 6), built for `wasm32-unknown-unknown` with wasm-bindgen.
//!
//! - [`WebEngine`] wraps [`ferrite_engine::ChartEngine`] on
//!   `Platform::in_memory`: catalogues and datasets are bytes supplied by the
//!   page, work runs on the calling thread (`SequentialExecutor`) and the
//!   wall clock is JavaScript's `Date.now()`.
//! - S-101 cells are portrayed with the catalogue's Lua rules (feature
//!   `lua-wasm`) or from precomputed Part 9a payloads made by a native build
//!   (`ferrite precompute`); S-102 bathymetry is read with the pure-Rust
//!   HDF5 reader (feature `s102-pure`).
//! - The page's cache store ([`core::PageCacheStore`]) reports the entries
//!   the engine wrote, so a page can keep per-cell portrayal caches in
//!   IndexedDB and load them before the next page's portrayal.
//! - [`WebCanvas`] renders prepared scenes with ferrite-gpu into a canvas or
//!   an `OffscreenCanvas` (a Web Worker), with WebGPU or WebGL2, presenting
//!   to the canvas surface; the emission (S-102 rasters included) is the
//!   CLI's ([`ferrite_cli::SceneEmitter`]).
//!
//! Results are JSON strings; failures are thrown as JavaScript errors.
pub mod core;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod web;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use web::*;
