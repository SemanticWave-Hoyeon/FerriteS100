//! Apply the locally authored IC fixture to authenticated real S-101/S-102 inputs.
//! This validates adapters, not operational catalogue trust or full S-98 conformance.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_interoperability::Catalogue;
use ferrite_lua::{ContextParameters, PortrayalContext, PortrayalEngine, TypeCatalogue};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use ferrite_render::{GeoBounds, RenderContext, Viewport};
use ferrite_s100_core::S101Cell;
use ferrite_security::{authorize_datasets, TrustAnchors, UnsignedPolicy};
use std::path::{Path, PathBuf};
fn paths(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut paths: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && !e.file_name().as_encoded_bytes().starts_with(b"._")
                && e.path()
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case(extension))
        })
        .map(|e| e.into_path())
        .collect();
    paths.sort();
    paths
}
fn without_composition(i: &ferrite_render::DrawingInstruction) -> serde_json::Value {
    let mut value = serde_json::to_value(i).unwrap();
    for fields in value.as_object_mut().unwrap().values_mut() {
        let fields = fields.as_object_mut().unwrap();
        for field in [
            "display_plane",
            "priority",
            "viewing_group",
            "additional_viewing_groups",
        ] {
            fields.remove(field);
        }
    }
    value
}
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 4,
        "Usage: validate_interoperability_data CHART_ROOT FC PC S102_ROOT"
    );
    let fc = FeatureCatalogue::load(&args[1])?;
    let mut feature_use_counts = std::collections::BTreeMap::<String, usize>::new();
    for definition in fc.feature_types.values() {
        let kind = definition
            .feature_use_type
            .with_context(|| format!("Missing official FC classification: {}", definition.code))?;
        *feature_use_counts.entry(kind.as_str().into()).or_default() += 1;
    }
    let pc = PortrayalCatalogue::load(&args[2])?;
    ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
    let catalogue = Catalogue::parse(include_str!(
        "../crates/ferrite-interoperability/tests/display-plane.xml"
    ))?;
    let charts = paths(Path::new(&args[0]), "000");
    let coverages = paths(Path::new(&args[3]), "h5");
    ensure!(
        !charts.is_empty() && !coverages.is_empty(),
        "Missing real-data fixtures"
    );
    let mut anchors = TrustAnchors::default();
    anchors.install_pem("IHO", &std::fs::read("Trust/IHO-S100-5.2.pem")?)?;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let authorization = authorize_datasets(
        &charts.iter().chain(&coverages).cloned().collect::<Vec<_>>(),
        &anchors,
        time,
        UnsignedPolicy::Reject,
    )?;
    let mut engine = PortrayalEngine::new(pc.root_path.join("Rules"))?;
    engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(&fc));
    engine.initialize()?;
    let parameters = ContextParameters::from_pc_context(pc.get_context_parameters());
    let mut rows = Vec::new();
    let mut changed = 0;
    for (cell_index, path) in charts.iter().enumerate() {
        let snapshot = authorization
            .snapshots
            .get(&path.canonicalize()?)
            .and_then(Option::as_ref)
            .context("Authenticated snapshot missing")?;
        let mut cell = S101Cell::load_from(path, snapshot.path())?;
        ferrite_s101::validate_dataset_catalogues(&cell.dsid, &fc, &pc.product_id, &pc.version)?;
        let mut incompatible_fc = fc.clone();
        incompatible_fc.version = "1.1.0".into();
        ensure!(
            ferrite_s101::validate_dataset_catalogues(
                &cell.dsid,
                &incompatible_fc,
                &pc.product_id,
                &pc.version
            )
            .is_err(),
            "Different-edition FC was accepted"
        );
        let mut future_dataset = cell.dsid.clone();
        future_dataset.product_edition = "2.1".into();
        future_dataset.product_identifier = "INT.IHO.S-101.2.1".into();
        ensure!(
            ferrite_s101::validate_dataset_catalogues(
                &future_dataset,
                &fc,
                &pc.product_id,
                &pc.version
            )
            .is_err(),
            "Older catalogue accepted a newer dataset revision"
        );
        cell.normalize_feature_codes(&fc.feature_type_codes());
        let portrayal = PortrayalContext::from_cell(&cell, parameters.clone());
        let data = portrayal.cell_data();
        let results = engine.process_cell(&data.read().unwrap(), parameters.clone())?;
        let mut context = RenderContext::new(Viewport::new(1200., 800.));
        context.set_bounds(GeoBounds::new(-180., -90., 180., 90.));
        ferrite_s101::convert_lua_results_for_cell(
            &results,
            &cell,
            &pc,
            &mut context,
            cell_index,
            "Day",
        )?;
        // Synthetic point probes use authenticated real meta-feature identities.
        // An IC rule must not change any field or evaluate a geographic-only filter on them.
        let mut meta_by_type = std::collections::BTreeMap::<String, Vec<i64>>::new();
        for (&id, feature) in &cell.features {
            if let Some(code) = feature.feature_code.as_deref() {
                if fc.feature_types.get(code).and_then(|f| f.feature_use_type)
                    == Some(ferrite_feature_catalog::FeatureUseType::Meta)
                {
                    meta_by_type.entry(code.into()).or_default().push(id);
                }
            }
        }
        let mut meta_probes = 0;
        for (code, ids) in &meta_by_type {
            let xml = include_str!("../crates/ferrite-interoperability/tests/display-plane.xml")
                .replace(
                    "<featureCode>Wreck</featureCode>",
                    &format!("<featureCode>{code}</featureCode>"),
                );
            let targeted = Catalogue::parse(&xml)?;
            let mut probes: Vec<_> = ids
                .iter()
                .map(|&id| {
                    ferrite_render::DrawingInstruction::Point(
                        ferrite_render::PointInstruction::new(
                            "META_PC_PROBE".into(),
                            ferrite_render::WorldPoint::new(0., 0.),
                        )
                        .with_cell_index(cell_index)
                        .with_feature_id(id),
                    )
                })
                .collect();
            let before = serde_json::to_value(&probes)?;
            let plan = ferrite_s101::plan_interoperability(
                &targeted,
                &cell,
                &fc,
                &probes,
                cell_index as u32,
            )?;
            ensure!(plan.is_empty(), "IC changed meta-feature {code}");
            plan.apply(&mut probes)?;
            ensure!(
                serde_json::to_value(&probes)? == before,
                "Meta PC probe fields changed"
            );
            meta_probes += probes.len();
        }
        ensure!(
            meta_probes > 0,
            "Real fixture contains no classified meta features"
        );
        let mut instructions = context.raw_instructions().to_vec();
        let original = instructions.clone();
        let plan = ferrite_s101::plan_interoperability(
            &catalogue,
            &cell,
            &fc,
            &instructions,
            cell_index as u32,
        )?;
        plan.apply(&mut instructions)?;
        for (before, after) in original.iter().zip(&instructions) {
            ensure!(
                without_composition(before) == without_composition(after),
                "IC changed symbolization/geometry/visibility/source identity"
            );
        }
        let second = ferrite_s101::plan_interoperability(
            &catalogue,
            &cell,
            &fc,
            &instructions,
            cell_index as u32,
        )?;
        ensure!(
            plan.len() == second.len(),
            "Non-idempotent composition selection"
        );
        second.apply(&mut instructions)?;
        changed += plan.len();
        rows.push(serde_json::json!({"source":path,"dataset_product":cell.dsid.product_identifier,"dataset_specification_edition":cell.dsid.product_edition,"catalogue_compatibility_verified":true,"different_edition_and_newer_dataset_rejected":true,"features":cell.features.len(),"instructions":instructions.len(),"assigned_instructions":plan.len(),"non_composition_fields_preserved":true,"meta_feature_probes_preserved":meta_probes,"meta_feature_types":meta_by_type.keys().collect::<Vec<_>>()}));
    }
    ensure!(
        changed > 0,
        "No real S-101 feature matched the local IC fixture"
    );
    let mut surfaces = Vec::new();
    for path in &coverages {
        let snapshot = authorization
            .snapshots
            .get(&path.canonicalize()?)
            .and_then(Option::as_ref)
            .context("Authenticated snapshot missing")?;
        for c in ferrite_s102::BathymetryCoverage::open(snapshot.path())? {
            let assignment = c
                .interoperability_assignment(&catalogue)?
                .context("S-102 rule missing")?;
            ensure!(
                assignment.plane.order.get() == -500
                    && assignment.priority == 3
                    && assignment.viewing_group == 11010,
                "Wrong S-102 composition assignment"
            );
            surfaces.push(serde_json::json!({"source":path,"instance":c.instance_name,"product_specification":c.product_specification,"plane":assignment.plane,"priority":assignment.priority,"viewing_group":assignment.viewing_group}));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"feature_use_counts":feature_use_counts,"os":std::env::consts::OS,"signed_source_files":authorization.signed_count,"s101":rows,"s101_assigned_instructions":changed,"s102":surfaces,"ic_fixture_unsigned_local_test":true,"app_ic_activated":false,"full_s98_verified":false})
        )?
    );
    Ok(())
}
