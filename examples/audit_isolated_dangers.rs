//! Trace actual ISODGR01 commands to producer attributes and official PC conditions.
use anyhow::{ensure, Context, Result};
use ferrite_render::DrawingInstruction;
use ferrite_s100_core::{FeatureRecord, S101Cell};
use std::path::PathBuf;
fn number(feature: &FeatureRecord, code: &str) -> Result<Option<f64>> {
    let mut values = feature
        .attributes
        .iter()
        .filter(|a| a.paix == 0 && a.code.as_deref() == Some(code));
    let Some(value) = values.next() else {
        return Ok(None);
    };
    ensure!(values.next().is_none(), "Repeated {code}");
    if value.atvl.is_empty() {
        return Ok(None);
    }
    let value = value
        .atvl
        .parse::<f64>()
        .with_context(|| format!("Invalid {code}"))?;
    ensure!(value.is_finite(), "Non-finite {code}");
    Ok(Some(value))
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let root = PathBuf::from(
        args.first()
            .context("Pass actual application portrayal audit directory")?,
    );
    let safety = args
        .get(1)
        .context("Pass capture SafetyContour")?
        .parse::<f64>()?;
    let shallow = args
        .get(2)
        .context("Pass capture ShallowWaterDangers true/false")?
        .parse::<bool>()?;
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("snapshot.json"))?)?;
    let cells = snapshot["source_cells"]
        .as_array()
        .context("Missing source_cells")?
        .iter()
        .map(|p| {
            S101Cell::load(p.as_str().context("Invalid source path")?).map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let commands: Vec<DrawingInstruction> =
        bincode::deserialize(&std::fs::read(root.join("instructions.bin"))?)?;
    let mut rows = Vec::new();
    let mut mismatches = 0;
    for (ordinal, command) in commands.iter().enumerate() {
        let DrawingInstruction::Point(point) = command else {
            continue;
        };
        if point.symbol_ref != "ISODGR01" {
            continue;
        }
        let cell = point.cell_index.context("Missing symbol source dataset")? as usize;
        let id = point.feature_id.context("Missing symbol source feature")?;
        let feature = cells
            .get(cell)
            .context("Unknown source dataset")?
            .features
            .get(&id)
            .context("Unknown source feature")?;
        let kind = feature
            .feature_code
            .as_deref()
            .context("Missing source type")?;
        let sounding = number(feature, "valueOfSounding")?;
        let clearance = number(feature, "defaultClearanceDepth")?;
        let surrounding = number(feature, "surroundingDepth")?;
        let water = number(feature, "waterLevelEffect")?;
        let expected = if matches!(kind, "Wreck" | "Obstruction" | "UnderwaterAwashRock") {
            let depth = sounding
                .or(clearance)
                .context("ISODGR01 source lacks both depth attributes")?;
            let depth_condition = depth <= safety;
            let water_condition = !matches!(water, Some(1.) | Some(2.));
            let surrounding_condition =
                surrounding.is_none_or(|s| s >= safety || (shallow && s >= 0. && s < safety));
            Some(depth_condition && water_condition && surrounding_condition)
        } else {
            None
        };
        mismatches += usize::from(expected == Some(false));
        rows.push(serde_json::json!({"ordinal":ordinal,"cell":cell,"feature_id":id,"feature_type":kind,"symbol":"ISODGR01","valueOfSounding":sounding,"defaultClearanceDepth":clearance,"surroundingDepth":surrounding,"waterLevelEffect":water,"matches_official_udwhaz05_conditions":expected}));
    }
    ensure!(!rows.is_empty(), "No actual ISODGR01 commands");
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"capture_safety_contour":safety,"capture_shallow_water_dangers":shallow,"commands":rows,"condition_mismatches":mismatches,"source_attributes_modified":false,"all_observed_feature_types_audited":rows.iter().all(|r|r["matches_official_udwhaz05_conditions"].is_boolean())})
        )?
    );
    ensure!(
        mismatches == 0,
        "ISODGR01 command contradicts producer depth conditions"
    );
    Ok(())
}
