//! Shared S-100 Part 10b GML infrastructure of the GML products (S-124
//! Navigational Warnings in `ferrite-s124`, S-129 Under Keel Clearance
//! Management in `ferrite-s129`): reading, feature-catalogue and exchange-set
//! validation, Part 9 XSLT portrayal input and conversion, the overlay layer
//! state and the product module interface ([`product`]).
pub mod exchange;
pub mod model;
pub mod overlay;
pub mod portrayal;
pub mod product;
pub mod reader;
pub mod validate;

pub use model::*;
pub use reader::{read_bytes, read_file, ReadLimits};
