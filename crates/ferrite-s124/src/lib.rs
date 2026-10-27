//! S-124 Navigational Warnings 2.0.0 product module.
//!
//! Everything S-124-specific on top of the shared GML infrastructure
//! (`ferrite-s100-gml`): the warning lifecycle ([`navwarn`]), imports of
//! non-S-100 warnings ([`import`]), the dataset and exchange-set rules
//! ([`rules`], registered as [`S124`]), the overlay module the engine hosts
//! ([`overlay::S124Overlay`]: lifecycle evaluation, classifications,
//! portrayal selection), the per-user state record ([`user_state`]) and the
//! warning list panel model ([`panel`]).
pub mod import;
pub mod navwarn;
pub mod overlay;
pub mod panel;
pub mod rules;
pub mod user_state;

pub use overlay::S124Overlay;
pub use rules::{S124Rules, S124};
