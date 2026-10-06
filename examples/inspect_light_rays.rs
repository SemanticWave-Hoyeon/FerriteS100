//! Identify source features and official PC-generated light-sector rays.
use anyhow::Result;
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{
    ContextParameters, DrawingCommand, PortrayalContext, PortrayalEngine, TypeCatalogue,
};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_s100_core::S101Cell;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let fc = FeatureCatalogue::load(&args[2])?;
    let pc = PortrayalCatalogue::load(&args[3])?;
    let mut params = ContextParameters::from_pc_context(pc.get_context_parameters());
    if args.iter().any(|a| a == "--full-sectors") {
        params.full_sectors = true;
        ferrite_s101::synchronize_legacy_context(&pc, &mut params);
    }
    let mut engine = PortrayalEngine::new(pc.root_path.join("Rules"))?;
    engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(&fc));
    engine.initialize()?;
    let mut paths: Vec<_> = walkdir::WalkDir::new(&args[1])
        .into_iter()
        .filter_map(|x| x.ok())
        .filter(|x| x.file_type().is_file())
        .map(|x| x.into_path())
        .filter(|x| x.extension().is_some_and(|e| e == "000"))
        .collect();
    paths.sort();
    let mut rows = Vec::new();
    let mut long_lines = Vec::new();
    let mut paths_rendered = Vec::new();
    for path in paths {
        let mut cell = S101Cell::load(&path)?;
        cell.normalize_feature_codes(&fc.feature_type_codes());
        let portrayal = PortrayalContext::from_cell(&cell, params.clone());
        let data = portrayal.cell_data();
        let results = engine.process_cell(&data.read().unwrap(), params.clone())?;
        let mut render =
            ferrite_render::RenderContext::new(ferrite_render::Viewport::new(3420.0, 2082.0));
        ferrite_s101::convert_lua_results_for_cell(&results, &cell, &pc, &mut render, 0, "Day")
            .unwrap();
        for inst in render.raw_instructions() {
            if let ferrite_render::DrawingInstruction::Line(line) = inst {
                if let Some(p) = &line.portrayal_path {
                    paths_rendered.push(serde_json::json!({"source":path,"id":line.feature_id,"origin":line.points.first(),"path":p,"width":line.style.width,"color":line.color_token}));
                }
                if line.screen_ray.is_none() {
                    for pair in line.points.windows(2) {
                        let dx = pair[1].x - pair[0].x;
                        let dy = pair[1].y - pair[0].y;
                        if dx.hypot(dy) > 0.01
                            && pair
                                .iter()
                                .any(|p| p.x > -2.2 && p.x < -1.9 && p.y > 48.55 && p.y < 48.75)
                        {
                            long_lines.push(serde_json::json!({"source":path,"id":line.feature_id,"feature":line.feature_id.and_then(|id|data.read().unwrap().features.get(&id).map(|f|f.code.clone())),"color":line.color_token,"width":line.style.width,"dash":line.style.dash_pattern,"points":pair}));
                        }
                    }
                }
            }
        }
        let guard = data.read().unwrap();
        for result in results {
            let f = result
                .feature_id
                .parse::<i64>()
                .ok()
                .and_then(|id| guard.features.get(&id));
            for p in result.instructions {
                for cmd in p.commands {
                    if let DrawingCommand::LineInstruction {
                        augmented_ray: Some(ray),
                        simple_style,
                        style_refs,
                        ..
                    }
                    | DrawingCommand::LineInstructionUnsuppressed {
                        augmented_ray: Some(ray),
                        simple_style,
                        style_refs,
                        ..
                    } = cmd
                    {
                        rows.push(serde_json::json!({"source":path,"id":result.feature_id,"feature":f.map(|f|&f.code),"direction_crs":ray.direction_crs,"direction":ray.direction,"length_crs":ray.length_crs,"length":ray.length,"inline_style":simple_style,"style_refs":style_refs}));
                    }
                }
            }
        }
    }
    std::fs::write(
        &args[4],
        serde_json::to_vec_pretty(
            &serde_json::json!({"rays":rows,"long_lines":long_lines,"portrayal_paths":paths_rendered,"effective_full_sectors":params.full_sectors,"source_files_not_modified":true,"navigation_certification":false}),
        )?,
    )?;
    Ok(())
}
