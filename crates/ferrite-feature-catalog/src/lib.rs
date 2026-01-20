//! S-100 Feature Catalogue XML parser
//!
//! This crate parses S-100 Feature Catalogue XML files dynamically,
//! supporting different product specifications (S-101, S-102, etc.).

mod attribute;
mod catalogue;
mod error;
mod feature_type;
mod information_type;
mod types;

pub use attribute::*;
pub use catalogue::*;
pub use error::*;
pub use feature_type::*;
pub use information_type::*;
pub use types::*;
