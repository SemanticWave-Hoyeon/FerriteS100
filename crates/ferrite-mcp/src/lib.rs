//! Native read-only S-100 dataset registry, product adapters and HTTP MCP service.
//! All products expose explicit dataset/product/catalogue metadata. S-101 adds
//! catalogue, attribute, feature and approximate spatial query adapters.
pub mod indices;
pub mod mcp;
pub mod validate;
pub use indices::Indices;
mod attributes;
pub mod http;
mod oauth;
pub mod service;

#[cfg(test)]
mod query_tests;
