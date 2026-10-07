use anyhow::{Context, Result};
use ferrite_kernel::{CoverageSource, GridWindow};
use ferrite_s102::BathymetryCoverage;
fn main() -> Result<()> {
    let mut rows = Vec::new();
    let root = std::env::args()
        .nth(1)
        .context("usage: validate_s102 S-102_DIRECTORY")?;
    let paths = dataset_paths(std::path::Path::new(&root))?;
    for p in paths {
        for c in BathymetryCoverage::open(&p)? {
            anyhow::ensure!(
                c.root_enclosure.encoding_compatible(),
                "Root geographic bounds do not have a verified encoding-compatible enclosure: {:?}",
                c.root_enclosure
            );
            if let Some(q) = c.quality.as_ref() {
                anyhow::ensure!(q.root_enclosure.is_some_and(|e|e.encoding_compatible()),"Root geographic bounds do not enclose own quality domain under supported profile");
            }
            let g = c.geometry();
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            let mut valid = 0usize;
            let mut uncertainty = 0usize;
            let mut min_uncertainty = f32::INFINITY;
            let mut max_uncertainty = f32::NEG_INFINITY;
            for row in (0..g.height).step_by(128) {
                for column in (0..g.width).step_by(2048) {
                    let tile = c.read_window(GridWindow {
                        column,
                        row,
                        width: 2048.min(g.width - column),
                        height: 128.min(g.height - row),
                    })?;
                    for s in tile.samples {
                        if let Some(v) = s.value {
                            min = min.min(v);
                            max = max.max(v);
                            valid += 1;
                        }
                        if let Some(u) = s.uncertainty {
                            min_uncertainty = min_uncertainty.min(u);
                            max_uncertainty = max_uncertainty.max(u);
                            uncertainty += 1;
                        }
                    }
                }
            }
            if valid == 0 {
                min = c.depth_fill;
                max = c.depth_fill;
            }
            anyhow::ensure!(
                min == c.declared_min_depth && max == c.declared_max_depth,
                "Stored depth bounds differ from grid values"
            );
            let observed_uncertainty_bounds = if uncertainty > 0 {
                (min_uncertainty, max_uncertainty)
            } else {
                (c.uncertainty_fill, c.uncertainty_fill)
            };
            anyhow::ensure!(
                observed_uncertainty_bounds
                    == (c.declared_min_uncertainty, c.declared_max_uncertainty),
                "Stored uncertainty bounds differ from grid values"
            );
            rows.push(serde_json::json!({"file":p,"instance":c.instance_name,"edition":c.product_specification,"shape":[g.height,g.width],"origin":[g.origin_x,g.origin_y],"spacing":[g.spacing_x,g.spacing_y],"horizontal_crs":g.horizontal_crs,"vertical_crs":c.vertical_crs,"vertical_datum":c.vertical_datum,"depth_fill":c.depth_fill,"valid_depths":valid,"valid_uncertainties":uncertainty,"min_depth":min,"max_depth":max,"maximum_read_window_rows":128,"maximum_read_window_columns":2048,"uncertainty_encoding":format!("{:?}",c.uncertainty_encoding),"min_uncertainty":observed_uncertainty_bounds.0,"max_uncertainty":observed_uncertainty_bounds.1,"time_point":c.time_point}));
        }
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

fn dataset_paths(root: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file()
            && !entry.file_name().to_string_lossy().starts_with("._")
            && entry
                .path()
                .extension()
                .is_some_and(|s| s.eq_ignore_ascii_case("h5"))
        {
            paths.push(entry.into_path());
        }
    }
    anyhow::ensure!(!paths.is_empty(), "No S-102 HDF5 datasets found");
    paths.sort();
    Ok(paths)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_errors_and_empty_input_cannot_succeed() {
        let p = std::env::temp_dir().join(format!("s102-scanner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir(&p).unwrap();
        assert!(dataset_paths(&p.join("missing")).is_err());
        assert!(dataset_paths(&p).is_err());
        std::fs::write(p.join("unrelated.txt"), b"x").unwrap();
        std::fs::write(p.join("._sidecar.H5"), b"x").unwrap();
        assert!(dataset_paths(&p).is_err());
        std::fs::create_dir(p.join("nested")).unwrap();
        std::fs::write(p.join("nested/second.h5"), b"x").unwrap();
        std::fs::write(p.join("first.H5"), b"x").unwrap();
        assert_eq!(
            dataset_paths(&p).unwrap(),
            [p.join("first.H5"), p.join("nested/second.h5")]
        );
        std::fs::remove_dir_all(p).unwrap();
    }
}
