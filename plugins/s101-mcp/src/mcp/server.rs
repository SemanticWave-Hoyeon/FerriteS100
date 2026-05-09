//! MCP stdio loop.
//!
//! Reads newline-delimited JSON-RPC 2.0 frames from stdin, dispatches to
//! tools, writes responses to stdout. Logs (which would corrupt the
//! protocol if mixed with frames) go to stderr.
//!
//! Methods supported:
//! - `initialize`: handshake, returns serverInfo + capabilities
//! - `tools/list`: enumerates the 9 read-only tools
//! - `tools/call`: invokes a tool by name with JSON arguments
//! - `ping`: keepalive (returns `{}`)
//!
//! All other methods reply with JSON-RPC error code -32601 (Method not found).

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::indices::Indices;

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

pub fn run(indices: Arc<Indices>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF — client closed stdin. Normal shutdown.
            tracing::info!("Client closed stdin; shutting down");
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Malformed JSON-RPC frame: {}", e);
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700, // Parse error
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                };
                write_frame(&stdout, &resp)?;
                continue;
            }
        };

        // Notifications (no id) get no reply.
        let id = match req.id.clone() {
            Some(v) => v,
            None => {
                tracing::trace!("Notification {}: no reply expected", req.method);
                continue;
            }
        };

        let resp = dispatch(&indices, &req.method, req.params, id);
        write_frame(&stdout, &resp)?;
    }
}

fn write_frame(stdout: &std::io::Stdout, resp: &JsonRpcResponse) -> Result<()> {
    let mut out = stdout.lock();
    let payload = serde_json::to_string(resp)?;
    out.write_all(payload.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn dispatch(idx: &Indices, method: &str, params: Value, id: Value) -> JsonRpcResponse {
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
