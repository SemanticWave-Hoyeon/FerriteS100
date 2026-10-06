//! Actual-file sequential update probe, CPU only; never creates a window.
use ferrite_s100_core::{FeatureRecord, S101Cell};
use std::path::{Path, PathBuf};
fn attrs(f: &FeatureRecord) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut paths = Vec::<Vec<(String, u16)>>::new();
    let mut out = Vec::new();
    for a in &f.attributes {
        let mut path = if a.paix == 0 {
            Vec::new()
        } else {
            paths
                .get(usize::from(a.paix) - 1)
                .ok_or_else(|| anyhow::anyhow!("Invalid materialized parent"))?
                .clone()
        };
        path.push((
            a.code
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Unmapped attribute"))?,
            a.atix,
        ));
        paths.push(path.clone());
        out.push(serde_json::json!({"path":path,"value":a.atvl}));
    }
    Ok(out)
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 3, "Usage: base .001 .002");
    let base = Path::new(&args[0]);
    let updates: Vec<_> = args[1..].iter().map(PathBuf::from).collect();
    let first = S101Cell::load_with_spatial_updates(base, &updates[..1])?;
    let final_cell = S101Cell::load_with_spatial_updates(base, &updates)?;
    let mut changed: Vec<_> = first
        .features
        .iter()
        .filter(|(id, f)| {
            final_cell
                .features
                .get(id)
                .is_none_or(|n| format!("{n:?}") != format!("{f:?}"))
        })
        .map(|(_, f)| f.frid.rcid)
        .collect();
    changed.sort();
    anyhow::ensure!(
        changed == [113],
        "Unexpected existing feature changes: {changed:?}"
    );
    let feature = final_cell
        .features
        .values()
        .find(|f| f.frid.rcid == 113)
        .ok_or_else(|| anyhow::anyhow!("Feature113 missing"))?;
    let base_cell = S101Cell::load(base)?;
    println!(
        "{}",
        serde_json::json!({"spatial_information":final_cell.spatial_information_associations.iter().filter(|(k,_)|[72,73,74,75].iter().any(|id|ferrite_s100_core::RecordId::new(110,*id).key()==**k)).map(|(k,associations)|serde_json::json!({"point_key":k,"associations":associations.iter().map(|a|serde_json::json!({"information_name":a.info_id.rcnm,"information_id":a.info_id.rcid,"association_code":a.niac,"role_code":a.narc,"instruction":a.update_instruction})).collect::<Vec<_>>()})).collect::<Vec<_>>(),"counts":[base_cell.features.len(),first.features.len(),final_cell.features.len()],"changed_existing_ids":changed,"final113":{"feature":feature.feature_code,"attributes":attrs(feature)?,"spatial_associations":feature.spatial_associations.iter().map(|s|serde_json::json!({"name":s.spatial_id.rcnm,"id":s.spatial_id.rcid,"orientation":s.ornt,"scale_min":s.scale_minimum,"scale_max":s.scale_maximum,"instruction":s.update_instruction})).collect::<Vec<_>>()}})
    );
    Ok(())
}
