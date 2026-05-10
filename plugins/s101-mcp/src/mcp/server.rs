//! MCP JSON-RPC method dispatcher.
//!
//! Maps the four MCP methods (`initialize`, `tools/list`, `tools/call`,
//! `ping`) onto the read-only tool dispatcher in [`super::tools`].
//! Transport-agnostic: the caller frames the JSON-RPC bytes however
//! it needs.
//!
//! Used by FerriteS100's [`crate::http_mcp`](../../../../../src/http_mcp.rs)
//! to serve OAuth-authenticated JSON-RPC over HTTP/1.1.
//!
//! Methods supported:
//! - `initialize`: handshake, returns serverInfo + capabilities
//! - `tools/list`: enumerates the 9 read-only tools
//! - `tools/call`: invokes a tool by name with JSON arguments
//! - `ping`: keepalive (returns `{}`)
//!
//! Anything else replies with JSON-RPC error code -32601 (Method not found).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::indices::Indices;

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC dispatcher. The caller is responsible for framing
/// (HTTP body, SSE chunks, etc.).
pub fn dispatch(idx: &Indices, method: &str, params: Value, id: Value) -> JsonRpcResponse {
    match method {
        "initialize" => ok(id, initialize_result()),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": super::tools::list_descriptors() })),
        "tools/call" => match call_tool(idx, params) {
            Ok(v) => ok(id, v),
            Err(e) => err(id, -32000, format!("tool call failed: {}", e), None),
        },
        other => err(id, -32601, format!("Method not found: {}", other), None),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "s101-mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Read-only S-101 ENC server for catalogue-aware QA research (plan2)"
        }
    })
}

fn call_tool(idx: &Indices, params: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct CallParams {
        name: String,
        #[serde(default)]
        arguments: Value,
    }
    let p: CallParams = serde_json::from_value(params)?;
    let payload = super::tools::dispatch(idx, &p.name, p.arguments)?;
    // MCP `tools/call` wraps tool output as a content array.
    Ok(json!({
        "content": [
            { "type": "text", "text": serde_json::to_string(&payload)? }
        ],
        "isError": false,
        "structuredContent": payload
    }))
}

fn ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Value, code: i32, message: String, data: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data,
        }),
    }
}
