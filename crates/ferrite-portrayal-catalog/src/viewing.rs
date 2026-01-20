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
