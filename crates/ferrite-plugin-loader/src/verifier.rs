//! Plugin signature and hash verification
//!
//! Uses Ed25519 for signature verification and SHA-256 for hash validation.

use std::path::Path;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::error::PluginError;

/// Plugin verifier using Ed25519 signatures
pub struct PluginVerifier {
    /// Public key for signature verification (None = skip verification)
    public_key: Option<VerifyingKey>,
    /// Whether to require signatures (false = allow unsigned plugins)
    require_signature: bool,
}

impl PluginVerifier {
    /// Create a new verifier with the given public key
    pub fn new(public_key_bytes: Option<&[u8; 32]>) -> Result<Self, PluginError> {
        let public_key = if let Some(bytes) = public_key_bytes {
            Some(VerifyingKey::from_bytes(bytes).map_err(|_| PluginError::InvalidSignature)?)
        } else {
            None
        };

        Ok(Self {
            public_key,
            require_signature: public_key.is_some(),
        })
    }

    /// Create a verifier that doesn't require signatures (development mode)
    pub fn development_mode() -> Self {
        warn!("Plugin verifier running in development mode - signatures not required");
        Self {
            public_key: None,
            require_signature: false,
        }
    }

    /// Create a verifier that rejects ALL plugins unconditionally.
    /// Use this in production when no signing key is configured —
    /// prevents loading unsigned/untrusted code.
    pub fn reject_all() -> Self {
        warn!("Plugin verifier in reject-all mode — no plugins will load");
        Self {
            public_key: None,
            require_signature: true, // require_signature=true + no key = always reject
        }
    }

    /// Verify plugin DLL signature
    ///
    /// 1. Compute SHA-256 hash of DLL
    /// 2. Load .sig file
    /// 3. Verify Ed25519 signature
    pub fn verify_signature(&self, dll_path: &Path) -> Result<bool, PluginError> {
        self.ensure_loading_allowed()?;
        // No signature was checked in explicit development mode. Report false,
        // with no IO; actual loader separately requires captured DLL hash matching.
        if !self.require_signature {
            return Ok(false);
        }

        let root = crate::snapshot::canonical_root(
            dll_path
                .parent()
                .ok_or_else(|| PluginError::LoadError("DLL parent missing".into()))?,
        )?;
        let name = dll_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PluginError::LoadError("Unsupported DLL filename".into()))?;
        let bytes =
            crate::snapshot::capture_regular(&root, name, crate::snapshot::MAX_PLUGIN_BYTES)?;
        let signature = self.capture_signature(&root, name)?;
        self.verify_signature_bytes(&bytes, signature.as_deref())?;
        Ok(true)
    }

    pub fn verify_hash(&self, dll_path: &Path, expected_hash: &str) -> Result<bool, PluginError> {
        let root = crate::snapshot::canonical_root(
            dll_path
                .parent()
                .ok_or_else(|| PluginError::LoadError("DLL parent missing".into()))?,
        )?;
        let name = dll_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PluginError::LoadError("Unsupported DLL filename".into()))?;
        let bytes =
            crate::snapshot::capture_regular(&root, name, crate::snapshot::MAX_PLUGIN_BYTES)?;
        self.verify_hash_bytes(&bytes, expected_hash)?;
        Ok(true)
    }

    pub(crate) fn ensure_loading_allowed(&self) -> Result<(), PluginError> {
        if self.require_signature && self.public_key.is_none() {
            return Err(PluginError::InvalidSignature);
        }
        Ok(())
    }
    pub(crate) fn capture_signature(
        &self,
        root: &Path,
        name: &str,
    ) -> Result<Option<Vec<u8>>, PluginError> {
        self.ensure_loading_allowed()?;
        if !self.require_signature {
            return Ok(None);
        }
        let signature_name = Path::new(name).with_extension("sig");
        let signature_name = signature_name
            .to_str()
            .ok_or(PluginError::InvalidSignature)?;
        match crate::snapshot::capture_regular(root, signature_name, 64) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(PluginError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(
                PluginError::SignatureNotFound(root.join(signature_name).display().to_string()),
            ),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn verify_hash_bytes(
        &self,
        bytes: &[u8],
        expected_hash: &str,
    ) -> Result<(), PluginError> {
        if bytes.len() > crate::snapshot::MAX_PLUGIN_BYTES
            || expected_hash.len() != 64
            || !expected_hash.bytes().all(|b| b.is_ascii_hexdigit())
            || !compute_sha256_hex(bytes).eq_ignore_ascii_case(expected_hash)
        {
            return Err(PluginError::HashMismatch);
        }
        Ok(())
    }
    pub(crate) fn verify_signature_bytes(
        &self,
        bytes: &[u8],
        signature: Option<&[u8]>,
    ) -> Result<(), PluginError> {
        self.ensure_loading_allowed()?;
        if !self.require_signature {
            debug!("Signature verification skipped (development mode)");
            return Ok(());
        }
        let key = self
            .public_key
            .as_ref()
            .ok_or(PluginError::InvalidSignature)?;
        let signature = Signature::from_slice(signature.ok_or(PluginError::InvalidSignature)?)
            .map_err(|_| PluginError::InvalidSignature)?;
        key.verify(&compute_sha256(bytes), &signature)
            .map_err(|_| PluginError::InvalidSignature)
    }
}

/// Compute SHA-256 hash of data
pub fn compute_sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Compute SHA-256 hash and return as hex string
pub fn compute_sha256_hex(data: &[u8]) -> String {
    let hash = compute_sha256(data);
    hex_encode(&hash)
}

/// Encode bytes as lowercase hex string
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_signature_authorize_the_same_owned_bytes_and_reject_mutation() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[19; 32]);
        let bytes = b"local non-executable signature fixture";
        let signature = key.sign(&compute_sha256(bytes)).to_bytes();
        let verifier = PluginVerifier::new(Some(&key.verifying_key().to_bytes())).unwrap();
        let digest = compute_sha256_hex(bytes);
        verifier.verify_hash_bytes(bytes, &digest).unwrap();
        verifier
            .verify_signature_bytes(bytes, Some(&signature))
            .unwrap();
        let mut changed = bytes.to_vec();
        changed[0] ^= 1;
        assert!(verifier.verify_hash_bytes(&changed, &digest).is_err());
        assert!(verifier
            .verify_signature_bytes(&changed, Some(&signature))
            .is_err());
        assert!(verifier.verify_signature_bytes(bytes, None).is_err());
        assert!(PluginVerifier::reject_all()
            .verify_signature_bytes(bytes, Some(&signature))
            .is_err());
    }
    #[test]
    fn test_sha256() {
        let data = b"hello world";
        let hash = compute_sha256_hex(data);
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_development_mode() {
        let verifier = PluginVerifier::development_mode();
        // Development mode should always pass
        assert!(!verifier.require_signature);
        assert!(!verifier
            .verify_signature(Path::new("/nonexistent/ferrite-signature-unit.dll"))
            .unwrap());
        assert!(matches!(
            PluginVerifier::reject_all()
                .verify_signature(Path::new("/nonexistent/ferrite-signature-unit.dll")),
            Err(PluginError::InvalidSignature)
        ));
    }

    #[test]
    fn same_captured_bytes_bind_hash_and_signature_and_mutation_rejects() {
        use ed25519_dalek::{Signer, SigningKey};
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let verifier = PluginVerifier::new(Some(&signer.verifying_key().to_bytes())).unwrap();
        let original = b"benign library byte fixture, never executed";
        let signature = signer.sign(&compute_sha256(original)).to_bytes();
        let hash = compute_sha256_hex(original);
        assert!(verifier.verify_hash_bytes(original, &hash).is_ok());
        assert!(verifier
            .verify_signature_bytes(original, Some(&signature))
            .is_ok());
        let changed = b"changed library byte fixture, never executed";
        assert!(verifier.verify_hash_bytes(changed, &hash).is_err());
        assert!(verifier
            .verify_signature_bytes(changed, Some(&signature))
            .is_err());
        assert!(verifier
            .verify_signature_bytes(original, Some(&signature[..63]))
            .is_err());
        assert!(PluginVerifier::reject_all()
            .ensure_loading_allowed()
            .is_err());
        assert!(PluginVerifier::development_mode()
            .verify_signature_bytes(original, None)
            .is_ok());
    }
}
