//! Original sample-position diagnostic, not entire-cell/product certification.
use anyhow::{Context, Result};
use ferrite_kernel::{CoverageSource, GridWindow};
use ferrite_s102::BathymetryCoverage;
fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .context("usage: audit_s102_domain_population S102_DIRECTORY")?;
    let mut output = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.file_name().to_string_lossy().starts_with("._")
            || !entry
                .path()
                .extension()
                .is_some_and(|s| s.eq_ignore_ascii_case("h5"))
        {
            continue;
        }
        let coverages = BathymetryCoverage::open(entry.path())?;
        let mut instances = Vec::new();
        for c in &coverages {
            let g = c.geometry();
            let mut populated = 0usize;
            let mut outside = 0usize;
            for row in (0..g.height).step_by(128) {
                for column in (0..g.width).step_by(2048) {
                    let window = GridWindow {
                        column,
                        row,
                        width: 2048.min(g.width - column),
                        height: 128.min(g.height - row),
                    };
                    for (i, s) in c.read_window(window)?.samples.iter().enumerate() {
                        if s.value.is_none() {
                            continue;
                        }
                        populated += 1;
                        if c.requires_geometric_mask() {
                            let (x, y) = g
                                .position(column + i % window.width, row + i / window.width)
                                .context("Invalid window position")?;
                            if !c.is_valid_position(x, y) {
                                outside += 1;
                            }
                        }
                    }
                }
            }
            anyhow::ensure!(
                c.observed_depth_centroids_outside_domain() == (outside > 0),
                "Depth diagnostic differs from independent window count"
            );
            instances.push(serde_json::json!({"instance":c.instance_name,"populated_depths":populated,"populated_original_depth_positions_outside_domain":outside,"range_violations":c.observed_range_violations()}));
        }
        let quality = if let Some(q) = coverages[0].quality.as_ref() {
            let g = q.geometry();
            let mut populated = 0usize;
            let mut outside = 0usize;
            for row in (0..g.height).step_by(128) {
                for column in (0..g.width).step_by(2048) {
                    let window = GridWindow {
                        column,
                        row,
                        width: 2048.min(g.width - column),
                        height: 128.min(g.height - row),
                    };
                    for (i, id) in q.read_window_ids(window)?.iter().enumerate() {
                        if *id == 0 {
                            continue;
                        }
                        populated += 1;
                        if q.domain.requires_mask() {
                            let (x, y) = g
                                .position(column + i % window.width, row + i / window.width)
                                .context("Invalid quality position")?;
                            if !q.domain.contains(x, y) {
                                outside += 1;
                            }
                        }
                    }
                }
            }
            anyhow::ensure!(
                q.observed_id_centroids_outside_domain() == (outside > 0),
                "Quality diagnostic differs from independent window count"
            );
            Some(
                serde_json::json!({"nonzero_ids":populated,"original_id_positions_outside_own_domain":outside}),
            )
        } else {
            None
        };
        output
            .push(serde_json::json!({"file":entry.path(),"instances":instances,"quality":quality}));
    }
    anyhow::ensure!(!output.is_empty(), "No S102 datasets found");
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"diagnostic":"original sample positions, not entire-cell intersections or producer certification","maximum_window_columns":2048,"maximum_window_rows":128,"datasets":output})
        )?
    );
    Ok(())
}
