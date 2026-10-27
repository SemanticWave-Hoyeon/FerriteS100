//! S-129 Under Keel Clearance Management 2.0.0 product module.
//!
//! Everything S-129-specific on top of the shared GML infrastructure
//! (`ferrite-s100-gml`): the S-158-129 dataset and exchange-set rules
//! ([`rules`], registered as [`S129`]), the virtual portrayal input features
//! ([`virtual_features`]), the overlay module the engine hosts
//! ([`overlay::S129Overlay`]: PlainBoundaries, feature scaleMinimum, errata
//! correction) and the plan panel model ([`panel`]).
pub mod overlay;
pub mod panel;
pub mod rules;
pub mod virtual_features;

pub use overlay::S129Overlay;
pub use rules::{S129Rules, S129};
