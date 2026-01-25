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
    pub id: u32,
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
}

impl ViewingGroups {
    pub fn new() -> Self {
        ViewingGroups::default()
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

    /// Get mode IDs mapped to common names
    /// Returns (base_id, standard_id, all_id)
    pub fn get_mode_ids(&self) -> (&'static str, &'static str, &'static str) {
        // S-101 standard mode IDs
        ("DisplayBase", "StandardDisplay", "OtherInformation")
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

    // Find which layer contains this viewing group
    let Some(layer_id) = viewing_group_layers.find_layer_for_viewing_group(viewing_group_id) else {
        // Viewing group not in any layer - show it (safety fallback)
        return true;
    };

    // Check if this layer is in the display mode's visible layers
    mode.viewing_group_layers.iter().any(|l| l == layer_id)
}
