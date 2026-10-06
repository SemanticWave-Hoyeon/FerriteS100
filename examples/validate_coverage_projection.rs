//! Real DataCoverage projected with the renderer's Scaler, including longitude copies.
use anyhow::{ensure, Context, Result};
use ferrite_render::*;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
fn collect(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            collect(&entry?.path(), files)?;
        }
    } else if path.extension().is_some_and(|e| e == "000")
        && !path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("._")
    {
        files.push(path.to_owned());
    }
    Ok(())
}
fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .context("Pass current S-101 data root")?;
    let mut paths = Vec::new();
    collect(Path::new(&root), &mut paths)?;
    paths.sort();
    ensure!(!paths.is_empty(), "No cells");
    let cells = paths
        .iter()
        .map(|p| ferrite_s100_core::S101Cell::load(p).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    let inventory =
        ferrite_s101::coverage_projection::GeographicCoverageInventory::from_cells(&cells)?;
    ensure!(
        inventory.current_dataset_count() == cells.len(),
        "Use current-edition fixture"
    );
    let mut bounds = GeoBounds::default();
    let mut probes = Vec::new();
    for (cell_id, cell) in cells.iter().enumerate() {
        let coverages = ferrite_s101::coverage_geometry::extract_current_coverages(cell)?;
        for coverage in coverages {
            for surface in coverage.surfaces {
                for p in &surface.exterior {
                    bounds.expand(WorldPoint::new(p[0], p[1]));
                }
                let p = surface.exterior[0];
                let mut command = DrawingInstruction::Point(
                    PointInstruction::new("ACHBRT07".into(), WorldPoint::new(p[0], p[1]))
                        .with_cell_index(cell_id),
                );
                command.set_portrayal_origin(PortrayalOrigin::feature_point(WorldPoint::new(
                    p[0], p[1],
                ))?);
                probes.push(command);
                let mut command = DrawingInstruction::Area(
                    AreaInstruction::new(
                        surface
                            .exterior
                            .iter()
                            .map(|p| WorldPoint::new(p[0], p[1]))
                            .collect(),
                    )
                    .with_cell_index(cell_id),
                );
                command.set_portrayal_origin(PortrayalOrigin::NonPoint);
                probes.push(command);
            }
        }
    }
    bounds.expand_by_percent(5.);
    let mut cases = Vec::new();
    for wrapping in [false, true] {
        for denominator in [4000., 12000., 45000., 90000., 180000., 180001., 300000.] {
            let mut context = RenderContext::new(Viewport::new(640., 400.));
            context.set_bounds(bounds);
            for command in &probes {
                context.add_instruction(command.clone());
            }
            context.get_sorted_instructions();
            context.scaler.display_scale = denominator;
            let prepared = inventory
                .prepare_flat(
                    &context,
                    &BTreeSet::new(),
                    [640, 400],
                    wrapping,
                    32 * 1024 * 1024,
                )?
                .context("Missing current coverage")?;
            ensure!(
                prepared.pass_count() == if wrapping { 3 } else { 1 },
                "Wrong pass count"
            );
            let mut hidden = 0;
            let mut clipped = 0;
            let mut unclipped = 0;
            for pass in 0..prepared.pass_count() {
                for ordinal in 0..context.instruction_count() {
                    match prepared.pass(pass)?.decision(ordinal)? {
                        ferrite_kernel::coverage_frame::FrameCoverageDecision::Hidden => {
                            hidden += 1
                        }
                        ferrite_kernel::coverage_frame::FrameCoverageDecision::ClipDataset(_) => {
                            clipped += 1
                        }
                        ferrite_kernel::coverage_frame::FrameCoverageDecision::Unclipped => {
                            unclipped += 1
                        }
                    }
                }
            }
            context.set_prepared_coverage(prepared)?;
            context.portrayal_visibility()?;
            // Direct scaler changes must invalidate the assigned frame.
            context.scaler.set_pixel_ratio(2.);
            ensure!(
                context.prepared_coverage().is_err(),
                "Stale scaler accepted"
            );
            cases.push(serde_json::json!({"wrapping":wrapping,"denominator":denominator,"hidden":hidden,"clipped":clipped,"unclipped":unclipped,"stale_scaler_rejected":true}));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"datasets":cells.len(),"probe_commands":probes.len(),"cases":cases,"actual_portrayal_commands":false,"application_binding_complete":false})
        )?
    );
    Ok(())
}
