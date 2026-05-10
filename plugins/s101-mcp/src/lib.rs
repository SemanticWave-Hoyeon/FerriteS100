//! `s101-mcp` — library crate exposing S-101 ENC indices, MCP tool
//! dispatch, and the §8.1 validation harness.
//!
//! Two consumers:
//!
//! 1. The FerriteS100 host (`../../../src/http_mcp.rs`) imports
//!    [`Indices`] for in-memory chart state, [`mcp::tools::dispatch`]
//!    for tool calls, and [`mcp::server::dispatch`] for JSON-RPC
//!    method dispatch. The host serves these over HTTP/1.1 with
//!    OAuth 2.1.
//!
//! 2. Research tooling (`SJLee_SCIE/scripts/`) imports the same
//!    `Indices` so question generation, baseline runners, and the
//!    evaluation harness see identical data shapes to what the live
//!    MCP server exposes. [`validate::run`] gives the same harness
//!    plan2 §8.1 calls for; build a small wrapper there if you need
//!    a CI-friendly exit code.
//!
//! There is **no `s101-mcp` binary** — the stdio MCP server role moved
//! to the FerriteS100 host's HTTP server, and the validation CLI moved
//! to SJLee_SCIE.

pub mod indices;
pub mod mcp;
pub mod validate;

pub use indices::Indices;
