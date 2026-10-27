//! S-421 Route Plan product (`docs/ENGINE_DESIGN.md`, "제품 모듈화", step 3).
//!
//! * Product data and rules, all targets: the bounded published 1.0 / CDV 2.0
//!   GML subset reader and exporter ([`s421`]), the route model ([`route`],
//!   [`navigation`], [`haversine`]), the catalogue status ([`catalogue`]),
//!   the editing portrayal contract ([`editing_style_contract`]) and the
//!   route panel data model ([`ui`]).
//! * [`layer`], all targets: the engine-facing product layer. Route input
//!   capture and discovery over a data source ([`layer::input`]), the
//!   product adapter ([`layer::S421`]) and the dataset store
//!   ([`layer::S421Layer`]) that portrays routes as the route overlay
//!   renderer extension (`ferrite_product_api::route_overlay`).
//! * Native only: the route controller with its abi_stable plugin interface
//!   ([`RouteController`], loaded dynamically by the plugin host) and the
//!   viewer's route editor host ([`editor`]).

pub mod catalogue;
pub mod editing_style_contract;
pub mod haversine;
pub mod layer;
pub mod navigation;
pub mod route;
pub mod s421;
pub mod ui;

#[cfg(not(target_arch = "wasm32"))]
mod controller;
#[cfg(not(target_arch = "wasm32"))]
pub mod editor;
#[cfg(not(target_arch = "wasm32"))]
pub use controller::*;

use serde::{Deserialize, Serialize};

/// Plugin settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSettings {
    /// Show distance labels on legs
    pub show_distance_labels: bool,
    /// Show waypoint numbers
    pub show_waypoint_numbers: bool,
    /// Show bearing on legs
    pub show_bearings: bool,
    /// Distance unit
    pub distance_unit: DistanceUnit,
    /// Line color (RGBA)
    pub line_color: u32,
    /// Line width
    pub line_width: f32,
    /// Waypoint symbol
    pub waypoint_symbol: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DistanceUnit {
    NauticalMiles,
    Kilometers,
    StatuteMiles,
}

/// Default PLRTE color from S-421 PC (Day palette): RGB(214, 63, 36) = #D63F24
const DEFAULT_PLRTE_COLOR: u32 = 0xD63F24FF;

impl Default for RouteSettings {
    fn default() -> Self {
        Self {
            show_distance_labels: true,
            show_waypoint_numbers: true,
            show_bearings: false,
            distance_unit: DistanceUnit::NauticalMiles,
            // PLRTE color from S-421 PC ColorProfile (Day palette)
            line_color: DEFAULT_PLRTE_COLOR,
            line_width: 2.0,
            waypoint_symbol: "RTEWPT01".to_string(),
        }
    }
}
