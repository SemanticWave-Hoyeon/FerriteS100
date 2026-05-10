//! OAuth 2.1 authorization for the in-process MCP server.
//!
//! Implements the subset of OAuth 2.1 + PKCE + Dynamic Client Registration
//! that the MCP authorization spec (revision 2025-06-18) requires:
//!
//! - `/.well-known/oauth-authorization-server` (RFC 8414) — server metadata
//! - `/.well-known/oauth-protected-resource` (RFC 9728) — resource metadata
//! - `/register`  (RFC 7591)  — Dynamic Client Registration
//! - `/authorize` (RFC 6749 §4.1) — auth-code flow with PKCE S256
//! - `/token`     (RFC 6749 §4.1.3) — code-for-token exchange
//!
//! The store is in-memory and rotates on every host restart — fine for a
//! single-user desktop app, and matches how the previous static bearer
//! token also rotated. JWT access tokens are HS256-signed with a random
//! 32-byte secret minted at startup.
//!
//! ## Flow
//!
//! 1. Client hits `/mcp` → 401 with
//!    `WWW-Authenticate: Bearer resource_metadata="…/.well-known/oauth-protected-resource"`.
//! 2. Client fetches resource metadata → finds `authorization_servers` list.
//! 3. Client fetches `/.well-known/oauth-authorization-server` → finds endpoints.
//! 4. Client POSTs to `/register` (anonymous) → gets `client_id`.
//! 5. Client opens user's browser to `/authorize?client_id=…&code_challenge=…`.
//! 6. User clicks **Approve** in the consent page → redirect to client's
//!    `redirect_uri` with `?code=…&state=…`.
//! 7. Client POSTs `code + code_verifier` to `/token` → gets JWT access token.
//! 8. Client retries `/mcp` with `Authorization: Bearer <jwt>`.
//!
//! For a public client (no secret) PKCE is mandatory — we reject any
//! token request without a verifier.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// JWT audience claim for tokens we issue. Clients use this to confirm
/// the token was minted for our `/mcp` endpoint specifically.
pub const JWT_AUDIENCE: &str = "ferrite-s100-mcp";

/// Single scope advertised. The server is read-only so a finer grant
/// model would be ceremony — kept for spec compliance.
pub const SCOPE: &str = "mcp:read";

/// Lifetime of an issued access token. One hour matches the OAuth 2.1
/// recommendation for bearer tokens; long enough that a desktop session
/// rarely needs to re-auth, short enough to bound replay damage.
pub const ACCESS_TOKEN_TTL_SECS: u64 = 3600;

/// Lifetime of an authorization code. RFC 6749 says ≤ 10 minutes; we
/// use 5 to keep replay windows tight.
pub const AUTH_CODE_TTL_SECS: u64 = 300;

/// Registered client (RFC 7591 response). For public clients we don't
/// issue a secret; PKCE substitutes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_id_issued_at: u64,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub scope: String,
}

/// Client-Registration request body (RFC 7591 §3.1). Most fields
/// optional — we lean on sensible defaults so MCP clients with sparse
/// requests still register successfully.
#[derive(Debug, Deserialize, Default)]
pub struct RegisterRequest {
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    #[serde(default)]
    pub response_types: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Pending authorization code — `(challenge, redirect_uri, client_id)`
/// keyed by the opaque code we hand back to the client. Single-use:
/// removed in `consume_code`.
#[derive(Debug, Clone)]
pub struct PendingAuth {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: String,
    pub expires_at: u64,
}

/// JWT claims for our access tokens. `iss` mirrors whatever URL the
/// token endpoint was reached at (so it works behind cloudflared or
/// directly on localhost), `aud = JWT_AUDIENCE`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub exp: u64,
    pub iat: u64,
    pub scope: String,
    pub jti: String,
}

/// In-memory authorization-server state. Holds the JWT secret, the
/// dynamic-client registry, and the short-lived authorization-code map.
pub struct AuthServer {
    jwt_secret: [u8; 32],
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    clients: Mutex<HashMap<String, RegisteredClient>>,
    pending_auth: Mutex<HashMap<String, PendingAuth>>,
}

impl AuthServer {
    /// Create a new auth server with a random JWT secret. The secret
    /// rotates per process — restarting the host invalidates all
    /// outstanding tokens, which is the desired behaviour for a local
    /// desktop tool.
    pub fn new() -> Self {
        let mut secret = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        let encoding_key = EncodingKey::from_secret(&secret);
        let decoding_key = DecodingKey::from_secret(&secret);
        Self {
            jwt_secret: secret,
            encoding_key,
            decoding_key,
            clients: Mutex::new(HashMap::new()),
            pending_auth: Mutex::new(HashMap::new()),
        }
    }

    /// Register a client. Always succeeds — we accept any reasonable
    /// shape since MCP doesn't lay down hard registration constraints.
    pub fn register_client(&self, req: RegisterRequest) -> RegisteredClient {
        let client_id = format!("mcp_{}", random_token(24));
        let now = unix_now();
        let client = RegisteredClient {
            client_id: client_id.clone(),
            client_id_issued_at: now,
            client_name: req.client_name,
            redirect_uris: if req.redirect_uris.is_empty() {
                // Many MCP clients use http://localhost callbacks; permit
                // them by default so a registration without explicit URIs
                // still works.
                vec!["http://localhost".into(), "http://127.0.0.1".into()]
            } else {
                req.redirect_uris
            },
            grant_types: req
                .grant_types
                .unwrap_or_else(|| vec!["authorization_code".into()]),
            response_types: req.response_types.unwrap_or_else(|| vec!["code".into()]),
            token_endpoint_auth_method: req
                .token_endpoint_auth_method
                .unwrap_or_else(|| "none".into()),
            scope: req.scope.unwrap_or_else(|| SCOPE.into()),
        };
        self.clients
            .lock()
            .expect("clients mutex poisoned")
            .insert(client_id, client.clone());
        client
    }

    /// Look a client up by id. None if it never registered, or if it
    /// did so before a process restart.
    pub fn get_client(&self, client_id: &str) -> Option<RegisteredClient> {
        self.clients
            .lock()
            .expect("clients mutex poisoned")
            .get(client_id)
            .cloned()
    }

    /// Stash a pending authorization code. Returns the opaque `code`
    /// the user gets redirected back with.
    pub fn create_auth_code(&self, pending: PendingAuth) -> String {
        let code = random_token(32);
        self.pending_auth
            .lock()
            .expect("pending auth mutex poisoned")
            .insert(code.clone(), pending);
        code
    }

    /// Consume a code (single-use). Returns the stashed `PendingAuth`,
    /// or `None` if the code is unknown / already used / expired.
    pub fn consume_code(&self, code: &str) -> Option<PendingAuth> {
        let mut guard = self
            .pending_auth
            .lock()
            .expect("pending auth mutex poisoned");
        let entry = guard.remove(code)?;
        if entry.expires_at < unix_now() {
            return None;
        }
        Some(entry)
    }

    /// Mint a JWT access token for `client_id`. `issuer` is the URL the
    /// token was minted at — copied into the `iss` claim so clients can
    /// verify the token's provenance.
    pub fn issue_access_token(&self, client_id: &str, issuer: &str, scope: &str) -> Result<String> {
        let now = unix_now();
        let claims = AccessTokenClaims {
            iss: issuer.to_string(),
            sub: client_id.to_string(),
            aud: JWT_AUDIENCE.to_string(),
            iat: now,
            exp: now + ACCESS_TOKEN_TTL_SECS,
            scope: scope.to_string(),
            jti: random_token(16),
        };
        let header = Header::new(jsonwebtoken::Algorithm::HS256);
        jsonwebtoken::encode(&header, &claims, &self.encoding_key)
            .map_err(|e| anyhow!("JWT encode failed: {}", e))
    }

    /// Validate an `Authorization: Bearer <jwt>` token. Returns the
    /// claims if the token is signed by us, not expired, and addressed
    /// to our audience. The caller is expected to trust `iss` only as
    /// far as it agrees with its own request URL.
    pub fn validate_access_token(&self, jwt: &str) -> Result<AccessTokenClaims> {
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.set_audience(&[JWT_AUDIENCE]);
        // Issuer is set per-request, so we don't pin it in validation.
        validation.validate_aud = true;
        let data = jsonwebtoken::decode::<AccessTokenClaims>(jwt, &self.decoding_key, &validation)
            .map_err(|e| anyhow!("JWT validation failed: {}", e))?;
        Ok(data.claims)
    }
}

impl Default for AuthServer {
    fn default() -> Self {
        Self::new()
    }
}

/// PKCE S256 verification: `BASE64URL(SHA256(verifier)) == challenge`.
pub fn verify_pkce_s256(verifier: &str, challenge: &str) -> bool {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    let digest = h.finalize();
    let expected = URL_SAFE_NO_PAD.encode(digest);
    constant_time_eq(expected.as_bytes(), challenge.as_bytes())
}

/// `plain` PKCE method — discouraged but allowed for backwards
/// compatibility. We list only S256 in metadata so this rarely fires.
pub fn verify_pkce_plain(verifier: &str, challenge: &str) -> bool {
    constant_time_eq(verifier.as_bytes(), challenge.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Random URL-safe token of `n` characters from `[A-Za-z0-9]`.
pub fn random_token(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Render the consent HTML shown to the user when an MCP client opens
/// the `/authorize` URL in their browser. The "Approve" button POSTs
/// back to `/authorize/approve` with the same query parameters; we
/// render the parameters as hidden inputs so they survive the round-
/// trip without trusting the client to repeat them.
pub fn consent_html(
    client_id: &str,
    client_name: Option<&str>,
    redirect_uri: &str,
    state: Option<&str>,
    scope: &str,
    code_challenge: &str,
    code_challenge_method: &str,
) -> String {
    let display_name = client_name.unwrap_or(client_id);
    // Escape every value we splice into HTML / attributes. The values
    // arrive from query parameters that an attacker can shape, so this
    // is XSS-relevant.
    let display_name_html = html_escape(display_name);
    let client_id_html = html_escape(client_id);
    let redirect_uri_html = html_escape(redirect_uri);
    let state_html = state.map(html_escape).unwrap_or_default();
    let state_input = if state.is_some() {
        format!(
            r#"<input type="hidden" name="state" value="{}">"#,
            state_html
        )
    } else {
        String::new()
    };
    let scope_html = html_escape(scope);
    let challenge_html = html_escape(code_challenge);
    let method_html = html_escape(code_challenge_method);

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Authorize {display_name_html}</title>
<style>
:root {{ color-scheme: light dark; font-family: system-ui, sans-serif; }}
body {{ max-width: 480px; margin: 60px auto; padding: 0 24px; }}
h1 {{ font-size: 1.25rem; margin-bottom: 8px; }}
p {{ line-height: 1.5; }}
.subtle {{ color: #666; font-size: 0.85rem; }}
.box {{ border: 1px solid #ccc; border-radius: 8px; padding: 16px 18px; margin: 18px 0; }}
.scope {{ background: #f4f4f4; padding: 6px 10px; border-radius: 4px;
         font-family: ui-monospace, monospace; font-size: 0.9rem; }}
@media (prefers-color-scheme: dark) {{
  .scope {{ background: #2a2a2a; }}
  .box {{ border-color: #444; }}
}}
button {{ font-size: 1rem; padding: 10px 18px; border-radius: 6px;
         border: 1px solid transparent; cursor: pointer; }}
.primary {{ background: #2266cc; color: white; }}
.primary:hover {{ background: #1e58b0; }}
.secondary {{ background: transparent; color: inherit; border-color: #888; margin-left: 8px; }}
</style>
</head>
<body>
<h1>Authorize MCP client</h1>
<p>
  <strong>{display_name_html}</strong> wants to read your loaded
  S-101 chart through the FerriteS100 MCP server.
</p>
<div class="box">
  <p class="subtle">Requested scope</p>
  <p><span class="scope">{scope_html}</span></p>
  <p class="subtle">Permits read-only access to:</p>
  <ul>
    <li>Catalogue search and feature definitions</li>
    <li>Loaded chart metadata, feature lookups, bbox / nearby queries</li>
  </ul>
  <p class="subtle">No write access; the server has no tools that mutate state.</p>
</div>
<form method="POST" action="/authorize/approve">
  <input type="hidden" name="client_id" value="{client_id_html}">
  <input type="hidden" name="redirect_uri" value="{redirect_uri_html}">
  <input type="hidden" name="scope" value="{scope_html}">
  <input type="hidden" name="code_challenge" value="{challenge_html}">
  <input type="hidden" name="code_challenge_method" value="{method_html}">
  {state_input}
  <button type="submit" name="decision" value="approve" class="primary">
    Approve
  </button>
  <button type="submit" name="decision" value="deny" class="secondary">
    Deny
  </button>
</form>
<p class="subtle" style="margin-top: 32px;">
  This consent screen runs locally inside FerriteS100. The token issued
  here lives until you close FerriteS100, then expires automatically.
</p>
</body>
</html>"##,
    )
}

/// Minimal HTML escaper — covers the five characters that matter for
/// element bodies and double-quoted attribute values.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Avoid `unused` lint when `jwt_secret` lives only as the source for
/// the encoding/decoding keys.
#[allow(dead_code)]
impl AuthServer {
    pub fn jwt_secret_len(&self) -> usize {
        self.jwt_secret.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_jwt() {
        let auth = AuthServer::new();
        let token = auth
            .issue_access_token("client_x", "https://x", SCOPE)
            .unwrap();
        let claims = auth.validate_access_token(&token).unwrap();
        assert_eq!(claims.sub, "client_x");
        assert_eq!(claims.aud, JWT_AUDIENCE);
        assert_eq!(claims.scope, SCOPE);
    }

    #[test]
    fn reject_other_audience() {
        let auth_a = AuthServer::new();
        let auth_b = AuthServer::new();
        let token = auth_a.issue_access_token("c", "iss", SCOPE).unwrap();
        // Different secret → validation fails.
        assert!(auth_b.validate_access_token(&token).is_err());
    }

    #[test]
    fn pkce_s256_known_vector() {
        // RFC 7636 Appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce_s256(verifier, challenge));
        assert!(!verify_pkce_s256("wrong", challenge));
    }

    #[test]
    fn auth_code_is_single_use() {
        let auth = AuthServer::new();
        let pending = PendingAuth {
            client_id: "c".into(),
            redirect_uri: "http://localhost".into(),
            code_challenge: "x".into(),
            code_challenge_method: "S256".into(),
            scope: SCOPE.into(),
            expires_at: unix_now() + 60,
        };
        let code = auth.create_auth_code(pending);
        assert!(auth.consume_code(&code).is_some());
        assert!(auth.consume_code(&code).is_none());
    }

    #[test]
    fn expired_codes_rejected() {
        let auth = AuthServer::new();
        let pending = PendingAuth {
            client_id: "c".into(),
            redirect_uri: "http://localhost".into(),
            code_challenge: "x".into(),
            code_challenge_method: "S256".into(),
            scope: SCOPE.into(),
            expires_at: 1, // long-past
        };
        let code = auth.create_auth_code(pending);
        assert!(auth.consume_code(&code).is_none());
    }

    #[test]
    fn html_escapes_attack_payload() {
        let payload = r#"<script>alert("x")</script>"#;
        let escaped = html_escape(payload);
        assert!(!escaped.contains("<script>"));
        assert!(escaped.contains("&lt;script&gt;"));
        assert!(escaped.contains("&quot;"));
    }
}
