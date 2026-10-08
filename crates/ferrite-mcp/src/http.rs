//! Opt-in loopback Streamable HTTP service. External exposure requires explicit tunnel enablement.
use crate::{
    oauth::{self, Auth, Grant, RegisterRequest, SCOPE},
    service::Registry,
};
use anyhow::{ensure, Result};
use axum::{
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::Semaphore,
};
#[derive(Clone, Default)]
pub struct Info {
    pub url: String,
    pub public_url: Option<String>,
    pub status: String,
}
struct Shared {
    registry: RwLock<Arc<Registry>>,
    auth: Auth,
    info: Mutex<Info>,
    queries: Arc<Semaphore>,
}
pub struct Server {
    runtime: tokio::runtime::Runtime,
    shared: Arc<Shared>,
}
impl Server {
    pub fn start(tunnel: bool) -> Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr: SocketAddr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            registry: RwLock::new(Arc::new(Registry::default())),
            auth: Auth::new(),
            info: Mutex::new(Info {
                url: format!("http://{addr}/mcp"),
                public_url: None,
                status: if tunnel {
                    "Tunnel starting"
                } else {
                    "Local service running"
                }
                .into(),
            }),
            queries: Arc::new(Semaphore::new(2)),
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("ferrite-s100-mcp")
            .build()?;
        let s = shared.clone();
        runtime.spawn(async move {
            match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => {
                    if let Err(e) = axum::serve(l, router(s.clone())).await {
                        s.info.lock().unwrap_or_else(|p| p.into_inner()).status =
                            format!("Service stopped: {e}");
                    }
                }
                Err(e) => {
                    s.info.lock().unwrap_or_else(|p| p.into_inner()).status =
                        format!("Service failed: {e}")
                }
            }
        });
        if tunnel {
            runtime.spawn(run_tunnel(shared.clone(), addr.port()));
        }
        Ok(Self { runtime, shared })
    }
    pub fn info(&self) -> Info {
        self.shared
            .info
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Owner-only native UI access. This value is never included in metadata/discovery responses.
    pub fn approval_code(&self) -> String {
        self.shared.auth.approval_code().into()
    }
    pub fn set_registry(&self, registry: Registry) {
        *self
            .shared
            .registry
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Arc::new(registry);
    }
    pub fn clear(&self) {
        self.set_registry(Registry::default());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.clear(); /* runtime drop cancels owned tunnel; no unrelated processes are killed */
        let _ = &self.runtime;
    }
}
fn base(s: &Shared) -> String {
    let i = s.info.lock().unwrap_or_else(|p| p.into_inner());
    i.public_url
        .as_ref()
        .unwrap_or(&i.url)
        .trim_end_matches("/mcp")
        .into()
}
fn resource(s: &Shared) -> String {
    format!("{}/mcp", base(s))
}
async fn boundary(State(s): State<Arc<Shared>>, r: Request, next: Next) -> Response {
    let i = s.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let local = i.url.trim_end_matches("/mcp");
    let public = i.public_url.as_deref().map(|u| u.trim_end_matches("/mcp"));
    if let Some(origin) = r.headers().get("origin") {
        if !origin
            .to_str()
            .is_ok_and(|o| o == local || Some(o) == public)
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if let Some(h) = r.headers().get("host") {
        let allowed = std::iter::once(local)
            .chain(public)
            .filter_map(|u| url::Url::parse(u).ok())
            .any(|u| {
                h.to_str().ok()
                    == Some(u[url::Position::BeforeHost..url::Position::AfterPort].as_ref())
            });
        if !allowed {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let mut response = next.run(r).await;
    for (k, v) in [
        ("cache-control", "no-store"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(k),
            axum::http::HeaderValue::from_static(v),
        );
    }
    response
}
fn router(s: Arc<Shared>) -> Router {
    Router::new()
        .route("/mcp", post(mcp))
        .route("/.well-known/oauth-protected-resource", get(protected))
        .route("/.well-known/oauth-protected-resource/mcp", get(protected))
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/authorize/approve", post(approve))
        .route("/token", post(token))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(s.clone(), boundary))
        .with_state(s)
}
async fn metadata(State(s): State<Arc<Shared>>) -> Json<Value> {
    let b = base(&s);
    Json(
        json!({"issuer":b,"authorization_endpoint":format!("{b}/authorize"),"token_endpoint":format!("{b}/token"),"registration_endpoint":format!("{b}/register"),"scopes_supported":[SCOPE],"response_types_supported":["code"],"grant_types_supported":["authorization_code"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]}),
    )
}
async fn protected(State(s): State<Arc<Shared>>) -> Json<Value> {
    Json(
        json!({"resource":resource(&s),"authorization_servers":[base(&s)],"scopes_supported":[SCOPE],"bearer_methods_supported":["header"]}),
    )
}
fn error(code: StatusCode, message: impl ToString) -> Response {
    (code, Json(json!({"error":message.to_string()}))).into_response()
}
async fn register(State(s): State<Arc<Shared>>, Json(r): Json<RegisterRequest>) -> Response {
    match s.auth.register(r) {
        Ok(c) => (StatusCode::CREATED, Json(c)).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}
#[derive(Clone, Deserialize)]
struct Authorization {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: String,
    resource: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    state: Option<String>,
}
fn validate_authorization(s: &Shared, a: &Authorization) -> Result<()> {
    ensure!(
        a.response_type == "code"
            && a.code_challenge_method == "S256"
            && oauth::valid_challenge(&a.code_challenge),
        "authorization_code with PKCE S256 required"
    );
    ensure!(a.resource == resource(s), "resource mismatch");
    ensure!(
        a.scope.as_deref().is_none_or(|v| v == SCOPE),
        "scope mismatch"
    );
    ensure!(
        a.state.as_ref().is_none_or(|v| v.len() <= 4096),
        "state too long"
    );
    let c = s
        .auth
        .client(&a.client_id)
        .ok_or_else(|| anyhow::anyhow!("unknown client"))?;
    ensure!(
        oauth::redirect_allowed(&c.redirect_uris, &a.redirect_uri),
        "redirect not registered"
    );
    Ok(())
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
async fn authorize(State(s): State<Arc<Shared>>, Query(a): Query<Authorization>) -> Response {
    if let Err(e) = validate_authorization(&s, &a) {
        return error(StatusCode::BAD_REQUEST, e);
    }
    let mut html=String::from("<!doctype html><meta charset=utf-8><title>FerriteS100 MCP authorization</title><h1>Allow read-only chart access</h1><p>Only approve a client you initiated. Copy the approval code from the application's S-100 MCP window.</p><form method=post action=/authorize/approve>");
    for (k, v) in [
        ("response_type", a.response_type),
        ("client_id", a.client_id.clone()),
        ("redirect_uri", a.redirect_uri.clone()),
        ("code_challenge", a.code_challenge),
        ("code_challenge_method", a.code_challenge_method),
        ("resource", a.resource),
        ("scope", SCOPE.into()),
        ("state", a.state.unwrap_or_default()),
    ] {
        html.push_str(&format!(
            "<input type=hidden name=\"{k}\" value=\"{}\">",
            escape(&v)
        ));
    }
    if let Some(client) = s.auth.client(&a.client_id) {
        html.push_str(&format!(
            "<p>Client: {}</p><p>Callback: {}</p>",
            escape(client.client_name.as_deref().unwrap_or(&client.client_id)),
            escape(&a.redirect_uri)
        ));
    }
    html.push_str("<label>Approval code <input type=password name=approval_code autocomplete=off required></label><button name=decision value=approve>Approve</button><button name=decision value=deny>Deny</button></form>");
    Html(html).into_response()
}
#[derive(Deserialize)]
struct Consent {
    #[serde(flatten)]
    authorization: Authorization,
    decision: String,
    approval_code: String,
}
async fn approve(State(s): State<Arc<Shared>>, Form(c): Form<Consent>) -> Response {
    if let Err(e) = validate_authorization(&s, &c.authorization) {
        return error(StatusCode::BAD_REQUEST, e);
    }
    let a = c.authorization;
    let mut u = match url::Url::parse(&a.redirect_uri) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if c.decision != "approve" {
        u.query_pairs_mut().append_pair("error", "access_denied");
    } else {
        if !s.auth.approve(&c.approval_code) {
            return error(
                StatusCode::FORBIDDEN,
                "approval must be granted by the desktop owner",
            );
        }
        let g = Grant {
            client_id: a.client_id,
            redirect_uri: a.redirect_uri,
            challenge: a.code_challenge,
            resource: a.resource,
            expires: oauth::now() + 300,
        };
        match s.auth.code(g) {
            Ok(code) => {
                u.query_pairs_mut().append_pair("code", &code);
            }
            Err(e) => return error(StatusCode::TOO_MANY_REQUESTS, e),
        }
    }
    if let Some(state) = a.state {
        u.query_pairs_mut().append_pair("state", &state);
    }
    Redirect::to(u.as_str()).into_response()
}
#[derive(Deserialize)]
struct Token {
    grant_type: String,
    client_id: String,
    code: String,
    redirect_uri: String,
    code_verifier: String,
    resource: String,
}
async fn token(State(s): State<Arc<Shared>>, Form(t): Form<Token>) -> Response {
    if t.grant_type != "authorization_code" {
        return error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    let Some(g) = s.auth.consume(&t.code) else {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    if g.client_id != t.client_id
        || g.redirect_uri != t.redirect_uri
        || g.resource != t.resource
        || g.resource != resource(&s)
        || !oauth::verify_pkce(&t.code_verifier, &g.challenge)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    match s.auth.token(&g) {
        Ok(token) => Json(
            json!({"access_token":token,"token_type":"Bearer","expires_in":3600,"scope":SCOPE}),
        )
        .into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn mcp(State(s): State<Arc<Shared>>, headers: HeaderMap, body: String) -> Response {
    let valid = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| s.auth.validate(t, &resource(&s)).is_ok());
    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                format!(
                    "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                    base(&s)
                ),
            )],
            Json(json!({"error":"OAuth bearer token required"})),
        )
            .into_response();
    }
    if headers
        .get("mcp-protocol-version")
        .is_some_and(|v| v != "2025-06-18" && v != "2025-03-26")
    {
        return error(StatusCode::BAD_REQUEST, "unsupported protocol version");
    }
    let req: crate::mcp::server::JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid JSON-RPC"),
    };
    if req.jsonrpc != "2.0"
        || req
            .id
            .as_ref()
            .is_some_and(|v| !v.is_null() && !v.is_string() && !v.is_number())
    {
        return error(StatusCode::BAD_REQUEST, "invalid JSON-RPC request");
    }
    let Some(id) = req.id else {
        return if req.method.starts_with("notifications/") {
            StatusCode::ACCEPTED.into_response()
        } else {
            error(
                StatusCode::BAD_REQUEST,
                "only MCP notifications may omit id",
            )
        };
    };
    let result = match req.method.as_str() {
        "initialize" => Ok(
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"ferrite-s100-mcp","version":env!("CARGO_PKG_VERSION")}}),
        ),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":crate::service::descriptors()})),
        "tools/call" => {
            let Some(name) = req
                .params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                return error(StatusCode::BAD_REQUEST, "tool name required");
            };
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let registry = s.registry.read().unwrap_or_else(|p| p.into_inner()).clone();
            let permit = match s.queries.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "query capacity reached"),
            };
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                registry.call(&name, args)
            })
            .await
            {
                Ok(Ok(payload)) => Ok(
                    json!({"content":[{"type":"text","text":payload.to_string()}],"structuredContent":payload,"isError":false}),
                ),
                Ok(Err(e)) => {
                    Ok(json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}))
                }
                Err(_) => Err((-32603, "query worker failed")),
            }
        }
        _ => Err((-32601, "method not found")),
    };
    match result {
        Ok(result) => Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response(),
        Err((code, message)) => {
            Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
                .into_response()
        }
    }
}
async fn run_tunnel(s: Arc<Shared>, port: u16) {
    let binary = std::env::var("NGROK_BIN").unwrap_or_else(|_| "ngrok".into());
    let mut cmd = Command::new(binary);
    cmd.args([
        "http",
        &format!("http://127.0.0.1:{port}"),
        "--log",
        "stdout",
        "--log-format",
        "json",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            s.info.lock().unwrap_or_else(|p| p.into_inner()).status =
                format!("Tunnel unavailable: {e}");
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    let mut lines = BufReader::new(stdout).lines();
    // Only the owned process is cancelled, and startup has a finite deadline.
    let startup = async {
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() > 8192 {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v["msg"] == "started tunnel" {
                    if let Some(u) = v["url"].as_str() {
                        if url::Url::parse(u).is_ok_and(|u| {
                            u.scheme() == "https"
                                && u.host_str().is_some()
                                && u.username().is_empty()
                                && u.password().is_none()
                                && u.path() == "/"
                        }) {
                            return Some(format!("{}/mcp", u.trim_end_matches('/')));
                        }
                    }
                }
            }
        }
        None
    };
    match tokio::time::timeout(std::time::Duration::from_secs(30), startup).await {
        Ok(Some(u)) => {
            let mut i = s.info.lock().unwrap_or_else(|p| p.into_inner());
            i.public_url = Some(u);
            i.status = "Public tunnel running".into();
        }
        _ => {
            let _ = child.kill().await;
            s.info.lock().unwrap_or_else(|p| p.into_inner()).status =
                "Tunnel failed to start".into();
            return;
        }
    }
    // Drain output while monitoring lifecycle to avoid a blocked stdout pipe.
    loop {
        tokio::select! {_ = child.wait()=>break,line=lines.next_line()=>{if !matches!(line,Ok(Some(_))){let _=child.wait().await;break}}}
    }
    let mut i = s.info.lock().unwrap_or_else(|p| p.into_inner());
    i.public_url = None;
    i.status = "Tunnel stopped; local service available".into();
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    fn state() -> Arc<Shared> {
        Arc::new(Shared {
            registry: RwLock::new(Arc::new(Registry::default())),
            auth: Auth::new(),
            info: Mutex::new(Info {
                url: "http://127.0.0.1:1234/mcp".into(),
                ..Default::default()
            }),
            queries: Arc::new(Semaphore::new(2)),
        })
    }
    #[tokio::test]
    async fn unauthenticated_and_rebinding_rejected() {
        let s = state();
        let app = router(s);
        for (header, value, status) in [
            ("authorization", "Bearer bad", 401),
            ("origin", "https://attacker.invalid", 403),
            ("host", "attacker.invalid", 403),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header(header, value)
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), status);
        }
    }
    #[tokio::test]
    async fn forwarded_headers_cannot_change_issuer() {
        let s = state();
        let resp = router(s)
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-authorization-server")
                    .header("x-forwarded-host", "attacker.invalid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(v["issuer"], "http://127.0.0.1:1234");
    }
    #[tokio::test]
    async fn notification_is_empty_202_and_tools_work_without_chart() {
        let s = state();
        let g = Grant {
            client_id: "c".into(),
            redirect_uri: "http://localhost/cb".into(),
            challenge: "a".repeat(43),
            resource: resource(&s),
            expires: oauth::now() + 300,
        };
        let token = s.auth.token(&g).unwrap();
        let app = router(s);
        for (method, id, status) in [
            ("notifications/initialized", None, 202),
            ("tools/list", Some(1), 200),
        ] {
            let mut body = json!({"jsonrpc":"2.0","method":method});
            if let Some(id) = id {
                body["id"] = json!(id)
            }
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), status);
            if status == 202 {
                assert!(to_bytes(resp.into_body(), 65536).await.unwrap().is_empty());
            }
        }
    }
    #[tokio::test]
    async fn mixed_product_registry_is_available_over_authenticated_http() {
        let s = state();
        *s.registry.write().unwrap() = Arc::new(
            Registry::new([crate::service::Dataset {
                id: "depth".into(),
                product: "S-102".into(),
                metadata: json!({"fc":"3.0.0","pc":"3.0.0"}),
                s101: None,
            }])
            .unwrap(),
        );
        let g = Grant {
            client_id: "test".into(),
            redirect_uri: "http://localhost/cb".into(),
            challenge: "a".repeat(43),
            resource: resource(&s),
            expires: oauth::now() + 300,
        };
        let bearer = s.auth.token(&g).unwrap();
        let app = router(s);
        for (name, args, is_error) in [
            ("datasets_list", json!({}), false),
            ("dataset_metadata", json!({"dataset_id":"depth"}), false),
            ("feature_get", json!({"dataset_id":"depth","id":1}), true),
        ] {
            let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header("authorization", format!("Bearer {bearer}"))
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let v: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                    .unwrap();
            assert_eq!(v["result"]["isError"], is_error);
        }
    }

    #[tokio::test]
    async fn oauth_requires_desktop_consent_and_single_use_pkce() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use sha2::{Digest, Sha256};
        let s = state();
        let client = s
            .auth
            .register(RegisterRequest {
                redirect_uris: vec!["http://localhost:7/cb".into()],
                client_name: None,
                scope: None,
                grant_types: None,
                response_types: None,
                token_endpoint_auth_method: None,
            })
            .unwrap();
        let app = router(s.clone());
        let verifier = "v".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let pairs = vec![
            ("response_type", "code"),
            ("client_id", &client.client_id),
            ("redirect_uri", "http://localhost:7/cb"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("resource", "http://127.0.0.1:1234/mcp"),
            ("decision", "approve"),
            ("scope", SCOPE),
        ];
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        let request = |body: String| {
            Request::builder()
                .method("POST")
                .uri("/authorize/approve")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };
        let response = app
            .clone()
            .oneshot(request(format!("{encoded}&approval_code=bad")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(request(format!(
                "{encoded}&approval_code={}",
                s.auth.approval_code()
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let redirect = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        let code = redirect
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned();
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "authorization_code"),
                ("client_id", client.client_id.as_str()),
                ("redirect_uri", "http://localhost:7/cb"),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("resource", "http://127.0.0.1:1234/mcp"),
            ])
            .finish();
        for expected in [StatusCode::OK, StatusCode::BAD_REQUEST] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/token")
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }
}
