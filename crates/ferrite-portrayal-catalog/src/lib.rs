//! S-100 Portrayal Catalogue XML parser
//!
//! This crate parses S-100 Portrayal Catalogue files including
//! color profiles, symbols, line styles, and area fills.

mod area_fill;
mod catalogue;
mod color;
mod context_validation;
mod error;
mod line_style;
mod line_style_xml;
mod rules;
mod symbol;
mod viewing;

pub use area_fill::*;
pub use catalogue::*;
pub use color::*;
pub use context_validation::*;
pub use error::*;
pub use line_style::*;
pub use rules::*;
pub use symbol::*;
pub use viewing::*;

mod viewing_metadata;

mod display_plane;
pub use display_plane::DisplayPlanes;

mod source_snapshot;
pub use source_snapshot::{BoundPortrayalCatalogue, CatalogueSources};
