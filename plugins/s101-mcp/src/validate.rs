//! Phase 0 backend validation (plan2 §8.1).
//!
//! Runs a battery of self-checks against the loaded indices and reports a
//! structured pass/fail JSON. The plan calls for **manual** validation
//! samples (e.g. confirm a few feature IDs by reading the GML), which is
//! the part a human author still has to fill in. What this module provides
//! is the *automated* half: invariants the parser/indices must hold for the
//! MCP server to be trustworthy at all.
//!
//! Pass-rate target: ≥ 95% (per plan2 §8.1). The CLI exits non-zero if
//! we fall under that bar.

use serde::Serialize;

use crate::indices::Indices;

#[derive(Debug, Serialize)]
pub struct ValidationReport {
    pub checks: Vec<CheckResult>,
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl ValidationReport {
    pub fn pass_rate(&self) -> f64 {
        if self.checks.is_empty() {
            return 0.0;
        }
        let passed = self.checks.iter().filter(|c| c.passed).count();
        passed as f64 / self.checks.len() as f64
    }
}

pub fn run(idx: &Indices) -> ValidationReport {
    let checks = vec![
        check_feature_index_complete(idx),
        check_geometry_index_coverage(idx),
        check_catalogue_loaded(idx),
        check_feature_codes_resolved(idx),
        check_extent_inside_world(idx),
        check_no_orphan_spatial_associations(idx),
        check_feature_type_codes_in_catalogue(idx),
    ];
    ValidationReport { checks }
}

fn check_feature_index_complete(idx: &Indices) -> CheckResult {
    let cell_count = idx.cell.features.len();
    let idx_count = idx.feature.len();
    let passed = cell_count == idx_count;
    CheckResult {
        name: "feature-index-completeness".to_string(),
        passed,
        detail: format!(
            "cell.features.len() = {} vs feature_index.len() = {}",
            cell_count, idx_count
        ),
    }
}

fn check_geometry_index_coverage(idx: &Indices) -> CheckResult {
    let total = idx.feature.len();
    let indexed = idx.geometry.indexed_count();
    // Some features (e.g. abstract metadata) may legitimately have no
    // spatial association. Below 50% indexed is suspicious though.
    let passed = total == 0 || (indexed as f64) / (total as f64) >= 0.5;
    CheckResult {
        name: "geometry-index-coverage".to_string(),
        passed,
        detail: format!(
            "{} of {} features have resolved geometry ({:.1}%)",
            indexed,
            total,
            (indexed as f64 / total.max(1) as f64) * 100.0
        ),
    }
}

fn check_catalogue_loaded(idx: &Indices) -> CheckResult {
    let f = idx.catalogue.feature_count();
    let a = idx.catalogue.simple_attribute_count();
    let passed = f > 0 && a > 0;
    CheckResult {
        name: "catalogue-loaded".to_string(),
        passed,
        detail: format!("{} feature types, {} simple attributes", f, a),
    }
}

fn check_feature_codes_resolved(idx: &Indices) -> CheckResult {
    let total = idx.cell.features.len();
    let unresolved = idx
        .cell
        .features
        .values()
        .filter(|f| f.feature_code.is_none() || f.feature_code.as_deref() == Some("UNKNOWN"))
        .count();
    // Some unresolved is normal during catalogue mismatch, but >5% is
    // a parsing or code-mapping issue.
    let ratio = unresolved as f64 / total.max(1) as f64;
    let passed = ratio <= 0.05;
    CheckResult {
        name: "feature-code-resolution".to_string(),
        passed,
        detail: format!(
            "{} of {} features have unresolved feature_code ({:.1}%)",
            unresolved,
            total,
            ratio * 100.0
        ),
    }
}

fn check_extent_inside_world(idx: &Indices) -> CheckResult {
    let Some(ext) = idx.geometry.extent() else {
        return CheckResult {
            name: "extent-inside-world".to_string(),
            passed: false,
            detail: "no spatial extent could be computed (geometry index empty)".to_string(),
        };
    };
    let in_world = ext.min_lon >= -180.0
        && ext.max_lon <= 180.0
        && ext.min_lat >= -90.0
        && ext.max_lat <= 90.0;
    CheckResult {
        name: "extent-inside-world".to_string(),
        passed: in_world,
        detail: format!(
            "[{:.4},{:.4}] – [{:.4},{:.4}]",
            ext.min_lon, ext.min_lat, ext.max_lon, ext.max_lat
        ),
    }
}

fn check_no_orphan_spatial_associations(idx: &Indices) -> CheckResult {
    let mut orphans = 0usize;
    let mut total = 0usize;
    for f in idx.cell.features.values() {
        for spas in &f.spatial_associations {
            total += 1;
            let key = spas.spatial_id.key();
            let resolved = idx.cell.points.contains_key(&key)
                || idx.cell.multi_points.contains_key(&key)
                || idx.cell.curves.contains_key(&key)
                || idx.cell.composite_curves.contains_key(&key)
                || idx.cell.surfaces.contains_key(&key);
            if !resolved {
                orphans += 1;
            }
        }
    }
    let ratio = orphans as f64 / total.max(1) as f64;
    // Spec compliance: spatial associations should resolve to a real
    // record. >1% orphans is a parser bug or a corrupt cell.
    let passed = ratio <= 0.01;
    CheckResult {
        name: "no-orphan-spatial-associations".to_string(),
        passed,
        detail: format!(
            "{} of {} associations orphan ({:.2}%)",
            orphans,
            total,
            ratio * 100.0
        ),
    }
}

fn check_feature_type_codes_in_catalogue(idx: &Indices) -> CheckResult {
    let mut missing: Vec<String> = Vec::new();
    for f in idx.cell.features.values() {
        if let Some(code) = f.feature_code.as_deref() {
            if code == "UNKNOWN" {
                continue;
            }
            if idx.catalogue.describe_feature(code).is_none()
                && !missing.contains(&code.to_string())
            {
                missing.push(code.to_string());
            }
        }
    }
    let passed = missing.is_empty();
    let preview: Vec<&str> = missing.iter().take(5).map(|s| s.as_str()).collect();
    CheckResult {
        name: "feature-type-codes-in-catalogue".to_string(),
        passed,
        detail: if passed {
            "all dataset feature codes resolve in the FC".to_string()
        } else {
            format!(
                "{} dataset feature codes not found in FC (showing up to 5: {:?})",
                missing.len(),
                preview
            )
        },
    }
}
