//! Immutable cryptographic evidence, independent of product lifecycle policy.
//! This is not permission to cancel a dataset. A product adapter must still
//! validate its complete signed discovery metadata and removal authority.
use super::*;
use std::sync::Arc;

/// Canonical DER and signer identity captured only after successful verification.
/// XML IDs are trace labels; they are never substitutes for signature values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSignatureDescriptor {
    pub(super) id: String,
    pub(super) certificate_id: String,
    pub(super) der: Vec<u8>,
    pub(super) signer_certificate_sha256: String,
    pub(super) signer_certificate_der: Arc<[u8]>,
    pub(super) signature_target: Option<String>,
}
impl VerifiedSignatureDescriptor {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn certificate_id(&self) -> &str {
        &self.certificate_id
    }
    pub fn der(&self) -> &[u8] {
        &self.der
    }
    pub fn signer_certificate_sha256(&self) -> &str {
        &self.signer_certificate_sha256
    }
    /// Canonical certificate bytes validated at the retained import time.
    /// This does not establish revocation status or present removal authority.
    pub fn signer_certificate_der(&self) -> &[u8] {
        &self.signer_certificate_der
    }
    /// None means a signature over the resource; Some names a signature parent.
    pub fn signature_target(&self) -> Option<&str> {
        self.signature_target.as_deref()
    }
    /// This verifier admits only NIST P-384 ECDSA over SHA-384.
    /// The original algorithm spelling and dataStatus remain in catalogue bytes.
    pub fn algorithm(&self) -> &str {
        "ECDSA-384-SHA2"
    }
}

#[derive(Debug)]
pub(super) struct ResourceAuthentication {
    pub(super) path: PathBuf,
    pub(super) sha384: String,
    pub(super) size: u64,
    pub(super) signatures: Vec<VerifiedSignatureDescriptor>,
}

/// A retained original verification result. Private fields and no Deserialize
/// prevent constructing authority from a user supplied report or matching IDs.
/// Catalogue storage is shared once per exchange, bounded by MAX_XML, and is
/// the exact SHA-384 authenticated byte sequence consumed by the verifier.
#[derive(Clone)]
pub struct OriginalDatasetAuthentication {
    pub(super) resource: Arc<ResourceAuthentication>,
    pub(super) catalogue: Arc<ResourceAuthentication>,
    pub(super) catalogue_bytes: Arc<[u8]>,
    pub(super) verified_unix_seconds: i64,
    pub(super) trust_anchor_sha256: Arc<HashMap<String, String>>,
    pub(super) resource_uri: String,
    pub(super) discovery_range: std::ops::Range<usize>,
}
impl std::fmt::Debug for OriginalDatasetAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OriginalDatasetAuthentication")
            .field("resource_uri", &self.resource_uri)
            .field("resource_sha384", &self.resource.sha384)
            .field("catalogue_sha384", &self.catalogue.sha384)
            .field("verified_unix_seconds", &self.verified_unix_seconds)
            .finish_non_exhaustive()
    }
}
impl OriginalDatasetAuthentication {
    /// Raw signed logical URI, independent of later live filesystem state.
    pub fn resource_uri(&self) -> &str {
        &self.resource_uri
    }
    /// Exact discovery entry bytes within the retained authenticated catalogue.
    /// XML namespace declarations may be inherited: parse the full catalogue
    /// and use this byte range to identify the entry, not standalone fragment XML.
    pub fn discovery_range(&self) -> std::ops::Range<usize> {
        self.discovery_range.clone()
    }
    pub fn discovery_bytes(&self) -> &[u8] {
        &self.catalogue_bytes[self.discovery_range.clone()]
    }

    pub fn original_entry_view(&self) -> Result<OriginalEntryView<'_>> {
        let view = CatalogueDiscoveryView::parse(&self.catalogue_bytes)?;
        let id = view.dataset_id_at(&self.discovery_range, &self.resource_uri)?;
        Ok(OriginalEntryView { view, id })
    }

    pub fn resource_path(&self) -> &Path {
        &self.resource.path
    }
    pub fn resource_sha384(&self) -> &str {
        &self.resource.sha384
    }
    pub fn resource_size(&self) -> u64 {
        self.resource.size
    }
    pub fn signatures(&self) -> &[VerifiedSignatureDescriptor] {
        &self.resource.signatures
    }
    pub fn catalogue_bytes(&self) -> &[u8] {
        &self.catalogue_bytes
    }
    pub fn catalogue_sha384(&self) -> &str {
        &self.catalogue.sha384
    }
    pub fn catalogue_signatures(&self) -> &[VerifiedSignatureDescriptor] {
        &self.catalogue.signatures
    }
    pub fn verified_unix_seconds(&self) -> i64 {
        self.verified_unix_seconds
    }
    pub fn trust_anchor_sha256(&self) -> &HashMap<String, String> {
        &self.trust_anchor_sha256
    }
    /// Capture original resource bytes now, rejecting mutation since verification.
    /// Existing parser-owned snapshots must be retained by their product owner;
    /// this proof does not invent an unavailable resource or new verification time.
    pub fn capture_resource(&self) -> Result<AuthenticatedSnapshot> {
        self.resource.snapshot()
    }
}
