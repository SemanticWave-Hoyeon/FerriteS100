//! Viewing groups and display modes

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Display plane
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DisplayPlane {
    #[default]
    UnderRadar,
    OverRadar,
}

/// Viewing group definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewingGroup {
    /// Compact local runtime handle; catalogue_id preserves the original identifier.
    pub id: u32,
    #[serde(default)]
    pub catalogue_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parent_id: Option<u32>,
}

/// Viewing group layer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewingGroupLayer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub viewing_group_ids: Vec<u32>,
    #[serde(default)]
    pub default_on: bool,
}

/// Display mode (e.g., base, standard, full)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayMode {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub viewing_group_layers: Vec<String>,
}

/// Collection of viewing groups
#[derive(Debug, Clone, Default)]
pub struct ViewingGroups {
    pub groups: HashMap<u32, ViewingGroup>,
    pub identifiers: HashMap<String, u32>,
    pub(crate) numeric_ids: std::collections::HashSet<u32>,
}

impl ViewingGroups {
    pub fn new() -> Self {
        ViewingGroups::default()
    }

    pub fn runtime_id(&self, id: &str) -> Option<u32> {
        self.identifiers.get(id).copied()
    }
    /// Numeric-only instructions borrow their group list. Strings are interned
    /// once in PC metadata and resolve to the same compact renderer handles.
    pub fn resolve_drawing_groups<'a>(
        &self,
        numeric: &'a [u32],
        named: &[String],
    ) -> crate::Result<std::borrow::Cow<'a, [u32]>> {
        for id in numeric {
            if !self.numeric_ids.contains(id) {
                return Err(crate::PCError::InvalidValue(format!(
                    "Undeclared numeric viewing group {id}"
                )));
            }
        }
        if named.is_empty() {
            return Ok(std::borrow::Cow::Borrowed(numeric));
        }
        let mut result = numeric.to_vec();
        for id in named {
            let handle = self.runtime_id(id).ok_or_else(|| {
                crate::PCError::InvalidValue(format!("Undeclared viewing group {id:?}"))
            })?;
            result.push(handle);
        }
        Ok(std::borrow::Cow::Owned(result))
    }

    /// Get viewing group by ID
    pub fn get(&self, id: u32) -> Option<&ViewingGroup> {
        self.groups.get(&id)
    }
}

/// Collection of viewing group layers
#[derive(Debug, Clone, Default)]
pub struct ViewingGroupLayers {
    pub layers: HashMap<String, ViewingGroupLayer>,
}

impl ViewingGroupLayers {
    pub fn new() -> Self {
        ViewingGroupLayers::default()
    }

    /// Get layer by ID
    pub fn get(&self, id: &str) -> Option<&ViewingGroupLayer> {
        self.layers.get(id)
    }

    /// Find which layer contains a viewing group ID
    pub fn find_layer_for_viewing_group(&self, viewing_group_id: u32) -> Option<&str> {
        for (layer_id, layer) in &self.layers {
            if layer.viewing_group_ids.contains(&viewing_group_id) {
                return Some(layer_id);
            }
        }
        None
    }

    /// Get all viewing group IDs for a given layer
    pub fn get_viewing_groups_for_layer(&self, layer_id: &str) -> Vec<u32> {
        self.layers
            .get(layer_id)
            .map(|l| l.viewing_group_ids.clone())
            .unwrap_or_default()
    }
}

/// Collection of display modes
#[derive(Debug, Clone, Default)]
pub struct DisplayModes {
    pub modes: HashMap<String, DisplayMode>,
    pub default_mode: Option<String>,
}

impl DisplayModes {
    pub fn new() -> Self {
        DisplayModes::default()
    }

    /// Get mode by ID
    pub fn get(&self, id: &str) -> Option<&DisplayMode> {
        self.modes.get(id)
    }

    /// Get default mode
    pub fn get_default(&self) -> Option<&DisplayMode> {
        self.default_mode.as_ref().and_then(|id| self.modes.get(id))
    }
}

/// Check if a viewing group is visible for a display mode
pub fn is_viewing_group_visible(
    viewing_group_id: u32,
    display_mode_id: &str,
    viewing_group_layers: &ViewingGroupLayers,
    display_modes: &DisplayModes,
) -> bool {
    // Get the display mode
    let Some(mode) = display_modes.get(display_mode_id) else {
        // If mode not found, show everything
        return true;
    };

    let mut known = false;
    for (id, layer) in &viewing_group_layers.layers {
        if layer.viewing_group_ids.contains(&viewing_group_id) {
            known = true;
            if mode.viewing_group_layers.contains(id) {
                return true;
            }
        }
    }
    !known
}
/// Read the mandatory foundation independently of group definitions. References
/// must never be inserted as anonymous viewing-group definitions (ID zero).
pub(crate) fn read_foundation_mode(
    doc: &roxmltree::Document<'_>,
    groups: &ViewingGroups,
) -> crate::Result<Vec<u32>> {
    let root = doc.root_element();
    let ns = root.tag_name().namespace();
    let nodes = root
        .children()
        .filter(|n| {
            n.is_element()
                && n.tag_name().name() == "foundationMode"
                && [None, ns].contains(&n.tag_name().namespace())
        })
        .collect::<Vec<_>>();
    if nodes.len() != 1 {
        return Err(crate::PCError::InvalidValue(
            "PC requires exactly one foundationMode".into(),
        ));
    }
    let foundation = nodes[0];
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in foundation.children().filter(|n| n.is_element()) {
        if n.tag_name().name() != "viewingGroup"
            || n.tag_name().namespace() != foundation.tag_name().namespace()
            || n.children().any(|c| c.is_element())
        {
            return Err(crate::PCError::InvalidValue(
                "Invalid foundation viewing-group reference".into(),
            ));
        }
        let original = n.text().unwrap_or("").trim();
        let id = groups.runtime_id(original).ok_or_else(|| {
            crate::PCError::InvalidValue(format!("Unknown foundation group {original:?}"))
        })?;
        if !seen.insert(id) {
            return Err(crate::PCError::InvalidValue(format!(
                "Duplicate foundation group {original:?}"
            )));
        }
        ids.push(id);
    }
    Ok(ids)
}
#[cfg(test)]
mod foundation_tests {
    use super::*;
    fn groups() -> ViewingGroups {
        let mut g = ViewingGroups::new();
        g.groups.insert(
            1,
            ViewingGroup {
                id: 1,
                catalogue_id: "1".into(),
                name: "Base".into(),
                description: None,
                parent_id: None,
            },
        );
        g.identifiers.insert("1".into(), 1);
        g.numeric_ids.insert(1);
        g
    }
    #[test]
    fn foundation_references_are_required_valid_and_deduplicated() {
        for xml in ["<pc/>","<pc><foundationMode/><foundationMode/></pc>","<pc><foundationMode><viewingGroup>2</viewingGroup></foundationMode></pc>","<pc><foundationMode><viewingGroup>1</viewingGroup><viewingGroup>1</viewingGroup></foundationMode></pc>","<pc><foundationMode><viewingGroup>0</viewingGroup></foundationMode></pc>","<pc><foundationMode><viewingGroup>abc</viewingGroup></foundationMode></pc>","<pc><foundationMode><viewingGroup><id>1</id></viewingGroup></foundationMode></pc>"] {
            assert!(read_foundation_mode(&roxmltree::Document::parse(xml).unwrap(),&groups()).is_err(),"{xml}");
        }
        assert_eq!(
            read_foundation_mode(
                &roxmltree::Document::parse(
                    "<pc><foundationMode><viewingGroup>1</viewingGroup></foundationMode></pc>"
                )
                .unwrap(),
                &groups()
            )
            .unwrap(),
            vec![1]
        );
        assert!(read_foundation_mode(
            &roxmltree::Document::parse("<pc><foundationMode/></pc>").unwrap(),
            &groups()
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn overlapping_layers_use_union_without_hash_iteration_dependency() {
        let mut layers = ViewingGroupLayers::new();
        for id in ["on", "off"] {
            layers.layers.insert(
                id.into(),
                ViewingGroupLayer {
                    id: id.into(),
                    name: id.into(),
                    viewing_group_ids: vec![1],
                    default_on: true,
                },
            );
        }
        let mut modes = DisplayModes::new();
        modes.modes.insert(
            "selected".into(),
            DisplayMode {
                id: "selected".into(),
                name: "Selected".into(),
                viewing_group_layers: vec!["on".into()],
            },
        );
        assert!(is_viewing_group_visible(1, "selected", &layers, &modes));
    }
}
