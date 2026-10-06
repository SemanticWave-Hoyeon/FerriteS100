//! Delivered PC metadata audit plus a malformed-reference rejection oracle.
use anyhow::{Context, Result};
use ferrite_portrayal_catalog::PortrayalCatalogue;
use std::{collections::HashSet, path::PathBuf};
fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let root = PathBuf::from(args.get(1).context("catalogue root")?);
    let out = PathBuf::from(args.get(2).context("JSON output")?);
    let mut rows = Vec::new();
    for version in ["1.0.2", "1.1.0", "2.0.0", "2.1.0"] {
        let pc = PortrayalCatalogue::load(root.join(version).join("PC"))?;
        anyhow::ensure!(
            !pc.viewing_groups.groups.contains_key(&0),
            "Foundation reference misread as group definition"
        );
        let mut selections = serde_json::Map::new();
        for preset in [
            ferrite_s101::DisplayPreset::Base,
            ferrite_s101::DisplayPreset::Standard,
            ferrite_s101::DisplayPreset::Other,
        ] {
            let mode = ferrite_s101::resolve_display_mode(&pc, preset)?;
            let mut enabled = ferrite_s101::viewing_groups_for_preset(&pc, preset)?
                .into_iter()
                .collect::<Vec<_>>();
            enabled.sort_unstable();
            selections.insert(
                format!("{preset:?}"),
                serde_json::json!({"id":mode.id,"enabled_groups":enabled}),
            );
        }
        let mut groups = pc.foundation_mode.clone();
        groups.sort_unstable();
        anyhow::ensure!(!groups.is_empty(), "Delivered S-101 foundation is empty");
        for preset in [
            ferrite_s101::DisplayPreset::Base,
            ferrite_s101::DisplayPreset::Standard,
            ferrite_s101::DisplayPreset::Other,
        ] {
            let mode = &ferrite_s101::resolve_display_mode(&pc, preset)?.id;
            anyhow::ensure!(
                groups.iter().all(|g| pc.is_viewing_group_visible(*g, mode)),
                "Foundation hidden by {mode}"
            );
        }
        let base = ferrite_s101::resolve_display_mode(&pc, ferrite_s101::DisplayPreset::Base)?;
        let base_groups = base
            .viewing_group_layers
            .iter()
            .flat_map(|l| pc.viewing_group_layers.get_viewing_groups_for_layer(l))
            .collect::<HashSet<_>>();
        rows.push(serde_json::json!({"version":version,"foundation_groups":groups,"foundation_missing_from_base_layers":groups.iter().filter(|g|!base_groups.contains(g)).copied().collect::<Vec<_>>(),"parsed_group_count":pc.viewing_groups.groups.len(),"group_zero_absent":true,"presets":selections}));
    }
    let s102 = PortrayalCatalogue::load(args.get(3).context("S102 PC")?)?;
    anyhow::ensure!(
        s102.foundation_mode == [13030] && !s102.viewing_groups.groups.contains_key(&0),
        "S102 foundation mismatch"
    );
    rows.push(serde_json::json!({"product":"S-102","version":s102.version,"foundation_groups":s102.foundation_mode,"group_zero_absent":true}));
    let fixture = out.with_extension("fixtures");
    std::fs::create_dir_all(&fixture)?;
    let header = r#"<portrayalCatalog productId="S-101" version="2.0.0"><viewingGroups><viewingGroup id="1"><description><name>Foundation</name></description></viewingGroup></viewingGroups>"#;
    let suffix = "</portrayalCatalog>";
    for part in ["","<foundationMode/><foundationMode/>","<foundationMode><viewingGroup>2</viewingGroup></foundationMode>","<foundationMode><viewingGroup>0</viewingGroup></foundationMode>","<foundationMode><viewingGroup>1</viewingGroup><viewingGroup>1</viewingGroup></foundationMode>","<foundationMode><viewingGroup>abc</viewingGroup></foundationMode>","<context><foundationMode><viewingGroup>1</viewingGroup></foundationMode></context>"] {
        std::fs::write(fixture.join("portrayal_catalogue.xml"),format!("{header}{part}{suffix}"))?;
        anyhow::ensure!(PortrayalCatalogue::load(&fixture).is_err(),"Malformed foundation accepted: {part}");
    }
    std::fs::write(
        fixture.join("portrayal_catalogue.xml"),
        format!("{header}<foundationMode><viewingGroup>1</viewingGroup></foundationMode>{suffix}"),
    )?;
    let pc = PortrayalCatalogue::load(&fixture)?;
    anyhow::ensure!(
        pc.foundation_mode == [1]
            && pc.viewing_groups.groups.len() == 1
            && pc.is_viewing_group_visible(1, "absent"),
        "Foundation reference handling failed"
    );
    std::fs::write(
        fixture.join("portrayal_catalogue.xml"),
        format!("{header}<foundationMode/>{suffix}"),
    )?;
    anyhow::ensure!(
        PortrayalCatalogue::load(&fixture)?
            .foundation_mode
            .is_empty(),
        "Empty foundation is permitted"
    );
    rows.push(serde_json::json!({"actual_loader_negative_cases":7,"valid_reference_case":true,"valid_empty_case":true}));
    std::fs::write(out, serde_json::to_vec_pretty(&rows)?)?;
    Ok(())
}
