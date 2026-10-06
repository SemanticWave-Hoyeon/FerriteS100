//! S-101 UI presets to catalogue-defined display modes. No UI or GPU types.
use anyhow::{bail, ensure, Context, Result};
use ferrite_portrayal_catalog::{DisplayMode, DisplayModes, PortrayalCatalogue};
use std::collections::HashSet;
#[derive(Debug, Clone, Copy)]
pub enum DisplayPreset {
    Base,
    Standard,
    Other,
}
fn normalized(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}
fn resolve(modes: &DisplayModes, preset: DisplayPreset) -> Result<&DisplayMode> {
    let aliases: &[&str] = match preset {
        DisplayPreset::Base => &["base", "displaybase"],
        DisplayPreset::Standard => &["standard", "standarddisplay"],
        DisplayPreset::Other => &["other", "otherinformation"],
    };
    let mut candidates = modes.modes.values().filter(|m| {
        aliases.contains(&normalized(&m.name).as_str())
            || aliases.contains(&normalized(&m.id).as_str())
    });
    let mode = candidates
        .next()
        .with_context(|| format!("S-101 catalogue has no {preset:?} display mode"))?;
    if candidates.next().is_some() {
        bail!("Ambiguous S-101 {preset:?} display mode")
    }
    Ok(mode)
}
pub fn resolve_display_mode(
    pc: &PortrayalCatalogue,
    preset: DisplayPreset,
) -> Result<&DisplayMode> {
    ensure!(
        pc.product_id == "S-101",
        "S-101 display selection requires S-101 PC"
    );
    resolve(&pc.display_modes, preset)
}
pub fn viewing_groups_for_preset(
    pc: &PortrayalCatalogue,
    preset: DisplayPreset,
) -> Result<HashSet<u32>> {
    let mode = resolve_display_mode(pc, preset)?;
    let mut groups = HashSet::new();
    for id in &mode.viewing_group_layers {
        let layer = pc
            .viewing_group_layers
            .get(id)
            .with_context(|| format!("Display mode {} references missing layer {id}", mode.id))?;
        for group in &layer.viewing_group_ids {
            ensure!(
                pc.viewing_groups.get(*group).is_some(),
                "Layer {id} references unknown viewing group {group}"
            );
            groups.insert(*group);
        }
    }
    groups.extend(pc.foundation_mode.iter().copied());
    Ok(groups)
}
/// Resolve individually selected layers, including text layers outside display modes.
/// Identifiers come from the installed catalogue; unknown layers fail closed.
pub fn viewing_groups_for_layers<'a>(
    pc: &PortrayalCatalogue,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashSet<u32>> {
    ensure!(
        pc.product_id == "S-101",
        "S-101 layer selection requires S-101 PC"
    );
    let mut groups = HashSet::new();
    for id in ids {
        let layer = pc
            .viewing_group_layers
            .get(id)
            .with_context(|| format!("Unknown S-101 viewing layer {id:?}"))?;
        for group in &layer.viewing_group_ids {
            ensure!(
                pc.viewing_groups.get(*group).is_some(),
                "Layer {id} references unknown viewing group {group}"
            );
            groups.insert(*group);
        }
    }
    Ok(groups)
}

/// Independently selectable layers which no delivered display mode includes.
pub fn optional_viewing_layers(pc: &PortrayalCatalogue) -> Vec<(String, String)> {
    let referenced: HashSet<_> = pc
        .display_modes
        .modes
        .values()
        .flat_map(|m| m.viewing_group_layers.iter())
        .collect();
    let mut layers: Vec<_> = pc
        .viewing_group_layers
        .layers
        .values()
        .filter(|l| !referenced.contains(&l.id))
        .map(|l| (l.id.clone(), l.name.clone()))
        .collect();
    layers.sort();
    layers
}
#[cfg(test)]
mod tests {
    use super::*;
    fn mode(id: &str, name: &str) -> DisplayMode {
        DisplayMode {
            id: id.into(),
            name: name.into(),
            viewing_group_layers: Vec::new(),
        }
    }
    #[test]
    fn opaque_legacy_ids_and_current_identifiers_resolve_from_delivered_metadata() {
        let mut m = DisplayModes::new();
        for v in [mode("1", "Base"), mode("2", "Standard"), mode("3", "Other")] {
            m.modes.insert(v.id.clone(), v);
        }
        assert_eq!(resolve(&m, DisplayPreset::Base).unwrap().id, "1");
        assert_eq!(resolve(&m, DisplayPreset::Standard).unwrap().id, "2");
        assert_eq!(resolve(&m, DisplayPreset::Other).unwrap().id, "3");
        m.modes.clear();
        for v in [
            mode("DisplayBase", "Display Base"),
            mode("StandardDisplay", "Standard Display"),
            mode("OtherInformation", "Other Information"),
        ] {
            m.modes.insert(v.id.clone(), v);
        }
        assert_eq!(
            resolve(&m, DisplayPreset::Standard).unwrap().id,
            "StandardDisplay"
        );
    }
    #[test]
    fn missing_and_ambiguous_modes_fail_instead_of_showing_all() {
        let mut m = DisplayModes::new();
        assert!(resolve(&m, DisplayPreset::Base).is_err());
        for v in [mode("first", "Base"), mode("second", "Display Base")] {
            m.modes.insert(v.id.clone(), v);
        }
        assert!(resolve(&m, DisplayPreset::Base).is_err());
    }
}
