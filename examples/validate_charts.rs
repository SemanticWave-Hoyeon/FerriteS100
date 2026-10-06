//! Load every base cell and execute the official Lua portrayal rules.
//! Usage: cargo run --example validate_charts -- ChartData/UKHO
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{ContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_s100_core::S101Cell;
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_writer(std::io::stderr)
        .init();
    let fc = FeatureCatalogue::load_bound(
        std::env::args()
            .nth(2)
            .unwrap_or_else(|| "Catalogues/FC/S-101/101_Feature_Catalogue_2.0.0.xml".into()),
    )?;
    let pc = PortrayalCatalogue::load_bound(
        std::env::args()
            .nth(3)
            .unwrap_or_else(|| "Catalogues/PC/S-101".into()),
    )?;
    let mut context = ContextParameters::from_pc_context(pc.get_context_parameters());
    if let Ok(v) = std::env::var("FERRITE_TEST_SHALLOW_DANGERS") {
        context.shallow_water_dangers = v.parse()?;
    }
    if let Ok(v) = std::env::var("FERRITE_TEST_SAFETY_CONTOUR") {
        context.safety_contour = v.parse()?;
    }
    let mut engine = PortrayalEngine::new_with_sources(pc.sources())?;
    engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(&fc));
    engine.initialize()?;
    let root = PathBuf::from(std::env::args().nth(1).context("Pass a chart directory")?);
    let mut paths: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "000")
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("._"))
        })
        .collect();
    paths.sort();
    ensure!(!paths.is_empty(), "No .000 cells found");
    let mut total_features = 0;
    let mut total_commands = 0;
    let mut rows = Vec::new();
    for path in paths {
        let start = Instant::now();
        let cell = S101Cell::load(&path).with_context(|| path.display().to_string())?;
        ensure!(
            !cell.features.is_empty(),
            "No features in {}",
            path.display()
        );
        let points: Vec<_> = cell
            .points
            .values()
            .map(|p| p.position)
            .chain(
                cell.multi_points
                    .values()
                    .flat_map(|p| p.positions.iter().copied()),
            )
            .chain(
                cell.curves
                    .values()
                    .flat_map(|c| c.positions_iter().copied()),
            )
            .collect();
        ensure!(!points.is_empty(), "No coordinates in {}", path.display());
        ensure!(
            points.iter().all(|p| p.x.is_finite()
                && p.y.is_finite()
                && p.x.abs() <= 180.0
                && p.y.abs() <= 90.0),
            "Invalid coordinates in {}",
            path.display()
        );
        let mut issues = Vec::new();
        let mut features: Vec<_> = cell.features.values().collect();
        features.sort_by_key(|f| f.frid.rcid);
        for feature in features {
            let code = feature.feature_code.as_deref().unwrap_or("UNMAPPED");
            if !fc.feature_types.contains_key(code) {
                issues.push(format!(
                    "feature {}: type {} absent from FC {}",
                    feature.frid.rcid, code, fc.version
                ));
            }
            for assoc in &feature.spatial_associations {
                let key = assoc.spatial_id.key();
                let exists = match assoc.spatial_id.rcnm {
                    110 => cell.points.contains_key(&key),
                    115 => cell.multi_points.contains_key(&key),
                    120 => cell.curves.contains_key(&key),
                    125 => cell.composite_curves.contains_key(&key),
                    130 => cell.surfaces.contains_key(&key),
                    _ => false,
                };
                if !exists {
                    issues.push(format!(
                        "feature {}: unresolved spatial {:?}",
                        feature.frid.rcid, assoc.spatial_id
                    ));
                }
            }
            for assoc in &feature.information_associations {
                if !cell.information.contains_key(&assoc.info_id.key()) {
                    issues.push(format!(
                        "feature {}: unresolved information {:?}",
                        feature.frid.rcid, assoc.info_id
                    ));
                }
            }
            for assoc in &feature.feature_associations {
                if !cell.features.contains_key(&assoc.feature_id.key()) {
                    issues.push(format!(
                        "feature {}: unresolved feature {:?}",
                        feature.frid.rcid, assoc.feature_id
                    ));
                }
            }
            for attr in &feature.attributes {
                let code = attr.code.as_deref().unwrap_or("UNMAPPED");
                if !fc.simple_attributes.contains_key(code)
                    && !fc.complex_attributes.contains_key(code)
                {
                    issues.push(format!(
                        "feature {}: attribute {} absent from FC {}",
                        feature.frid.rcid, code, fc.version
                    ));
                }
                if let Some(def) = fc.simple_attributes.get(code) {
                    if def.value_type == ferrite_feature_catalog::AttributeValueType::Enumeration
                        && !attr.atvl.is_empty()
                    {
                        if let Ok(value) = attr.atvl.parse::<u32>() {
                            if def.get_listed_value(value).is_none() {
                                issues.push(format!(
                                    "feature {}: {} enumeration {} absent from FC {}",
                                    feature.frid.rcid, code, value, fc.version
                                ));
                            }
                        }
                    }
                }
            }
        }
        let portrayal = PortrayalContext::from_cell(&cell, context.clone());
        let data = portrayal.cell_data();
        let results = engine.process_cell(&data.read().unwrap(), context.clone())?;
        let mut hazard_rows = Vec::new();
        let mut symbol_counts = std::collections::BTreeMap::<String, usize>::new();
        for result in &results {
            let id = result.feature_id.parse::<i64>().ok();
            let guard = data.read().unwrap();
            let feature = id.and_then(|id| guard.features.get(&id));
            let mut symbols = Vec::new();
            for command in result.instructions.iter().flat_map(|i| &i.commands) {
                if let ferrite_lua::DrawingCommand::PointInstruction { symbol_ref, .. } = command {
                    *symbol_counts.entry(symbol_ref.clone()).or_default() += 1;
                    symbols.push(symbol_ref.clone());
                }
            }
            if feature.is_some_and(|f| {
                matches!(
                    f.code.as_str(),
                    "Wreck" | "Obstruction" | "UnderwaterAwashRock"
                )
            }) {
                let feature = feature.unwrap();
                let attrs: std::collections::BTreeMap<_, _> = feature
                    .attributes
                    .iter()
                    .filter(|(k, _)| {
                        matches!(
                            k.as_str(),
                            "valueOfSounding"
                                | "defaultClearanceDepth"
                                | "surroundingDepth"
                                | "waterLevelEffect"
                                | "categoryOfWreck"
                        )
                    })
                    .map(|(k, v)| (k.clone(), format!("{:?}", v)))
                    .collect();
                hazard_rows.push(serde_json::json!({"id":result.feature_id,"code":feature.code,"attributes":attrs,"symbols":symbols}));
            }
        }
        let temporal_commands: usize = results
            .iter()
            .flat_map(|r| &r.instructions)
            .flat_map(|i| &i.commands)
            .filter_map(|c| c.visibility())
            .filter(|v| !v.time_intervals.is_empty())
            .count();
        let temporal_intervals: usize = results
            .iter()
            .flat_map(|r| &r.instructions)
            .flat_map(|i| &i.commands)
            .filter_map(|c| c.visibility())
            .map(|v| v.time_intervals.len())
            .sum();
        let mut render_context =
            ferrite_render::RenderContext::new(ferrite_render::Viewport::new(960., 640.));
        ferrite_s101::convert_lua_results_for_cell(
            &results,
            &cell,
            &pc,
            &mut render_context,
            0,
            "Day",
        )?;
        let temporal_primitives = render_context
            .raw_instructions()
            .iter()
            .filter(|i| !i.time_intervals().is_empty())
            .count();
        let mut date_snapshots = Vec::new();
        for date in ["2026-01-15", "2026-07-15", "2026-10-04"] {
            render_context.settings.current_date = Some(date.into());
            let (_, hidden, diagnostics) = render_context.date_visibility();
            if diagnostics > 0 && date == "2026-01-15" {
                for inst in render_context.raw_instructions() {
                    if let Err(error) = ferrite_kernel::date_intervals_visible(
                        inst.time_intervals(),
                        ferrite_kernel::parse_viewing_date(date)?,
                    ) {
                        eprintln!(
                            "Temporal diagnostic {} feature {:?}: {error}; {:?}",
                            path.display(),
                            inst.feature_id(),
                            inst.time_intervals()
                        );
                    }
                }
            }
            date_snapshots
                .push(serde_json::json!({"date":date,"hidden":hidden,"diagnostics":diagnostics}));
        }
        let commands: usize = results
            .iter()
            .flat_map(|r| &r.instructions)
            .map(|i| i.commands.len())
            .sum();
        ensure!(commands > 0, "No drawing commands for {}", path.display());
        total_features += cell.features.len();
        total_commands += commands;
        let row = serde_json::json!({"date_snapshots":date_snapshots,"temporal_commands":temporal_commands,"temporal_intervals":temporal_intervals,"temporal_primitives":temporal_primitives,"hazards":hazard_rows,"symbol_counts":symbol_counts,"safety_contour":context.safety_contour,"safety_depth":context.safety_depth,"shallow_water_dangers":context.shallow_water_dangers,"file":path,"audit_issues":issues,"dataset":cell.dsid.dataset_name,"title":cell.dsid.dataset_title,"product_edition":cell.dsid.product_edition,"features":cell.features.len(),"information":cell.information.len(),"coordinate_factors":[cell.coord_factor,cell.coord_factor_y,cell.coord_factor_z],"coordinate_origins":[cell.coord_origin_x,cell.coord_origin_y,cell.coord_origin_z],"points":points.len(),"min_lon":points.iter().map(|p|p.x).fold(f64::INFINITY,f64::min),"max_lon":points.iter().map(|p|p.x).fold(f64::NEG_INFINITY,f64::max),"min_lat":points.iter().map(|p|p.y).fold(f64::INFINITY,f64::min),"max_lat":points.iter().map(|p|p.y).fold(f64::NEG_INFINITY,f64::max),"portrayal_results":results.len(),"drawing_commands":commands,"elapsed_ms":start.elapsed().as_millis()});
        eprintln!(
            "{}: {} features, {} commands",
            path.display(),
            cell.features.len(),
            commands
        );
        rows.push(row);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"fc_version":fc.version,"pc_version":pc.version,"cells":rows.len(),"features":total_features,"drawing_commands":total_commands,"results":rows})
        )?
    );
    Ok(())
}
