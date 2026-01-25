//! Additional ABI-stable types for plugin API

use abi_stable::{std_types::RString, StableAbi};

/// UI Panel data structure for plugin side panel
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub struct UiPanelData {
    /// Panel title
    pub title: RString,
    /// Panel content items
    pub items: abi_stable::std_types::RVec<UiPanelItem>,
    /// Action buttons
    pub actions: abi_stable::std_types::RVec<UiAction>,
}

/// UI panel item types
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub enum UiPanelItem {
    /// Label text
    Label { text: RString },
    /// Labeled value
    LabeledValue { label: RString, value: RString },
    /// Separator line
    Separator,
    /// Collapsible section
    Section {
        title: RString,
        items: abi_stable::std_types::RVec<UiPanelItem>,
        expanded: bool,
    },
    /// List item with optional icon
    ListItem {
        text: RString,
        secondary: RString,
        icon: abi_stable::std_types::ROption<RString>,
        selectable: bool,
        selected: bool,
        id: u32,
    },
}

/// UI action button
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub struct UiAction {
    /// Action identifier
    pub id: RString,
    /// Button label
    pub label: RString,
    /// Whether button is enabled
    pub enabled: bool,
    /// Optional icon name
    pub icon: abi_stable::std_types::ROption<RString>,
}

/// UI event types sent from host to plugin
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub enum UiEvent {
    /// Action button clicked
    ActionClicked { action_id: RString },
    /// List item selected
    ItemSelected { item_id: u32 },
    /// List item double-clicked
    ItemDoubleClicked { item_id: u32 },
}

/// Waypoint data for Route plugin
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub struct WaypointData {
    pub id: u32,
    pub lon: f64,
    pub lat: f64,
    pub name: abi_stable::std_types::ROption<RString>,
    pub radius_nm: abi_stable::std_types::ROption<f64>,
}

/// Leg data for Route plugin
#[repr(C)]
#[derive(StableAbi, Clone, Debug)]
pub struct LegData {
    pub from_wp_id: u32,
    pub to_wp_id: u32,
    pub distance_nm: f64,
    pub geometry_type: LegGeometryType,
}

/// Leg geometry type per S-421
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LegGeometryType {
    /// Rhumb line (constant bearing)
    #[default]
    Loxodrome = 1,
    /// Great circle (shortest path)
    Orthodrome = 2,
}

/// Distance unit for display
#[repr(C)]
#[derive(StableAbi, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DistanceUnit {
    #[default]
    NauticalMiles,
    Kilometers,
    StatuteMiles,
}

impl DistanceUnit {
    /// Convert nautical miles to this unit
    pub fn from_nm(&self, nm: f64) -> f64 {
        match self {
            Self::NauticalMiles => nm,
            Self::Kilometers => nm * 1.852,
            Self::StatuteMiles => nm * 1.15078,
        }
    }

    /// Get unit abbreviation
    pub fn abbrev(&self) -> &'static str {
        match self {
            Self::NauticalMiles => "NM",
            Self::Kilometers => "km",
            Self::StatuteMiles => "mi",
        }
    }
}
