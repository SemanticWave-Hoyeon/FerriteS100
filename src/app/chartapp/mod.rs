//! `ChartApp` impl blocks split by concern.
//!
//! `ChartApp` itself stays defined in `main.rs` (along with its constructor,
//! `RenderedSymbol`, etc.). The struct's fields are `pub(crate)` so these
//! sub-modules can attach `impl ChartApp { ... }` extensions without
//! reaching through accessors. Each module groups methods that share a
//! single concern; the boundary lives in the module name.

mod chart_io;
mod event_handler;
mod hit_test;
mod portrayal;
mod view;
