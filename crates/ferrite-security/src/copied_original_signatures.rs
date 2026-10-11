//! Equality of copied declarations with retained cryptographic evidence.
//! This proof supplies neither incoming-resource absence nor producer authority,
//! lifecycle admission, replay protection, replacement staging, or deletion.
use super::*;
use std::collections::BTreeMap;

/// Owns an authenticated copy of the original resource and borrows the exact
/// authenticated incoming catalogue. There is no public constructor/Deserialize.
pub struct CopiedOriginalSignatureBinding<'a> {
    original: OriginalDatasetAuthentication,
    incoming: &'a AuthenticatedExchangeCatalogue,
    snapshot: std::sync::Arc<AuthenticatedSnapshot>,
}
impl std::fmt::Debug for CopiedOriginalSignatureBinding<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopiedOriginalSignatureBinding")
            .field("resource_uri", &self.original.resource_uri())
            .field("resource_sha384", &self.original.resource_sha384())
            .field(
                "incoming_catalogue_sha384",
                &self.incoming.catalogue_sha384(),
            )
            .finish_non_exhaustive()
    }
}
impl CopiedOriginalSignatureBinding<'_> {
    pub fn original(&self) -> &OriginalDatasetAuthentication {
        &self.original
    }
    pub fn incoming_catalogue(&self) -> &AuthenticatedExchangeCatalogue {
        self.incoming
    }
    pub fn resource_snapshot(&self) -> &AuthenticatedSnapshot {
        &self.snapshot
    }
}

struct Declaration {
    signature: Signature,
    namespace: String,
    kind: String,
    attributes: Vec<(Option<String>, String, String)>,
}
fn declarations(entry: Node<'_, '_>) -> Result<BTreeMap<String, Declaration>> {
    let mut out = BTreeMap::new();
    let containers = entry
        .children()
        .filter(|n| is(*n, XC, "digitalSignatureValue"));
    for container in containers {
        let nodes: Vec<_> = container.children().filter(Node::is_element).collect();
        ensure!(
            nodes.len() == 1,
            "Copied signature container must have one declaration"
        );
        let node = nodes[0];
        ensure!(
            is(node, SE, "S100_SE_SignatureOnData")
                || is(node, SE, "S100_SE_SignatureOnSignature")
                || is(node, SE, "S100_SE_DigitalSignature"),
            "Unsupported copied signature type"
        );
        ensure!(
            !node.children().any(|n| n.is_element()),
            "Copied signature must be scalar"
        );
        let signature = parse_signature(node)?;
        ensure!(
            signature.bytes.len() <= 104,
            "Copied signature DER length exceeds P-384 bound"
        );
        ensure!(
            openssl::ecdsa::EcdsaSig::from_der(&signature.bytes)?.to_der()? == signature.bytes,
            "Noncanonical copied signature DER"
        );
        let mut attributes = node
            .attributes()
            .map(|a| {
                (
                    a.namespace().map(str::to_owned),
                    a.name().to_owned(),
                    a.value().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        attributes.sort();
        let id = signature.id.clone();
        ensure!(
            out.insert(
                id,
                Declaration {
                    signature,
                    namespace: node.tag_name().namespace().unwrap_or("").to_owned(),
                    kind: node.tag_name().name().to_owned(),
                    attributes,
                }
            )
            .is_none(),
            "Duplicate copied signature ID"
        );
    }
    ensure!(!out.is_empty(), "Missing copied original signatures");
    Ok(out)
}

impl OriginalDatasetAuthentication {
    /// Bind declarations in the incoming catalogue to this specific retained
    /// original. Algorithm spelling, expanded signature type/attributes, DER,
    /// certificate reference, canonical certificate bytes and parent graph must
    /// match. Declaration order and base64 whitespace do not affect equality.
    /// No filesystem lookup is used to identify the incoming resource. Capturing
    /// the original then rehashes its bytes into an owned snapshot; mutation or
    /// absence of the original at capture time rejects the binding.
    pub fn bind_copied_signatures<'a>(
        &self,
        incoming: &'a AuthenticatedExchangeCatalogue,
    ) -> Result<CopiedOriginalSignatureBinding<'a>> {
        self.validate_copied_signature_declarations(incoming)?;
        let snapshot = std::sync::Arc::new(self.capture_resource()?);
        Ok(CopiedOriginalSignatureBinding {
            original: self.clone(),
            incoming,
            snapshot,
        })
    }

    /// Bind an existing parser-owned authenticated snapshot without another
    /// payload copy or any lookup of the later live resource. The exact snapshot
    /// bytes are rehashed with constant memory. Its public source label is not
    /// used as authentication, a lookup path, or an authority identifier.
    /// Keeping an Arc pins ownership; this does not provide hostile-filesystem
    /// immutability after admission, producer authority, or removal permission.
    pub fn bind_copied_signatures_from_snapshot<'a>(
        &self,
        incoming: &'a AuthenticatedExchangeCatalogue,
        snapshot: std::sync::Arc<AuthenticatedSnapshot>,
    ) -> Result<CopiedOriginalSignatureBinding<'a>> {
        self.validate_copied_signature_declarations(incoming)?;
        // Unlike opening path()/source, tempfile::reopen checks file identity
        // and gives an independent handle without disturbing parser cursors.
        let mut input = snapshot.file.reopen()?;
        let mut digest = openssl::hash::Hasher::new(MessageDigest::sha384())?;
        let mut buffer = [0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size = size
                .checked_add(n as u64)
                .context("Snapshot binding size overflow")?;
            ensure!(
                size <= self.resource_size(),
                "Retained snapshot size differs from original"
            );
            digest.update(&buffer[..n])?;
        }
        ensure!(
            size == self.resource_size() && hex(&digest.finish()?) == self.resource_sha384(),
            "Retained snapshot bytes differ from original authentication"
        );
        Ok(CopiedOriginalSignatureBinding {
            original: self.clone(),
            incoming,
            snapshot,
        })
    }

    fn validate_copied_signature_declarations(
        &self,
        incoming: &AuthenticatedExchangeCatalogue,
    ) -> Result<()> {
        let view = incoming.discovery_view()?;
        let mut matches = view.dataset_entries().filter(|n| {
            unique_child(*n, XC, "fileName")
                .and_then(text)
                .is_ok_and(|uri| uri == self.resource_uri())
        });
        let entry = matches
            .next()
            .context("No exact incoming original resource URI")?;
        ensure!(
            matches.next().is_none(),
            "Ambiguous incoming original resource URI"
        );
        let original_view = self.original_entry_view()?;
        let original_entry = original_view.entry();
        ensure!(
            text(unique_child(entry, XC, "digitalSignatureReference")?)?
                == text(unique_child(
                    original_entry,
                    XC,
                    "digitalSignatureReference"
                )?)?,
            "Copied signature algorithm declaration differs from original"
        );
        let old = declarations(original_entry)?;
        let copied = declarations(entry)?;
        ensure!(
            old.len() == self.signatures().len() && copied.len() == old.len(),
            "Copied original signature set differs"
        );
        let certificates = unique_child(entry.document().root_element(), XC, "certificates")?;
        // Existing certificate parser rejects duplicate IDs and unknown types.
        let parsed_certificates = Certificates::parse(entry.document().root_element())?;
        for verified in self.signatures() {
            let expected = old
                .get(verified.id())
                .context("Retained original signature ID mismatch")?;
            let actual = copied
                .get(verified.id())
                .context("Missing retained original signature ID")?;
            ensure!(
                expected.signature.bytes == verified.der()
                    && expected.signature.certificate == verified.certificate_id()
                    && expected.signature.target.as_deref() == verified.signature_target(),
                "Retained original declaration differs from cryptographic proof"
            );
            ensure!(
                actual.signature.bytes == verified.der()
                    && actual.signature.certificate == verified.certificate_id()
                    && actual.signature.target.as_deref() == verified.signature_target()
                    && actual.namespace == expected.namespace
                    && actual.kind == expected.kind
                    && actual.attributes == expected.attributes,
                "Copied signature value, certificate reference, graph or attributes differ"
            );
            let mut cert_nodes = certificates.children().filter(|n| {
                is(*n, SE, "certificate") && n.attribute("id") == Some(verified.certificate_id())
            });
            let cert_node = cert_nodes
                .next()
                .context("Missing copied signer certificate")?;
            ensure!(
                cert_nodes.next().is_none(),
                "Ambiguous copied signer certificate"
            );
            let bytes = decode(cert_node)?;
            let canonical = parsed_certificates
                .members
                .get(verified.certificate_id())
                .context("Missing parsed copied signer")?
                .cert
                .to_der()?;
            ensure!(
                bytes == canonical
                    && bytes == verified.signer_certificate_der()
                    && hex(&hash(MessageDigest::sha256(), &bytes)?)
                        == verified.signer_certificate_sha256(),
                "Copied signer certificate differs from retained canonical certificate"
            );
        }
        Ok(())
    }
}
