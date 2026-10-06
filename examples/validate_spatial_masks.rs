//! Inspect MASK tuples and resolve complete feature line geometry without a GUI.
use anyhow::{Context, Result};
use ferrite_s100_core::S101Cell;
fn main() -> Result<()> {
    let root = std::env::args().nth(1).context("Pass dataset directory")?;
    let mut paths = Vec::new();
    for e in walkdir::WalkDir::new(root) {
        let e = e?;
        if e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "000") {
            paths.push(e.into_path());
        }
    }
    paths.sort();
    let mut rows = Vec::new();
    for path in paths {
        let cell = S101Cell::load(&path)?;
        let mut masks = Vec::new();
        let mut errors = Vec::new();
        let mut leaf_curves = 0;
        let mut vertices = 0;
        for (key, f) in &cell.features {
            for m in &f.masks {
                masks.push(serde_json::json!({"feature":key,"rcnm":m.spatial_id.rcnm,"rcid":m.spatial_id.rcid,"mind":m.mask_type,"muin":m.update_instruction}));
            }
            match ferrite_s101::resolve_feature_line_geometry(&cell, f, &[]) {
                Ok(lines) => {
                    leaf_curves += lines.len();
                    vertices += lines.iter().map(|l| l.points.len()).sum::<usize>();
                    for l in &lines {
                        anyhow::ensure!(
                            !f.masks.iter().any(|m| m.update_instruction == 1
                                && matches!(m.mask_type, 1 | 2)
                                && m.spatial_id.key() == l.spatial_id.key()),
                            "Masked leaf survived"
                        );
                    }
                }
                Err(error) => errors.push(serde_json::json!({"feature":key,"error":error})),
            }
        }
        masks.sort_by_key(|x| x.to_string());
        errors.sort_by_key(|x| x.to_string());
        rows.push(serde_json::json!({"source":path,"features":cell.features.len(),"masks":masks,"geometry_errors":errors,"unmasked_leaf_curves":leaf_curves,"vertices":vertices}));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
