//! Render Context
//!
//! Manages rendering state, display settings, and instruction collection.

use std::collections::{HashMap, HashSet};

use crate::{Color, DrawingInstruction, GeoBounds, Scaler, ViewingGroup, Viewport};

/// Display settings
#[derive(Debug, Clone)]
pub struct DisplaySettings {
    /// Active color profile (Day, Dusk, Night)
    pub color_profile: String,
    /// Safety depth in meters
    pub safety_depth: f64,
    /// Safety contour in meters
    pub safety_contour: f64,
    /// Shallow contour in meters
    pub shallow_contour: f64,
    /// Deep contour in meters
    pub deep_contour: f64,
    /// Display isolated dangers in shallow water
    pub display_isolated_dangers: bool,
    /// Two shades (simple) vs multi-shade depth display
    pub two_shade_depth: bool,
    /// Symbolized boundaries vs plain boundaries
    pub symbolized_boundaries: bool,
    /// Full light sectors vs simplified
    pub full_light_sectors: bool,
    /// Paper chart symbols vs simplified
    pub paper_chart_symbols: bool,
    /// Display date-dependent features
    pub date_dependent: bool,
    /// Current date for date-dependent display
    pub current_date: Option<String>,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        DisplaySettings {
            color_profile: "Day".to_string(),
            safety_depth: 30.0,
            safety_contour: 30.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            display_isolated_dangers: true,
            two_shade_depth: false,
            symbolized_boundaries: true,
            full_light_sectors: true,
            paper_chart_symbols: true,
            date_dependent: true,
            current_date: None,
        }
    }
}

/// Viewing group layer visibility
#[derive(Debug, Clone)]
pub struct ViewingGroupState {
    /// Viewing groups that are currently visible
    visible_groups: HashMap<u32, bool>,
}

impl ViewingGroupState {
    pub fn new() -> Self {
        ViewingGroupState {
            visible_groups: HashMap::new(),
        }
    }

    pub fn set_visible(&mut self, group: u32, visible: bool) {
        self.visible_groups.insert(group, visible);
    }

    pub fn is_visible(&self, group: ViewingGroup) -> bool {
        // By default, groups are visible
        *self.visible_groups.get(&group.0).unwrap_or(&true)
    }

    /// Enable standard display groups
    pub fn enable_standard(&mut self) {
        // DISPLBASE - always displayed
        self.set_visible(21010, true);
        // Standard display groups
        self.set_visible(22210, true); // DEPARE
        self.set_visible(22220, true); // DEPCNT
        self.set_visible(23010, true); // LIGHTS
        self.set_visible(24010, true); // BUOYS
        self.set_visible(25010, true); // BEACONS
    }

    /// Disable all groups
    pub fn disable_all(&mut self) {
        for (_, visible) in self.visible_groups.iter_mut() {
            *visible = false;
        }
    }
}

impl Default for ViewingGroupState {
    fn default() -> Self {
        let mut state = ViewingGroupState::new();
        state.enable_standard();
        state
    }
}

/// Render context - manages rendering state and instruction collection
#[derive(Debug)]
pub struct RenderContext {
    /// Coordinate scaler
    pub scaler: Scaler,
    /// Display settings
    pub settings: DisplaySettings,
    /// Viewing group visibility
    pub viewing_groups: ViewingGroupState,
    /// Background color
    pub background_color: Color,
    /// Collected drawing instructions (by priority)
    instructions: Vec<DrawingInstruction>,
    /// Unique feature IDs (for statistics only - no instruction duplication)
    feature_ids: HashSet<i64>,
    /// Optimization: cache sorted state to avoid re-sorting during animation
    sorted: bool,
    /// Optimization: animation mode (skip expensive operations)
    pub animation_mode: bool,
}

impl RenderContext {
    /// Create new render context
    pub fn new(viewport: Viewport) -> Self {
        RenderContext {
            scaler: Scaler::new(GeoBounds::default(), viewport),
            settings: DisplaySettings::default(),
            viewing_groups: ViewingGroupState::default(),
            background_color: Color::from_hex("#DEEBF7").unwrap_or(Color::WHITE), // Light blue water
            instructions: Vec::new(),
            feature_ids: HashSet::new(),
            sorted: false,
            animation_mode: false,
        }
    }

    /// Set animation mode (enables fast-path optimizations)
    #[inline]
    pub fn set_animation_mode(&mut self, animating: bool) {
        self.animation_mode = animating;
    }

    /// Set viewport size
    pub fn set_viewport(&mut self, width: f32, height: f32) {
        self.scaler.set_viewport(Viewport::new(width, height));
    }

    /// Set viewport with origin (for UI panel-aware rendering)
    pub fn set_viewport_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.scaler
            .set_viewport(Viewport::with_origin(x, y, width, height));
    }

    /// Set geographic bounds
    pub fn set_bounds(&mut self, bounds: GeoBounds) {
        self.scaler.set_bounds(bounds);
    }

    /// Zoom to fit bounds
    pub fn zoom_to_fit(&mut self, bounds: GeoBounds) {
        self.scaler.zoom_to_fit(bounds);
    }

    /// Add a drawing instruction
    pub fn add_instruction(&mut self, instruction: DrawingInstruction) {
        // Check viewing group visibility
        if !self.viewing_groups.is_visible(instruction.viewing_group()) {
            return;
        }

        // Mark as unsorted when new instructions are added
        self.sorted = false;

        // Track unique feature IDs (no instruction cloning - just the ID)
        if let Some(feature_id) = instruction.feature_id() {
            self.feature_ids.insert(feature_id);
        }

        self.instructions.push(instruction);
    }

    /// Get all instructions sorted by S-101 render order
    ///
    /// Sort order: (1) display priority, (2) geometry type (Area < Line < Point < Text)
    /// Lower values rendered first (background)
    ///
    /// Note: Uses stable sort to ensure consistent decluttering results across frames.
    /// par_sort_by_key is NOT stable, which caused symbols to appear/disappear inconsistently.
    /// Skips re-sorting if already sorted or in animation mode.
    pub fn get_sorted_instructions(&mut self) -> &[DrawingInstruction] {
        // Skip sort during animation mode for better performance
        if !self.sorted && !self.animation_mode {
            // Use stable sort to ensure consistent symbol decluttering
            // Unstable sorts can reorder same-priority instructions differently each frame
            self.instructions.sort_by_key(|i| i.render_order());
            self.sorted = true;
        }
        &self.instructions
    }

    /// Clear all instructions
    pub fn clear_instructions(&mut self) {
        self.instructions.clear();
        self.feature_ids.clear();
        self.sorted = false;
    }

    /// Get total instruction count
    pub fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    /// Truncate instructions to a specific count
    /// Used to remove plugin instructions while keeping chart instructions
    pub fn truncate_instructions(&mut self, count: usize) {
        if count < self.instructions.len() {
            self.instructions.truncate(count);
            self.sorted = false;
        }
    }

    /// Get statistics about collected instructions
    pub fn statistics(&self) -> RenderStatistics {
        let mut stats = RenderStatistics::default();

        for instruction in &self.instructions {
            match instruction {
                DrawingInstruction::Point(_) => stats.point_count += 1,
                DrawingInstruction::Line(_) => stats.line_count += 1,
                DrawingInstruction::Area(_) => stats.area_count += 1,
                DrawingInstruction::Text(_) => stats.text_count += 1,
            }
        }

        stats.total_count = self.instructions.len();
        stats.feature_count = self.feature_ids.len();

        stats
    }
}

/// Render statistics
#[derive(Debug, Clone, Default)]
pub struct RenderStatistics {
    pub total_count: usize,
    pub feature_count: usize,
    pub point_count: usize,
    pub line_count: usize,
    pub area_count: usize,
    pub text_count: usize,
}

impl std::fmt::Display for RenderStatistics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} instructions ({} features): {} points, {} lines, {} areas, {} texts",
            self.total_count,
            self.feature_count,
            self.point_count,
            self.line_count,
            self.area_count,
            self.text_count
        )
    }
}
