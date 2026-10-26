//! Host display settings and catalogue status: plain data shared by the
//! chart engine (portrayal inputs) and the GUI (settings panels).

/// S-101 Display Mode following IHO standard ViewingGroupLayers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    /// Base display - essential navigation information only
    Base,
    /// Standard display - default operational view
    #[default]
    Standard,
    /// All - display all available information
    All,
}

impl DisplayMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisplayMode::Base => "Base",
            DisplayMode::Standard => "Standard",
            DisplayMode::All => "All",
        }
    }
}

/// S-101 Context Parameters settings
/// Following IHO S-101 standard for mariner-settable parameters
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    /// Safety depth in meters (for danger highlighting)
    pub safety_depth: f64,
    /// Safety contour in meters (primary safety boundary)
    pub safety_contour: f64,
    /// Shallow contour in meters
    pub shallow_contour: f64,
    /// Deep contour in meters
    pub deep_contour: f64,
    /// Two shades depth display (simplified)
    pub two_shades: bool,
    /// Use simplified point symbols (vs paper chart symbols)
    pub simplified_symbols: bool,
    /// Display isolated dangers in shallow water
    pub isolated_dangers: bool,
    /// Show full light sectors
    pub full_light_sectors: bool,
    /// Ignore scale minimum attribute
    pub ignore_scale_minimum: bool,
    /// Use plain boundaries (vs symbolized)
    pub plain_boundaries: bool,
    /// Display mode (Base/Standard/All)
    pub display_mode: DisplayMode,
    /// Additional catalogue layer IDs, independent of display modes.
    pub viewing_layers: std::collections::BTreeSet<String>,
    /// Host-captured primary S-101 PC digest for these lexical layer IDs.
    /// UI intent identity only; not dataset authentication or portrayal authority.
    pub viewing_layer_owner: Option<[u8; 32]>,
    pub interoperability_enabled: bool,
    /// Show shallow water pattern overlay (DIAMOND1 - areas less than safety contour)
    pub show_shallow_pattern: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            safety_depth: 30.0,
            safety_contour: 30.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            two_shades: false,
            simplified_symbols: false,
            isolated_dangers: true,
            full_light_sectors: true,
            ignore_scale_minimum: false,
            plain_boundaries: false,
            display_mode: DisplayMode::Standard,
            viewing_layers: Default::default(),
            viewing_layer_owner: None,
            interoperability_enabled: true,
            show_shallow_pattern: true,
        }
    }
}

/// Feature Catalogue status information
#[derive(Debug, Clone, Default)]
pub struct CatalogueStatus {
    /// Whether the catalogue is loaded and valid
    pub loaded: bool,
    /// Product ID (e.g., "S-101")
    pub product_id: String,
    /// Version string
    pub version: String,
    /// File path
    pub path: String,
    /// Number of items (feature types for FC, symbols for PC)
    pub item_count: usize,
    /// Validation status message
    pub validation_message: Option<String>,
}
