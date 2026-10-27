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
    Some(
        read_file(
            &workspace_file(relative)?,
            &[&ferrite_s129::rules::IDENTITY],
            Some(fc),
            ReadLimits::default(),
        )
        .unwrap(),
    )
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

pub const S129_FC: &str = "Catalogues/FC/S-129/129_Feature_Catalogue_2.0.0.XML";
pub const S129_130: &str = "TestData/IHO-S129-Test-Datasets/12900MCTDS130TS.gml";
pub const S129_200: &str = "TestData/IHO-S129-Test-Datasets/12900MCTDS200TS.gml";
