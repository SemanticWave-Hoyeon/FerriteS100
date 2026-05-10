//! Model Context Protocol (MCP) tool + protocol layer.
//!
//! - [`tools`]: the 9 read-only tools, dispatched by name.
//! - [`server`]: JSON-RPC method dispatcher (transport-agnostic — used
//!   by FerriteS100's in-process HTTP MCP server in `src/http_mcp.rs`).
//!
//! Tools are strictly read-only (plan2 §10) — no file writes, no shell,
//! no network.

pub mod server;
pub mod tools;
