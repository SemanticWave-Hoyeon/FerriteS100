//! Stable S102 receiver inputs. Disabling signatures never means reading a
//! mutable external file throughout a multi-product publication transaction.
use anyhow::{ensure, Context, Result};
use ferrite_security::{AuthenticatedSnapshot, AuthorizedDatasets, UnauthenticatedSnapshot};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, sync::Arc};

pub(crate) struct CapturedInput {
    pub authenticated: Option<Arc<AuthenticatedSnapshot>>,
    pub unchecked: Option<Arc<UnauthenticatedSnapshot>>,
    length: u64,
    sha256: [u8; 32],
}

impl CapturedInput {
    pub fn capture(
        report: &AuthorizedDatasets,
        source: &Path,
        required: bool,
        remaining: &mut u64,
    ) -> Result<Self> {
        let authenticated =
            crate::dataset_signature_policy::dataset_snapshot(report, source, required)?;
        let unchecked = if authenticated.is_none() {
            Some(Arc::new(UnauthenticatedSnapshot::copy_bounded(
                source, *remaining,
            )?))
        } else {
            None
        };
        let path = authenticated
            .as_ref()
            .map(|s| s.path())
            .or_else(|| unchecked.as_ref().map(|s| s.path()))
            .context("No captured S102 input")?;
        let (length, sha256) = fingerprint(path, *remaining)?;
        *remaining -= length;
        Ok(Self {
            authenticated,
            unchecked,
            length,
            sha256,
        })
    }

    pub fn path(&self) -> &Path {
        self.authenticated
            .as_ref()
            .map(|s| s.path())
            .or_else(|| self.unchecked.as_ref().map(|s| s.path()))
            .expect("Captured input owner")
    }

    pub fn verify(&self) -> Result<()> {
        ensure!(
            fingerprint(self.path(), self.length)? == (self.length, self.sha256),
            "Captured S102 input changed before publication"
        );
        Ok(())
    }
}

fn fingerprint(path: &Path, limit: u64) -> Result<(u64, [u8; 32])> {
    let mut input = std::fs::File::open(path)?;
    ensure!(
        input.metadata()?.is_file(),
        "S102 input is not a regular file"
    );
    let mut hash = Sha256::new();
    let mut length = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        length = length
            .checked_add(n as u64)
            .context("S102 input size overflow")?;
        ensure!(
            length <= limit,
            "S102 aggregate input snapshot budget exceeded"
        );
        hash.update(&buffer[..n]);
    }
    Ok((length, hash.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "s102-input-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn retained_off_input_survives_external_mutation_and_checks_private_mutation() {
        let folder = Temp::new();
        let source = folder.path().join("coverage.h5");
        std::fs::write(&source, b"original").unwrap();
        let report =
            crate::dataset_signature_policy::unchecked_datasets(&[source.clone()]).unwrap();
        let mut budget = 8;
        let captured = CapturedInput::capture(&report, &source, false, &mut budget).unwrap();
        assert_eq!(budget, 0);
        assert!(captured.authenticated.is_none());
        std::fs::write(&source, b"external replacement").unwrap();
        assert_eq!(std::fs::read(captured.path()).unwrap(), b"original");
        captured.verify().unwrap();
        std::fs::write(captured.path(), b"tampered").unwrap();
        assert!(captured.verify().is_err());
    }
    #[test]
    fn aggregate_receiver_budget_and_required_authentication_remain_independent() {
        let folder = Temp::new();
        let source = folder.path().join("coverage.h5");
        std::fs::write(&source, b"12345").unwrap();
        let report =
            crate::dataset_signature_policy::unchecked_datasets(&[source.clone()]).unwrap();
        let mut budget = 8;
        let _first = CapturedInput::capture(&report, &source, false, &mut budget).unwrap();
        assert_eq!(budget, 3);
        assert!(CapturedInput::capture(&report, &source, false, &mut budget).is_err());
        assert_eq!(budget, 3);
        budget = 8;
        assert!(CapturedInput::capture(&report, &source, true, &mut budget).is_err());
        assert_eq!(budget, 8);
    }
}
