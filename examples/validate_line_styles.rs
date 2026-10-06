//! Official catalogue definitions -> Lua command -> actual S101 surface adapter.
//! This is a CPU metadata gate, not a native GPU/OS gesture test.
use anyhow::{ensure, Context, Result};
use ferrite_portrayal_catalog::{LineStyle, PortrayalCatalogue};
use ferrite_render::{DrawingInstruction, RenderContext, Viewport};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: validate_line_styles CATALOGUE_ROOT CELL OUTPUT_JSON"
    );
    let root = PathBuf::from(&args[1]);
    let mut rows = Vec::new();
    for (version, count) in [("1.0.2", 64), ("1.1.0", 64), ("2.0.0", 59), ("2.1.0", 65)] {
        let pc = PortrayalCatalogue::load(root.join(version).join("PC"))?;
        ensure!(
            pc.line_styles.len() == count,
            "Wrong style count in {version}"
        );
        let mut parts = Vec::new();
        for (id, widths, offsets, colors) in [
            (
                "INDHLT02",
                vec![1.28, 0.64],
                vec![0., 0.],
                vec!["BKAJ1", "CHYLW"],
            ),
            (
                "SCLBDY51",
                vec![0.96, 0.32, 0.32],
                vec![-1., 1.74, 2.43],
                vec!["CHGRF"; 3],
            ),
        ] {
            let LineStyle::Composite(c) = pc.get_line_style(id).context("Missing composite")?
            else {
                anyhow::bail!("Composite collapsed in {version}: {id}");
            };
            ensure!(c.components.len() == widths.len(), "Lost composite part");
            for (((s, width), offset), color) in
                c.components.iter().zip(widths).zip(offsets).zip(colors)
            {
                ensure!(
                    s.pen.width == width && s.offset_mm == offset && s.pen.color_token == color,
                    "Altered official component in {version}/{id}"
                );
                if id == "SCLBDY51" {
                    ensure!(
                        s.interval_length == 6.1
                            && s.dashes.len() == 1
                            && s.dashes[0].start == 0.
                            && s.dashes[0].length == 6.1,
                        "Altered boundary repeat"
                    );
                }
                parts.push(
                    serde_json::json!({"id":id,"width_mm":width,"offset_mm":offset,"color":color}),
                );
            }
        }
        rows.push(serde_json::json!({"version":version,"styles":count,"components":parts}));
    }
    let pc = PortrayalCatalogue::load(root.join("2.0.0/PC"))?;
    let cell = ferrite_s100_core::S101Cell::load(&args[2])?;
    let id = *cell
        .features
        .iter()
        .find(|(_, f)| {
            f.spatial_associations
                .iter()
                .any(|s| cell.surfaces.contains_key(&s.spatial_id.key()))
        })
        .context("No actual surface feature")?
        .0;
    let result = ferrite_lua::PortrayalResult::parse(
        &id.to_string(),
        "AreaCRS:LocalGeometry;HatchFill:1,0,2,INDHLT02,SCLBDY51",
        "",
    )?;
    let mut ctx = RenderContext::new(Viewport::new(240., 180.));
    ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut ctx, 0, "Day")?;
    let mut areas = 0;
    for instruction in ctx.raw_instructions() {
        if let DrawingInstruction::Area(area) = instruction {
            ensure!(
                area.hatch_strokes.len() == 5,
                "Adapter lost an official composite component"
            );
            for (s, (width, offset, color)) in area.hatch_strokes.iter().zip([
                (1.28f32, 0., "BKAJ1"),
                (0.64, 0., "CHYLW"),
                (0.96, -1., "CHGRF"),
                (0.32, 1.74, "CHGRF"),
                (0.32, 2.43, "CHGRF"),
            ]) {
                ensure!(
                    s.style.width == width
                        && s.style.offset_mm == offset
                        && s.style.color_token.as_deref() == Some(color),
                    "Adapter changed stroke order/metadata"
                );
            }
            areas += 1;
        }
    }
    ensure!(areas > 0, "No area adapter output");
    let point_id = *cell
        .features
        .iter()
        .find(|(_, f)| {
            f.spatial_associations
                .iter()
                .any(|a| cell.points.contains_key(&a.spatial_id.key()))
        })
        .context("No actual point feature")?
        .0;
    let line_id = *cell
        .features
        .iter()
        .find(|(_, f)| {
            f.spatial_associations
                .iter()
                .any(|a| cell.curves.contains_key(&a.spatial_id.key()))
        })
        .context("No actual curve feature")?
        .0;
    let mut line_cases = Vec::new();
    for (kind, feature, prefix) in [
        ("source_curve", line_id, ""),
        (
            "screen_ray",
            point_id,
            "AugmentedRay:LocalCRS,90,LocalCRS,25;",
        ),
        (
            "local_path",
            point_id,
            "Polyline:0,0,10,0;AugmentedPath:LocalCRS,LocalCRS,LocalCRS;",
        ),
        (
            "geographic_path",
            point_id,
            "Polyline:-1,50,-1.01,50.01;AugmentedPath:GeographicCRS,GeographicCRS,GeographicCRS;",
        ),
    ] {
        for unsuppressed in [false, true] {
            let command = if unsuppressed {
                "LineInstructionUnsuppressed"
            } else {
                "LineInstruction"
            };
            let parsed = ferrite_lua::PortrayalResult::parse(
                &feature.to_string(),
                &format!("{prefix}{command}:INDHLT02,SCLBDY51"),
                "",
            )?;
            let mut context = RenderContext::new(Viewport::new(240., 180.));
            ferrite_s101::convert_lua_results_for_cell(
                &[parsed],
                &cell,
                &pc,
                &mut context,
                3,
                "Day",
            )?;
            let lines: Vec<_> = context
                .raw_instructions()
                .iter()
                .filter_map(|i| {
                    if let DrawingInstruction::Line(line) = i {
                        Some(line)
                    } else {
                        None
                    }
                })
                .collect();
            ensure!(
                !lines.is_empty() && lines.len() % 5 == 0,
                "Incomplete {kind} composite emission"
            );
            let expected = [
                (1.28f32, 0., "BKAJ1"),
                (0.64, 0., "CHYLW"),
                (0.96, -1., "CHGRF"),
                (0.32, 1.74, "CHGRF"),
                (0.32, 2.43, "CHGRF"),
            ];
            // Source geometry may have several curve pieces. Each parser command
            // preserves its own composite ordering: 2 highlight parts, then 3 boundary parts.
            let geometry_count = lines.len() / 5;
            for (ref_index, range) in [(0, 0..2), (1, 2..5)] {
                for geometry in 0..geometry_count {
                    let base = if ref_index == 0 {
                        0
                    } else {
                        geometry_count * 2
                    };
                    for (part, index) in range.clone().enumerate() {
                        let line = lines[base + part * geometry_count + geometry];
                        let (width, offset, color) = expected[index];
                        ensure!(
                            line.style.width == width
                                && line.style.offset_mm == offset
                                && line.color_token.as_deref() == Some(color),
                            "Changed {kind} composite style/order"
                        );
                        ensure!(
                            line.points == lines[base + geometry].points
                                && line.feature_id == Some(feature as i64)
                                && line.cell_index == Some(3)
                                && line.suppressible != unsuppressed,
                            "Changed {kind} geometry/identity"
                        );
                    }
                }
            }
            line_cases.push(serde_json::json!({"geometry":kind,"unsuppressed":unsuppressed,"lines":lines.len(),"strokes_per_geometry":5}));
        }
    }
    let missing = ferrite_lua::PortrayalResult::parse(
        &point_id.to_string(),
        "ColorFill:DEPIT;LineInstruction:MISSING_STYLE",
        "",
    )?;
    let mut empty = RenderContext::new(Viewport::new(240., 180.));
    ensure!(
        ferrite_s101::convert_lua_results_for_cell(&[missing], &cell, &pc, &mut empty, 0, "Day")
            .is_err()
            && empty.instruction_count() == 0,
        "Invalid style partially mutated context"
    );
    std::fs::write(
        &args[3],
        serde_json::to_vec_pretty(
            &serde_json::json!({"catalogues":rows,"adapter_areas":areas,"ordered_strokes_per_area":5,"line_cases":line_cases,"invalid_line_preflight_atomic":true,"native_gpu":false}),
        )?,
    )?;
    Ok(())
}
