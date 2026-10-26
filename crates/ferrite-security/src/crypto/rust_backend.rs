//! Pure-Rust backend: `x509-cert`/`der`/`spki` parsing, `p256`/`p384` ECDSA,
//! `sha2`/`sha3` digests.
//!
//! [`verify_path`] reproduces the checks `X509_verify_cert` performs for the
//! way this crate calls it (one self-signed trust anchor, explicit issuer
//! path, fixed check time, `X509_V_FLAG_CHECK_SS_SIGNATURE`, no purpose,
//! policy, CRL, host or strict flags):
//!
//! * chain linkage: issuer name, authority key identifier (key id, serial,
//!   directory name) and the issuer's keyUsage keyCertSign;
//! * every certificate (anchor included): signature under its issuer's key
//!   with matching inner/outer signature algorithm, and
//!   `notBefore <= time < notAfter` (OpenSSL treats `time == notAfter` as
//!   expired);
//! * unhandled critical extensions; CA status (basicConstraints/keyUsage,
//!   the V1 self-signed anchor rule); path length constraints; explicit EC
//!   parameters.
//!
//! Fail-closed cases (the Rust backend rejects; OpenSSL may accept):
//! certificates that are not exact DER (BER, trailing bytes, unsorted SET OF,
//! times before 1970 or with fractions), issuer keys other than ECDSA P-256
//! or P-384, signature algorithms other than ecdsa-with-SHA224/256/384/512
//! (with absent parameters), duplicate extensions, malformed basicConstraints
//! / keyUsage / key identifier extensions on any certificate, pathLen without
//! CA, name constraints on a CA, RFC 3779 and proxy extensions, an AKID
//! directory name that differs from the issuer's issuer name in raw DER, a
//! CA without basicConstraints and keyUsage whose only CA marker would be a
//! Netscape cert type, and paths in which two certificates have names that
//! could compare equal under OpenSSL's case/whitespace-insensitive name
//! canonicalisation (OpenSSL could build a different path from those).
use super::{strict_der, Cert, Curve, Inner, MessageHash};
use anyhow::{bail, ensure, Context, Result};
use der::{
    asn1::{BitString, ObjectIdentifier as Oid},
    Decode, Encode,
};
use std::sync::Arc;
use x509_cert::{
    certificate::Version,
    ext::pkix::{AuthorityKeyIdentifier, BasicConstraints, SubjectKeyIdentifier},
    name::Name,
    Certificate,
};

const EC_PUBLIC_KEY: Oid = Oid::new_unwrap("1.2.840.10045.2.1");
const SECP256R1: Oid = Oid::new_unwrap("1.2.840.10045.3.1.7");
const SECP384R1: Oid = Oid::new_unwrap("1.3.132.0.34");
const ECDSA_SHA224: Oid = Oid::new_unwrap("1.2.840.10045.4.3.1");
const ECDSA_SHA256: Oid = Oid::new_unwrap("1.2.840.10045.4.3.2");
const ECDSA_SHA384: Oid = Oid::new_unwrap("1.2.840.10045.4.3.3");
const ECDSA_SHA512: Oid = Oid::new_unwrap("1.2.840.10045.4.3.4");
const COMMON_NAME: Oid = Oid::new_unwrap("2.5.4.3");

const SUBJECT_KEY_ID: Oid = Oid::new_unwrap("2.5.29.14");
const KEY_USAGE: Oid = Oid::new_unwrap("2.5.29.15");
const SUBJECT_ALT_NAME: Oid = Oid::new_unwrap("2.5.29.17");
const BASIC_CONSTRAINTS: Oid = Oid::new_unwrap("2.5.29.19");
const NAME_CONSTRAINTS: Oid = Oid::new_unwrap("2.5.29.30");
const CRL_DISTRIBUTION_POINTS: Oid = Oid::new_unwrap("2.5.29.31");
const CERTIFICATE_POLICIES: Oid = Oid::new_unwrap("2.5.29.32");
const POLICY_MAPPINGS: Oid = Oid::new_unwrap("2.5.29.33");
const AUTHORITY_KEY_ID: Oid = Oid::new_unwrap("2.5.29.35");
const POLICY_CONSTRAINTS: Oid = Oid::new_unwrap("2.5.29.36");
const EXT_KEY_USAGE: Oid = Oid::new_unwrap("2.5.29.37");
const INHIBIT_ANY_POLICY: Oid = Oid::new_unwrap("2.5.29.54");
const NETSCAPE_CERT_TYPE: Oid = Oid::new_unwrap("2.16.840.1.113730.1.1");
const IP_ADDR_BLOCKS: Oid = Oid::new_unwrap("1.3.6.1.5.5.7.1.7");
const AS_IDENTIFIERS: Oid = Oid::new_unwrap("1.3.6.1.5.5.7.1.8");
const PROXY_CERT_INFO: Oid = Oid::new_unwrap("1.3.6.1.5.5.7.1.14");
const OCSP_NO_CHECK: Oid = Oid::new_unwrap("1.3.6.1.5.5.7.48.1.5");

/// Critical extensions OpenSSL handles (`X509_supported_extension`).
const SUPPORTED_CRITICAL: [Oid; 15] = [
    NETSCAPE_CERT_TYPE,
    KEY_USAGE,
    SUBJECT_ALT_NAME,
    BASIC_CONSTRAINTS,
    CERTIFICATE_POLICIES,
    CRL_DISTRIBUTION_POINTS,
    EXT_KEY_USAGE,
    IP_ADDR_BLOCKS,
    AS_IDENTIFIERS,
    OCSP_NO_CHECK,
    POLICY_CONSTRAINTS,
    PROXY_CERT_INFO,
    NAME_CONSTRAINTS,
    POLICY_MAPPINGS,
    INHIBIT_ANY_POLICY,
];

const KU_KEY_CERT_SIGN: usize = 5;

/// Extensions as the path validation reads them.
#[derive(Default)]
struct Extensions {
    /// First reason the certificate cannot take part in any path.
    invalid: Option<&'static str>,
    critical_unhandled: bool,
    basic: Option<BasicConstraints>,
    /// keyUsage bits (bit 0 = digitalSignature).
    key_usage: Option<Vec<bool>>,
    subject_key_id: Option<Vec<u8>>,
    authority_key_id: Option<AuthorityKeyIdentifier>,
    name_constraints: bool,
}

impl Extensions {
    fn parse(cert: &Certificate) -> Self {
        let mut out = Self::default();
        let mut seen: Vec<Oid> = Vec::new();
        let invalid = |out: &mut Self, why: &'static str| {
            out.invalid.get_or_insert(why);
        };
        for ext in cert.tbs_certificate.extensions.iter().flatten() {
            if seen.contains(&ext.extn_id) {
                invalid(&mut out, "duplicate certificate extension");
                continue;
            }
            seen.push(ext.extn_id);
            if ext.critical && !SUPPORTED_CRITICAL.contains(&ext.extn_id) {
                out.critical_unhandled = true;
            }
            let value = ext.extn_value.as_bytes();
            match ext.extn_id {
                BASIC_CONSTRAINTS => match BasicConstraints::from_der(value) {
                    Ok(b) if b.path_len_constraint.is_some() && !b.ca => {
                        invalid(&mut out, "pathLenConstraint without CA");
                    }
                    Ok(b) => out.basic = Some(b),
                    Err(_) => invalid(&mut out, "malformed basicConstraints"),
                },
                KEY_USAGE => match BitString::from_der(value) {
                    Ok(bits) => {
                        let bytes = bits.raw_bytes();
                        let count = bytes.len() * 8 - bits.unused_bits() as usize;
                        out.key_usage = Some(
                            (0..count)
                                .map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0)
                                .collect(),
                        );
                    }
                    Err(_) => invalid(&mut out, "malformed keyUsage"),
                },
                SUBJECT_KEY_ID => match SubjectKeyIdentifier::from_der(value) {
                    Ok(id) => out.subject_key_id = Some(id.0.as_bytes().to_vec()),
                    Err(_) => invalid(&mut out, "malformed subjectKeyIdentifier"),
                },
                AUTHORITY_KEY_ID => match AuthorityKeyIdentifier::from_der(value) {
                    Ok(id) => out.authority_key_id = Some(id),
                    Err(_) => invalid(&mut out, "malformed authorityKeyIdentifier"),
                },
                NAME_CONSTRAINTS => out.name_constraints = true,
                IP_ADDR_BLOCKS | AS_IDENTIFIERS => {
                    invalid(&mut out, "RFC 3779 extensions are not supported")
                }
                PROXY_CERT_INFO => invalid(&mut out, "proxy certificates not allowed"),
                _ => {}
            }
        }
        out
    }
    fn has_key_usage(&self, bit: usize) -> bool {
        self.key_usage
            .as_ref()
            .is_none_or(|bits| bits.get(bit).copied().unwrap_or(false))
    }
}

pub(crate) struct ParsedCert {
    cert: Certificate,
    tbs: Vec<u8>,
    subject: Vec<u8>,
    issuer: Vec<u8>,
    /// Over-approximation of OpenSSL's canonical name (case, whitespace and
    /// string type folded away): equal canonical names give equal keys.
    subject_key: Vec<(Oid, Vec<u8>)>,
    extensions: Extensions,
    not_before: i64,
    not_after: i64,
}

fn name_key(name: &Name) -> Vec<(Oid, Vec<u8>)> {
    let mut key: Vec<_> = name
        .0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .map(|atv| {
            let folded = atv
                .value
                .value()
                .iter()
                .filter(|b| b.is_ascii_alphanumeric())
                .map(u8::to_ascii_lowercase)
                .collect();
            (atv.oid, folded)
        })
        .collect();
    key.sort();
    key
}

fn unix(time: &x509_cert::time::Time) -> Result<i64> {
    Ok(i64::try_from(time.to_unix_duration().as_secs())?)
}

pub(super) fn from_der(der: &[u8]) -> Result<Cert> {
    let cert = Certificate::from_der(der).context("Invalid X.509 certificate DER")?;
    ensure!(
        cert.to_der()? == der,
        "Certificate DER is not canonical (crypto-rust admits exact DER only)"
    );
    let tbs = cert.tbs_certificate.to_der()?;
    let subject = cert.tbs_certificate.subject.to_der()?;
    let issuer = cert.tbs_certificate.issuer.to_der()?;
    let validity = &cert.tbs_certificate.validity;
    let parsed = ParsedCert {
        not_before: unix(&validity.not_before)?,
        not_after: unix(&validity.not_after)?,
        subject_key: name_key(&cert.tbs_certificate.subject),
        extensions: Extensions::parse(&cert),
        tbs,
        subject: subject.clone(),
        issuer: issuer.clone(),
        cert,
    };
    Ok(Cert {
        der: der.into(),
        subject: subject.into(),
        issuer: issuer.into(),
        inner: Inner::Rust(Arc::new(parsed)),
    })
}

/// Like `PEM_read_bio_X509` in a loop: every `CERTIFICATE` or
/// `X509 CERTIFICATE` block in order; text and other blocks are skipped.
/// Encapsulated headers (RFC 1421) are rejected.
pub(super) fn stack_from_pem(pem: &[u8]) -> Result<Vec<Cert>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let text = std::str::from_utf8(pem).context("PEM is not UTF-8")?;
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after = &rest[start + 11..];
        let label_end = after.find("-----").context("Malformed PEM header")?;
        let label = &after[..label_end];
        ensure!(!label.contains('\n'), "Malformed PEM header");
        let body_start = &after[label_end + 5..];
        let end_marker = format!("-----END {label}-----");
        let end = body_start
            .find(&end_marker)
            .context("PEM block without END line")?;
        let body = &body_start[..end];
        rest = &body_start[end + end_marker.len()..];
        if label != "CERTIFICATE" && label != "X509 CERTIFICATE" {
            continue;
        }
        ensure!(
            !body.contains(':'),
            "PEM encapsulated headers are not supported"
        );
        let compact: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let der = STANDARD.decode(compact).context("Invalid PEM base64")?;
        out.push(from_der(&der)?);
    }
    Ok(out)
}

pub(super) fn verifies_own_signature(cert: &ParsedCert) -> bool {
    signed_by(cert, cert).is_ok()
}

impl ParsedCert {
    pub(super) fn subject_common_names(&self) -> Vec<Vec<u8>> {
        self.cert
            .tbs_certificate
            .subject
            .0
            .iter()
            .flat_map(|rdn| rdn.0.iter())
            .filter(|atv| atv.oid == COMMON_NAME)
            .map(|atv| atv.value.value().to_vec())
            .collect()
    }
    /// Named curve of an EC key; explicit parameters are `Other` (the path
    /// validation rejects them before any key is used).
    pub(super) fn ec_curve(&self) -> Result<Curve> {
        let spki = &self.cert.tbs_certificate.subject_public_key_info;
        ensure!(
            spki.algorithm.oid == EC_PUBLIC_KEY,
            "Public key is not an EC key"
        );
        let Some(parameters) = &spki.algorithm.parameters else {
            bail!("EC key without parameters");
        };
        Ok(match parameters.decode_as::<Oid>() {
            Ok(SECP384R1) => Curve::P384,
            Ok(SECP256R1) => Curve::P256,
            _ => Curve::Other,
        })
    }
    fn explicit_ec_parameters(&self) -> bool {
        let spki = &self.cert.tbs_certificate.subject_public_key_info;
        spki.algorithm.oid == EC_PUBLIC_KEY
            && !spki
                .algorithm
                .parameters
                .as_ref()
                .is_some_and(|p| p.decode_as::<Oid>().is_ok())
    }
    fn self_issued(&self) -> bool {
        self.subject == self.issuer
    }
    fn serial(&self) -> &[u8] {
        self.cert.tbs_certificate.serial_number.as_bytes()
    }
}

/// Left-pad a big-endian magnitude to the field size; None if too long.
fn field<const N: usize>(magnitude: &[u8]) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    let start = N.checked_sub(magnitude.len())?;
    out[start..].copy_from_slice(magnitude);
    Some(out)
}

fn verify_raw(key: &ParsedCert, digest: &[u8], r: &[u8], s: &[u8]) -> bool {
    use ecdsa::signature::hazmat::PrehashVerifier;
    let spki = &key.cert.tbs_certificate.subject_public_key_info;
    let Some(point) = spki.subject_public_key.as_bytes() else {
        return false;
    };
    match key.ec_curve() {
        Ok(Curve::P384) => {
            let (Some(r), Some(s)) = (field::<48>(r), field::<48>(s)) else {
                return false;
            };
            let (Ok(key), Ok(sig)) = (
                p384::ecdsa::VerifyingKey::from_sec1_bytes(point),
                p384::ecdsa::Signature::from_scalars(r, s),
            ) else {
                return false;
            };
            key.verify_prehash(digest, &sig).is_ok()
        }
        Ok(Curve::P256) => {
            let (Some(r), Some(s)) = (field::<32>(r), field::<32>(s)) else {
                return false;
            };
            let (Ok(key), Ok(sig)) = (
                p256::ecdsa::VerifyingKey::from_sec1_bytes(point),
                p256::ecdsa::Signature::from_scalars(r, s),
            ) else {
                return false;
            };
            key.verify_prehash(digest, &sig).is_ok()
        }
        _ => false,
    }
}

pub(super) fn verify_ecdsa_digest(
    cert: &ParsedCert,
    digest: &[u8],
    signature: &[u8],
) -> Result<bool> {
    let (r, s) = strict_der::ecdsa_sig(signature)?.context("Noncanonical ECDSA signature DER")?;
    // Same key preconditions as OpenSSL's `public_key()?.ec_key()?`.
    cert.ec_curve()?;
    Ok(verify_raw(cert, digest, r, s))
}

pub(super) fn message_digest(hash: MessageHash, data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    match hash {
        MessageHash::Sha256 => sha2::Sha256::digest(data).to_vec(),
        MessageHash::Sha384 => sha2::Sha384::digest(data).to_vec(),
        MessageHash::Sha3_256 => sha3::Sha3_256::digest(data).to_vec(),
        MessageHash::Sha3_384 => sha3::Sha3_384::digest(data).to_vec(),
    }
}

/// `child` is signed by `issuer`: matching inner/outer algorithm, ECDSA with
/// a SHA-2 digest, canonical signature DER, valid under the issuer's key.
fn signed_by(child: &ParsedCert, issuer: &ParsedCert) -> std::result::Result<(), String> {
    use sha2::Digest;
    let outer = &child.cert.signature_algorithm;
    if *outer != child.cert.tbs_certificate.signature {
        return Err("certificate signature algorithm mismatch".into());
    }
    if outer.parameters.is_some() {
        return Err("unsupported certificate signature algorithm parameters".into());
    }
    let digest = match outer.oid {
        ECDSA_SHA224 => sha2::Sha224::digest(&child.tbs).to_vec(),
        ECDSA_SHA256 => sha2::Sha256::digest(&child.tbs).to_vec(),
        ECDSA_SHA384 => sha2::Sha384::digest(&child.tbs).to_vec(),
        ECDSA_SHA512 => sha2::Sha512::digest(&child.tbs).to_vec(),
        _ => return Err("unsupported certificate signature algorithm".into()),
    };
    if !matches!(issuer.ec_curve(), Ok(Curve::P256 | Curve::P384)) {
        return Err("unsupported issuer public key".into());
    }
    let Some(signature) = child.cert.signature.as_bytes() else {
        return Err("certificate signature failure".into());
    };
    match strict_der::ecdsa_sig(signature) {
        Ok(Some((r, s))) if verify_raw(issuer, &digest, r, s) => Ok(()),
        _ => Err("certificate signature failure".into()),
    }
}

/// `X509_check_akid`, with directory names compared as raw DER.
fn akid_matches(child: &ParsedCert, issuer: &ParsedCert) -> bool {
    let Some(akid) = &child.extensions.authority_key_id else {
        return true;
    };
    if let (Some(key_id), Some(skid)) = (&akid.key_identifier, &issuer.extensions.subject_key_id) {
        if key_id.as_bytes() != skid.as_slice() {
            return false;
        }
    }
    if let Some(serial) = &akid.authority_cert_serial_number {
        if serial.as_bytes() != issuer.serial() {
            return false;
        }
    }
    if let Some(names) = &akid.authority_cert_issuer {
        let directory = names.iter().find_map(|n| match n {
            x509_cert::ext::pkix::name::GeneralName::DirectoryName(name) => Some(name),
            _ => None,
        });
        if let Some(name) = directory {
            if name.to_der().ok().as_deref() != Some(issuer.issuer.as_slice()) {
                return false;
            }
        }
    }
    true
}

/// See the module documentation for the checks and fail-closed cases.
pub(super) fn verify_path(
    leaf: &Arc<ParsedCert>,
    chain: &[Arc<ParsedCert>],
    root: &Arc<ParsedCert>,
    time: i64,
) -> std::result::Result<(), String> {
    let path: Vec<&ParsedCert> = std::iter::once(leaf.as_ref())
        .chain(chain.iter().map(Arc::as_ref))
        .chain(std::iter::once(root.as_ref()))
        .collect();
    let num = path.len();
    // Distinct (folded) names: OpenSSL then builds exactly this path.
    for (i, a) in path.iter().enumerate() {
        if path[i + 1..].iter().any(|b| b.subject_key == a.subject_key) {
            return Err("ambiguous certificate names in path (crypto-rust fails closed)".into());
        }
    }
    // Chain building: name, AKID and keyCertSign of each issuer.
    for pair in path.windows(2) {
        let (child, issuer) = (pair[0], pair[1]);
        if child.issuer != issuer.subject || !akid_matches(child, issuer) {
            return Err("unable to get local issuer certificate".into());
        }
        if !issuer.extensions.has_key_usage(KU_KEY_CERT_SIGN) {
            return Err("key usage does not include certificate signing".into());
        }
    }
    // The trust anchor must be self-signed (self-issued with matching AKID).
    let anchor = path[num - 1];
    if !anchor.self_issued() || !akid_matches(anchor, anchor) {
        return Err("unable to get local issuer certificate".into());
    }
    // check_extensions.
    for (i, cert) in path.iter().enumerate() {
        if let Some(why) = cert.extensions.invalid {
            return Err(format!("invalid or unsupported extension ({why})"));
        }
        if cert.extensions.critical_unhandled {
            return Err("unhandled critical extension".into());
        }
        if i > 0 {
            if cert.extensions.name_constraints {
                return Err("name constraints are not supported (crypto-rust fails closed)".into());
            }
            let ext = &cert.extensions;
            let ca = if !ext.has_key_usage(KU_KEY_CERT_SIGN) {
                0
            } else if let Some(basic) = &ext.basic {
                u8::from(basic.ca)
            } else if cert.cert.tbs_certificate.version == Version::V1 && cert.self_issued() {
                3
            } else if ext.key_usage.is_some() {
                4
            } else {
                0
            };
            if ca == 0 || (i + 1 < num && ca != 1) {
                return Err("invalid CA certificate".into());
            }
            // Non-self-issued intermediates below this one (path names are
            // distinct, so every one counts).
            if let Some(limit) = ext.basic.as_ref().and_then(|b| b.path_len_constraint) {
                if i > 1 && i - 1 > usize::from(limit) {
                    return Err("path length constraint exceeded".into());
                }
            }
        }
        if cert.explicit_ec_parameters() {
            return Err("certificate uses explicit EC parameters".into());
        }
    }
    // internal_verify: from the anchor down, signature then validity time.
    for i in (0..num).rev() {
        let issuer = path[(i + 1).min(num - 1)];
        let cert = path[i];
        signed_by(cert, issuer)?;
        if cert.not_before > time {
            return Err("certificate is not yet valid".into());
        }
        if cert.not_after <= time {
            return Err("certificate has expired".into());
        }
    }
    Ok(())
}
