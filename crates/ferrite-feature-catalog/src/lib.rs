//! S-100 Feature Catalogue XML parser
//!
//! This crate parses S-100 Feature Catalogue XML files dynamically,
//! supporting different product specifications (S-101, S-102, etc.).

mod error;
mod types;
mod attribute;
mod feature_type;
mod information_type;
mod catalogue;

pub use error::*;
pub use types::*;
pub use attribute::*;
pub use feature_type::*;
pub use information_type::*;
pub use catalogue::*;
