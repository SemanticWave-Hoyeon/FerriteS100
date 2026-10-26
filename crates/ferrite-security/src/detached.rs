//! Transport-independent authentication for externally delivered signed payloads.
use super::*;
use crypto::MessageHash;

#[derive(Clone, Copy)]
pub enum DetachedAlgorithm {
    Ecdsa256Sha256,
    Ecdsa256Sha3,
    Ecdsa384Sha384,
    Ecdsa384Sha3,
}

/// Roots must be supplied from independent configuration, never from message metadata.
/// The caller selects the PKI for this transport; S-100 Part 15 roots are not implicit.
#[expect(
    clippy::too_many_arguments,
    reason = "Each transport authentication input is explicit; preserve the public verification API."
)]
pub fn verify_detached(
    data: &[u8],
    signature: &[u8],
    leaf_der: &[u8],
    intermediate_der: &[Vec<u8>],
    trusted_root_pem: &[u8],
    root_sha256: Option<&str>,
    algorithm: DetachedAlgorithm,
    time: i64,
) -> Result<()> {
    verify_detached_with(
        CryptoBackend::default(),
        data,
        signature,
        leaf_der,
        intermediate_der,
        trusted_root_pem,
        root_sha256,
        algorithm,
        time,
    )
}

#[cfg(feature = "crypto-rust")]
/// The issuer path from `leaf` through the supplied intermediates (any
/// order) to `root`, as OpenSSL's chain building finds it: the trusted root
/// first, otherwise the one intermediate named as issuer. More than one
/// candidate is ambiguous and rejected (the Rust backend fails closed;
/// OpenSSL's choice would depend on its search order).
fn issuer_path(leaf: &Cert, intermediates: &[Cert], root: &Cert) -> Result<Vec<Cert>> {
    let mut path = Vec::new();
    let mut current = leaf;
    loop {
        if current.issuer_der() == root.subject_der() {
            return Ok(path);
        }
        let mut candidates = intermediates
            .iter()
            .filter(|c| c.subject_der() == current.issuer_der());
        let next = candidates.next().context(
            "Detached certificate path validation failed: unable to get local issuer certificate",
        )?;
        ensure!(
            candidates.next().is_none(),
            "Detached certificate path validation failed: ambiguous intermediate certificates"
        );
        ensure!(
            path.len() < intermediates.len(),
            "Detached certificate path validation failed: issuer cycle"
        );
        path.push(next.clone());
        current = next;
    }
}

/// [`verify_detached`] with an explicit [`CryptoBackend`].
#[expect(
    clippy::too_many_arguments,
    reason = "Each transport authentication input is explicit; preserve the public verification API."
)]
pub fn verify_detached_with(
    backend: CryptoBackend,
    data: &[u8],
    signature: &[u8],
    leaf_der: &[u8],
    intermediate_der: &[Vec<u8>],
    trusted_root_pem: &[u8],
    root_sha256: Option<&str>,
    algorithm: DetachedAlgorithm,
    time: i64,
) -> Result<()> {
    let roots = Cert::stack_from_pem(backend, trusted_root_pem)?;
    ensure!(
        roots.len() == 1,
        "Exactly one independently installed root required"
    );
    let root = &roots[0];
    ensure!(
        root.verifies_own_signature()? && root.subject_der() == root.issuer_der(),
        "Root is not self-signed"
    );
    if let Some(expected) = root_sha256 {
        ensure!(
            expected.len() == 64 && expected.bytes().all(|c| c.is_ascii_hexdigit()),
            "Invalid root thumbprint"
        );
        ensure!(
            root.sha256_hex()?.eq_ignore_ascii_case(expected),
            "Root thumbprint does not match installed trust anchor"
        );
    }
    ensure!(
        intermediate_der.len() <= 16,
        "Certificate chain exceeds limit"
    );
    let leaf = Cert::from_der(backend, leaf_der)?;
    ensure!(
        leaf.der() == leaf_der,
        "Trailing or noncanonical certificate DER"
    );
    let mut chain = Vec::new();
    for der in intermediate_der {
        let cert = Cert::from_der(backend, der)?;
        ensure!(
            cert.der() == der.as_slice(),
            "Trailing or noncanonical intermediate DER"
        );
        chain.push(cert);
    }
    let verdict = match backend {
        // OpenSSL builds the path from the untrusted set itself.
        #[cfg(feature = "crypto-openssl")]
        CryptoBackend::OpenSsl => crypto::verify_path(&leaf, &chain, root, time),
        #[cfg(feature = "crypto-rust")]
        CryptoBackend::Rust => {
            crypto::verify_path(&leaf, &issuer_path(&leaf, &chain, root)?, root, time)
        }
    };
    if let Err(error) = verdict {
        bail!("Detached certificate path validation failed: {error}");
    }
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf_der)
        .map_err(|e| anyhow::anyhow!("Invalid certificate: {e}"))?;
    ensure!(
        !parsed.basic_constraints()?.is_some_and(|b| b.value.ca),
        "CA cannot sign payloads"
    );
    if let Some(usage) = parsed.key_usage()? {
        ensure!(
            usage.value.digital_signature(),
            "Certificate does not permit digital signatures"
        );
    }
    let (curve, hash) = match algorithm {
        DetachedAlgorithm::Ecdsa256Sha256 => (Curve::P256, MessageHash::Sha256),
        DetachedAlgorithm::Ecdsa256Sha3 => (Curve::P256, MessageHash::Sha3_256),
        DetachedAlgorithm::Ecdsa384Sha384 => (Curve::P384, MessageHash::Sha384),
        DetachedAlgorithm::Ecdsa384Sha3 => (Curve::P384, MessageHash::Sha3_384),
    };
    ensure!(
        leaf.ec_curve()? == curve,
        "Signature curve does not match algorithm"
    );
    ensure!(
        crypto::canonical_ecdsa_der(backend, signature)?,
        "Trailing or noncanonical signature DER"
    );
    ensure!(
        leaf.verify_ecdsa_message(hash, data, signature)?,
        "Invalid detached payload signature"
    );
    Ok(())
}
