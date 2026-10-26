//! Cryptographic backends behind one crate-internal API.
//!
//! `crypto-openssl` (default) is the original OpenSSL implementation.
//! `crypto-rust` is a pure-Rust implementation (RustCrypto) that reproduces
//! the verdicts of the OpenSSL path validation used here; where an OpenSSL
//! behaviour is not reproduced exactly, the Rust backend rejects (fails
//! closed). With both features, OpenSSL is the default and the backend is
//! chosen per [`crate::TrustAnchors`]; cross tests compare their verdicts.
use anyhow::{bail, Result};
use std::sync::Arc;

#[cfg(not(any(feature = "crypto-openssl", feature = "crypto-rust")))]
compile_error!("ferrite-security needs the crypto-openssl or crypto-rust feature");

#[cfg(feature = "crypto-openssl")]
mod openssl_backend;
#[cfg(feature = "crypto-rust")]
mod rust_backend;
#[cfg(feature = "crypto-rust")]
pub(crate) mod strict_der;

/// Which implementation verifies certificates and signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CryptoBackend {
    /// OpenSSL (`crypto-openssl`).
    #[cfg(feature = "crypto-openssl")]
    OpenSsl,
    /// Pure Rust, RustCrypto (`crypto-rust`).
    #[cfg(feature = "crypto-rust")]
    Rust,
}

impl Default for CryptoBackend {
    /// OpenSSL when compiled in, otherwise the Rust backend.
    fn default() -> Self {
        #[cfg(feature = "crypto-openssl")]
        {
            Self::OpenSsl
        }
        #[cfg(not(feature = "crypto-openssl"))]
        {
            Self::Rust
        }
    }
}

impl CryptoBackend {
    /// Every backend compiled into this build.
    pub fn available() -> &'static [CryptoBackend] {
        &[
            #[cfg(feature = "crypto-openssl")]
            Self::OpenSsl,
            #[cfg(feature = "crypto-rust")]
            Self::Rust,
        ]
    }
}

/// Named elliptic curves this crate distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Curve {
    P256,
    P384,
    Other,
}

/// Digests of detached payload signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageHash {
    Sha256,
    Sha3_256,
    Sha384,
    Sha3_384,
}

#[derive(Clone)]
enum Inner {
    #[cfg(feature = "crypto-openssl")]
    OpenSsl(openssl::x509::X509),
    #[cfg(feature = "crypto-rust")]
    Rust(Arc<rust_backend::ParsedCert>),
}

/// A parsed X.509 certificate bound to the backend that parsed it.
#[derive(Clone)]
pub(crate) struct Cert {
    /// Encoding as the backend re-encodes it (OpenSSL `i2d_X509`; the Rust
    /// backend only admits certificates whose DER round-trips exactly).
    der: Arc<[u8]>,
    subject: Arc<[u8]>,
    issuer: Arc<[u8]>,
    inner: Inner,
}

impl Cert {
    pub(crate) fn from_der(backend: CryptoBackend, der: &[u8]) -> Result<Self> {
        match backend {
            #[cfg(feature = "crypto-openssl")]
            CryptoBackend::OpenSsl => openssl_backend::from_der(der),
            #[cfg(feature = "crypto-rust")]
            CryptoBackend::Rust => rust_backend::from_der(der),
        }
    }
    /// Certificates of a PEM text, in order (other PEM blocks are skipped).
    pub(crate) fn stack_from_pem(backend: CryptoBackend, pem: &[u8]) -> Result<Vec<Self>> {
        match backend {
            #[cfg(feature = "crypto-openssl")]
            CryptoBackend::OpenSsl => openssl_backend::stack_from_pem(pem),
            #[cfg(feature = "crypto-rust")]
            CryptoBackend::Rust => rust_backend::stack_from_pem(pem),
        }
    }
    pub(crate) fn backend(&self) -> CryptoBackend {
        match self.inner {
            #[cfg(feature = "crypto-openssl")]
            Inner::OpenSsl(_) => CryptoBackend::OpenSsl,
            #[cfg(feature = "crypto-rust")]
            Inner::Rust(_) => CryptoBackend::Rust,
        }
    }
    pub(crate) fn der(&self) -> &[u8] {
        &self.der
    }
    /// DER of the subject Name.
    pub(crate) fn subject_der(&self) -> &[u8] {
        &self.subject
    }
    /// DER of the issuer Name.
    pub(crate) fn issuer_der(&self) -> &[u8] {
        &self.issuer
    }
    pub(crate) fn sha256_hex(&self) -> Result<String> {
        Ok(crate::hex(&sha256(&self.der)?))
    }
    /// Whether the certificate signature verifies under its own public key.
    pub(crate) fn verifies_own_signature(&self) -> Result<bool> {
        match &self.inner {
            #[cfg(feature = "crypto-openssl")]
            Inner::OpenSsl(cert) => openssl_backend::verifies_own_signature(cert),
            #[cfg(feature = "crypto-rust")]
            Inner::Rust(cert) => Ok(rust_backend::verifies_own_signature(cert)),
        }
    }
    /// Raw string contents of every subject commonName attribute.
    pub(crate) fn subject_common_names(&self) -> Vec<Vec<u8>> {
        match &self.inner {
            #[cfg(feature = "crypto-openssl")]
            Inner::OpenSsl(cert) => openssl_backend::subject_common_names(cert),
            #[cfg(feature = "crypto-rust")]
            Inner::Rust(cert) => cert.subject_common_names(),
        }
    }
    /// Curve of an EC public key; an error for other key types.
    pub(crate) fn ec_curve(&self) -> Result<Curve> {
        match &self.inner {
            #[cfg(feature = "crypto-openssl")]
            Inner::OpenSsl(cert) => openssl_backend::ec_curve(cert),
            #[cfg(feature = "crypto-rust")]
            Inner::Rust(cert) => cert.ec_curve(),
        }
    }
    /// ECDSA verification of a precomputed digest with this certificate's
    /// key. `signature` must already be canonical DER ([`canonical_ecdsa_der`]).
    pub(crate) fn verify_ecdsa_digest(&self, digest: &[u8], signature: &[u8]) -> Result<bool> {
        match &self.inner {
            #[cfg(feature = "crypto-openssl")]
            Inner::OpenSsl(cert) => openssl_backend::verify_ecdsa_digest(cert, digest, signature),
            #[cfg(feature = "crypto-rust")]
            Inner::Rust(cert) => rust_backend::verify_ecdsa_digest(cert, digest, signature),
        }
    }
    /// ECDSA verification of a message hashed with `hash`.
    pub(crate) fn verify_ecdsa_message(
        &self,
        hash: MessageHash,
        data: &[u8],
        signature: &[u8],
    ) -> Result<bool> {
        match &self.inner {
            #[cfg(feature = "crypto-openssl")]
            Inner::OpenSsl(cert) => {
                openssl_backend::verify_ecdsa_message(cert, hash, data, signature)
            }
            #[cfg(feature = "crypto-rust")]
            Inner::Rust(cert) => {
                let digest = rust_backend::message_digest(hash, data);
                rust_backend::verify_ecdsa_digest(cert, &digest, signature)
            }
        }
    }
}

/// Validate `leaf` → `chain` (issuer order, nearest first) → `root` at Unix
/// time `time`, as `X509_verify_cert` does with only `root` trusted, a fixed
/// check time and `X509_V_FLAG_CHECK_SS_SIGNATURE`. The error is the reason.
pub(crate) fn verify_path(
    leaf: &Cert,
    chain: &[Cert],
    root: &Cert,
    time: i64,
) -> std::result::Result<(), String> {
    let backend = leaf.backend();
    if chain.iter().chain([root]).any(|c| c.backend() != backend) {
        return Err("certificates of different crypto backends".into());
    }
    match (&leaf.inner, &root.inner) {
        #[cfg(feature = "crypto-openssl")]
        (Inner::OpenSsl(leaf), Inner::OpenSsl(root)) => {
            let chain = chain
                .iter()
                .map(|c| match &c.inner {
                    Inner::OpenSsl(c) => c.clone(),
                    #[allow(unreachable_patterns)]
                    _ => unreachable!("backend checked above"),
                })
                .collect::<Vec<_>>();
            openssl_backend::verify_path(leaf, &chain, root, time)
        }
        #[cfg(feature = "crypto-rust")]
        (Inner::Rust(leaf), Inner::Rust(root)) => {
            let chain = chain
                .iter()
                .map(|c| match &c.inner {
                    Inner::Rust(c) => Arc::clone(c),
                    #[allow(unreachable_patterns)]
                    _ => unreachable!("backend checked above"),
                })
                .collect::<Vec<_>>();
            rust_backend::verify_path(leaf, &chain, root, time)
        }
        #[allow(unreachable_patterns)]
        _ => Err("certificates of different crypto backends".into()),
    }
}

/// Whether `bytes` is exactly the canonical DER of an ECDSA-Sig-Value
/// (OpenSSL: `d2i_ECDSA_SIG` then `i2d_ECDSA_SIG` gives the same bytes).
/// An error means the bytes do not parse at all.
pub(crate) fn canonical_ecdsa_der(backend: CryptoBackend, bytes: &[u8]) -> Result<bool> {
    match backend {
        #[cfg(feature = "crypto-openssl")]
        CryptoBackend::OpenSsl => openssl_backend::canonical_ecdsa_der(bytes),
        #[cfg(feature = "crypto-rust")]
        CryptoBackend::Rust => strict_der::canonical_ecdsa_der(bytes),
    }
}

/// Streaming SHA-384 (OpenSSL when compiled in, otherwise `sha2`).
pub(crate) struct Sha384 {
    #[cfg(feature = "crypto-openssl")]
    inner: openssl::hash::Hasher,
    #[cfg(not(feature = "crypto-openssl"))]
    state: sha2::Sha384,
}

impl Sha384 {
    pub(crate) fn new() -> Result<Self> {
        #[cfg(feature = "crypto-openssl")]
        {
            Ok(Self {
                inner: openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha384())?,
            })
        }
        #[cfg(not(feature = "crypto-openssl"))]
        {
            use sha2::Digest;
            Ok(Self {
                state: sha2::Sha384::new(),
            })
        }
    }
    pub(crate) fn update(&mut self, bytes: &[u8]) -> Result<()> {
        #[cfg(feature = "crypto-openssl")]
        {
            self.inner.update(bytes)?;
        }
        #[cfg(not(feature = "crypto-openssl"))]
        {
            use sha2::Digest;
            self.state.update(bytes);
        }
        Ok(())
    }
    pub(crate) fn finish(&mut self) -> Result<Vec<u8>> {
        #[cfg(feature = "crypto-openssl")]
        {
            Ok(self.inner.finish()?.to_vec())
        }
        #[cfg(not(feature = "crypto-openssl"))]
        {
            use sha2::Digest;
            Ok(self.state.finalize_reset().to_vec())
        }
    }
}

pub(crate) fn sha384(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut hasher = Sha384::new()?;
    hasher.update(bytes)?;
    hasher.finish()
}

pub(crate) fn sha256(bytes: &[u8]) -> Result<Vec<u8>> {
    #[cfg(feature = "crypto-openssl")]
    {
        Ok(openssl::hash::hash(openssl::hash::MessageDigest::sha256(), bytes)?.to_vec())
    }
    #[cfg(not(feature = "crypto-openssl"))]
    {
        use sha2::Digest;
        Ok(sha2::Sha256::digest(bytes).to_vec())
    }
}

/// Reject a certificate list that mixes backends (programming error guard).
pub(crate) fn ensure_backend(cert: &Cert, backend: CryptoBackend) -> Result<()> {
    if cert.backend() != backend {
        bail!("Certificate parsed by a different crypto backend");
    }
    Ok(())
}
