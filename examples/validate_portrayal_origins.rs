//! Official PC output through the S-101 adapter, including source-origin cache
//! roundtrip checks. This does not verify view projection or coverage rendering.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_lua::{ContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{
    DrawingInstruction, PointOriginCrs, PointOriginGeometry, PortrayalOrigin, RenderContext,
    Viewport,
};
use ferrite_s100_core::{S101Cell, SpatialPrimitiveType};
use std::collections::BTreeMap;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let root = args.first().context("Pass chart directory")?;
    let fc = FeatureCatalogue::load(
        args.get(1)
            .map(String::as_str)
            .unwrap_or("Catalogues/FC/S-101/101_Feature_Catalogue_2.0.0.xml"),
    )?;
    let pc = PortrayalCatalogue::load(
        args.get(2)
            .map(String::as_str)
            .unwrap_or("Catalogues/PC/S-101"),
    )?;
    let parameters = ContextParameters::from_pc_context(pc.get_context_parameters());
    let mut engine = PortrayalEngine::new(pc.root_path.join("Rules"))?;
    engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(&fc));
    engine.initialize()?;
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.with_context(|| format!("Scanning chart directory {root}"))?;
        // macOS archive metadata has the same .000 suffix as its source cell,
        // but contains an AppleDouble header rather than ISO 8211 records.
        if entry.file_type().is_file()
            && !entry.file_name().to_string_lossy().starts_with("._")
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "000")
        {
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    ensure!(!paths.is_empty(), "No cells found");
    let mut rows = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let cell = S101Cell::load(path)
            .with_context(|| format!("Loading S-101 cell {}", path.display()))?;
        let portrayal = PortrayalContext::from_cell(&cell, parameters.clone());
        let data = portrayal.cell_data();
        let results = engine.process_cell(&data.read().unwrap(), parameters.clone())?;
        let mut context = RenderContext::new(Viewport::new(1200., 800.));
        ferrite_s101::convert_lua_results_for_cell(
            &results,
            &cell,
            &pc,
            &mut context,
            index,
            "Day",
        )?;
        let mut counts = BTreeMap::<String, usize>::new();
        for instruction in context.get_sorted_instructions() {
            let label = match instruction.portrayal_origin() {
                PortrayalOrigin::CoverageExempt => {
                    anyhow::bail!("Product adapter produced a host overlay")
                }
                PortrayalOrigin::Unspecified => anyhow::bail!(
                    "Unspecified origin: {} {:?}",
                    path.display(),
                    instruction.feature_id()
                ),
                PortrayalOrigin::NonPoint => "nonpoint",
                PortrayalOrigin::Point(origin) => match &**origin {
                    PointOriginGeometry::FeaturePoint(point) => {
                        let feature = cell
                            .features
                            .get(
                                &instruction
                                    .feature_id()
                                    .context("Missing feature identity")?,
                            )
                            .context("Missing source feature")?;
                        ensure!(
                            feature.primitive_type == SpatialPrimitiveType::Point,
                            "Non-point feature labelled point"
                        );
                        ensure!(
                            feature
                                .spatial_associations
                                .iter()
                                .filter_map(|association| cell
                                    .points
                                    .get(&association.spatial_id.key()))
                                .any(|source| source.position.x == point.x
                                    && source.position.y == point.y),
                            "Source point replaced by portrayal position"
                        );
                        "feature-point"
                    }
                    PointOriginGeometry::AugmentedLocalPoint {
                        reference_point, ..
                    } => {
                        let feature = cell
                            .features
                            .get(&instruction.feature_id().context("Missing feature ID")?)
                            .context("Unknown feature for local origin")?;
                        ensure!(
                            feature.primitive_type
                                == ferrite_s100_core::SpatialPrimitiveType::Point,
                            "Local augmented origin is not a point feature"
                        );
                        ensure!(
                            feature
                                .spatial_associations
                                .iter()
                                .filter_map(|a| cell.points.get(&a.spatial_id.key()))
                                .any(|p| p.position.x == reference_point.x
                                    && p.position.y == reference_point.y),
                            "Local reference point lost original geometry"
                        );
                        "augmented-local-anchored"
                    }
                    PointOriginGeometry::AugmentedPoint {
                        crs: PointOriginCrs::Geographic,
                        ..
                    } => "augmented-geographic",
                    PointOriginGeometry::AugmentedPoint {
                        crs: PointOriginCrs::Local,
                        ..
                    } => "augmented-local",
                    PointOriginGeometry::AugmentedPoint {
                        crs: PointOriginCrs::Portrayal,
                        ..
                    } => "augmented-portrayal",
                },
            };
            *counts.entry(label.into()).or_default() += 1;
        }
        let bytes = bincode::serialize(context.raw_instructions())?;
        let restored: Vec<DrawingInstruction> = bincode::deserialize(&bytes)?;
        ensure!(
            restored.len() == context.instruction_count(),
            "Cache lost instructions"
        );
        ensure!(
            restored
                .iter()
                .zip(context.raw_instructions())
                .all(|(a, b)| a.portrayal_origin() == b.portrayal_origin()),
            "Cache changed source origins"
        );
        // Exercise the actual adapter using real point/curve feature references.
        let group = pc
            .viewing_groups
            .groups
            .values()
            .filter_map(|group| group.catalogue_id.parse::<u32>().ok())
            .min()
            .context("No numeric group")?;
        let mut fixture_count = 0;
        for primitive in [SpatialPrimitiveType::Point, SpatialPrimitiveType::Curve] {
            if let Some((&key, feature)) = cell
                .features
                .iter()
                .filter(|(_, feature)| feature.primitive_type == primitive)
                .min_by_key(|(key, _)| *key)
            {
                let commands = if primitive == SpatialPrimitiveType::Point {
                    format!("ViewingGroup:{group};AugmentedRay:GeographicCRS,90,LocalCRS,5;LineStyle:origin,,0.32,CHBLK;LineInstruction:origin")
                } else {
                    format!("ViewingGroup:{group};PointInstruction:WRECKS01")
                };
                let result = ferrite_lua::PortrayalResult::parse(&key.to_string(), &commands, "")?;
                let mut fixture = RenderContext::new(Viewport::new(1200., 800.));
                ferrite_s101::convert_lua_results_for_cell(
                    &[result],
                    &cell,
                    &pc,
                    &mut fixture,
                    index,
                    "Day",
                )?;
                ensure!(
                    fixture.instruction_count() > 0,
                    "Source fixture produced no instructions: {:?}",
                    feature.primitive_type
                );
                for instruction in fixture.raw_instructions() {
                    ensure!(
                        matches!(instruction.portrayal_origin(), PortrayalOrigin::Point(_))
                            == (primitive == SpatialPrimitiveType::Point),
                        "Rendered primitive type confused with source geometry"
                    );
                }
                fixture_count += 1;
            }
        }
        rows.push(serde_json::json!({"cell":path.file_name().unwrap().to_string_lossy(),"features":cell.features.len(),"instructions":context.instruction_count(),"origin_counts":counts,"binary_cache_bytes":bytes.len(),"source_geometry_fixtures":fixture_count}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"cells":rows,"all_emitted_origins_assigned":true,"binary_cache_origins_equal":true,"application_coverage_rendering_verified":false})
        )?
    );
    Ok(())
}
