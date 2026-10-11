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

mod overscale_pattern;
pub use overscale_pattern::OverscalePatternDefinition;

/// Parse one captured line resource independently of catalogue metadata.
/// Composite references to other files require full catalogue loading instead;
/// this API never invents missing referenced styles or drawing-plane orders.
pub fn parse_standalone_line_style(bytes: &[u8], id: &str) -> Result<LineStyle> {
    let mut definitions = std::collections::HashMap::new();
    definitions.insert(id.to_owned(), line_style_xml::parse_bytes(bytes, id)?);
    line_style_xml::resolve(&definitions)?
        .remove(id)
        .ok_or_else(|| PCError::ResourceNotFound(id.to_owned()))
}

mod font_reference;
pub use font_reference::{BoundFontDeclarations, BoundFontReference};
