//! SECOM wire payload authentication, independent of chart products and GUI.
//! Reference interoperability profile: GLA-RAD SECOMLib (IEC 63173-2 ED1 draft).
//! This module does not claim IEC certification or implement encrypted payloads.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ferrite_security::{verify_detached, DetachedAlgorithm};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureValue {
    pub public_root_certificate_thumbprint: Option<String>,
    pub public_certificate: String,
    pub digital_signature: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeMetadata {
    pub data_protection: bool,
    pub protection_scheme: String,
    pub digital_signature_reference: String,
    pub digital_signature_value: SignatureValue,
    pub compression_flag: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataResponse {
    pub data: String,
    pub exchange_metadata: ExchangeMetadata,
    pub ack_request: u8,
}
/// Constructible only after cryptographic authentication succeeds.
pub struct VerifiedPayload {
    bytes: Vec<u8>,
    ack_request: u8,
}
impl VerifiedPayload {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// 0 none, 1 delivered, 2 opened, 3 delivered/opened. No acknowledgement sent yet.
    pub fn ack_request(&self) -> u8 {
        self.ack_request
    }
}
pub struct PayloadTrust {
    root_pem: Vec<u8>,
    intermediates: Vec<Vec<u8>>,
    max_payload: usize,
}
impl PayloadTrust {
    pub fn new(root_pem: Vec<u8>, intermediates: Vec<Vec<u8>>, max_payload: usize) -> Result<Self> {
        ensure!(
            max_payload > 0 && max_payload <= 256 * 1024 * 1024,
            "Invalid payload size limit"
        );
        ensure!(
            root_pem.len() <= 64 * 1024
                && intermediates.len() <= 16
                && intermediates.iter().all(|v| v.len() <= 64 * 1024),
            "Certificate input exceeds limit"
        );
        Ok(Self {
            root_pem,
            intermediates,
            max_payload,
        })
    }
    pub fn authenticate(&self, response: DataResponse, time: i64) -> Result<VerifiedPayload> {
        ensure!(response.ack_request <= 3, "Unknown acknowledgement request");
        let m = response.exchange_metadata;
        ensure!(
            !m.data_protection,
            "Encrypted SECOM payload requires a configured protection provider"
        );
        ensure!(
            !m.compression_flag,
            "Compressed SECOM payload requires an explicitly configured compression provider"
        );
        let algorithm = match m.digital_signature_reference.as_str() {
            "ecdsa-256-sha2-256" => DetachedAlgorithm::Ecdsa256Sha256,
            "ecdsa-256-sha3-256" => DetachedAlgorithm::Ecdsa256Sha3,
            "ecdsa-384-sha2" => DetachedAlgorithm::Ecdsa384Sha384,
            "ecdsa-384-sha3" => DetachedAlgorithm::Ecdsa384Sha3,
            other => bail!("Unsupported SECOM signature algorithm {other}"),
        };
        let data = bounded_base64(&response.data, self.max_payload)?;
        let sig = &m.digital_signature_value;
        let certificate = bounded_base64(&sig.public_certificate, 64 * 1024)?;
        let signature = decode_hex(&sig.digital_signature)?;
        verify_detached(
            &data,
            &signature,
            &certificate,
            &self.intermediates,
            &self.root_pem,
            sig.public_root_certificate_thumbprint.as_deref(),
            algorithm,
            time,
        )?;
        Ok(VerifiedPayload {
            bytes: data,
            ack_request: response.ack_request,
        })
    }
}
fn bounded_base64(encoded: &str, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        encoded.len() <= limit.div_ceil(3) * 4,
        "Encoded payload exceeds size limit"
    );
    let bytes = STANDARD.decode(encoded).context("Invalid SECOM base64")?;
    ensure!(bytes.len() <= limit, "Decoded payload exceeds size limit");
    Ok(bytes)
}
fn decode_hex(encoded: &str) -> Result<Vec<u8>> {
    ensure!(
        !encoded.is_empty() && encoded.len() <= 512 && encoded.len() % 2 == 0,
        "Invalid signature hex length"
    );
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |c: u8| {
                (c as char)
                    .to_digit(16)
                    .context("Invalid signature hex digit")
            };
            Ok(((digit(pair[0])? << 4) | digit(pair[1])?) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests;

mod client;
pub use client::{SecomClient, VerifiedPage};

#[cfg(test)]
mod transport_tests;
