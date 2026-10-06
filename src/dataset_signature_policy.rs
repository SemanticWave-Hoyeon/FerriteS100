//! Application choice to open datasets without authenticating their signatures.
//! Content identities and parsing remain independent of this setting.
use anyhow::{ensure, Context, Result};
use ferrite_security::{AuthenticatedSnapshot, AuthorizedDatasets, DatasetDiscoveryAuthorization};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) fn dataset_snapshot(
    report: &AuthorizedDatasets,
    path: &Path,
    require_signature: bool,
) -> Result<Option<Arc<AuthenticatedSnapshot>>> {
    let canonical = path.canonicalize()?;
    let snapshot = report
        .snapshots
        .get(&canonical)
        .context("Dataset path changed after authorization; reopen the dataset")?
        .clone();
    ensure!(
        !require_signature || snapshot.is_some(),
        "Required authenticated dataset snapshot is missing; reopen the dataset"
    );
    Ok(snapshot)
}

pub(crate) fn unchecked_datasets(paths: &[PathBuf]) -> Result<AuthorizedDatasets> {
    let mut report = AuthorizedDatasets {
        dataset_discovery: Default::default(),
        snapshots: Default::default(),
        signed_count: 0,
        unsigned_count: 0,
        metadata_warnings: Vec::new(),
    };
    for path in paths {
        // Match the canonical keys used by authenticated dataset loading.
        // None means no authenticated snapshot; it does not assert unsignedness.
        let canonical = path.canonicalize()?;
        report.dataset_discovery.insert(
            canonical.clone(),
            DatasetDiscoveryAuthorization::SignatureVerificationDisabled,
        );
        report.snapshots.insert(canonical, None);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_security::{authorize_datasets, TrustAnchors, UnsignedPolicy};
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferrite-signature-setting-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn off_skips_bad_signature_metadata_without_claiming_verification() {
        let temp = Temp::new();
        let path = temp.0.join("cell.000");
        std::fs::write(&path, b"Dataset parsing is tested by the cell loader").unwrap();
        std::fs::write(temp.0.join("CATALOG.SIGN"), b"invalid signature").unwrap();
        std::fs::write(temp.0.join("CATALOG.XML"), b"invalid catalogue").unwrap();
        let report = unchecked_datasets(&[path.clone(), path.clone()]).unwrap();
        assert_eq!(report.signed_count, 0);
        assert_eq!(report.unsigned_count, 0);
        assert_eq!(report.dataset_discovery.len(), 1);
        assert!(matches!(
            report.dataset_discovery[&path.canonicalize().unwrap()],
            DatasetDiscoveryAuthorization::SignatureVerificationDisabled
        ));
        assert_eq!(report.snapshots.len(), 1);
        assert!(report.snapshots[&path.canonicalize().unwrap()].is_none());
        assert!(
            authorize_datasets(&[path], &TrustAnchors::default(), 0, UnsignedPolicy::Reject)
                .is_err()
        );
    }
    #[test]
    fn off_still_reports_missing_input() {
        let temp = Temp::new();
        assert!(unchecked_datasets(&[temp.0.join("missing.000")]).is_err());
    }
    #[test]
    fn required_signature_never_falls_back_to_a_live_input() {
        let temp = Temp::new();
        let a = temp.0.join("a.000");
        let b = temp.0.join("b.000");
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&b, b"B").unwrap();
        let report = unchecked_datasets(&[a.clone()]).unwrap();
        assert!(dataset_snapshot(&report, &a, false).unwrap().is_none());
        assert!(dataset_snapshot(&report, &a, true).is_err());
        assert!(dataset_snapshot(&report, &b, false).is_err());
        assert!(dataset_snapshot(&report, &b, true).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn retargeting_a_selected_symlink_after_authorization_rejects_new_target() {
        let temp = Temp::new();
        let a = temp.0.join("a.000");
        let b = temp.0.join("b.000");
        let selected = temp.0.join("selected.000");
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&b, b"B").unwrap();
        std::os::unix::fs::symlink(&a, &selected).unwrap();
        let report = unchecked_datasets(&[selected.clone()]).unwrap();
        std::fs::remove_file(&selected).unwrap();
        std::os::unix::fs::symlink(&b, &selected).unwrap();
        assert!(dataset_snapshot(&report, &selected, true).is_err());
        assert!(dataset_snapshot(&report, &selected, false).is_err());
    }
}
