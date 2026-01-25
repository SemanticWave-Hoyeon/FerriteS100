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

    /// Verify plugin DLL signature
    ///
    /// 1. Compute SHA-256 hash of DLL
    /// 2. Load .sig file
    /// 3. Verify Ed25519 signature
    pub fn verify_signature(&self, dll_path: &Path) -> Result<bool, PluginError> {
        if !self.require_signature {
            debug!("Signature verification skipped (development mode)");
            return Ok(true);
        }

        let public_key = self
            .public_key
            .as_ref()
            .ok_or(PluginError::InvalidSignature)?;

        // Read DLL and compute hash
        let dll_bytes = std::fs::read(dll_path)?;
        let hash = compute_sha256(&dll_bytes);

        // Load signature file
        let sig_path = dll_path.with_extension("sig");
        if !sig_path.exists() {
            return Err(PluginError::SignatureNotFound(
                sig_path.display().to_string(),
            ));
        }

        let sig_bytes = std::fs::read(&sig_path)?;
        if sig_bytes.len() != 64 {
            return Err(PluginError::InvalidSignature);
        }

        let signature =
            Signature::from_slice(&sig_bytes).map_err(|_| PluginError::InvalidSignature)?;

        // Verify signature over hash
        public_key
            .verify(&hash, &signature)
            .map_err(|_| PluginError::InvalidSignature)?;

        debug!("Plugin signature verified: {}", dll_path.display());
        Ok(true)
    }

    /// Verify DLL hash matches manifest
    pub fn verify_hash(&self, dll_path: &Path, expected_hash: &str) -> Result<bool, PluginError> {
        let dll_bytes = std::fs::read(dll_path)?;
        let actual_hash = compute_sha256_hex(&dll_bytes);

        if actual_hash.eq_ignore_ascii_case(expected_hash) {
            debug!("Plugin hash verified: {}", dll_path.display());
            Ok(true)
        } else {
            warn!(
                "Plugin hash mismatch: expected {}, got {}",
                expected_hash, actual_hash
            );
            Err(PluginError::HashMismatch)
        }
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
    }
}
