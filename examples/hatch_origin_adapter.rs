//! Official Lua command parser -> actual S101 surface adapter metadata proof.
//! This executable does not claim renderer/native GPU origin resolution.
use ferrite_render::{DrawingInstruction, PatternCrs, RenderContext, Viewport};
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 4,
        "usage: hatch_origin_adapter CELL PC OUTPUT_JSON"
    );
    let cell = ferrite_s100_core::S101Cell::load(&args[1])?;
    let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&args[2])?;
    let id = *cell
        .features
        .iter()
        .find(|(_, f)| {
            f.spatial_associations
                .iter()
                .any(|s| cell.surfaces.contains_key(&s.spatial_id.key()))
        })
        .ok_or_else(|| anyhow::anyhow!("No surface feature"))?
        .0;
    let mut line_refs: Vec<_> = pc.line_styles.keys().map(|s| s.to_string()).collect();
    line_refs.sort();
    anyhow::ensure!(line_refs.len() >= 2, "Need two catalogue line styles");
    let mut fills: Vec<_> = pc.area_fills.keys().map(|s| s.to_string()).collect();
    fills.sort();
    anyhow::ensure!(!fills.is_empty(), "Need catalogue fill");
    let mut cases = Vec::new();
    for (name, crs) in [
        ("Global", PatternCrs::Global),
        ("LocalGeometry", PatternCrs::LocalGeometry),
        ("GlobalGeometry", PatternCrs::GlobalGeometry),
    ] {
        for command in [
            format!("AreaFillReference:{}", fills[0]),
            format!("PixmapFill:{}", fills[0]),
            "SymbolFill:ISODGR01,2,0,1,2,true".into(),
            format!("HatchFill:1,1,0.125,{},{}", line_refs[0], line_refs[1]),
        ] {
            let result = ferrite_lua::PortrayalResult::parse(
                &id.to_string(),
                &format!("AreaCRS:{name};{command}"),
                "",
            )?;
            let mut ctx = RenderContext::new(Viewport::new(240., 180.));
            ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut ctx, 0, "Day")?;
            let areas: Vec<_> = ctx
                .raw_instructions()
                .iter()
                .filter_map(|i| {
                    if let DrawingInstruction::Area(a) = i {
                        Some(a)
                    } else {
                        None
                    }
                })
                .collect();
            anyhow::ensure!(!areas.is_empty(), "Command did not produce area: {command}");
            for a in &areas {
                anyhow::ensure!(a.pattern_crs == crs, "AreaCRS lost for {command}");
                if command.starts_with("HatchFill") {
                    anyhow::ensure!(
                        &*a.hatch_line_style_refs == &line_refs[..2],
                        "Second line reference lost"
                    );
                    anyhow::ensure!(a.hatch_strokes.len() >= 2, "Second resolved stroke lost");
                    let ferrite_render::AreaFillType::HatchFill { spacing, angle, .. } = a.fill
                    else {
                        anyhow::bail!("Not hatch")
                    };
                    anyhow::ensure!(spacing == 0.125 && angle == 45., "Hatch dimensions altered");
                }
            }
            cases.push(serde_json::json!({"area_crs":name,"command":command,"areas":areas.len()}));
        }
    }
    let mut invalid = 0;
    for tail in [
        "AreaCRS:Unknown;HatchFill:1,0,1,CSTLN",
        "HatchFill:0,0,1,CSTLN",
        "HatchFill:1,0,0,CSTLN",
        "HatchFill:1,0,1,A,B,C",
    ] {
        let result = ferrite_lua::PortrayalResult::parse(
            &id.to_string(),
            &format!("ColorFill:DEPIT;{tail}"),
            "",
        )?;
        let mut ctx = RenderContext::new(Viewport::new(240., 180.));
        anyhow::ensure!(
            ferrite_s101::convert_lua_results_for_cell(&[result], &cell, &pc, &mut ctx, 0, "Day")
                .is_err(),
            "Invalid hatch accepted: {tail}"
        );
        anyhow::ensure!(
            ctx.instruction_count() == 0,
            "Invalid hatch partially mutated context"
        );
        invalid += 1;
    }
    std::fs::write(
        &args[3],
        serde_json::to_vec_pretty(
            &serde_json::json!({"cases":cases,"invalid_cases_rejected_before_mutation":invalid,"native_gpu":false}),
        )?,
    )?;
    Ok(())
}
