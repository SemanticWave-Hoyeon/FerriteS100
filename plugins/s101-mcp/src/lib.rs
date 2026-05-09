//! Library face of the `s101-mcp` crate.
//!
//! The crate's primary product is the `s101-mcp` binary, but the index
//! types and validation harness it builds on top of are useful to other
//! research tooling (the question generator, baseline runners, eval
//! harness) that lives outside FerriteS100. Exposing them here lets those
//! tools reuse the same `Indices` shape FerriteS100's MCP server speaks,
//! so an MCP-result and a direct-library-call return identical data.

pub mod indices;
pub mod mcp;
pub mod validate;

pub use indices::Indices;
