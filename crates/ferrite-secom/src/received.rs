//! Exact authenticated payload bytes and independent TLS routing provenance.
//! A GET request UUID is not an authenticated Upload transaction identifier.
use crate::VerifiedPayload;

/// These fields describe the local GET request over configured mutual TLS.
/// The data signature does not cover any of these routing fields.
pub struct GetRoutingReceipt {
    pub(crate) provider: String,
    pub(crate) requested_reference: uuid::Uuid,
    pub(crate) page: u32,
    pub(crate) page_size: u32,
    pub(crate) response_index: usize,
    pub(crate) received_at: i64,
    pub(crate) server_ca_sha256: [u8; 32],
    pub(crate) client_certificate_sha256: [u8; 32],
}
impl GetRoutingReceipt {
    pub fn provider(&self) -> &str {
        &self.provider
    }
    pub fn requested_reference(&self) -> uuid::Uuid {
        self.requested_reference
    }
    pub fn page(&self) -> u32 {
        self.page
    }
    pub fn page_size(&self) -> u32 {
        self.page_size
    }
    pub fn response_index(&self) -> usize {
        self.response_index
    }
    pub fn received_at(&self) -> i64 {
        self.received_at
    }
    pub fn server_ca_sha256(&self) -> &[u8; 32] {
        &self.server_ca_sha256
    }
    pub fn client_certificate_sha256(&self) -> &[u8; 32] {
        &self.client_certificate_sha256
    }
}

/// Product adapters inspect these exact bytes and independently validate their
/// catalogue/resource identity. This type neither extracts an assumed archive
/// nor interprets a subscription operation as a dataset cancellation.
pub struct ReceivedPayload {
    payload: VerifiedPayload,
    routing: GetRoutingReceipt,
    sha256: [u8; 32],
}
impl ReceivedPayload {
    pub(crate) fn from_get(payload: VerifiedPayload, routing: GetRoutingReceipt) -> Self {
        let sha256 = openssl::sha::sha256(payload.bytes());
        Self {
            payload,
            routing,
            sha256,
        }
    }
    pub fn bytes(&self) -> &[u8] {
        self.payload.bytes()
    }
    pub fn byte_length(&self) -> usize {
        self.payload.bytes().len()
    }
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
    pub fn routing(&self) -> &GetRoutingReceipt {
        &self.routing
    }
    /// TLS response metadata only; no delivery/opened acknowledgement is sent.
    pub fn requested_acknowledgement(&self) -> u8 {
        self.payload.ack_request()
    }
    /// The GET reference must not be repurposed as an Upload/ACK transaction ID.
    pub fn acknowledgement_transaction(&self) -> Option<uuid::Uuid> {
        None
    }
}

pub struct ReceivedPage {
    pub payloads: Vec<ReceivedPayload>,
    /// TLS response metadata, not covered by the individual data signatures.
    pub pagination: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn received(reference: uuid::Uuid, index: usize) -> ReceivedPayload {
        ReceivedPayload::from_get(
            VerifiedPayload {
                bytes: b"same signed chart bytes\0\xff".to_vec(),
                ack_request: 3,
            },
            GetRoutingReceipt {
                provider: "https://example.invalid/service/".into(),
                requested_reference: reference,
                page: 2,
                page_size: 4,
                response_index: index,
                received_at: 100,
                server_ca_sha256: [1; 32],
                client_certificate_sha256: [2; 32],
            },
        )
    }
    #[test]
    fn routing_identity_does_not_change_signed_bytes_or_create_ack_transaction() {
        let a = received(uuid::Uuid::from_u128(1), 0);
        let b = received(uuid::Uuid::from_u128(2), 1);
        assert_eq!(a.bytes(), b.bytes());
        assert_eq!(a.sha256(), b.sha256());
        assert_ne!(
            a.routing().requested_reference(),
            b.routing().requested_reference()
        );
        assert_eq!(a.byte_length(), a.bytes().len());
        assert_eq!(*a.sha256(), openssl::sha::sha256(a.bytes()));
        assert_eq!(a.requested_acknowledgement(), 3);
        assert!(a.acknowledgement_transaction().is_none());
        assert!(b.acknowledgement_transaction().is_none());
    }
}
