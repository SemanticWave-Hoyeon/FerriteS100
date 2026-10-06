//! Read-only audit of catalogue visibility and end-user S-101 Pick Reports.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::{AttributeBinding, AttributeVisibility, FeatureCatalogue};
use ferrite_s100_core::S101Cell;
use std::collections::BTreeMap;
fn main() -> Result<()> {
    let fc = FeatureCatalogue::load(std::env::args().nth(1).context("FC XML path")?)?;
    let mut bindings = Vec::new();
    let mut add = |owner: &str, list: &[AttributeBinding]| {
        for b in list {
            if b.visibility != AttributeVisibility::Public {
                bindings.push(serde_json::json!({"owner":owner,"attribute":b.attribute_code,"visibility":format!("{:?}",b.visibility)}));
            }
        }
    };
    for (owner, ty) in &fc.feature_types {
        add(owner, &ty.attribute_bindings);
    }
    for (owner, ty) in &fc.information_types {
        add(owner, &ty.attribute_bindings);
    }
    for (owner, ty) in &fc.complex_attributes {
        add(owner, &ty.sub_attributes);
    }
    bindings.sort_by_key(|b| b.to_string());
    let mut files = Vec::new();
    if let Some(root) = std::env::args().nth(2) {
        for e in walkdir::WalkDir::new(root) {
            let e = e?;
            if e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "000") {
                files.push(e.into_path());
            }
        }
        files.sort();
        ensure!(!files.is_empty(), "No base cells");
    }
    let mut cells = Vec::new();
    for path in files {
        let mut cell = S101Cell::load(&path)?;
        cell.normalize_feature_codes(&fc.feature_type_codes());
        let mut omitted = BTreeMap::<String, usize>::new();
        let (mut raw_count, mut shown_count) = (0, 0);
        for feature in cell.features.values() {
            let report = ferrite_s101::pick_report_attributes(
                &fc,
                feature.feature_code.as_deref().unwrap_or(""),
                &feature.attributes,
            );
            raw_count += feature.attributes.len();
            shown_count += report.len();
            let mut shown = BTreeMap::new();
            for pair in report {
                *shown.entry(pair).or_insert(0usize) += 1;
            }
            for a in &feature.attributes {
                let pair = (
                    a.code
                        .clone()
                        .unwrap_or_else(|| format!("Attribute {}", a.natc)),
                    a.value
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| a.atvl.clone()),
                );
                if let Some(n) = shown.get_mut(&pair).filter(|n| **n > 0) {
                    *n -= 1;
                } else {
                    *omitted.entry(pair.0).or_insert(0) += 1;
                }
            }
        }
        let foid_present = cell.features.values().filter(|f| f.foid.is_some()).count();
        let unique_foids: std::collections::HashSet<_> =
            cell.features.values().filter_map(|f| f.foid).collect();
        cells.push(serde_json::json!({"source":path,"features":cell.features.len(),"foid_present":foid_present,"unique_foids":unique_foids.len(),"raw_attributes":raw_count,"shown_attributes":shown_count,"omitted_by_code":omitted}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"fc_version":fc.version,"bindings":bindings,"cells":cells,"native_ui_verified":false})
        )?
    );
    Ok(())
}
