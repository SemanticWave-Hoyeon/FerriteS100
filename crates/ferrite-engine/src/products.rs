//! The product modules compiled into this build (`docs/ENGINE_DESIGN.md`,
//! "제품 모듈화"). Each product crate is behind an engine feature of the same
//! name (all on by default); this is the only place the engine names them.
//! The rest of the engine reaches them through the product interfaces
//! ([`ferrite_product_api`], [`ferrite_s100_gml::product`]).
use ferrite_s100_gml::product::GmlRules;
use ferrite_s100_gml::Product;

/// The GML (S-100 Part 10b) products, in registration order: their static
/// rules, which also create their overlay modules.
// The pushes depend on the enabled features.
#[allow(clippy::vec_init_then_push)]
pub fn gml_rules() -> Vec<&'static dyn GmlRules> {
    #[allow(unused_mut)]
    let mut rules: Vec<&'static dyn GmlRules> = Vec::new();
    #[cfg(feature = "s124")]
    rules.push(&ferrite_s124::S124);
    #[cfg(feature = "s129")]
    rules.push(&ferrite_s129::S129);
    rules
}

/// The GML products registered in this build.
pub fn gml_products() -> Vec<Product> {
    gml_rules().iter().map(|r| r.product()).collect()
}

/// The S-421 route product (`ferrite_s421::layer`), when built: route input
/// capture and discovery, the adapter and the route layer.
#[cfg(feature = "s421")]
pub mod s421 {
    pub use ferrite_s421::layer::input::{
        capture_if_route, capture_if_route_in, discover_routes_in, CapturedRouteInput,
        RouteInputError,
    };
    pub use ferrite_s421::layer::{S421Layer, S421};
}
