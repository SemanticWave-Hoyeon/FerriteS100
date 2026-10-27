//! Signature reports of the S-101 update planner, keyed by the data
//! source's canonical paths (the keys `ferrite_security::authorize_datasets_in`
//! uses). The engine's `dataset_signature_policy` offers the same lookups to
//! hosts.
use anyhow::Result;
use ferrite_product_api::platform::{DataSource, NativeDataSource};
use ferrite_security::{AuthenticatedSnapshot, AuthorizedDatasets};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The authenticated snapshot of `path`, by its canonical path in `data`.
pub fn dataset_snapshot_in(
    data: &dyn DataSource,
    report: &AuthorizedDatasets,
    path: &Path,
    require_signature: bool,
) -> Result<Option<Arc<AuthenticatedSnapshot>>> {
    report.snapshot_for(&data.canonicalize(path)?, require_signature)
}

/// A report for datasets opened without signature verification.
pub fn unchecked_datasets(paths: &[PathBuf]) -> Result<AuthorizedDatasets> {
    unchecked_datasets_in(&NativeDataSource, paths)
}

/// [`unchecked_datasets`] keyed by the data source's canonical paths.
pub fn unchecked_datasets_in(
    data: &dyn DataSource,
    paths: &[PathBuf],
) -> Result<AuthorizedDatasets> {
    // Match the canonical keys used by authenticated dataset loading.
    // None means no authenticated snapshot; it does not assert unsignedness.
    let canonical = paths
        .iter()
        .map(|path| data.canonicalize(path))
        .collect::<io::Result<Vec<_>>>()?;
    Ok(AuthorizedDatasets::unchecked(canonical))
}
