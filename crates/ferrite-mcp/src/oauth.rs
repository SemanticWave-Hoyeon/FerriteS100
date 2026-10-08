//! Process-local OAuth authorization codes, PKCE S256 and resource-bound JWTs.
use anyhow::{ensure, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
pub const SCOPE: &str = "mcp:read";
const CAP: usize = 128;
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn random_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}
pub fn equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
pub fn valid_challenge(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
pub fn verify_pkce(verifier: &str, challenge: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
        && equal(
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            challenge,
        )
}
#[derive(Deserialize)]
pub struct RegisterRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    #[serde(default)]
    pub response_types: Option<Vec<String>>,
}
#[derive(Clone, Serialize)]
pub struct Client {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub token_endpoint_auth_method: &'static str,
    pub scope: &'static str,
    pub grant_types: [&'static str; 1],
    pub response_types: [&'static str; 1],
}
#[derive(Clone)]
pub struct Grant {
    pub client_id: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub resource: String,
    pub expires: u64,
}
#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub exp: u64,
    pub iat: u64,
    pub scope: String,
}
pub struct Auth {
    secret: [u8; 32],
    clients: Mutex<HashMap<String, Client>>,
    codes: Mutex<HashMap<String, Grant>>,
    approval: String,
}
impl Auth {
    pub fn new() -> Self {
        let mut secret = [0; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        Self {
            secret,
            clients: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
            approval: random_token(),
        }
    }
    pub fn approval_code(&self) -> &str {
        &self.approval
    }
    pub fn approve(&self, code: &str) -> bool {
        equal(code, &self.approval)
    }
    pub fn register(&self, r: RegisterRequest) -> Result<Client> {
        ensure!(
            !r.redirect_uris.is_empty() && r.redirect_uris.len() <= 8,
            "1..8 redirect URIs required"
        );
        for u in &r.redirect_uris {
            ensure!(u.len() <= 4096 && valid_redirect(u), "invalid redirect URI");
        }
        ensure!(
            r.client_name.as_ref().is_none_or(|s| s.len() <= 256),
            "client name too long"
        );
        ensure!(
            r.token_endpoint_auth_method
                .as_deref()
                .is_none_or(|v| v == "none"),
            "only public PKCE clients supported"
        );
        ensure!(
            r.scope.as_deref().is_none_or(|v| v == SCOPE),
            "unsupported scope"
        );
        ensure!(
            r.grant_types
                .as_ref()
                .is_none_or(|v| v == &["authorization_code"]),
            "unsupported grant type"
        );
        ensure!(
            r.response_types.as_ref().is_none_or(|v| v == &["code"]),
            "unsupported response type"
        );
        let mut clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            clients.len() < CAP,
            "client capacity reached; restart service to reset"
        );
        let c = Client {
            client_id: random_token(),
            redirect_uris: r.redirect_uris,
            client_name: r.client_name,
            token_endpoint_auth_method: "none",
            scope: SCOPE,
            grant_types: ["authorization_code"],
            response_types: ["code"],
        };
        clients.insert(c.client_id.clone(), c.clone());
        Ok(c)
    }
    pub fn client(&self, id: &str) -> Option<Client> {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }
    pub fn code(&self, g: Grant) -> Result<String> {
        let mut codes = self.codes.lock().unwrap_or_else(|p| p.into_inner());
        codes.retain(|_, g| g.expires > now());
        ensure!(codes.len() < CAP, "pending authorization capacity reached");
        let code = random_token();
        codes.insert(code.clone(), g);
        Ok(code)
    }
    pub fn consume(&self, code: &str) -> Option<Grant> {
        self.codes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(code)
            .filter(|g| g.expires > now())
    }
    pub fn token(&self, g: &Grant) -> Result<String> {
        let issuer = g.resource.trim_end_matches("/mcp").to_owned();
        Ok(jsonwebtoken::encode(
            &Header::new(jsonwebtoken::Algorithm::HS256),
            &Claims {
                iss: issuer,
                aud: g.resource.clone(),
                sub: g.client_id.clone(),
                iat: now(),
                exp: now() + 3600,
                scope: SCOPE.into(),
            },
            &EncodingKey::from_secret(&self.secret),
        )?)
    }
    pub fn validate(&self, token: &str, resource: &str) -> Result<Claims> {
        let mut v = Validation::new(jsonwebtoken::Algorithm::HS256);
        v.set_audience(&[resource]);
        v.set_issuer(&[resource.trim_end_matches("/mcp")]);
        v.leeway = 0;
        let c = jsonwebtoken::decode::<Claims>(token, &DecodingKey::from_secret(&self.secret), &v)?
            .claims;
        ensure!(c.scope == SCOPE, "invalid scope");
        Ok(c)
    }
}
fn valid_redirect(s: &str) -> bool {
    let Ok(u) = url::Url::parse(s) else {
        return false;
    };
    u.fragment().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && (u.scheme() == "https" && u.host_str().is_some()
            || u.scheme() == "http"
                && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
}
pub fn redirect_allowed(registered: &[String], s: &str) -> bool {
    if !valid_redirect(s) {
        return false;
    }
    registered.iter().any(|r| {
        if r == s {
            return true;
        }
        let (Ok(mut a), Ok(mut b)) = (url::Url::parse(r), url::Url::parse(s)) else {
            return false;
        };
        if a.scheme() != "http"
            || b.scheme() != "http"
            || !matches!(a.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        {
            return false;
        }
        let _ = a.set_port(None);
        let _ = b.set_port(None);
        a == b
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirects_do_not_accept_prefix_spoof_or_different_path() {
        let r = vec!["http://localhost:1/cb".into()];
        assert!(redirect_allowed(&r, "http://localhost:777/cb"));
        for u in [
            "http://localhost.evil/cb",
            "http://localhost@evil/cb",
            "http://127.0.0.1.evil/cb",
            "http://localhost:1/else",
            "javascript:alert(1)",
        ] {
            assert!(!redirect_allowed(&r, u));
        }
    }
    #[test]
    fn jwt_is_process_and_resource_bound() {
        let a = Auth::new();
        let g = Grant {
            client_id: "c".into(),
            redirect_uri: "http://localhost/cb".into(),
            challenge: "x".into(),
            resource: "http://127.0.0.1:1/mcp".into(),
            expires: now() + 300,
        };
        let t = a.token(&g).unwrap();
        assert!(a.validate(&t, &g.resource).is_ok());
        assert!(a.validate(&t, "http://127.0.0.1:2/mcp").is_err());
        assert!(Auth::new().validate(&t, &g.resource).is_err());
    }
    #[test]
    fn codes_single_use_and_expired() {
        let a = Auth::new();
        let mut g = Grant {
            client_id: "c".into(),
            redirect_uri: "http://localhost/cb".into(),
            challenge: "x".into(),
            resource: "http://127.0.0.1:1/mcp".into(),
            expires: now() + 300,
        };
        let c = a.code(g.clone()).unwrap();
        assert!(a.consume(&c).is_some());
        assert!(a.consume(&c).is_none());
        g.expires = now() - 1;
        let c = a.code(g).unwrap();
        assert!(a.consume(&c).is_none());
    }
    #[test]
    fn pkce_requires_valid_verifier() {
        let v = "a".repeat(43);
        let c = URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes()));
        assert!(verify_pkce(&v, &c));
        assert!(!verify_pkce("short", &c));
        assert!(!verify_pkce(&"b".repeat(43), &c));
    }
    #[test]
    fn consent_secret_is_not_client_registration() {
        let a = Auth::new();
        assert!(!a.approve("approve"));
        assert!(a.approve(a.approval_code()));
    }
}
