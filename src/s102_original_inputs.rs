//! Publication-owned original authentication, one record per signed dataset file.
//! Lookup paths and memory ownership confer no cancellation/removal authority.
use anyhow::{ensure, Context, Result};
use ferrite_security::{
    retained_original_authentication_storage, AuthenticatedSnapshot, AuthorizedDatasets,
    DatasetDiscoveryAuthorization, OriginalDatasetAuthentication,
};
use std::{
    collections::{BTreeMap, HashSet},
    mem::size_of,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) const MAX_FILES: usize = 512;
const EVIDENCE_BUDGET: usize = 128 * 1024 * 1024;
#[derive(Clone)]
pub(crate) struct OwnedOriginalInput {
    pub(crate) original: OriginalDatasetAuthentication,
    pub(crate) snapshot: Arc<AuthenticatedSnapshot>,
}
pub(crate) type Registry = BTreeMap<PathBuf, Arc<OwnedOriginalInput>>;

pub(crate) fn capture(
    report: &AuthorizedDatasets,
    canonical: &Path,
    snapshot: Option<&Arc<AuthenticatedSnapshot>>,
) -> Result<Option<Arc<OwnedOriginalInput>>> {
    match report.checked_dataset_discovery(canonical)? {
        DatasetDiscoveryAuthorization::Authenticated(discovery) => {
            let snapshot =
                snapshot.context("Original authentication needs its retained snapshot")?;
            let report_snapshot = report
                .snapshots
                .get(canonical)
                .and_then(Option::as_ref)
                .context("Missing original snapshot owner")?;
            ensure!(
                Arc::ptr_eq(snapshot, report_snapshot),
                "Foreign S102 snapshot owner"
            );
            Ok(Some(Arc::new(OwnedOriginalInput {
                original: discovery.original_authentication().clone(),
                snapshot: snapshot.clone(),
            })))
        }
        DatasetDiscoveryAuthorization::SignatureVerificationDisabled
        | DatasetDiscoveryAuthorization::UnsignedEvaluation => {
            ensure!(
                snapshot.is_none(),
                "Unauthenticated S102 must not retain an original proof"
            );
            Ok(None)
        }
    }
}

fn charge(total: &mut usize, amount: usize) -> Result<()> {
    *total = total
        .checked_add(amount)
        .context("Original evidence charge overflow")?;
    ensure!(
        *total <= EVIDENCE_BUDGET,
        "Original evidence retention policy exceeded"
    );
    Ok(())
}
fn container_charge(registry: &Registry, total: &mut usize) -> Result<()> {
    ensure!(
        registry.len() <= MAX_FILES,
        "Original evidence exceeds 512 dataset records"
    );
    // Logical payload plus a fixed per-node admission charge, not allocator/RSS accounting.
    charge(total, size_of::<Registry>())?;
    for path in registry.keys() {
        charge(total, size_of::<(PathBuf, Arc<OwnedOriginalInput>)>() + 96)?;
        charge(total, path.capacity())?;
    }
    Ok(())
}

/// Build the replacement privately; account OLD and NEXT live containers, distinct
/// record owners, and the authorization report's separately cloned URI storage.
/// Shared Arc identity is used solely for live owned-storage accounting.
pub(crate) fn stage<'a>(
    old: &Registry,
    incoming: impl IntoIterator<Item = (&'a Path, Option<&'a Arc<OwnedOriginalInput>>)>,
    report: &AuthorizedDatasets,
) -> Result<Registry> {
    let mut next = old.clone();
    for (path, record) in incoming {
        if let Some(record) = record {
            ensure!(
                record.original.resource_path() == path && record.snapshot.source == path,
                "S102 original evidence/parser snapshot path mismatch"
            );
            ensure!(
                !next.contains_key(path),
                "Duplicate original evidence publication"
            );
            ensure!(
                next.len() < MAX_FILES,
                "Original evidence exceeds 512 dataset records"
            );
            next.insert(path.to_path_buf(), record.clone());
        }
    }
    let mut total = 0;
    container_charge(old, &mut total)?;
    container_charge(&next, &mut total)?;
    let mut seen = HashSet::new();
    seen.try_reserve(MAX_FILES)?;
    let mut proofs = Vec::new();
    proofs.try_reserve(MAX_FILES * 2)?;
    for record in old.values().chain(next.values()) {
        if seen.insert(Arc::as_ptr(record)) {
            charge(
                &mut total,
                size_of::<OwnedOriginalInput>() - size_of::<OriginalDatasetAuthentication>(),
            )?;
            proofs.push(&record.original);
        }
    }
    for discovery in report.dataset_discovery.values() {
        if let DatasetDiscoveryAuthorization::Authenticated(bound) = discovery {
            ensure!(
                proofs.len() < MAX_FILES * 2,
                "Original evidence peak record count exceeded"
            );
            proofs.push(bound.original_authentication());
        }
    }
    charge(
        &mut total,
        seen.capacity()
            .checked_mul(size_of::<*const OwnedOriginalInput>() + 1)
            .context("Original evidence scratch charge overflow")?,
    )?;
    charge(
        &mut total,
        proofs
            .capacity()
            .checked_mul(size_of::<&OriginalDatasetAuthentication>())
            .context("Original evidence scratch charge overflow")?,
    )?;
    retained_original_authentication_storage(proofs, EVIDENCE_BUDGET - total, MAX_FILES * 2)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_boundary_and_overflow_reject_without_truncation() {
        let mut total = EVIDENCE_BUDGET - 1;
        charge(&mut total, 1).unwrap();
        assert!(charge(&mut total, 1).is_err());
        let mut total = usize::MAX;
        assert!(charge(&mut total, 1).is_err());
    }
    #[test]
    fn unchecked_retention_preserves_bytes_without_edition_or_original_authentication() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("legacy21.h5");
        std::fs::write(&path, b"original legacy product bytes").unwrap();
        let path = path.canonicalize().unwrap();
        let report = crate::dataset_signature_policy::unchecked_datasets(&[path.clone()]).unwrap();
        let mut budget = 1024;
        let captured =
            crate::s102_input_capture::CapturedInput::capture(&report, &path, false, &mut budget)
                .unwrap();
        assert!(capture(&report, &path, captured.authenticated.as_ref())
            .unwrap()
            .is_none());
        assert!(stage(&Registry::new(), [(path.as_path(), None)], &report)
            .unwrap()
            .is_empty());
        std::fs::write(&path, b"external mutation").unwrap();
        captured.verify().unwrap();
        assert_eq!(
            std::fs::read(captured.path()).unwrap(),
            b"original legacy product bytes"
        );
    }
    #[test]
    fn empty_stage_retains_no_authentication() {
        let report = AuthorizedDatasets {
            dataset_discovery: Default::default(),
            snapshots: Default::default(),
            signed_count: 0,
            unsigned_count: 0,
            metadata_warnings: Vec::new(),
        };
        let old = Registry::new();
        let next = stage(&old, std::iter::empty(), &report).unwrap();
        assert!(old.is_empty() && next.is_empty());
    }
}
