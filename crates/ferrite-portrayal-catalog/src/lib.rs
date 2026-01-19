//! S-100 Portrayal Catalogue XML parser
//!
//! This crate parses S-100 Portrayal Catalogue files including
//! color profiles, symbols, line styles, and area fills.

mod error;
mod color;
mod symbol;
mod line_style;
mod area_fill;
mod viewing;
mod rules;
mod catalogue;

pub use error::*;
pub use color::*;
pub use symbol::*;
pub use line_style::*;
pub use area_fill::*;
pub use viewing::*;
pub use rules::*;
pub use catalogue::*;
