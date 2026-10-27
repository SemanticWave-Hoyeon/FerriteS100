//! Test data helpers (the data is git-ignored; tests skip without it).
#![allow(dead_code)]
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_gml::{read_file, Dataset, Issue, ReadLimits};
use std::path::{Path, PathBuf};

/// Workspace-relative path, or None when the (git-ignored) data is absent.
pub fn workspace_file(relative: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: {relative} not present");
        None
    }
}

pub fn fc(relative: &str) -> Option<FeatureCatalogue> {
    Some(FeatureCatalogue::load(workspace_file(relative)?).unwrap())
}

pub fn read(relative: &str, fc: &FeatureCatalogue) -> Option<Dataset> {
    Some(read_file(&workspace_file(relative)?, Some(fc), ReadLimits::default()).unwrap())
}

pub fn count(issues: &[Issue], check: &str) -> usize {
    issues.iter().filter(|i| i.check == check).count()
}

pub fn checks(issues: &[Issue]) -> std::collections::BTreeMap<&str, usize> {
    let mut m = std::collections::BTreeMap::new();
    for i in issues {
        *m.entry(i.check.as_str()).or_default() += 1;
    }
    m
}

pub const S124_DIR: &str =
    "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES";
pub const S124_FC: &str = "Catalogues/FC/S-124/124_FC_2.0.0.xml";
