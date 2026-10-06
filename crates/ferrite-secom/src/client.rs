use super::*;
use reqwest::{blocking::Client, Certificate, Identity, Url};
use std::{
    io::Read,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Blocking read client. Run outside the UI thread. Transport PKI and payload PKI
/// are independently configured; redirects, implicit proxies and HTTP are disabled.
pub struct SecomClient {
    http: Client,
    base: Url,
    trust: PayloadTrust,
    max_response: usize,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetResponse {
    data_response_object: Vec<DataResponse>,
    pagination: serde_json::Value,
}
pub struct VerifiedPage {
    pub payloads: Vec<VerifiedPayload>,
    pub pagination: serde_json::Value,
}
impl SecomClient {
    pub fn new(
        base: &str,
        server_ca_pem: &[u8],
        client_pkcs12: &[u8],
        password: &str,
        trust: PayloadTrust,
        max_response: usize,
    ) -> Result<Self> {
        let mut base = Url::parse(base)?;
        ensure!(
            base.scheme() == "https"
                && base.host_str().is_some()
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "SECOM base must be an HTTPS URL without credentials, query or fragment"
        );
        ensure!(
            max_response > 0 && max_response <= 256 * 1024 * 1024,
            "Invalid response size limit"
        );
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        // Parse the existing PKCS#12 input without delegating the TLS handshake
        // to OS-specific client certificate implementations.
        let identity = openssl::pkcs12::Pkcs12::from_der(client_pkcs12)?
            .parse2(password)
            .context("Invalid SECOM client PKCS#12 identity")?;
        let cert = identity
            .cert
            .context("Client identity has no certificate")?;
        let key = identity
            .pkey
            .context("Client identity has no private key")?;
        ensure!(
            cert.public_key()?.public_eq(&key),
            "Client identity key mismatch"
        );
        let mut pem = cert.to_pem()?;
        if let Some(chain) = identity.ca {
            for certificate in chain {
                pem.extend_from_slice(&certificate.to_pem()?);
            }
        }
        pem.extend_from_slice(&key.private_key_to_pem_pkcs8()?);
        let tls_identity = Identity::from_pem(&pem)?;
        let http = Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(Certificate::from_pem(server_ca_pem)?)
            .identity(tls_identity)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            base,
            trust,
            max_response,
        })
    }
    fn request<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.base.join(endpoint)?;
        let response = self
            .http
            .get(url)
            .query(query)
            .header("Accept", "application/json")
            .send()
            .map_err(|e| anyhow::Error::new(e.without_url()))
            .context("SECOM HTTPS request failed")?;
        ensure!(
            response.status().is_success(),
            "SECOM HTTP status {}",
            response.status().as_u16()
        );
        let ct = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|s| s.to_str().ok())
            .unwrap_or("");
        ensure!(
            ct.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json")),
            "SECOM response is not JSON"
        );
        if let Some(n) = response.content_length() {
            ensure!(
                n <= self.max_response as u64,
                "SECOM response exceeds size limit"
            );
        }
        let mut bytes = Vec::new();
        response
            .take(self.max_response as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= self.max_response,
            "SECOM response exceeds size limit"
        );
        serde_json::from_slice(&bytes).context("Invalid SECOM response JSON")
    }
    pub fn ping(&self) -> Result<serde_json::Value> {
        self.request("v1/ping", &[])
    }
    pub fn capability(&self) -> Result<serde_json::Value> {
        self.request("v1/capability", &[])
    }
    pub fn summary(&self, page: u32, page_size: u32) -> Result<serde_json::Value> {
        ensure!(
            page > 0 && page_size > 0 && page_size <= 1000,
            "Invalid summary pagination"
        );
        self.request(
            "v1/object/summary",
            &[
                ("page", page.to_string()),
                ("pageSize", page_size.to_string()),
            ],
        )
    }
    /// A whole response page is authenticated before any payload becomes available.
    /// Pagination is returned to the caller; no implicit unbounded download loop.
    pub fn get(&self, reference: uuid::Uuid, page: u32, page_size: u32) -> Result<VerifiedPage> {
        ensure!(
            page > 0 && page_size > 0 && page_size <= 1000,
            "Invalid object pagination"
        );
        let response: GetResponse = self.request(
            "v1/object",
            &[
                ("dataReference", reference.to_string()),
                ("page", page.to_string()),
                ("pageSize", page_size.to_string()),
            ],
        )?;
        ensure!(
            response.data_response_object.len() <= page_size as usize,
            "Server exceeded requested page size"
        );
        let time = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
        let payloads = response
            .data_response_object
            .into_iter()
            .map(|r| self.trust.authenticate(r, time))
            .collect::<Result<_>>()?;
        Ok(VerifiedPage {
            payloads,
            pagination: response.pagination,
        })
    }
}
