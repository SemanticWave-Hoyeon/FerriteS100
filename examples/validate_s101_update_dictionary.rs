//! CPU-only actual insertion update chain probe, no UI or GPU.
use ferrite_s100_core::S101Cell;
use std::path::{Path, PathBuf};
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() >= 2, "Usage: base updates...");
    let base = Path::new(&args[0]);
    let before = S101Cell::load(base)?;
    let updates: Vec<_> = args[1..].iter().map(PathBuf::from).collect();
    let after = S101Cell::load_with_spatial_updates(base, &updates)?;
    // This insertion-only fixture must preserve every existing object exactly.
    for (id, f) in &before.features {
        anyhow::ensure!(
            after
                .features
                .get(id)
                .is_some_and(|n| format!("{n:?}") == format!("{f:?}")),
            "Existing feature changed: {id}"
        );
    }
    for (id, p) in &before.points {
        anyhow::ensure!(
            after
                .points
                .get(id)
                .is_some_and(|n| format!("{n:?}") == format!("{p:?}")),
            "Existing point changed: {id}"
        );
    }
    for (id, p) in &before.curves {
        anyhow::ensure!(
            after
                .curves
                .get(id)
                .is_some_and(|n| format!("{n:?}") == format!("{p:?}")),
            "Existing curve changed: {id}"
        );
    }
    for (id, p) in &before.surfaces {
        anyhow::ensure!(
            after
                .surfaces
                .get(id)
                .is_some_and(|n| format!("{n:?}") == format!("{p:?}")),
            "Existing surface changed: {id}"
        );
    }
    let mut inserted:Vec<_>=after.features.iter().filter(|(k,_)|!before.features.contains_key(*k)).map(|(_,f)|serde_json::json!({"id":f.frid.rcid,"code":f.feature_code,"attributes":f.attributes.iter().map(|a|serde_json::json!({"code":a.code,"atix":a.atix,"paix":a.paix,"value":a.atvl})).collect::<Vec<_>>()})).collect();
    inserted.sort_by_key(|f| f["id"].as_u64());
    println!(
        "{}",
        serde_json::json!({"before":{"points":before.points.len(),"curves":before.curves.len(),"surfaces":before.surfaces.len(),"features":before.features.len()},"after":{"points":after.points.len(),"curves":after.curves.len(),"surfaces":after.surfaces.len(),"features":after.features.len()},"inserted":inserted,"categoryOfPylon":after.code_mappings.attributes.get_numeric("categoryOfPylon"),"PylonBridgeSupport":after.code_mappings.feature_types.get_numeric("PylonBridgeSupport")})
    );
    Ok(())
}
