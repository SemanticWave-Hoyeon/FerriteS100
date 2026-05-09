//! Model Context Protocol (MCP) server implementation.
//!
//! Speaks JSON-RPC 2.0 over stdio per the MCP spec.
//! Tools are read-only (plan2 §10) — no file writes, no shell, no network.

pub mod server;
pub mod tools;
