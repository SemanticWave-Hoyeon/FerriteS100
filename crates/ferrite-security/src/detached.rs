//! Transport-independent authentication for externally delivered signed payloads.
use super::*;

#[derive(Clone, Copy)]
pub enum DetachedAlgorithm {
    Ecdsa256Sha256,
    Ecdsa256Sha3,
    Ecdsa384Sha384,
    Ecdsa384Sha3,
}

/// Roots must be supplied from independent configuration, never from message metadata.
/// The caller selects the PKI for this transport; S-100 Part 15 roots are not implicit.
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
    let roots = X509::stack_from_pem(trusted_root_pem)?;
    ensure!(
        roots.len() == 1,
        "Exactly one independently installed root required"
    );
    let root = &roots[0];
    let root_key = root.public_key()?;
    ensure!(
        root.verify(&root_key)? && root.subject_name().to_der()? == root.issuer_name().to_der()?,
        "Root is not self-signed"
    );
    if let Some(expected) = root_sha256 {
        ensure!(
            expected.len() == 64 && expected.bytes().all(|c| c.is_ascii_hexdigit()),
            "Invalid root thumbprint"
        );
        ensure!(
            hex(&root.digest(MessageDigest::sha256())?).eq_ignore_ascii_case(expected),
            "Root thumbprint does not match installed trust anchor"
        );
    }
    ensure!(
        intermediate_der.len() <= 16,
        "Certificate chain exceeds limit"
    );
    let leaf = X509::from_der(leaf_der)?;
    ensure!(
        leaf.to_der()? == leaf_der,
        "Trailing or noncanonical certificate DER"
    );
    let mut chain = Stack::new()?;
    for der in intermediate_der {
        let cert = X509::from_der(der)?;
        ensure!(
            cert.to_der()? == *der,
            "Trailing or noncanonical intermediate DER"
        );
        chain.push(cert)?;
    }
    let mut store = X509StoreBuilder::new()?;
    store.add_cert(root.clone())?;
    let mut param = X509VerifyParam::new()?;
    param.set_time(time);
    param.set_flags(X509VerifyFlags::CHECK_SS_SIGNATURE)?;
    store.set_param(&param)?;
    let mut context = X509StoreContext::new()?;
    let (valid, error) = context.init(&store.build(), &leaf, &chain, |ctx| {
        Ok((ctx.verify_cert()?, ctx.error().to_string()))
    })?;
    ensure!(
        valid,
        "Detached certificate path validation failed: {error}"
    );
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
    let (curve, digest) = match algorithm {
        DetachedAlgorithm::Ecdsa256Sha256 => (Nid::X9_62_PRIME256V1, MessageDigest::sha256()),
        DetachedAlgorithm::Ecdsa256Sha3 => (Nid::X9_62_PRIME256V1, MessageDigest::sha3_256()),
        DetachedAlgorithm::Ecdsa384Sha384 => (Nid::SECP384R1, MessageDigest::sha384()),
        DetachedAlgorithm::Ecdsa384Sha3 => (Nid::SECP384R1, MessageDigest::sha3_384()),
    };
    let key = leaf.public_key()?;
    ensure!(
        key.ec_key()?.group().curve_name() == Some(curve),
        "Signature curve does not match algorithm"
    );
    let parsed_signature = openssl::ecdsa::EcdsaSig::from_der(signature)?;
    ensure!(
        parsed_signature.to_der()? == signature,
        "Trailing or noncanonical signature DER"
    );
    let mut verifier = Verifier::new(digest, &key)?;
    verifier.update(data)?;
    ensure!(
        verifier.verify(signature)?,
        "Invalid detached payload signature"
    );
    Ok(())
}
