//! In-process HTTP MCP server + ngrok tunnel + OAuth 2.1.
//!
//! Runs alongside the FerriteS100 GUI so external LLM clients (Claude
//! Desktop, ChatGPT custom connectors, OpenRouter) can register the
//! same nine read-only tools the s101-explorer panel uses, without the
//! user spawning a separate `s101-mcp` process.
//!
//! Architecture
//! ------------
//! - A multi-threaded Tokio runtime is parked on a dedicated OS thread.
//!   Hosting it on the main thread would tangle with winit's event loop.
//! - The bound port is chosen by the OS (binding `127.0.0.1:0`) so two
//!   instances can co-exist (e.g. a debug + release build).
//! - Auth is **OAuth 2.1 with PKCE + Dynamic Client Registration**, per
//!   the MCP authorization spec (revision 2025-06-18). The MCP endpoint
//!   accepts JWT bearer tokens minted by our own `/token` endpoint.
//!   See `crate::oauth` for the protocol implementation.
//! - Public exposure happens through `ngrok http <port> --log stdout
//!   --log-format json`, spawned as a subprocess. We parse its JSON
//!   log lines for the `started tunnel` event and surface the
//!   `https://*.ngrok-free.app` (or paid-plan) URL. If `ngrok` isn't
//!   on PATH or the user hasn't configured an authtoken, we capture
//!   ngrok's own error output and show it in the panel.
//!
//! The server receives the loaded chart's `Indices` through an
//! `ArcSwap`-style `RwLock<Option<Arc<Indices>>>`. Loading a different
//! chart simply replaces the inner Arc — no need to tear down + restart
//! the server.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use s101_mcp::mcp::server::{dispatch as mcp_dispatch, JsonRpcRequest};
use s101_mcp::Indices;

use crate::oauth::{
    self, AuthServer, PendingAuth, RegisterRequest, AUTH_CODE_TTL_SECS, JWT_AUDIENCE, SCOPE,
};

/// State of the ngrok subprocess as observed by the launcher task.
/// Drives the public-URL display in the explorer panel.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    /// `ngrok http <port>` succeeded and we parsed a public URL.
    Ready,
    /// Subprocess running, no URL yet (first ~2 s after spawn).
    Starting,
    /// `ngrok` not on PATH, or process exited / failed to spawn.
    Unavailable,
    /// Server started but the user opted out of tunnelling
    /// (`--no-tunnel` flag, or future setting).
    Disabled,
}

/// Snapshot of the current MCP server state. Cloned cheaply (`String`s).
#[derive(Debug, Clone, Serialize)]
pub struct McpServerInfo {
    /// True once the axum server is bound and accepting requests.
    pub running: bool,
    /// Local URL clients on the same machine can use, e.g.
    /// `http://127.0.0.1:54123/mcp`. Always present once `running`.
    pub local_url: String,
    /// Public tunnel URL (`https://<random>.trycloudflare.com/mcp`).
    /// `None` until the tunnel is up.
    pub public_url: Option<String>,
    pub tunnel_state: TunnelState,
    /// User-facing one-liner explaining `tunnel_state`. Empty when
    /// nothing meaningful to say.
    pub tunnel_message: String,
    /// Auth scheme advertised. Always `"oauth2"` now that bearer is
    /// gone; kept as an explicit field so the panel can branch in
    /// future without needing to introspect URLs.
    pub auth: &'static str,
    /// Number of clients that completed Dynamic Client Registration
    /// during this process. Resets on restart.
    pub registered_clients: usize,
    /// Always `"streamable-http"`. Kept in the JSON so future clients
    /// can sniff the transport without introspecting the URL.
    pub transport: &'static str,
}

/// Mutable shared state. `RwLock<Indices>` lets the host swap the chart
/// in without tearing the server down. `info` uses `std::sync::Mutex`
/// so the GUI thread can read it without entering the tokio runtime.
pub struct ServerState {
    pub indices: RwLock<Option<Arc<Indices>>>,
    pub auth: AuthServer,
    pub info: StdMutex<McpServerInfo>,
}

impl ServerState {
    pub fn snapshot(&self) -> McpServerInfo {
        self.info.lock().expect("info mutex poisoned").clone()
    }

    pub async fn set_indices(&self, idx: Option<Arc<Indices>>) {
        let mut guard = self.indices.write().await;
        *guard = idx;
    }

    fn update_info<F: FnOnce(&mut McpServerInfo)>(&self, f: F) {
        let mut guard = self.info.lock().expect("info mutex poisoned");
        f(&mut guard);
    }
}

/// Handle returned to the host. Holds the runtime + a shared-state
/// pointer used to swap chart indices and read the registration JSON.
pub struct McpServerHandle {
    /// Drop-on-shutdown owner of the tokio runtime worker thread.
    /// `None` after `shutdown()`.
    _runtime: tokio::runtime::Runtime,
    pub state: Arc<ServerState>,
}

impl McpServerHandle {
    /// Latest snapshot of the registration info, serialized as JSON.
    /// Plugins receive this through `HostApi::mcp_server_info()`.
    #[allow(dead_code)]
    pub fn info_json(&self) -> String {
        serde_json::to_string(&self.state.snapshot()).unwrap_or_else(|_| "{}".into())
    }

    /// Replace the in-memory chart indices the server queries against.
    /// Call this from `rebuild_chart_indices`.
    pub fn set_indices(&self, idx: Arc<Indices>) {
        let state = self.state.clone();
        self._runtime
            .spawn(async move { state.set_indices(Some(idx)).await });
    }

    /// Drop the chart indices (chart cleared). Tools return a "no
    /// chart loaded" error until the next chart load.
    pub fn clear_indices(&self) {
        let state = self.state.clone();
        self._runtime
            .spawn(async move { state.set_indices(None).await });
    }
}

/// Spawn the HTTP MCP server on its own runtime + worker thread. Returns
/// a handle whose `info_json()` produces what the plugin renders.
///
/// `enable_tunnel = false` skips ngrok entirely (panel shows
/// "tunnel disabled"). Useful for offline development.
pub fn start(enable_tunnel: bool) -> Result<McpServerHandle> {
    let state = Arc::new(ServerState {
        indices: RwLock::new(None),
        auth: AuthServer::new(),
        info: StdMutex::new(McpServerInfo {
            running: false,
            local_url: String::new(),
            public_url: None,
            tunnel_state: if enable_tunnel {
                TunnelState::Starting
            } else {
                TunnelState::Disabled
            },
            tunnel_message: String::new(),
            auth: "oauth2",
            registered_clients: 0,
            transport: "streamable-http",
        }),
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("ferrite-mcp")
        .build()
        .map_err(|e| anyhow!("failed to build tokio runtime: {}", e))?;

    let server_state = state.clone();
    runtime.spawn(async move {
        if let Err(e) = run_server(server_state, enable_tunnel).await {
            error!("HTTP MCP server stopped: {}", e);
        }
    });

    Ok(McpServerHandle {
        _runtime: runtime,
        state,
    })
}

/// The actual axum app + (optional) tunnel launcher. Lives on the tokio
/// runtime spawned by `start()`.
async fn run_server(state: Arc<ServerState>, enable_tunnel: bool) -> Result<()> {
    // Bind to an OS-chosen port on loopback. We use std::net so we can
    // read the port back out before handing the listener to axum.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr: SocketAddr = listener.local_addr()?;
    let local_url = format!("http://{}/mcp", local_addr);
    info!("HTTP MCP server listening on {}", local_url);

    state.update_info(|info| {
        info.running = true;
        info.local_url = local_url.clone();
    });

    if enable_tunnel {
        let cf_state = state.clone();
        let port = local_addr.port();
        tokio::spawn(async move {
            run_ngrok(cf_state, port).await;
        });
    }

    let app = build_router(state.clone());
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the axum `Router` shared by both the live server and tests.
/// Factored out so integration tests can drive the routes via
/// `tower::ServiceExt::oneshot` without binding a real socket.
fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/healthz", get(handle_healthz))
        // RFC 8414 — authorization-server metadata
        .route(
            "/.well-known/oauth-authorization-server",
            get(handle_as_metadata),
        )
        // RFC 9728 — protected-resource metadata. Some MCP clients
        // probe this on the resource path (`/mcp`) and others at the
        // root; we serve both.
        .route(
            "/.well-known/oauth-protected-resource",
            get(handle_pr_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(handle_pr_metadata),
        )
        // RFC 7591 — Dynamic Client Registration
        .route("/register", post(handle_register))
        // RFC 6749 §4.1 — auth-code flow (with PKCE)
        .route("/authorize", get(handle_authorize_get))
        .route("/authorize/approve", post(handle_authorize_approve))
        .route("/token", post(handle_token))
        .with_state(state)
}

async fn handle_healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Single-shot JSON-RPC over HTTP. The MCP "Streamable HTTP" transport
/// allows either streaming (Server-Sent Events) or single response; we
/// implement single response which is sufficient for stateless tool
/// calls. Streaming can be layered on later if a tool grows pagination.
async fn handle_mcp(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let claims = match bearer {
        Some(token) => match state.auth.validate_access_token(token) {
            Ok(c) => Some(c),
            Err(e) => {
                debug!("rejected /mcp request: invalid token: {}", e);
                None
            }
        },
        None => None,
    };

    if claims.is_none() {
        let resource = base_url(&headers, &state) + "/mcp";
        let www_authenticate = format!(
            "Bearer realm=\"mcp\", error=\"invalid_token\", \
             resource_metadata=\"{}\"",
            base_url(&headers, &state) + "/.well-known/oauth-protected-resource"
        );
        return (
            StatusCode::UNAUTHORIZED,
            [
                ("WWW-Authenticate", www_authenticate.as_str()),
                ("Cache-Control", "no-store"),
            ],
            Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {
                    "code": -32000,
                    "message": "missing or invalid OAuth bearer token",
                    "data": {
                        "resource_metadata": format!("{}/.well-known/oauth-protected-resource", base_url(&headers, &state)),
                        "resource": resource
                    }
                }
            })),
        )
            .into_response();
    }

    let req: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {}", e) }
                })),
            )
                .into_response();
        }
    };
    let id = req.id.clone().unwrap_or(Value::Null);

    // Dispatch into the s101-mcp router. Methods that don't touch the
    // chart (`initialize`, `ping`, `tools/list`) work even with no chart
    // loaded — only `tools/call` requires `Indices`.
    let needs_indices = req.method == "tools/call";
    let indices = state.indices.read().await.clone();
    if needs_indices {
        match indices {
            Some(idx) => {
                let resp = mcp_dispatch(&idx, &req.method, req.params, id);
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)).into_response()
            }
            None => Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32001,
                    "message": "no chart loaded — open an S-101 cell in FerriteS100 first"
                }
            }))
            .into_response(),
        }
    } else {
        // Non-tools methods don't need Indices for their reply, but
        // dispatching through `mcp_dispatch` requires one. When no chart
        // is loaded we synthesise the responses inline instead.
        match indices {
            Some(idx) => {
                let resp = mcp_dispatch(&idx, &req.method, req.params, id);
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)).into_response()
            }
            None => {
                let resp = handle_no_chart(&req.method, id.clone());
                Json(resp).into_response()
            }
        }
    }
}

/// Replies for `initialize`, `tools/list`, `ping` when no chart is
/// loaded. Mirrors `s101_mcp::mcp::server::dispatch` for the methods
/// that don't depend on `Indices`.
fn handle_no_chart(method: &str, id: Value) -> Value {
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "ferrite-s100-mcp",
                    "version": crate::VERSION,
                    "description": "In-process HTTP MCP server for S-101 ENC catalogue-aware QA"
                }
            }
        }),
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": s101_mcp::mcp::tools::list_descriptors() }
        }),
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {}", other) }
        }),
    }
}

/// Compute the base URL the client used to reach us, by reading
/// `X-Forwarded-Proto` / `X-Forwarded-Host` (ngrok sets both),
/// then falling back to the `Host` header, then to the locally-bound
/// URL. This is what we splice into discovery documents and `iss`
/// claims so tokens issued through the public tunnel and tokens
/// issued locally each verify against the URL they were obtained at.
fn base_url(headers: &HeaderMap, state: &Arc<ServerState>) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let (Some(p), Some(h)) = (proto, host.clone()) {
        return format!("{}://{}", p, h);
    }
    if let Some(h) = host {
        // No proto header — guess: if the host looks like a public
        // hostname (no port, contains a dot) prefer https; otherwise
        // http. ngrok always sets x-forwarded-proto so we rarely hit
        // the guess path through it.
        let proto = if h.contains(':') || !h.contains('.') {
            "http"
        } else {
            "https"
        };
        return format!("{}://{}", proto, h);
    }
    // Fall back to whatever URL the server bound to.
    let info = state.snapshot();
    if let Some(public) = &info.public_url {
        public.trim_end_matches("/mcp").to_string()
    } else {
        info.local_url.trim_end_matches("/mcp").to_string()
    }
}

// === OAuth 2.1 endpoints ===========================================

/// `GET /.well-known/oauth-authorization-server` — RFC 8414.
/// Tells the client where to register, authorize, and exchange tokens.
async fn handle_as_metadata(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let base = base_url(&headers, &state);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{}/authorize", base),
        "token_endpoint": format!("{}/token", base),
        "registration_endpoint": format!("{}/register", base),
        "scopes_supported": [SCOPE],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "service_documentation":
            "https://modelcontextprotocol.io/specification/draft/basic/authorization"
    }))
}

/// `GET /.well-known/oauth-protected-resource` — RFC 9728.
/// Tells the client which authorization servers can mint tokens for us.
async fn handle_pr_metadata(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let base = base_url(&headers, &state);
    Json(json!({
        "resource": format!("{}/mcp", base),
        "authorization_servers": [base],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
        "resource_documentation":
            "https://modelcontextprotocol.io/specification/draft/basic/authorization"
    }))
}

/// `POST /register` — RFC 7591 Dynamic Client Registration.
/// Anonymous (no auth required); we accept the client's metadata
/// best-effort and mint a fresh `client_id`.
async fn handle_register(
    State(state): State<Arc<ServerState>>,
    body: Option<Json<RegisterRequest>>,
) -> impl IntoResponse {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let client = state.auth.register_client(req);
    state.update_info(|info| info.registered_clients += 1);
    info!(
        "Registered MCP client: {} ({})",
        client.client_id,
        client.client_name.as_deref().unwrap_or("anonymous")
    );
    (StatusCode::CREATED, Json(json!(client)))
}

/// `GET /authorize` query parameters per RFC 6749 §4.1.1 + RFC 7636.
#[derive(Deserialize)]
struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    code_challenge: String,
    #[serde(default = "default_pkce_method")]
    code_challenge_method: String,
}

fn default_pkce_method() -> String {
    "plain".into()
}

/// `GET /authorize` — render a consent page for the user. We reject
/// requests with a missing PKCE challenge since we advertise S256 as
/// mandatory in the AS metadata.
async fn handle_authorize_get(
    State(state): State<Arc<ServerState>>,
    Query(p): Query<AuthorizeParams>,
) -> axum::response::Response {
    if p.response_type != "code" {
        return (
            StatusCode::BAD_REQUEST,
            "unsupported response_type (only `code` is supported)",
        )
            .into_response();
    }
    let Some(client) = state.auth.get_client(&p.client_id) else {
        return (StatusCode::BAD_REQUEST, "unknown client_id").into_response();
    };
    if !redirect_uri_allowed(&client.redirect_uris, &p.redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri not registered").into_response();
    }
    if p.code_challenge.is_empty() {
        return (StatusCode::BAD_REQUEST, "PKCE code_challenge is required").into_response();
    }
    if !matches!(p.code_challenge_method.as_str(), "S256") {
        // RFC 7636 allows `plain` but it's discouraged; we only document
        // S256 so reject anything else explicitly.
        return (
            StatusCode::BAD_REQUEST,
            "code_challenge_method must be S256",
        )
            .into_response();
    }
    let scope = p.scope.unwrap_or_else(|| SCOPE.into());
    let html = oauth::consent_html(
        &p.client_id,
        client.client_name.as_deref(),
        &p.redirect_uri,
        p.state.as_deref(),
        &scope,
        &p.code_challenge,
        &p.code_challenge_method,
    );
    Html(html).into_response()
}

/// Consent-form submission. Server-side we mint the auth code and
/// redirect back to the client.
#[derive(Deserialize)]
struct ApproveForm {
    decision: String,
    client_id: String,
    redirect_uri: String,
    scope: String,
    code_challenge: String,
    code_challenge_method: String,
    #[serde(default)]
    state: Option<String>,
}

async fn handle_authorize_approve(
    State(state): State<Arc<ServerState>>,
    Form(form): Form<ApproveForm>,
) -> axum::response::Response {
    let Some(client) = state.auth.get_client(&form.client_id) else {
        return (StatusCode::BAD_REQUEST, "unknown client_id").into_response();
    };
    if !redirect_uri_allowed(&client.redirect_uris, &form.redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri not registered").into_response();
    }

    let mut url = form.redirect_uri.clone();
    let sep = if url.contains('?') { '&' } else { '?' };
    if form.decision != "approve" {
        let mut redirect = format!("{}{}error=access_denied", url, sep);
        if let Some(s) = &form.state {
            redirect.push_str(&format!("&state={}", urlencoding::encode(s)));
        }
        return Redirect::to(&redirect).into_response();
    }

    let pending = PendingAuth {
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        code_challenge: form.code_challenge,
        code_challenge_method: form.code_challenge_method,
        scope: form.scope.clone(),
        expires_at: now_secs() + AUTH_CODE_TTL_SECS,
    };
    let code = state.auth.create_auth_code(pending);

    url.push(sep);
    url.push_str(&format!("code={}", urlencoding::encode(&code)));
    if let Some(s) = &form.state {
        url.push_str(&format!("&state={}", urlencoding::encode(s)));
    }
    info!("Auth code issued for client {}", form.client_id);
    Redirect::to(&url).into_response()
}

/// `POST /token` body — RFC 6749 §4.1.3 with PKCE (RFC 7636).
#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
}

async fn handle_token(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> axum::response::Response {
    if form.grant_type != "authorization_code" {
        return token_error(
            "unsupported_grant_type",
            "only authorization_code is supported",
        );
    }
    let Some(code) = form.code else {
        return token_error("invalid_request", "missing code");
    };
    let Some(verifier) = form.code_verifier else {
        return token_error("invalid_request", "missing code_verifier (PKCE required)");
    };
    let Some(client_id) = form.client_id else {
        return token_error("invalid_request", "missing client_id");
    };

    let Some(pending) = state.auth.consume_code(&code) else {
        return token_error(
            "invalid_grant",
            "code is unknown, expired, or already redeemed",
        );
    };
    if pending.client_id != client_id {
        return token_error("invalid_grant", "client_id mismatch");
    }
    if let Some(req_redirect) = &form.redirect_uri {
        if req_redirect != &pending.redirect_uri {
            return token_error("invalid_grant", "redirect_uri mismatch");
        }
    }
    let pkce_ok = match pending.code_challenge_method.as_str() {
        "S256" => oauth::verify_pkce_s256(&verifier, &pending.code_challenge),
        "plain" => oauth::verify_pkce_plain(&verifier, &pending.code_challenge),
        _ => false,
    };
    if !pkce_ok {
        return token_error("invalid_grant", "PKCE verifier did not match challenge");
    }

    let issuer = base_url(&headers, &state);
    let token = match state
        .auth
        .issue_access_token(&pending.client_id, &issuer, &pending.scope)
    {
        Ok(t) => t,
        Err(e) => {
            error!("JWT mint failed: {}", e);
            return token_error("server_error", "failed to mint access token");
        }
    };

    info!(
        "Issued access token for client {} (audience={})",
        pending.client_id, JWT_AUDIENCE
    );

    (
        StatusCode::OK,
        [("Cache-Control", "no-store"), ("Pragma", "no-cache")],
        Json(json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": oauth::ACCESS_TOKEN_TTL_SECS,
            "scope": pending.scope
        })),
    )
        .into_response()
}

fn token_error(code: &str, description: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": code,
            "error_description": description
        })),
    )
        .into_response()
}

/// Allow either an exact registered match or any `http://localhost*` /
/// `http://127.0.0.1*` URI when the client registered with one of those
/// loopback hosts. MCP clients tend to use ephemeral ports for their
/// loopback callback (e.g. `http://127.0.0.1:54123/oauth/callback`),
/// so we accept any localhost URI as long as the client registered
/// for the loopback family.
fn redirect_uri_allowed(registered: &[String], requested: &str) -> bool {
    if registered.iter().any(|r| r == requested) {
        return true;
    }
    let is_loopback = |s: &str| {
        s.starts_with("http://localhost")
            || s.starts_with("http://127.0.0.1")
            || s.starts_with("http://[::1]")
    };
    if registered.iter().any(|r| is_loopback(r)) && is_loopback(requested) {
        return true;
    }
    false
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Kill any leftover ngrok agent process before spawning ours.
///
/// On Windows we shell out to `taskkill /F /IM ngrok.exe /T` to also
/// catch grandchildren (the MSIX alias launches ngrok via an aliased
/// stub, and `taskkill /T` traverses the tree). On other platforms
/// `pkill -f ngrok` does the same job.
///
/// Always best-effort — failures are logged at debug level and don't
/// block the spawn that follows. After the kill we sleep ~700 ms so
/// Cloudflare's side has time to expire the orphan's TCP session;
/// without that, even with the orphan dead, our fresh agent still
/// races into ERR_NGROK_334 occasionally.
async fn sweep_orphan_ngroks() {
    #[cfg(windows)]
    {
        let result = Command::new("taskkill")
            .args(["/F", "/IM", "ngrok.exe", "/T"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .creation_flags(0x0800_0000 | 0x0000_0008)
            .status()
            .await;
        match result {
            Ok(s) if s.success() => {
                info!("Swept orphan ngrok agents before spawn");
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            Ok(_) => {
                // Exit code 128 = "no process found" — the common case
                // on a clean launch.
                debug!("No orphan ngrok agents to sweep");
            }
            Err(e) => {
                debug!("taskkill probe failed (non-fatal): {}", e);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let result = Command::new("pkill")
            .args(["-f", "ngrok"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .status()
            .await;
        match result {
            Ok(s) if s.success() => {
                info!("Swept orphan ngrok agents before spawn");
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            Ok(_) => debug!("No orphan ngrok agents to sweep"),
            Err(e) => debug!("pkill probe failed (non-fatal): {}", e),
        }
    }
}

/// Spawn `ngrok http <port> --log stdout --log-format json` and parse
/// its JSON log lines for the `started tunnel` event.
///
/// We read both stdout and stderr concurrently and keep a ring buffer
/// of the last few lines so a startup failure (no authtoken, ngrok not
/// signed in, network issue) surfaces verbatim in the panel — far more
/// actionable than a generic "tunnel unavailable".
///
/// Binary lookup: `NGROK_BIN` env var override > `ngrok.exe` (Windows)
/// / `ngrok` (other) on `PATH`. Authtoken is read from ngrok's own
/// global config (`ngrok config add-authtoken …` once); we don't try
/// to manage credentials in-process.
async fn run_ngrok(state: Arc<ServerState>, local_port: u16) {
    let binary = std::env::var("NGROK_BIN").unwrap_or_else(|_| {
        if cfg!(windows) {
            "ngrok.exe".to_string()
        } else {
            "ngrok".to_string()
        }
    });

    // Sweep any orphan ngrok process before we spawn a fresh one. This
    // catches:
    //   - previous FerriteS100 crash that left ngrok detached
    //   - MSIX-launched ngrok where our `kill_on_drop` killed the
    //     stub but Windows kept the real worker alive
    //   - the user manually starting ngrok before launching us (rare,
    //     but worth bypassing the friendly ERR_NGROK_334 loop)
    //
    // ngrok's free plan ties one static endpoint to the agent; two
    // agents can only co-exist with `--pooling-enabled` on BOTH, and
    // an orphan started without pooling refuses the new session. So
    // killing first is more robust than just adding the flag.
    sweep_orphan_ngroks().await;

    let mut cmd = Command::new(&binary);
    cmd.arg("http")
        .arg(local_port.to_string())
        .arg("--log")
        .arg("stdout")
        .arg("--log-format")
        .arg("json")
        .arg("--log-level")
        .arg("info")
        // Belt-and-suspenders alongside `sweep_orphan_ngroks`: if a
        // brand-new orphan slips between sweep and spawn, pooling lets
        // both sessions co-exist instead of crashing.
        .arg("--pooling-enabled")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        // Ensure ngrok dies with us. tokio defaults `kill_on_drop` to
        // false; without this, an abrupt FerriteS100 exit leaves
        // ngrok detached, claiming the endpoint until its TCP session
        // times out — exactly the orphan that triggers ERR_NGROK_334
        // on the next launch.
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW (0x08000000) suppresses the console window
        // for ferrite-spawned ngrok. DETACHED_PROCESS (0x00000008)
        // additionally severs the inherited console so the MSIX alias
        // launcher (`%LOCALAPPDATA%\Microsoft\WindowsApps\ngrok.exe`)
        // can't reattach one. Without DETACHED_PROCESS, MSIX-aliased
        // executables still pop a brief console flash on Windows 11.
        // `creation_flags` is provided by `tokio::process::Command` on
        // Windows directly — no extra trait import needed.
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("ngrok spawn failed ({:?}): {}", binary, e);
            state.update_info(|info| {
                info.tunnel_state = TunnelState::Unavailable;
                info.tunnel_message = format!(
                    "ngrok not found on PATH ({}). Install from \
                     https://ngrok.com/download, then run \
                     `ngrok config add-authtoken <your-token>` once. \
                     Or set NGROK_BIN to the full path of ngrok.exe \
                     and restart FerriteS100. The local URL above keeps \
                     working for same-machine clients.",
                    e
                );
            });
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    // Tail buffer of recent log lines. Capped so a chatty ngrok can't
    // grow it unboundedly. Surfaced in the panel on failure.
    let mut tail: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(16);
    const TAIL_CAP: usize = 12;

    let mut url_found = false;
    let scan_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut stdout_open = true;
    let mut stderr_open = true;

    loop {
        if !stdout_open && !stderr_open {
            let status = child.wait().await;
            report_ngrok_exit(&state, status, &tail, url_found);
            return;
        }

        let line_result: std::io::Result<Option<String>> = tokio::select! {
            line = stdout_lines.next_line(), if stdout_open => match line {
                Ok(Some(l)) => Ok(Some(l)),
                Ok(None) => { stdout_open = false; continue; }
                Err(e) => Err(e),
            },
            line = stderr_lines.next_line(), if stderr_open => match line {
                Ok(Some(l)) => Ok(Some(l)),
                Ok(None) => { stderr_open = false; continue; }
                Err(e) => Err(e),
            },
            _ = tokio::time::sleep_until(scan_deadline), if !url_found => {
                state.update_info(|info| {
                    info.tunnel_state = TunnelState::Unavailable;
                    info.tunnel_message = format!(
                        "ngrok started but didn't produce a public URL within 30 s. \
                         Last output:\n{}",
                        tail.iter().cloned().collect::<Vec<_>>().join("\n")
                    );
                });
                continue;
            }
            status = child.wait() => {
                report_ngrok_exit(&state, status, &tail, url_found);
                return;
            }
        };

        let line = match line_result {
            Ok(Some(l)) => l,
            Ok(None) => continue,
            Err(e) => {
                error!("ngrok log read error: {}", e);
                continue;
            }
        };

        debug!(target: "ngrok", "{}", line);
        if tail.len() == TAIL_CAP {
            tail.pop_front();
        }
        tail.push_back(line.clone());

        if !url_found {
            if let Some(url) = extract_ngrok_url(&line) {
                let public = format!("{}/mcp", url);
                info!("ngrok tunnel ready: {}", public);
                state.update_info(|info| {
                    info.tunnel_state = TunnelState::Ready;
                    info.public_url = Some(public);
                    info.tunnel_message.clear();
                });
                url_found = true;
            }
        }
    }
}

/// Common path for "ngrok went away". If we already had a URL the
/// tunnel may have just dropped silently — keep `Ready` but warn.
/// Otherwise surface the tail buffer so the user can see why.
fn report_ngrok_exit(
    state: &Arc<ServerState>,
    status: std::io::Result<std::process::ExitStatus>,
    tail: &std::collections::VecDeque<String>,
    url_found: bool,
) {
    let code_msg = match status {
        Ok(s) => format!("ngrok exited: {}", s),
        Err(e) => format!("ngrok wait error: {}", e),
    };
    warn!("{}", code_msg);

    let tail_msg = if tail.is_empty() {
        String::from("(no log output captured)")
    } else {
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    };

    state.update_info(|info| {
        if !url_found {
            info.tunnel_state = TunnelState::Unavailable;
            info.public_url = None;
        }
        info.tunnel_message = format!("{}\n\nLast output:\n{}", code_msg, tail_msg);
    });
}

/// Extract the public URL from one ngrok JSON log line.
///
/// ngrok's `--log-format json` emits one JSON object per line. The line
/// we care about looks like:
///
/// ```json
/// {"t":"…","lvl":"info","msg":"started tunnel","obj":"tunnels",
///  "name":"command_line","addr":"http://localhost:9999",
///  "url":"https://abc-123.ngrok-free.app"}
/// ```
///
/// We accept any HTTPS URL on a `started tunnel` line — paid plans use
/// `*.ngrok.app` or custom domains, free plan uses `*.ngrok-free.app`.
/// Returns `None` for any line that isn't the tunnel-started event.
fn extract_ngrok_url(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let msg = v.get("msg")?.as_str()?;
    if !msg.eq_ignore_ascii_case("started tunnel") {
        return None;
    }
    let url = v.get("url")?.as_str()?;
    if url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    #[test]
    fn parses_ngrok_started_tunnel_line() {
        let line = r#"{"t":"2026-05-10T01:23:45Z","lvl":"info","msg":"started tunnel","obj":"tunnels","name":"command_line","addr":"http://localhost:9999","url":"https://abc-123.ngrok-free.app"}"#;
        assert_eq!(
            extract_ngrok_url(line),
            Some("https://abc-123.ngrok-free.app".into())
        );
    }

    #[test]
    fn parses_ngrok_paid_domain() {
        // Paid plans use *.ngrok.app or custom domains.
        let line =
            r#"{"lvl":"info","msg":"started tunnel","url":"https://team.example.ngrok.app"}"#;
        assert_eq!(
            extract_ngrok_url(line),
            Some("https://team.example.ngrok.app".into())
        );
    }

    #[test]
    fn skips_other_ngrok_log_lines() {
        let line = r#"{"lvl":"info","msg":"client session established","obj":"tunnels.session"}"#;
        assert_eq!(extract_ngrok_url(line), None);
    }

    #[test]
    fn skips_non_json_text() {
        // Older ngrok versions or non-JSON lines must not panic.
        assert_eq!(extract_ngrok_url("Forwarding https://abc.ngrok.io"), None);
    }

    #[test]
    fn skips_http_only_url() {
        // The HTTPS URL is the canonical one; the parallel http://… URL
        // must not be picked up.
        let line = r#"{"lvl":"info","msg":"started tunnel","url":"http://abc-123.ngrok-free.app"}"#;
        assert_eq!(extract_ngrok_url(line), None);
    }

    #[test]
    fn loopback_redirect_uri_allowed_when_registered_loopback() {
        let registered = vec!["http://127.0.0.1".to_string()];
        assert!(redirect_uri_allowed(
            &registered,
            "http://127.0.0.1:54123/cb"
        ));
        assert!(redirect_uri_allowed(
            &registered,
            "http://localhost:54123/cb"
        ));
        assert!(!redirect_uri_allowed(&registered, "https://example.com/cb"));
    }

    #[test]
    fn redirect_uri_exact_match_allowed() {
        let registered = vec!["https://example.com/cb".to_string()];
        assert!(redirect_uri_allowed(&registered, "https://example.com/cb"));
        assert!(!redirect_uri_allowed(
            &registered,
            "https://attacker.example.com/cb"
        ));
    }

    /// Build a `ServerState` for tests — no chart loaded, no tunnel.
    fn test_state() -> Arc<ServerState> {
        Arc::new(ServerState {
            indices: RwLock::new(None),
            auth: AuthServer::new(),
            info: StdMutex::new(McpServerInfo {
                running: true,
                local_url: "http://127.0.0.1:0/mcp".to_string(),
                public_url: None,
                tunnel_state: TunnelState::Disabled,
                tunnel_message: String::new(),
                auth: "oauth2",
                registered_clients: 0,
                transport: "streamable-http",
            }),
        })
    }

    fn pkce_pair() -> (String, String) {
        let verifier: String = (0..43).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut h = Sha256::new();
        h.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(h.finalize());
        (verifier, challenge)
    }

    /// Walks the full RFC 6749 §4.1 + RFC 7636 + RFC 7591 flow against
    /// the in-process Router: register → authorize/approve → token →
    /// `/mcp` with bearer JWT. Exercises every wired endpoint without
    /// touching a real socket.
    #[tokio::test]
    async fn oauth_full_flow_e2e() {
        let state = test_state();
        let app = build_router(state.clone());

        // 1. /healthz
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 2. POST /mcp without auth → 401 with WWW-Authenticate
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(www.contains("Bearer"), "WWW-Authenticate: {}", www);
        assert!(
            www.contains("resource_metadata="),
            "WWW-Authenticate: {}",
            www
        );

        // 3. AS metadata
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-authorization-server")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 4. Register a client
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"client_name":"test","redirect_uris":["http://localhost:9999/cb"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        let reg: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let client_id = reg["client_id"].as_str().unwrap().to_string();

        // 5. Mint PKCE pair and approve consent
        let (verifier, challenge) = pkce_pair();
        let approve_body = format!(
            "decision=approve&client_id={}&redirect_uri={}&scope=mcp%3Aread\
             &code_challenge={}&code_challenge_method=S256",
            urlencoding::encode(&client_id),
            urlencoding::encode("http://localhost:9999/cb"),
            urlencoding::encode(&challenge)
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/authorize/approve")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(approve_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum::Redirect → 303 See Other
        assert!(
            resp.status().is_redirection(),
            "expected redirect, got {}",
            resp.status()
        );
        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        let code = location
            .split_once("code=")
            .map(|(_, rest)| rest.split('&').next().unwrap_or(rest))
            .map(|s| urlencoding::decode(s).unwrap().into_owned())
            .expect("redirect should contain code=");

        // 6. Exchange code + verifier for access token
        let token_body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding::encode(&code),
            urlencoding::encode("http://localhost:9999/cb"),
            urlencoding::encode(&client_id),
            urlencoding::encode(&verifier)
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/token")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(token_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        let tok: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let access_token = tok["access_token"].as_str().unwrap().to_string();
        assert_eq!(tok["token_type"].as_str().unwrap(), "Bearer");

        // 7. /mcp with the JWT — initialize works without a chart
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {}", access_token))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tools = v["result"]["tools"].as_array().expect("tools list");
        assert_eq!(tools.len(), 9, "expected 9 read-only MCP tools");
    }

    /// PKCE mismatch must be rejected at the token endpoint.
    #[tokio::test]
    async fn oauth_rejects_bad_pkce_verifier() {
        let state = test_state();
        let app = build_router(state.clone());
        // Register
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"redirect_uris":["http://localhost:1/cb"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        let reg: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let client_id = reg["client_id"].as_str().unwrap().to_string();
        // Approve with one challenge
        let (_, challenge) = pkce_pair();
        let approve = format!(
            "decision=approve&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb&scope=mcp%3Aread&code_challenge={}&code_challenge_method=S256",
            urlencoding::encode(&client_id),
            urlencoding::encode(&challenge)
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/authorize/approve")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(approve))
                    .unwrap(),
            )
            .await
            .unwrap();
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        let code = loc
            .split_once("code=")
            .map(|(_, rest)| rest.split('&').next().unwrap_or(rest))
            .map(|s| urlencoding::decode(s).unwrap().into_owned())
            .unwrap();
        // Send a *different* verifier
        let bad_token = format!(
            "grant_type=authorization_code&code={}&redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcb&client_id={}&code_verifier=NOT-THE-RIGHT-VERIFIER",
            urlencoding::encode(&code),
            urlencoding::encode(&client_id)
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/token")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(bad_token))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
