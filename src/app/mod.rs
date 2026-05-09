//! Application support modules.
//!
//! This split keeps `main.rs` focused on the `ChartApp` state and event loop;
//! free functions and helpers that don't borrow `ChartApp` live here in
//! domain-specific modules. The boundary between this directory and
//! `main.rs` is intentionally "what doesn't need ChartApp state" — any
//! restructuring of `ChartApp` itself is out of scope for this split.

pub mod catalogue;
pub mod chartapp;
pub mod config;
pub mod error_dialog;
pub mod icon;
pub mod logging;
pub mod lua_runtime;
pub mod path_resolution;
pub mod portrayal;
pub mod world_map;
