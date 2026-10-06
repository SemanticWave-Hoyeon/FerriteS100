//! Headless real-data test of DataCoverage topology, projected region selection,
//! regional obscuring masks and overscale flags. Application rendering is separate.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::{
    coverage_selection::{CoverageFootprint, Region},
    geodesy::{GeographicPosition, Mercator},
};
use std::path::{Path, PathBuf};
fn collect(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            collect(&entry?.path(), out)?;
        }
    } else if root.extension().is_some_and(|s| s == "000")
        && !root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("._")
    {
        out.push(root.into());
    }
    Ok(())
}
fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .context("Pass current S-101 chart root")?;
    let mut paths = Vec::new();
    collect(Path::new(&root), &mut paths)?;
    paths.sort();
    ensure!(!paths.is_empty(), "No S-101 cells");
    let mut raw = Vec::new();
    let mut rows = Vec::new();
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    for (dataset, path) in paths.iter().enumerate() {
        let cell = ferrite_s100_core::S101Cell::load(path)?;
        let reference_scale = ferrite_s101::coverage_scale::dataset_reference_scale(&cell)?;
        // Part 11.3.2: producer-defined filename suffixes carry no scale.
        // Parse the same retained bytes with source names that previously
        // triggered invented overview/berthing scales.
        for alias in ["101AA001TEST.000", "101AA006TEST.000"] {
            let aliased =
                ferrite_s100_core::S101Cell::load_from(&path.with_file_name(alias), path)?;
            ensure!(
                ferrite_s101::coverage_scale::dataset_reference_scale(&aliased)? == reference_scale,
                "Filename changed DataCoverage reference scale"
            );
            ensure!(
                aliased.dsid.dataset_name == cell.dsid.dataset_name,
                "Filename changed encoded dataset identification"
            );
        }
        let coverages = ferrite_s101::coverage_geometry::extract_current_coverages(&cell)?;
        let mut cr = Vec::new();
        for coverage in coverages {
            let mut surfaces = Vec::new();
            for surface in coverage.surfaces {
                let project = |ring: &[[f64; 2]]| -> Result<Vec<[f64; 2]>> {
                    ring.iter()
                        .map(|p| Mercator::World.project(GeographicPosition::new(p[1], p[0])?))
                        .collect()
                };
                let exterior = project(&surface.exterior)?;
                let holes = surface
                    .holes
                    .iter()
                    .map(|h| project(h))
                    .collect::<Result<Vec<_>>>()?;
                for p in exterior.iter().chain(holes.iter().flatten()) {
                    for axis in 0..2 {
                        min[axis] = min[axis].min(p[axis]);
                        max[axis] = max[axis].max(p[axis]);
                    }
                }
                surfaces.push((exterior, holes));
            }
            cr.push(serde_json::json!({"key":coverage.feature_key,"scales":coverage.scales,"drawing_index":coverage.drawing_index,"surfaces":surfaces.len()}));
            raw.push((dataset, coverage.feature_key, coverage.scales, surfaces));
        }
        rows.push(serde_json::json!({"source":path,"reference_scale_denominator":reference_scale,"filename_independence_verified":true,"coverages":cr}));
    }
    ensure!(
        min.iter().chain(max.iter()).all(|v| v.is_finite()) && max[0] > min[0] && max[1] > min[1],
        "Invalid fixture extent"
    );
    let width = max[0] - min[0];
    let height = max[1] - min[1];
    let device = |p: [f64; 2]| {
        [
            50. + 1100. * (p[0] - min[0]) / width,
            50. + 700. * (max[1] - p[1]) / height,
        ]
    };
    let mut inventory = Vec::new();
    for (dataset_id, coverage_id, scales, surfaces) in raw {
        let mut region: Option<Region> = None;
        for (exterior, holes) in surfaces {
            let exterior = exterior.into_iter().map(device).collect::<Vec<_>>();
            let holes = holes
                .into_iter()
                .map(|h| h.into_iter().map(device).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            let part = Region::from_rings(&exterior, &holes)?;
            region = Some(region.map_or_else(|| part.clone(), |r| r.union(&part)));
        }
        inventory.push(CoverageFootprint {
            dataset_id,
            coverage_id,
            scales,
            region: region.context("Empty coverage geometry")?,
        });
    }
    let viewport = Region::from_rings(
        &[[0., 0.], [1200., 0.], [1200., 800.], [0., 800.], [0., 0.]],
        &[],
    )?;
    let mut cases = Vec::new();
    for denominator in [
        4000., 6000., 12000., 22000., 45000., 90000., 180000., 180001., 300000.,
    ] {
        let plan =
            ferrite_s101::coverage_loading::display_plan(&inventory, denominator, &viewport)?;
        let pixel_masks = ferrite_kernel::coverage_raster::rasterize_selected_masks(
            &plan.eligible_inventory,
            &plan.selection,
            [1200, 800],
            32 * 1024 * 1024,
        )?;
        let mut footprint: Option<Region> = None;
        let mut visible: Option<Region> = None;
        for mask in &plan.masks.coverages {
            let part = plan.eligible_inventory[mask.inventory_index]
                .region
                .intersection(&viewport);
            footprint = Some(footprint.map_or_else(|| part.clone(), |r| r.union(&part)));
            visible =
                Some(visible.map_or_else(|| mask.visible.clone(), |r| r.union(&mask.visible)));
        }
        let error = (footprint.as_ref().map_or(0., Region::area)
            - visible.as_ref().map_or(0., Region::area))
        .abs();
        ensure!(error < 0.1, "Obscuring masks lost covered pixels: {error}");
        let union_mask = ferrite_kernel::coverage_raster::rasterize(
            footprint.as_ref().context("Missing selected footprints")?,
            [1200, 800],
            1200 * 800,
        )?;
        let mut lost = 0;
        let mut excess = 0;
        for y in 0..800 {
            for x in 0..1200 {
                let expected = union_mask.contains_pixel(x, y);
                let visible = pixel_masks.iter().any(|m| m.mask.contains_pixel(x, y));
                lost += usize::from(expected && !visible);
                excess += usize::from(!expected && visible);
            }
        }
        ensure!(
            lost == 0 && excess == 0,
            "Pixel coverage changed: lost {lost}, excess {excess}"
        );
        let pixel_bytes = pixel_masks
            .iter()
            .map(|m| m.mask.pixels().len())
            .sum::<usize>();

        let selected=plan.selection.coverages.iter().map(|s| {
            let c=&inventory[s.inventory_index];
            Ok(serde_json::json!({"dataset":c.dataset_id,"coverage":c.coverage_id,"selection_band":s.selection_band,"gap_fill":s.selected_to_fill_gap,"minimum_retention":plan.minimum_retention.contains(&s.inventory_index),"overscale":c.scales.overscale(denominator,s.selected_to_fill_gap)?}))
        }).collect::<Result<Vec<_>>>()?;
        cases.push(serde_json::json!({"pixel_mask_bytes":pixel_bytes,"lost_pixels":lost,"excess_pixels":excess,"denominator":denominator,"display_band":plan.selection.display_band,"selected":selected,"uncovered_pixels_squared":plan.selection.uncovered.area(),"visible_pixels_squared":visible.as_ref().map_or(0.,Region::area),"mask_conservation_error_pixels_squared":error}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"projection":"WGS84 ellipsoidal Mercator, affine device 1200x800 fixture","application_rendering_verified":false,"current_coverage_model_only":true,"datasets":rows,"cases":cases})
        )?
    );
    Ok(())
}
