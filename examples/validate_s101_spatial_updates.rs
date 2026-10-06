//! Actual-file CPU loader probe; never creates a window or GPU device.
use ferrite_s100_core::S101Cell;
use std::path::{Path, PathBuf};
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 2, "Usage: base update");
    let base = Path::new(&args[0]);
    let update = PathBuf::from(&args[1]);
    let before = S101Cell::load(base)?;
    anyhow::ensure!(
        S101Cell::load(&update).is_err(),
        "Standalone update was admitted as a complete chart"
    );
    let after = S101Cell::load_with_spatial_updates(base, &[update])?;
    let stats = |c: &S101Cell| serde_json::json!({"points":c.points.len(),"curves":c.curves.len(),"surfaces":c.surfaces.len(),"features":c.features.len()});
    // This probe fixture is the SHOM 369400 edition-2 .001: exactly four
    // whole insertions. Expected details are independently checked by Python.
    anyhow::ensure!(
        after.points.len() == before.points.len() + 1
            && after.curves.len() == before.curves.len() + 1
            && after.surfaces.len() == before.surfaces.len() + 1
            && after.features.len() == before.features.len() + 1,
        "Unexpected fixture counts"
    );
    let mut new = Vec::new();
    for (key, f) in &after.features {
        if !before.features.contains_key(key) {
            new.push(serde_json::json!({"id":f.frid.rcid,"version":f.frid.rver,"code":f.feature_code,
            "attributes":f.attributes.iter().map(|a|serde_json::json!({"code":a.code,"atix":a.atix,"paix":a.paix,"value":a.atvl})).collect::<Vec<_>>() }));
        }
    }
    let existing_feature_changes = before
        .features
        .iter()
        .filter(|(key, f)| {
            after
                .features
                .get(key)
                .is_none_or(|next| format!("{f:?}") != format!("{next:?}"))
        })
        .count();
    anyhow::ensure!(
        existing_feature_changes == 0,
        "Existing feature changed while applying insertion-only update"
    );
    let existing_geometry_changes = before
        .points
        .iter()
        .filter(|(k, v)| {
            after
                .points
                .get(*k)
                .is_none_or(|n| format!("{v:?}") != format!("{n:?}"))
        })
        .count()
        + before
            .curves
            .iter()
            .filter(|(k, v)| {
                after
                    .curves
                    .get(*k)
                    .is_none_or(|n| format!("{v:?}") != format!("{n:?}"))
            })
            .count()
        + before
            .surfaces
            .iter()
            .filter(|(k, v)| {
                after
                    .surfaces
                    .get(*k)
                    .is_none_or(|n| format!("{v:?}") != format!("{n:?}"))
            })
            .count();
    anyhow::ensure!(
        existing_geometry_changes == 0,
        "Existing geometry changed in insertion-only update"
    );
    let new_points = after
        .points
        .iter()
        .filter(|(k, _)| !before.points.contains_key(*k))
        .map(|(_, p)| serde_json::json!({"id":p.id.rcid,"x":p.position.x,"y":p.position.y}))
        .collect::<Vec<_>>();
    let new_curves=after.curves.iter().filter(|(k,_)|!before.curves.contains_key(*k)).map(|(_,c)|
        serde_json::json!({"id":c.id.rcid,"start":c.start_point.map(|p|p.rcid),"end":c.end_point.map(|p|p.rcid),
            "segments":c.segments.iter().map(|s|s.positions.iter().map(|p|[p.x,p.y]).collect::<Vec<_>>()).collect::<Vec<_>>() })).collect::<Vec<_>>();
    let new_surfaces=after.surfaces.iter().filter(|(k,_)|!before.surfaces.contains_key(*k)).map(|(_,s)|
        serde_json::json!({"id":s.id.rcid,"exterior":s.exterior_ring.iter().map(|c|[c.curve_id.rcid,u32::from(c.orientation)]).collect::<Vec<_>>(),"interior_count":s.interior_rings.len()})).collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"before":stats(&before),"after":stats(&after),
        "dataset_edition":after.dsid.edition_number,"applied_update":after.dsid.update_number,
            "new_features":new,"new_points":new_points,"new_curves":new_curves,"new_surfaces":new_surfaces,
            "existing_geometry_changes":existing_geometry_changes,"existing_feature_changes":existing_feature_changes,"window_created":false,
        "scope":"ordered spatial controls and whole record insertion/deletion; unsupported partial attributes/associations rejected"})
        )?
    );
    Ok(())
}
