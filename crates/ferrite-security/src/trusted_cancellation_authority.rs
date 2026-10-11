//! Explicit local producer/delegate authority, separate from PKI membership.
//! Grants must come from independently administered configuration. Never derive
//! them from an incoming catalogue, certificate subject, or resource countersigner.
//! A proof records authority at the catalogue's retained verification time;
//! commit-time policy/revocation, replay history and publication remain gates.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationAuthorityRole {
    Producer,
    Delegate,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationDatasetScope {
    ExactUri(String),
    AllProducerDatasets,
}

pub struct TrustedCancellationGrant {
    product: String,
    producer: String,
    scope: CancellationDatasetScope,
    certificate_der: Vec<u8>,
    role: CancellationAuthorityRole,
    not_before: i64,
    not_after: i64,
}
fn scope_codes(product: &str, producer: &str) -> Result<()> {
    ensure!(
        !product.is_empty()
            && product.len() <= 32
            && product
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid authority product code"
    );
    ensure!(
        !producer.is_empty()
            && producer.len() <= 32
            && producer.bytes().all(|b| b.is_ascii_alphanumeric()),
        "Invalid authority producer code"
    );
    Ok(())
}
fn authority_uri(uri: &str) -> Result<()> {
    ensure!(
        uri.len() <= 4096 && uri.is_ascii() && uri.starts_with("file:/") && !uri.contains('\\'),
        "Authority requires a bounded ASCII file URI"
    );
    resource_relative_name(uri)?;
    Ok(())
}
impl TrustedCancellationGrant {
    /// Installing a grant is an administrative trust decision, not certificate
    /// authentication. Explicit AllProducerDatasets grants cover future names.
    pub fn from_certificate_der(
        product: &str,
        producer: &str,
        scope: CancellationDatasetScope,
        certificate_der: &[u8],
        role: CancellationAuthorityRole,
        not_before: i64,
        not_after: i64,
    ) -> Result<Self> {
        scope_codes(product, producer)?;
        if let CancellationDatasetScope::ExactUri(uri) = &scope {
            authority_uri(uri)?;
        }
        ensure!(not_before <= not_after, "Inverted authority window");
        ensure!(
            !certificate_der.is_empty() && certificate_der.len() <= 65536,
            "Authority certificate budget"
        );
        let cert = X509::from_der(certificate_der)?;
        ensure!(
            cert.to_der()? == certificate_der,
            "Noncanonical authority certificate DER"
        );
        Ok(Self {
            product: product.into(),
            producer: producer.into(),
            scope,
            certificate_der: certificate_der.to_vec(),
            role,
            not_before,
            not_after,
        })
    }
}
pub struct TrustedCancellationPolicy {
    grants: Vec<TrustedCancellationGrant>,
    revision: String,
    sha384: String,
}
impl TrustedCancellationPolicy {
    /// Empty policies deny all requests. No incoming metadata can add a grant.
    pub fn new(revision: &str, grants: Vec<TrustedCancellationGrant>) -> Result<Self> {
        ensure!(
            !revision.is_empty() && revision.len() <= 128 && revision.is_ascii(),
            "Invalid authority policy revision"
        );
        ensure!(grants.len() <= 256, "Authority grant count budget");
        let mut bytes = grants
            .capacity()
            .checked_mul(std::mem::size_of::<TrustedCancellationGrant>())
            .context("Authority policy capacity overflow")?;
        bytes = bytes
            .checked_add(revision.len())
            .context("Authority policy capacity overflow")?;
        ensure!(
            bytes <= 2 * 1024 * 1024,
            "Authority policy owned capacity budget"
        );
        for (i, grant) in grants.iter().enumerate() {
            for capacity in [
                grant.product.capacity(),
                grant.producer.capacity(),
                grant.certificate_der.capacity(),
                match &grant.scope {
                    CancellationDatasetScope::ExactUri(uri) => uri.capacity(),
                    CancellationDatasetScope::AllProducerDatasets => 0,
                },
            ] {
                bytes = bytes
                    .checked_add(capacity)
                    .context("Authority policy capacity overflow")?;
            }
            ensure!(
                bytes <= 2 * 1024 * 1024,
                "Authority policy owned capacity budget"
            );
            for old in &grants[..i] {
                let targets_overlap = match (&old.scope, &grant.scope) {
                    (
                        CancellationDatasetScope::ExactUri(a),
                        CancellationDatasetScope::ExactUri(b),
                    ) => a == b,
                    _ => true,
                };
                ensure!(
                    !(old.product == grant.product
                        && old.producer == grant.producer
                        && old.certificate_der == grant.certificate_der
                        && targets_overlap
                        && old.not_before <= grant.not_after
                        && grant.not_before <= old.not_after),
                    "Overlapping ambiguous authority grants"
                );
            }
        }
        // Length framing prevents concatenation ambiguities in the durable audit ID.
        let mut hash = openssl::hash::Hasher::new(MessageDigest::sha384())?;
        let mut field = |value: &[u8]| -> Result<()> {
            hash.update(&(value.len() as u64).to_be_bytes())?;
            hash.update(value)?;
            Ok(())
        };
        field(b"FerriteTrustedCancellationPolicy-v1")?;
        field(revision.as_bytes())?;
        for grant in &grants {
            field(grant.product.as_bytes())?;
            field(grant.producer.as_bytes())?;
            match &grant.scope {
                CancellationDatasetScope::ExactUri(uri) => {
                    field(b"exact")?;
                    field(uri.as_bytes())?;
                }
                CancellationDatasetScope::AllProducerDatasets => {
                    field(b"producer-all")?;
                }
            }
            field(&grant.certificate_der)?;
            field(match grant.role {
                CancellationAuthorityRole::Producer => b"producer",
                CancellationAuthorityRole::Delegate => b"delegate",
            })?;
            field(&grant.not_before.to_be_bytes())?;
            field(&grant.not_after.to_be_bytes())?;
        }
        let sha384 = hex(&hash.finish()?);
        Ok(Self {
            grants,
            revision: revision.into(),
            sha384,
        })
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn sha384(&self) -> &str {
        &self.sha384
    }
    /// Only a verified signature directly over these catalogue bytes can supply
    /// authority. Signature-on-signature and copied original signatures cannot.
    pub fn authorize_catalogue<'p>(
        &'p self,
        catalogue: &AuthenticatedExchangeCatalogue,
        product: &str,
        producer: &str,
        resource_uri: &str,
    ) -> Result<CatalogueCancellationAuthority<'p>> {
        scope_codes(product, producer)?;
        authority_uri(resource_uri)?;
        let time = catalogue.verified_unix_seconds();
        for signature in catalogue
            .signatures()
            .iter()
            .filter(|s| s.signature_target().is_none())
        {
            for grant in &self.grants {
                let resource_matches = match &grant.scope {
                    CancellationDatasetScope::ExactUri(uri) => uri == resource_uri,
                    CancellationDatasetScope::AllProducerDatasets => true,
                };
                if grant.product == product
                    && grant.producer == producer
                    && resource_matches
                    && grant.not_before <= time
                    && time <= grant.not_after
                    && grant.certificate_der == signature.signer_certificate_der()
                {
                    return Ok(CatalogueCancellationAuthority {
                        policy: self,
                        grant,
                        catalogue_sha384: catalogue.catalogue_sha384().into(),
                        resource_uri: resource_uri.into(),
                        signer_certificate_sha256: signature.signer_certificate_sha256().into(),
                        signature_id: signature.id().into(),
                        verified_unix_seconds: time,
                    });
                }
            }
        }
        bail!("No explicitly authorized direct catalogue signer for this cancellation scope")
    }
}
/// Immutable policy borrow pins the exact grant set. This evidence is neither a
/// replay reservation nor permission to remove a published dataset.
pub struct CatalogueCancellationAuthority<'p> {
    policy: &'p TrustedCancellationPolicy,
    grant: &'p TrustedCancellationGrant,
    catalogue_sha384: String,
    resource_uri: String,
    signer_certificate_sha256: String,
    signature_id: String,
    verified_unix_seconds: i64,
}
impl CatalogueCancellationAuthority<'_> {
    pub fn policy(&self) -> &TrustedCancellationPolicy {
        self.policy
    }
    pub fn role(&self) -> CancellationAuthorityRole {
        self.grant.role
    }
    pub fn product(&self) -> &str {
        &self.grant.product
    }
    pub fn producer_code(&self) -> &str {
        &self.grant.producer
    }
    pub fn catalogue_sha384(&self) -> &str {
        &self.catalogue_sha384
    }
    pub fn resource_uri(&self) -> &str {
        &self.resource_uri
    }
    pub fn signer_certificate_sha256(&self) -> &str {
        &self.signer_certificate_sha256
    }
    pub fn signature_id(&self) -> &str {
        &self.signature_id
    }
    pub fn verified_unix_seconds(&self) -> i64 {
        self.verified_unix_seconds
    }
    /// Recheck only the installed local grant window at a caller-trusted time.
    /// The App must sample its OS clock after fallible scene preparation and
    /// reauthorize against its CURRENT policy first. This is not revocation,
    /// replay admission, or permission to publish a removal.
    pub fn validate_local_window_at(&self, trusted_now: i64) -> Result<()> {
        ensure!(
            trusted_now >= self.verified_unix_seconds,
            "Trusted cancellation clock precedes catalogue verification"
        );
        ensure!(
            self.grant.not_before <= trusted_now && trusted_now <= self.grant.not_after,
            "Local cancellation grant is not valid at publication time"
        );
        Ok(())
    }
}
