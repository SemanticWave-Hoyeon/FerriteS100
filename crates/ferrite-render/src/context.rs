//! Render Context
//!
//! Manages rendering state, display settings, and instruction collection.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_GEOMETRY_REVISION: AtomicU64 = AtomicU64::new(1);
fn next_geometry_revision() -> u64 {
    NEXT_GEOMETRY_REVISION.fetch_add(1, Ordering::Relaxed)
}

use crate::{Color, DrawingInstruction, GeoBounds, Scaler, ViewingGroup, Viewport};

/// Display settings
#[derive(Debug, Clone, PartialEq)]
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
    /// Apply date-dependent interval filtering (false shows all dates)
    pub date_dependent: bool,
    /// Current date for date-dependent display
    pub current_date: Option<String>,
    /// Explicit viewing instant with Z/UTC offset; takes precedence over date.
    pub current_datetime: Option<String>,
    /// Explicit source-local offset for unzoned temporal bounds; UTC by default.
    pub local_time_offset_seconds: i32,
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
            current_datetime: None,
            local_time_offset_seconds: 0,
        }
    }
}

/// Viewing group layer visibility
#[derive(Debug, Clone, PartialEq)]
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
        for visible in self.visible_groups.values_mut() {
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

/// Opaque constant-size owner of the exact current instruction order/content.
/// Only RenderContext can mint it. It contains no group permission or visibility.
#[derive(Debug)]
pub struct StaticInstructionOrderIdentity {
    _private: (),
}

/// Source-only classification in the exact current raw instruction order.
/// It contains no visibility, scale, palette, coverage, projection or pick result.
#[derive(Debug)]
pub struct StaticSourceClassification {
    device_fixed: Vec<bool>,
    view_dependent: bool,
}
impl StaticSourceClassification {
    fn compile(instructions: &[DrawingInstruction]) -> Self {
        let device_fixed = instructions
            .iter()
            .map(|i| i.portrayal_origin().is_device_fixed())
            .collect();
        let view_dependent = instructions.iter().any(|p| {
            p.portrayal_origin().requires_view_reprojection()
                || match p {
                    DrawingInstruction::Point(p) => {
                        p.line_placement.is_some()
                            || p.rotation_crs == crate::RotationCrs::Geographic
                            || p.curve_tangent_bearing.is_some()
                    }
                    DrawingInstruction::Text(t) => {
                        t.rotation_crs == crate::RotationCrs::Geographic
                            || t.curve_tangent_bearing.is_some()
                    }
                    _ => false,
                }
        });
        Self {
            device_fixed,
            view_dependent,
        }
    }
    pub fn is_device_fixed(&self, ordinal: usize) -> bool {
        self.device_fixed.get(ordinal).copied().unwrap_or(false)
    }
    pub fn requires_view_reprojection(&self) -> bool {
        self.view_dependent
    }
}

/// Negative source-content memo only; never coverage permission or visibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoverageExemptPresence {
    KnownEmpty,
    Unknown,
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
    /// Collected drawing instructions (by priority). Shared copy-on-write with
    /// `fork_for_emission` owners; every mutation goes through `instructions_mut`.
    instructions: std::sync::Arc<Vec<DrawingInstruction>>,
    /// Conservative presence hint only; KnownEmpty proves no coverage-exempt command.
    coverage_exempt_presence: CoverageExemptPresence,
    coverage_exempt_empty_fast_path: bool,
    retained_path_cache:
        std::sync::Arc<std::sync::Mutex<crate::retained_path_cache::RetainedPathCache>>,
    retained_path_cache_enabled: bool,
    /// Unique feature IDs (for statistics only - no instruction duplication)
    feature_ids: HashSet<i64>,
    /// Optimization: cache sorted state to avoid re-sorting during animation
    sorted: bool,
    /// Process-unique identity for the lifetime of this instruction geometry.
    geometry_revision: u64,
    static_line_relation_epoch: crate::StaticLineRelationEpoch,
    static_area_geometry_epoch: crate::StaticAreaGeometryEpoch,
    /// Immutable command topology; camera, palette and date changes do not rebuild it.
    dependency_graph: std::sync::OnceLock<std::sync::Arc<crate::DrawingDependencyGraph>>,
    dependency_plan_cache_enabled: bool,
    prepared_coverage: Option<std::sync::Arc<crate::PreparedCoverage>>,
    coverage_required: bool,
    coverage_visibility_fusion_enabled: bool,
    coverage_view_revision: u64,
    coverage_scaler_signature: Option<[u64; 15]>,
    scene_spatial: std::sync::OnceLock<std::sync::Arc<crate::SceneSpatialIndex>>,
    static_source_classification: std::sync::OnceLock<std::sync::Arc<StaticSourceClassification>>,
    static_instruction_order: std::sync::OnceLock<std::sync::Arc<StaticInstructionOrderIdentity>>,
    temporal_indices: Vec<usize>,
    temporal_index_dirty: bool,
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
            instructions: std::sync::Arc::new(Vec::new()),
            coverage_exempt_presence: CoverageExemptPresence::KnownEmpty,
            // Qualified source-bound native comparison; explicit 0 retains the reference scan.
            coverage_exempt_empty_fast_path: std::env::var(
                "FERRITE_COVERAGE_EXEMPT_EMPTY_FAST_PATH",
            )
            .as_deref()
                != Ok("0"),
            retained_path_cache: std::sync::Arc::new(std::sync::Mutex::new(
                crate::retained_path_cache::RetainedPathCache::default(),
            )),
            retained_path_cache_enabled: std::env::var("FERRITE_RETAINED_PATH_CACHE").as_deref()
                != Ok("0"),
            feature_ids: HashSet::new(),
            sorted: false,
            geometry_revision: next_geometry_revision(),
            static_line_relation_epoch: crate::StaticLineRelationEpoch::fresh(),
            static_area_geometry_epoch: crate::StaticAreaGeometryEpoch::fresh(),
            dependency_graph: std::sync::OnceLock::new(),
            dependency_plan_cache_enabled: true,
            prepared_coverage: None,
            coverage_required: false,
            coverage_visibility_fusion_enabled: false,
            coverage_view_revision: next_geometry_revision(),
            coverage_scaler_signature: None,
            scene_spatial: std::sync::OnceLock::new(),
            static_source_classification: std::sync::OnceLock::new(),
            static_instruction_order: std::sync::OnceLock::new(),
            temporal_indices: Vec::new(),
            temporal_index_dirty: false,
            animation_mode: false,
        }
    }

    /// Stage replacement instructions without cloning existing geometry. Commit only on success.
    pub fn empty_for_rebuild(&self) -> Self {
        let mut next = Self::new(self.scaler.viewport);
        next.scaler = self.scaler.clone();
        next.settings = self.settings.clone();
        next.viewing_groups = self.viewing_groups.clone();
        next.background_color = self.background_color;
        next.animation_mode = self.animation_mode;
        next.dependency_plan_cache_enabled = self.dependency_plan_cache_enabled;
        next.retained_path_cache_enabled = self.retained_path_cache_enabled;
        next.coverage_exempt_empty_fast_path = self.coverage_exempt_empty_fast_path;
        next
    }

    /// Copy-on-write access: clones the instruction list only while a
    /// `fork_for_emission` owner still shares it.
    fn instructions_mut(&mut self) -> &mut Vec<DrawingInstruction> {
        std::sync::Arc::make_mut(&mut self.instructions)
    }

    /// Same source, order, identities and immutable caches as `self`, without
    /// copying geometry. Emission caches keyed by these identities stay valid on
    /// the fork. A later mutation of either owner copies its list first and
    /// issues fresh identities, so neither observes the other's change.
    pub fn fork_for_emission(&self) -> Self {
        Self {
            scaler: self.scaler.clone(),
            settings: self.settings.clone(),
            viewing_groups: self.viewing_groups.clone(),
            background_color: self.background_color,
            instructions: std::sync::Arc::clone(&self.instructions),
            coverage_exempt_presence: self.coverage_exempt_presence,
            coverage_exempt_empty_fast_path: self.coverage_exempt_empty_fast_path,
            retained_path_cache: std::sync::Arc::clone(&self.retained_path_cache),
            retained_path_cache_enabled: self.retained_path_cache_enabled,
            feature_ids: self.feature_ids.clone(),
            sorted: self.sorted,
            geometry_revision: self.geometry_revision,
            static_line_relation_epoch: self.static_line_relation_epoch,
            static_area_geometry_epoch: self.static_area_geometry_epoch,
            dependency_graph: self.dependency_graph.clone(),
            dependency_plan_cache_enabled: self.dependency_plan_cache_enabled,
            prepared_coverage: self.prepared_coverage.clone(),
            coverage_required: self.coverage_required,
            coverage_visibility_fusion_enabled: self.coverage_visibility_fusion_enabled,
            coverage_view_revision: self.coverage_view_revision,
            coverage_scaler_signature: self.coverage_scaler_signature,
            scene_spatial: self.scene_spatial.clone(),
            static_source_classification: self.static_source_classification.clone(),
            static_instruction_order: self.static_instruction_order.clone(),
            temporal_indices: self.temporal_indices.clone(),
            temporal_index_dirty: self.temporal_index_dirty,
            animation_mode: self.animation_mode,
        }
    }

    /// True when both owners hold the very same instruction allocation.
    pub fn shares_instructions_with(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.instructions, &other.instructions)
    }

    /// Set animation mode (enables fast-path optimizations)
    #[inline]
    pub fn set_animation_mode(&mut self, animating: bool) {
        self.animation_mode = animating;
    }

    /// Set viewport size
    pub fn set_viewport(&mut self, width: f32, height: f32) {
        self.invalidate_coverage_view();
        self.scaler.set_viewport(Viewport::new(width, height));
    }

    /// Set viewport with origin (for UI panel-aware rendering)
    pub fn set_viewport_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.invalidate_coverage_view();
        self.scaler
            .set_viewport(Viewport::with_origin(x, y, width, height));
    }

    /// Set geographic bounds
    pub fn set_bounds(&mut self, bounds: GeoBounds) {
        self.invalidate_coverage_view();
        self.scaler.set_bounds(bounds);
    }

    /// Zoom to fit bounds
    pub fn zoom_to_fit(&mut self, bounds: GeoBounds) {
        self.invalidate_coverage_view();
        self.scaler.zoom_to_fit(bounds);
    }

    /// Add a drawing instruction
    pub fn add_instruction(&mut self, instruction: DrawingInstruction) {
        // Check viewing group visibility
        if !instruction
            .viewing_groups()
            .all(|g| self.viewing_groups.is_visible(g))
        {
            return;
        }

        // Mark as unsorted when new instructions are added
        self.sorted = false;

        // Track unique feature IDs (no instruction cloning - just the ID)
        if let Some(feature_id) = instruction.feature_id() {
            self.feature_ids.insert(feature_id);
        }

        if matches!(
            instruction.portrayal_origin(),
            crate::PortrayalOrigin::CoverageExempt
        ) {
            self.coverage_exempt_presence = CoverageExemptPresence::Unknown;
        }
        self.instructions_mut().push(instruction);
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
        self.dependency_graph.take();
        self.scene_spatial.take();
        self.static_source_classification.take();
        self.static_instruction_order.take();
        self.temporal_index_dirty = true;
    }

    /// Get all instructions sorted by S-101 render order
    ///
    /// Sort order: (1) display priority, (2) geometry type (Area < Line < Point < Text)
    /// Lower values rendered first (background)
    ///
    /// Note: Uses stable sort to ensure consistent decluttering results across frames.
    /// par_sort_by_key is NOT stable, which caused symbols to appear/disappear inconsistently.
    /// Reuses sorted content during animation; newly changed instructions still sort.
    /// Read-only guard for caches bound to the stable raw instruction order.
    pub fn instructions_are_sorted(&self) -> bool {
        self.sorted
    }
    pub fn get_sorted_instructions(&mut self) -> &[DrawingInstruction] {
        // Animation must retain the same portrayal order as a stationary view.
        if !self.sorted {
            self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
            self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
            // Use stable sort to ensure consistent symbol decluttering
            // Unstable sorts can reorder same-priority instructions differently each frame
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.static_source_classification.take();
            self.static_instruction_order.take();
            self.instructions_mut().sort_by_key(|i| i.render_order());
            self.sorted = true;
        }
        if self.temporal_index_dirty {
            self.temporal_indices = self
                .instructions
                .iter()
                .enumerate()
                .filter_map(|(index, i)| (!i.time_intervals().is_empty()).then_some(index))
                .collect();
            self.temporal_index_dirty = false;
        }
        &self.instructions
    }

    /// Clear all instructions
    pub fn clear_instructions(&mut self) {
        self.instructions_mut().clear();
        self.coverage_exempt_presence = CoverageExemptPresence::KnownEmpty;
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
        self.dependency_graph.take();
        self.scene_spatial.take();
        self.static_source_classification.take();
        self.static_instruction_order.take();
        self.temporal_indices.clear();
        self.temporal_index_dirty = false;
        self.feature_ids.clear();
        self.sorted = false;
    }

    /// Preserve source geometry on every primitive expanded from a portrayal command.
    pub fn set_portrayal_origin_from(&mut self, start: usize, origin: crate::PortrayalOrigin) {
        if start >= self.instructions.len() {
            return;
        }
        if matches!(origin, crate::PortrayalOrigin::CoverageExempt) {
            self.coverage_exempt_presence = CoverageExemptPresence::Unknown;
        }
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
        self.static_source_classification.take();
        self.static_instruction_order.take();
        for instruction in self.instructions_mut().iter_mut().skip(start) {
            instruction.set_portrayal_origin(origin.clone());
        }
    }

    /// Bind one command's identity/dependency to all expanded primitives.
    pub fn set_dependency_from(
        &mut self,
        start: usize,
        dependency: Option<crate::DrawingDependency>,
    ) {
        if dependency.is_none() {
            return;
        }
        if start < self.instructions.len() {
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.static_source_classification.take();
            self.static_instruction_order.take();
        }
        for instruction in self.instructions_mut().iter_mut().skip(start) {
            instruction.set_dependency(dependency.clone());
        }
    }

    /// Bind conditions to every primitive generated by one product portrayal command.
    pub fn set_time_intervals_from(
        &mut self,
        start: usize,
        intervals: &[ferrite_kernel::TemporalInterval],
    ) {
        if intervals.is_empty() {
            return;
        }
        for instruction in self.instructions_mut().iter_mut().skip(start) {
            instruction.set_time_intervals(intervals);
        }
        self.temporal_index_dirty = true;
    }

    /// Call on every non-affine camera/projection change, including a projection change.
    /// Retain the old binding so stale state is rejected rather than bypassed.
    pub fn invalidate_coverage_view(&mut self) {
        self.coverage_view_revision = next_geometry_revision();
    }
    pub fn coverage_view_revision(&self) -> u64 {
        self.coverage_view_revision
    }
    fn coverage_signature(&self) -> [u64; 15] {
        let s = &self.scaler;
        [
            s.geo_bounds.min_x,
            s.geo_bounds.min_y,
            s.geo_bounds.max_x,
            s.geo_bounds.max_y,
            s.viewport.x as f64,
            s.viewport.y as f64,
            s.viewport.width as f64,
            s.viewport.height as f64,
            s.scale_x(),
            s.scale_y(),
            s.offset_x(),
            s.offset_y(),
            s.pixels_per_mm(),
            s.projection() as u8 as f64,
            s.display_scale,
        ]
        .map(f64::to_bits)
    }
    pub fn set_prepared_coverage(
        &mut self,
        coverage: crate::PreparedCoverage,
    ) -> crate::error::Result<()> {
        if !self.sorted {
            return Err(crate::RenderError::Render(
                "Sort instructions before coverage binding".into(),
            ));
        }
        coverage.validate(
            self.geometry_revision,
            self.coverage_view_revision,
            self.instructions.len(),
        )?;
        self.coverage_scaler_signature = Some(self.coverage_signature());
        self.prepared_coverage = Some(std::sync::Arc::new(coverage));
        self.coverage_required = true;
        Ok(())
    }
    pub fn prepared_coverage_binding(
        &self,
    ) -> crate::error::Result<Option<std::sync::Arc<crate::PreparedCoverage>>> {
        self.prepared_coverage()?;
        Ok(self.prepared_coverage.clone())
    }
    pub fn prepared_coverage(&self) -> crate::error::Result<Option<&crate::PreparedCoverage>> {
        if self.coverage_required && self.prepared_coverage.is_none() {
            return Err(crate::RenderError::Render(
                "Required coverage frame is missing".into(),
            ));
        }
        if let Some(coverage) = self.prepared_coverage.as_deref() {
            coverage.validate(
                self.geometry_revision,
                self.coverage_view_revision,
                self.instructions.len(),
            )?;
            if self.coverage_scaler_signature != Some(self.coverage_signature()) {
                return Err(crate::RenderError::Render(
                    "Coverage binding belongs to stale view coordinates".into(),
                ));
            }
        }
        Ok(self.prepared_coverage.as_deref())
    }
    /// Calendar diagnostics retain their meaning; coverage visibility is an
    /// independent execution pre-filter before suppression and decluttering.
    pub fn set_coverage_visibility_fusion_enabled(&mut self, enabled: bool) {
        self.coverage_visibility_fusion_enabled = enabled;
    }
    pub fn portrayal_visibility(&self) -> crate::error::Result<(Vec<bool>, usize, usize)> {
        let (mut visible, hidden, diagnostics) = self.date_visibility();
        if let Some(coverage) = self.prepared_coverage()? {
            if self.coverage_visibility_fusion_enabled {
                coverage.intersect_visibility(
                    self.geometry_revision,
                    self.coverage_view_revision,
                    &mut visible,
                )?;
            } else {
                let mask = coverage.visibility(
                    self.geometry_revision,
                    self.coverage_view_revision,
                    self.instructions.len(),
                )?;
                for (v, c) in visible.iter_mut().zip(mask) {
                    *v &= c;
                }
            }
        }
        Ok((visible, hidden, diagnostics))
    }
    pub fn coverage_fragment_visible(
        &self,
        index: usize,
        pass: usize,
        point: [f64; 2],
    ) -> crate::error::Result<bool> {
        match self.prepared_coverage()? {
            Some(coverage) => coverage.pass(pass)?.accepts_fragment(index, point),
            None => Ok(true),
        }
    }

    /// Calendar visibility in current sorted instruction order. Unsupported or
    /// malformed declarations remain visible and are counted for diagnostics.
    pub fn date_visibility(&self) -> (Vec<bool>, usize, usize) {
        if !self.settings.date_dependent {
            return (vec![true; self.instructions.len()], 0, 0);
        }
        let Some(local_offset) =
            chrono::FixedOffset::east_opt(self.settings.local_time_offset_seconds)
        else {
            return (
                vec![true; self.instructions.len()],
                0,
                self.temporal_entries().count(),
            );
        };
        let selected = if let Some(value) = self.settings.current_datetime.as_deref() {
            ferrite_kernel::parse_viewing_instant(value)
        } else if let Some(value) = self.settings.current_date.as_deref() {
            ferrite_kernel::parse_viewing_date(value).and_then(|date| {
                ferrite_kernel::parse_viewing_instant(&format!(
                    "{}T00:00:00{}",
                    date.format("%Y-%m-%d"),
                    local_offset
                ))
            })
        } else {
            Ok(chrono::Utc::now().fixed_offset())
        };
        let instant = match selected {
            Ok(value) => value,
            Err(_) => {
                return (
                    vec![true; self.instructions.len()],
                    0,
                    self.temporal_entries().count(),
                )
            }
        };
        let mut hidden = 0;
        let mut diagnostics = 0;
        // Empty interval selectors always return true without diagnostics in the
        // kernel. The existing index preserves instruction order and falls back
        // to a source scan while dirty; no viewing instant/result is cached.
        let mut visibility = vec![true; self.instructions.len()];
        for (index, instruction) in self.temporal_entries() {
            match ferrite_kernel::temporal_intervals_visible_with_offset(
                instruction.time_intervals(),
                &instant,
                local_offset,
            ) {
                Ok(value) => {
                    if !value {
                        hidden += 1;
                    }
                    visibility[index] = value;
                }
                Err(_) => {
                    diagnostics += 1;
                    // Keep unsupported or malformed selectors visible.
                }
            }
        }
        (visibility, hidden, diagnostics)
    }

    fn temporal_entries(&self) -> Box<dyn Iterator<Item = (usize, &DrawingInstruction)> + '_> {
        if self.temporal_index_dirty {
            Box::new(
                self.instructions
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| !i.time_intervals().is_empty()),
            )
        } else {
            Box::new(
                self.temporal_indices
                    .iter()
                    .map(|&index| (index, &self.instructions[index])),
            )
        }
    }
    /// O(T) indexed checks on a stable instruction list, without a full mask allocation.
    pub fn temporal_statuses(&self) -> Vec<(usize, bool)> {
        let mut settings = self.settings.clone();
        // Evaluate through the same selector policy as the full rendering mask.
        if settings.current_datetime.is_none() && settings.current_date.is_none() {
            settings.current_datetime = Some(chrono::Utc::now().to_rfc3339());
        }
        let local = chrono::FixedOffset::east_opt(settings.local_time_offset_seconds);
        let view = if let Some(value) = settings.current_datetime.as_deref() {
            ferrite_kernel::parse_viewing_instant(value).ok()
        } else {
            settings
                .current_date
                .as_deref()
                .and_then(|v| ferrite_kernel::parse_viewing_date(v).ok())
                .and_then(|d| {
                    local.and_then(|o| {
                        ferrite_kernel::parse_viewing_instant(&format!(
                            "{}T00:00:00{}",
                            d.format("%Y-%m-%d"),
                            o
                        ))
                        .ok()
                    })
                })
        };
        self.temporal_entries()
            .map(|(index, i)| {
                let visible = if !settings.date_dependent {
                    true
                } else {
                    view.zip(local)
                        .and_then(|(v, o)| {
                            ferrite_kernel::temporal_intervals_visible_with_offset(
                                i.time_intervals(),
                                &v,
                                o,
                            )
                            .ok()
                        })
                        .unwrap_or(true)
                };
                (index, visible)
            })
            .collect()
    }
    pub fn has_live_temporal_conditions(&self) -> bool {
        self.settings.date_dependent
            && self.settings.current_date.is_none()
            && self.settings.current_datetime.is_none()
            && if self.temporal_index_dirty {
                self.instructions
                    .iter()
                    .any(|i| !i.time_intervals().is_empty())
            } else {
                !self.temporal_indices.is_empty()
            }
    }
    pub fn next_live_temporal_change(&self) -> Option<ferrite_kernel::ViewingInstant> {
        if !self.settings.date_dependent
            || self.settings.current_date.is_some()
            || self.settings.current_datetime.is_some()
        {
            return None;
        }
        let offset = chrono::FixedOffset::east_opt(self.settings.local_time_offset_seconds)?;
        let now = chrono::Utc::now().fixed_offset();
        self.temporal_entries()
            .filter_map(|(_, i)| {
                ferrite_kernel::next_temporal_change_after(i.time_intervals(), &now, offset)
                    .ok()
                    .flatten()
            })
            .min()
    }

    /// Get total instruction count
    pub fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    /// Presence hint is not a visibility/coverage authorization. True can be stale;
    /// callers must retain the original exact ordinal filter whenever it is true.
    pub fn may_have_coverage_exempt_instructions(&self) -> bool {
        !self.coverage_exempt_empty_fast_path
            || self.coverage_exempt_presence == CoverageExemptPresence::Unknown
    }

    /// Remove explicit host overlays even after they interleave with chart commands.
    pub fn remove_coverage_exempt_instructions(&mut self) {
        if !self.may_have_coverage_exempt_instructions() {
            return;
        }
        let before = self.instructions.len();
        self.instructions_mut()
            .retain(|i| !matches!(i.portrayal_origin(), crate::PortrayalOrigin::CoverageExempt));
        self.coverage_exempt_presence = CoverageExemptPresence::KnownEmpty;
        if before != self.instructions.len() {
            self.geometry_revision = next_geometry_revision();
            self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
            self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.static_source_classification.take();
            self.static_instruction_order.take();
            self.temporal_index_dirty = true;
            self.sorted = false;
        }
    }
    /// A different projection/product view must prepare its own coverage frame.
    pub fn require_prepared_coverage(&mut self) {
        self.prepared_coverage = None;
        self.coverage_scaler_signature = None;
        self.coverage_required = true;
        self.invalidate_coverage_view();
    }
    pub fn clear_prepared_coverage(&mut self) {
        self.coverage_required = false;
        self.prepared_coverage = None;
        self.coverage_scaler_signature = None;
        self.invalidate_coverage_view();
    }

    /// Truncate instructions to a specific count
    /// Used to remove plugin instructions while keeping chart instructions
    pub fn truncate_instructions(&mut self, count: usize) {
        if count < self.instructions.len() {
            self.instructions_mut().truncate(count);
            self.geometry_revision = next_geometry_revision();
            self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
            self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.static_source_classification.take();
            self.static_instruction_order.take();
            self.temporal_index_dirty = true;
            self.sorted = false;
        }
    }

    /// Remap all instruction colors using a token-to-color lookup function.
    /// Used for color profile switch (Day/Dusk/Night) without re-running Lua.
    pub fn remap_colors(&mut self, lookup: &dyn Fn(&str) -> Color) {
        // A palette can change alpha and therefore original line-stroke admission.
        // Retire metadata identity BEFORE any possibly panicking lookup mutates colors.
        // Geometry/topology identities remain valid: no points/order changed.
        self.static_instruction_order.take();
        for inst in self.instructions_mut().iter_mut() {
            inst.remap_colors(lookup);
        }
    }

    /// Independent immutable area topology identity; never geometry ownership.
    pub fn static_area_geometry_epoch(&self) -> crate::StaticAreaGeometryEpoch {
        self.static_area_geometry_epoch
    }
    pub fn inherit_static_area_geometry_from(&mut self, previous: &Self) -> bool {
        if !self.sorted
            || !previous.sorted
            || !crate::area_relation_identity::same_area_inputs(
                &self.instructions,
                &previous.instructions,
            )
        {
            return false;
        }
        self.static_area_geometry_epoch = previous.static_area_geometry_epoch;
        true
    }

    /// Read-only relation identity, independent from owned geometry lifetime.
    pub fn static_line_relation_epoch(&self) -> crate::StaticLineRelationEpoch {
        self.static_line_relation_epoch
    }
    /// Once per staged publication. Never digest/pointer equality and never per-frame.
    /// Both contexts must already have the same stable ordinal sorting applied.
    pub fn inherit_static_line_relations_from(&mut self, previous: &Self) -> bool {
        if !self.sorted
            || !previous.sorted
            || !crate::line_relation_identity::same_static_line_relation_inputs(
                &self.instructions,
                &previous.instructions,
            )
        {
            return false;
        }
        self.static_line_relation_epoch = previous.static_line_relation_epoch;
        true
    }

    /// Geometry lifetime token. Viewport, colors, sorting and temporal settings
    /// preserve it; any instruction ownership change invalidates borrowed-pointer caches.
    pub fn geometry_revision(&self) -> u64 {
        self.geometry_revision
    }

    /// Compile command topology once in the current instruction order. A shared
    /// owner lets backends evaluate changing visibility while borrowing this
    /// context mutably, without copying geometry or the graph. Mutations and
    /// sorting invalidate the plan; view and portrayal settings do not.
    pub fn dependency_graph(&self) -> std::sync::Arc<crate::DrawingDependencyGraph> {
        if !self.dependency_plan_cache_enabled {
            return std::sync::Arc::new(crate::DrawingDependencyGraph::compile(
                self.instructions.iter().map(DrawingInstruction::dependency),
            ));
        }
        self.dependency_graph
            .get_or_init(|| {
                std::sync::Arc::new(crate::DrawingDependencyGraph::compile(
                    self.instructions.iter().map(DrawingInstruction::dependency),
                ))
            })
            .clone()
    }

    /// Diagnostic reference path: bypass retained topology while keeping the
    /// same binary, coordinates, visibility and rendering code. Turning the
    /// cache off releases its owner, including on staged rebuilds.
    pub fn set_dependency_plan_cache_enabled(&mut self, enabled: bool) {
        self.dependency_plan_cache_enabled = enabled;
        if !enabled {
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.static_source_classification.take();
            self.static_instruction_order.take();
        }
    }

    /// Camera-independent area envelopes shared by scene backends. Source changes
    /// and sorting invalidate ordinals; camera and palette changes preserve them.
    pub fn scene_spatial_index(&self) -> std::sync::Arc<crate::SceneSpatialIndex> {
        self.scene_spatial
            .get_or_init(|| {
                std::sync::Arc::new(crate::SceneSpatialIndex::compile(&self.instructions))
            })
            .clone()
    }

    /// Caller must sort before borrowing this ordinal classification for emission.
    /// Unsorted callers still receive the exact current order; a subsequent sort invalidates it.
    pub fn static_source_classification(&self) -> std::sync::Arc<StaticSourceClassification> {
        self.static_source_classification
            .get_or_init(|| {
                std::sync::Arc::new(StaticSourceClassification::compile(&self.instructions))
            })
            .clone()
    }

    /// Constant-size source identity for exact ordered caches. Source changes and
    /// sorting and color remapping replace it; palette alpha can affect stroke admission.
    /// Camera/date/coverage still do not grant cached execution visibility.
    pub fn static_instruction_order_identity(
        &self,
    ) -> std::sync::Arc<StaticInstructionOrderIdentity> {
        self.static_instruction_order
            .get_or_init(|| std::sync::Arc::new(StaticInstructionOrderIdentity { _private: () }))
            .clone()
    }

    /// Pure geometry reuse does not authorize visibility, coverage, suppression or picking.
    pub fn resolved_line_paths<'a>(
        &'a self,
        ordinal: usize,
        scaler: &Scaler,
    ) -> Option<crate::ResolvedLinePaths<'a>> {
        let DrawingInstruction::Line(line) = self.instructions.get(ordinal)? else {
            return None;
        };
        if self.retained_path_cache_enabled
            && matches!(
                line.portrayal_path.as_ref(),
                Some(
                    crate::PortrayalPath::GeographicArc { .. }
                        | crate::PortrayalPath::GeographicAnnulus { .. }
                )
            )
        {
            // Charge snapshots too; current policy is checked on every lookup, never hashed approximately.
            let policy_bytes = self
                .settings
                .color_profile
                .capacity()
                .saturating_add(
                    self.settings
                        .current_date
                        .as_ref()
                        .map_or(0, String::capacity),
                )
                .saturating_add(
                    self.settings
                        .current_datetime
                        .as_ref()
                        .map_or(0, String::capacity),
                )
                .saturating_add(
                    self.viewing_groups
                        .visible_groups
                        .capacity()
                        .saturating_mul(64),
                )
                .saturating_add(1024);
            if let Some(runs) = self.retained_path_cache.lock().ok().and_then(|mut cache| {
                cache.resolve(
                    self.static_instruction_order_identity(),
                    self.geometry_revision,
                    self.coverage_view_revision,
                    &self.settings,
                    &self.viewing_groups,
                    policy_bytes,
                    ordinal,
                    line,
                    scaler,
                )
            }) {
                return Some(crate::ResolvedLinePaths::Shared { runs, next: 0 });
            }
        }
        Some(line.render_paths(scaler))
    }
    pub fn set_retained_path_cache_enabled(&mut self, enabled: bool) {
        self.retained_path_cache_enabled = enabled;
        if !enabled {
            if let Ok(mut cache) = self.retained_path_cache.lock() {
                cache.clear();
            }
        }
    }
    pub fn retained_path_cache_stats(&self) -> crate::RetainedPathCacheStats {
        self.retained_path_cache
            .lock()
            .map(|c| c.stats())
            .unwrap_or_default()
    }
    /// Get raw instructions slice for cache serialization
    pub fn raw_instructions(&self) -> &[DrawingInstruction] {
        &self.instructions
    }

    /// Set instructions from a pre-built cache (skips viewing group filtering)
    pub fn set_instructions_from_cache(&mut self, instructions: Vec<DrawingInstruction>) {
        self.instructions = std::sync::Arc::new(instructions);
        // Imported vectors are conservatively unknown until the first exact removal.
        self.coverage_exempt_presence = if self.instructions.is_empty() {
            CoverageExemptPresence::KnownEmpty
        } else {
            CoverageExemptPresence::Unknown
        };
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch = crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch = crate::StaticAreaGeometryEpoch::fresh();
        self.dependency_graph.take();
        self.scene_spatial.take();
        self.static_source_classification.take();
        self.static_instruction_order.take();
        self.temporal_index_dirty = true;
        self.sorted = false;
        self.feature_ids.clear();
        for inst in self.instructions.iter() {
            if let Some(feature_id) = inst.feature_id() {
                self.feature_ids.insert(feature_id);
            }
        }
    }

    /// Get statistics about collected instructions
    pub fn statistics(&self) -> RenderStatistics {
        let mut stats = RenderStatistics::default();

        for instruction in self.instructions.iter() {
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

#[cfg(test)]
mod animation_order_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    #[test]
    fn newly_changed_instructions_sort_by_priority_even_during_animation() {
        let mut context = RenderContext::new(Viewport::new(1000., 1000.));
        context.set_animation_mode(true);
        for priority in [9, 1, 5] {
            context.add_instruction(DrawingInstruction::Line(
                LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)])
                    .with_priority(priority),
            ));
        }
        assert_eq!(
            context
                .get_sorted_instructions()
                .iter()
                .map(|i| i.priority().0)
                .collect::<Vec<_>>(),
            [1, 5, 9]
        );
        context.set_animation_mode(false);
        assert_eq!(
            context
                .get_sorted_instructions()
                .iter()
                .map(|i| i.priority().0)
                .collect::<Vec<_>>(),
            [1, 5, 9]
        );
    }
}

#[cfg(test)]
mod temporal_binding_tests {
    use super::*;
    use crate::{AreaInstruction, LineInstruction, PointInstruction, TextInstruction, WorldPoint};
    #[test]
    fn conditions_survive_all_primitive_types_and_binary_cache() {
        let bounds =
            ferrite_kernel::TemporalBounds::new(Some("----1101".into()), Some("----0331".into()))
                .unwrap();
        let condition = ferrite_kernel::TemporalInterval::new(
            Some(bounds),
            None,
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        let p = WorldPoint::new(0., 0.);
        ctx.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "A".into(),
            p,
        )));
        let start = ctx.instruction_count();
        ctx.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "B".into(),
            p,
        )));
        ctx.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![p, p])));
        ctx.add_instruction(DrawingInstruction::Area(AreaInstruction::new(vec![
            p, p, p,
        ])));
        ctx.add_instruction(DrawingInstruction::Text(TextInstruction::new(
            "T".into(),
            p,
        )));
        ctx.set_time_intervals_from(start, std::slice::from_ref(&condition));
        assert!(ctx.raw_instructions()[0].time_intervals().is_empty());
        for inst in &ctx.raw_instructions()[start..] {
            let decoded: DrawingInstruction =
                bincode::deserialize(&bincode::serialize(inst).unwrap()).unwrap();
            assert_eq!(decoded.time_intervals(), std::slice::from_ref(&condition));
        }
    }
}

#[cfg(test)]
mod temporal_visibility_tests {
    use super::*;
    use crate::{LineInstruction, LineSuppressionCache, WorldPoint};
    #[test]
    fn inactive_high_priority_line_does_not_suppress_active_line() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        let points = vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)];
        for priority in [1, 8] {
            ctx.add_instruction(DrawingInstruction::Line(
                LineInstruction::new(points.clone()).with_priority(priority),
            ));
        }
        let interval = ferrite_kernel::TemporalInterval::new(
            Some(
                ferrite_kernel::TemporalBounds::new(
                    Some("----1101".into()),
                    Some("----0331".into()),
                )
                .unwrap(),
            ),
            None,
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        ctx.set_time_intervals_from(1, &[interval]);
        ctx.get_sorted_instructions();
        let mut cache = LineSuppressionCache::default();
        for (date, expected, suppressed) in [
            ("2026-01-15", vec![true, true], true),
            ("2026-07-15", vec![true, false], false),
            ("2026-01-15", vec![true, true], true),
        ] {
            ctx.settings.current_date = Some(date.into());
            let (mask, _, errors) = ctx.date_visibility();
            assert_eq!(mask, expected);
            assert_eq!(errors, 0);
            let plan =
                cache.plan_with_visibility(ctx.raw_instructions(), 1000, None, None, Some(&mask));
            assert_eq!(plan.contains(&0), suppressed);
        }
        ctx.settings.date_dependent = false;
        assert_eq!(ctx.date_visibility().0, vec![true, true]);
    }
    #[test]
    fn invalid_clock_conditions_remain_visible_with_diagnostic() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        ctx.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ])));
        let i = ferrite_kernel::TemporalInterval::new(
            None,
            Some(
                ferrite_kernel::TemporalBounds::new(Some("250000".into()), Some("130000".into()))
                    .unwrap(),
            ),
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        ctx.set_time_intervals_from(0, &[i]);
        assert_eq!(ctx.date_visibility(), (vec![true], 0, 1));
    }
}

#[cfg(test)]
mod clock_selector_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    #[test]
    fn instant_precedence_and_date_midnight_are_reproducible() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        ctx.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ])));
        let i = ferrite_kernel::TemporalInterval::new(
            None,
            None,
            Some(
                ferrite_kernel::TemporalBounds::new(
                    Some("20261004T090000+0900".into()),
                    Some("20261004T100000+0900".into()),
                )
                .unwrap(),
            ),
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        ctx.set_time_intervals_from(0, &[i]);
        ctx.settings.current_date = Some("2026-01-01".into());
        ctx.settings.current_datetime = Some("20261004T003000Z".into());
        assert_eq!(ctx.date_visibility(), (vec![true], 0, 0));
        ctx.settings.current_datetime = Some("20261004T020000Z".into());
        assert_eq!(ctx.date_visibility(), (vec![false], 1, 0));
        ctx.settings.current_datetime = None;
        assert_eq!(ctx.date_visibility(), (vec![false], 1, 0));
        ctx.settings.current_date = Some("2026-10-04".into());
        assert_eq!(ctx.date_visibility(), (vec![true], 0, 0));
        ctx.settings.current_datetime = Some("invalid".into());
        assert_eq!(ctx.date_visibility(), (vec![true], 0, 1));
    }
}

#[cfg(test)]
mod local_calendar_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    #[test]
    fn date_only_setting_means_midnight_in_explicit_local_calendar() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        ctx.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ])));
        let i = ferrite_kernel::TemporalInterval::new(
            Some(
                ferrite_kernel::TemporalBounds::new(
                    Some("20261004".into()),
                    Some("20261004".into()),
                )
                .unwrap(),
            ),
            None,
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        ctx.set_time_intervals_from(0, &[i]);
        ctx.settings.local_time_offset_seconds = -5 * 3600;
        ctx.settings.current_date = Some("2026-10-04".into());
        assert_eq!(ctx.date_visibility(), (vec![true], 0, 0));
        for instant in ["20261004T040000Z", "20261003T230000-0500"] {
            ctx.settings.current_datetime = Some(instant.into());
            assert_eq!(ctx.date_visibility(), (vec![false], 1, 0));
        }
    }
}

#[cfg(test)]
mod live_index_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    #[test]
    fn indices_follow_sort_cache_truncate_and_clear_and_fixed_views_do_not_tick() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        for priority in [8, 2] {
            ctx.add_instruction(DrawingInstruction::Line(
                LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)])
                    .with_priority(priority),
            ));
        }
        let i = ferrite_kernel::TemporalInterval::new(
            Some(
                ferrite_kernel::TemporalBounds::new(
                    Some("----0101".into()),
                    Some("----1231".into()),
                )
                .unwrap(),
            ),
            None,
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        let mut commands = ctx.raw_instructions().to_vec();
        commands[0].set_time_intervals(&[i]);
        ctx.set_instructions_from_cache(commands);
        ctx.get_sorted_instructions();
        assert_eq!(ctx.temporal_statuses(), vec![(1, true)]);
        assert!(ctx.next_live_temporal_change().is_some());
        ctx.settings.current_date = Some("2026-10-04".into());
        assert!(ctx.next_live_temporal_change().is_none());
        ctx.settings.current_date = None;
        ctx.truncate_instructions(1);
        ctx.get_sorted_instructions();
        assert!(ctx.temporal_statuses().is_empty());
        ctx.clear_instructions();
        assert!(ctx.next_live_temporal_change().is_none());
    }
}

#[cfg(test)]
mod expired_clock_guard_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    #[test]
    fn expired_finite_intervals_still_require_live_clock_guard() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        assert!(!ctx.has_live_temporal_conditions());
        ctx.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ])));
        let i = ferrite_kernel::TemporalInterval::new(
            None,
            None,
            Some(
                ferrite_kernel::TemporalBounds::new(
                    Some("20000101T000000Z".into()),
                    Some("20000102T000000Z".into()),
                )
                .unwrap(),
            ),
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        ctx.set_time_intervals_from(0, &[i]);
        ctx.get_sorted_instructions();
        assert!(ctx.has_live_temporal_conditions());
        assert!(ctx.next_live_temporal_change().is_none());
        ctx.settings.current_datetime = Some("20261004T000000Z".into());
        assert!(!ctx.has_live_temporal_conditions());
    }
}

#[cfg(test)]
mod geometry_revision_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    #[test]
    fn geometry_lifetime_is_unique_and_invalidates_on_ownership_changes() {
        let mut ctx = RenderContext::new(Viewport::new(960., 640.));
        let another = RenderContext::new(Viewport::new(960., 640.));
        assert_ne!(ctx.geometry_revision(), another.geometry_revision());
        let first = ctx.geometry_revision();
        ctx.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ])));
        let added = ctx.geometry_revision();
        assert_ne!(first, added);
        ctx.get_sorted_instructions();
        ctx.set_viewport(100., 100.);
        ctx.set_bounds(GeoBounds::new(0., 0., 10., 10.));
        ctx.remap_colors(&|_| Color::WHITE);
        ctx.settings.current_date = Some("2026-10-04".into());
        assert_eq!(added, ctx.geometry_revision());
        ctx.truncate_instructions(1);
        assert_eq!(added, ctx.geometry_revision());
        ctx.truncate_instructions(0);
        assert_ne!(added, ctx.geometry_revision());
        let truncated = ctx.geometry_revision();
        ctx.set_instructions_from_cache(Vec::new());
        assert_ne!(truncated, ctx.geometry_revision());
        let replaced = ctx.geometry_revision();
        ctx.clear_instructions();
        assert_ne!(replaced, ctx.geometry_revision());
    }
}

#[cfg(test)]
mod staged_rebuild_tests {
    use super::*;
    #[test]
    fn failed_stage_preserves_geometry_and_display_state_without_cloning_instructions() {
        let mut current = RenderContext::new(Viewport::with_origin(40., 50., 600., 400.));
        current.set_bounds(GeoBounds::new(-2., 48., 1., 51.));
        current.settings.safety_depth = 17.;
        current.background_color = Color::BLACK;
        current.animation_mode = true;
        current.add_instruction(DrawingInstruction::Point(
            crate::PointInstruction::new("OLD".into(), crate::WorldPoint::new(0., 50.))
                .with_feature_id(1),
        ));
        let old = serde_json::to_value(current.raw_instructions()).unwrap();
        let revision = current.geometry_revision;
        let mut staged = current.empty_for_rebuild();
        assert_eq!(staged.instruction_count(), 0);
        assert_ne!(revision, staged.geometry_revision);
        assert_eq!(staged.scaler.viewport.x, 40.);
        assert_eq!(staged.scaler.viewport.y, 50.);
        assert_eq!(staged.settings.safety_depth, 17.);
        assert_eq!(staged.background_color, Color::BLACK);
        assert!(staged.animation_mode);
        staged.add_instruction(DrawingInstruction::Point(
            crate::PointInstruction::new("PARTIAL".into(), crate::WorldPoint::new(0., 50.))
                .with_feature_id(2),
        ));
        drop(staged); // Failed build: the caller does not commit.
        assert_eq!(current.geometry_revision, revision);
        assert_eq!(
            serde_json::to_value(current.raw_instructions()).unwrap(),
            old
        );
        let mut staged = current.empty_for_rebuild();
        staged.add_instruction(DrawingInstruction::Point(
            crate::PointInstruction::new("NEW".into(), crate::WorldPoint::new(0., 50.))
                .with_feature_id(3),
        ));
        current = staged;
        assert_eq!(current.raw_instructions()[0].feature_id(), Some(3));
        assert_ne!(current.geometry_revision, revision);
    }
}

#[cfg(test)]
mod dependency_tests {
    use super::*;
    use crate::{
        AreaInstruction, DrawingDependency, DrawingDependencyGraph, LineInstruction,
        PointInstruction, TextInstruction, WorldPoint,
    };
    #[test]
    fn expanded_primitives_keep_command_identity_through_sort_and_binary_cache() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        let p = WorldPoint::new(0., 0.);
        c.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "root".into(),
            p,
        )));
        let start = c.instruction_count();
        c.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "child".into(),
            p,
        )));
        c.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![p, p])));
        c.add_instruction(DrawingInstruction::Area(AreaInstruction::new(vec![
            p, p, p,
        ])));
        c.add_instruction(DrawingInstruction::Text(TextInstruction::new(
            "label".into(),
            p,
        )));
        let dependency =
            DrawingDependency::new(7, Some("child-id"), Some("root-id"), true).unwrap();
        c.set_dependency_from(start, Some(dependency.clone()));
        assert!(c.raw_instructions()[0].dependency().is_none());
        for i in &c.raw_instructions()[start..] {
            let restored: DrawingInstruction =
                bincode::deserialize(&bincode::serialize(i).unwrap()).unwrap();
            assert_eq!(restored.dependency(), Some(&dependency));
        }
        c.get_sorted_instructions();
        assert_eq!(
            c.raw_instructions()
                .iter()
                .filter(|i| i.dependency() == Some(&dependency))
                .count(),
            4
        );
        let g = DrawingDependencyGraph::compile(
            c.raw_instructions()
                .iter()
                .map(DrawingInstruction::dependency),
        );
        assert_eq!(
            g.resolve(&[true; 5])
                .unwrap()
                .executed
                .iter()
                .filter(|v| **v)
                .count(),
            1
        );
        c.truncate_instructions(2);
        assert_eq!(c.instruction_count(), 2);
        c.clear_instructions();
        assert_eq!(c.instruction_count(), 0);
    }
}

#[cfg(test)]
mod retained_dependency_plan_tests {
    use super::*;
    use crate::{DrawingDependency, DrawingDependencyGraph, PointInstruction, WorldPoint};
    use std::sync::Arc;

    fn point(priority: i32) -> DrawingInstruction {
        let mut p = PointInstruction::new("test".into(), WorldPoint::new(0., 0.));
        p.priority = crate::DisplayPriority(priority);
        DrawingInstruction::Point(p)
    }
    fn fresh_equivalent(c: &RenderContext) {
        let cached = c.dependency_graph();
        let fresh = DrawingDependencyGraph::compile(
            c.raw_instructions()
                .iter()
                .map(DrawingInstruction::dependency),
        );
        assert_eq!(cached.has_parents(), fresh.has_parents());
        // Compare every eligible mask, including absent roots and blocked children.
        for bits in 0..(1usize << c.instruction_count()) {
            let mask: Vec<_> = (0..c.instruction_count())
                .map(|i| bits & (1 << i) != 0)
                .collect();
            let a = cached.resolve(&mask).unwrap();
            assert_eq!(a, fresh.resolve(&mask).unwrap());
            assert_eq!(
                cached.permitted_by_executed(&a.executed),
                fresh.permitted_by_executed(&a.executed)
            );
        }
    }
    #[test]
    fn topology_survives_camera_palette_date_and_noop_changes() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(point(2));
        c.set_dependency_from(0, DrawingDependency::new(3, Some("root"), None, false));
        c.add_instruction(point(1));
        c.set_dependency_from(1, DrawingDependency::new(3, None, Some("root"), false));
        c.get_sorted_instructions();
        let before = c.dependency_graph();
        c.set_bounds(GeoBounds::new(-2., 48., 2., 52.));
        c.set_viewport(1600., 1200.);
        c.remap_colors(&|_| Color::WHITE);
        c.settings.current_date = Some("2026-10-05".into());
        c.truncate_instructions(2);
        c.set_dependency_from(2, DrawingDependency::new(3, None, Some("other"), false));
        c.get_sorted_instructions();
        assert!(Arc::ptr_eq(&before, &c.dependency_graph()));
        fresh_equivalent(&c);
    }
    #[test]
    fn cache_bypass_and_staged_context_preserve_reference_semantics() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(point(1));
        c.set_dependency_from(0, DrawingDependency::new(5, None, Some("absent"), false));
        let cached = c.dependency_graph();
        c.set_dependency_plan_cache_enabled(false);
        assert!(c.dependency_graph.get().is_none());
        assert!(!Arc::ptr_eq(&c.dependency_graph(), &c.dependency_graph()));
        assert!(!c.empty_for_rebuild().dependency_plan_cache_enabled);
        fresh_equivalent(&c);
        c.set_dependency_plan_cache_enabled(true);
        assert!(Arc::ptr_eq(&c.dependency_graph(), &c.dependency_graph()));
        assert!(!Arc::ptr_eq(&cached, &c.dependency_graph()));
        fresh_equivalent(&c);
    }
    #[test]
    fn every_topology_mutation_and_sort_invalidates_the_plan() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        let empty = c.dependency_graph();
        c.add_instruction(point(3));
        let added = c.dependency_graph();
        assert!(!Arc::ptr_eq(&empty, &added));
        c.set_dependency_from(0, DrawingDependency::new(4, Some("root"), None, false));
        let bound = c.dependency_graph();
        assert!(!Arc::ptr_eq(&added, &bound));
        c.add_instruction(point(1));
        c.set_dependency_from(1, DrawingDependency::new(4, None, Some("root"), false));
        let unsorted = c.dependency_graph();
        fresh_equivalent(&c);
        c.get_sorted_instructions();
        assert!(!Arc::ptr_eq(&unsorted, &c.dependency_graph()));
        fresh_equivalent(&c);
        let sorted = c.dependency_graph();
        c.set_dependency_from(0, DrawingDependency::new(4, None, Some("missing"), false));
        assert!(!Arc::ptr_eq(&sorted, &c.dependency_graph()));
        fresh_equivalent(&c);
        let old = c.dependency_graph();
        c.truncate_instructions(1);
        assert!(!Arc::ptr_eq(&old, &c.dependency_graph()));
        fresh_equivalent(&c);
        let old = c.dependency_graph();
        c.set_instructions_from_cache(vec![point(0)]);
        assert!(!Arc::ptr_eq(&old, &c.dependency_graph()));
        fresh_equivalent(&c);
        let old = c.dependency_graph();
        c.clear_instructions();
        assert!(!Arc::ptr_eq(&old, &c.dependency_graph()));
        assert_eq!(old.len(), 1); // In-flight owner remains valid without copying.
        fresh_equivalent(&c);
        assert!(!Arc::ptr_eq(
            &old,
            &c.empty_for_rebuild().dependency_graph()
        ));
    }
}

#[cfg(test)]
mod scene_spatial_tests {
    use super::*;
    use crate::{AreaInstruction, DrawingInstruction, WorldPoint};
    fn area(x: f64) -> DrawingInstruction {
        DrawingInstruction::Area(AreaInstruction::new(vec![
            WorldPoint::new(x, 0.),
            WorldPoint::new(x + 1., 0.),
            WorldPoint::new(x + 1., 1.),
        ]))
    }
    #[test]
    fn camera_and_palette_reuse_but_order_and_content_invalidate() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(area(0.));
        c.get_sorted_instructions();
        let a = c.scene_spatial_index();
        assert_eq!(a.areas.ids().collect::<Vec<_>>(), vec![0]);
        c.set_viewport(100., 100.);
        c.set_bounds(GeoBounds::new(-5., -5., 5., 5.));
        c.remap_colors(&|_| Color::WHITE);
        assert!(std::sync::Arc::ptr_eq(&a, &c.scene_spatial_index()));
        c.add_instruction(area(10.));
        let b = c.scene_spatial_index();
        assert!(!std::sync::Arc::ptr_eq(&a, &b));
        assert_eq!(b.areas.ids().count(), 2);
        c.get_sorted_instructions();
        assert!(!std::sync::Arc::ptr_eq(&b, &c.scene_spatial_index()));
        c.truncate_instructions(1);
        assert_eq!(c.scene_spatial_index().areas.ids().count(), 1);
        c.set_instructions_from_cache(vec![area(20.)]);
        let b = c.scene_spatial_index();
        let mut ids = vec![];
        b.areas.query(|v| v.max[0] < 5., |id| ids.push(id));
        assert!(ids.is_empty());
        c.clear_instructions();
        assert_eq!(c.scene_spatial_index().areas.ids().count(), 0);
    }
    #[test]
    fn malformed_area_is_not_silently_indexed_or_discarded() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(area(f64::NAN));
        c.add_instruction(DrawingInstruction::Area(AreaInstruction::new(vec![])));
        assert_eq!(c.scene_spatial_index().areas.ids().count(), 0);
        assert_eq!(c.instruction_count(), 2);
    }
}

#[cfg(test)]
mod origin_binding_tests {
    use super::*;
    use crate::{
        AreaInstruction, LineInstruction, PointInstruction, PointOriginCrs, PortrayalOrigin,
        TextInstruction, WorldPoint,
    };
    #[test]
    fn command_origin_survives_sorting_expansion_and_binary_cache() {
        let p = WorldPoint::new(127., 35.);
        let source = PortrayalOrigin::feature_point(p).unwrap();
        let mut context = RenderContext::new(Viewport::new(800., 600.));
        context.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "earlier".into(),
            p,
        )));
        let start = context.instruction_count();
        context.add_instruction(DrawingInstruction::Line(
            LineInstruction::new(vec![p, WorldPoint::new(128., 35.)]).with_priority(1),
        ));
        context.add_instruction(DrawingInstruction::Area(
            AreaInstruction::new(vec![p, p, p]).with_priority(2),
        ));
        context.add_instruction(DrawingInstruction::Text(
            TextInstruction::new("expanded".into(), p).with_priority(3),
        ));
        context.set_portrayal_origin_from(start, source.clone());
        assert_eq!(
            context.raw_instructions()[0].portrayal_origin(),
            &PortrayalOrigin::Unspecified
        );
        for instruction in &context.raw_instructions()[start..] {
            assert_eq!(instruction.portrayal_origin(), &source);
        }
        let instructions = context.get_sorted_instructions();
        assert!(matches!(instructions[0], DrawingInstruction::Line(_)));
        let restored: Vec<DrawingInstruction> =
            bincode::deserialize(&bincode::serialize(instructions).unwrap()).unwrap();
        for (a, b) in restored.iter().zip(instructions) {
            assert_eq!(a.portrayal_origin(), b.portrayal_origin());
        }
    }
    #[test]
    fn resolved_symbol_keeps_authored_source_instead_of_its_new_draw_position() {
        let mut point = PointInstruction::new("symbol".into(), WorldPoint::new(127., 35.));
        point.portrayal_origin =
            PortrayalOrigin::augmented_point(PointOriginCrs::Local, [3.2, 0.]).unwrap();
        let resolved = point.resolved_at(WorldPoint::new(128., 36.), Some(90.));
        assert_eq!(resolved.portrayal_origin, point.portrayal_origin);
        assert_ne!(resolved.position, point.position);
    }
}
#[cfg(test)]
mod coverage_binding_tests {
    use super::*;
    use crate::{
        InstructionCoverageClass, PointInstruction, PortrayalOrigin, PreparedCoverage,
        PreparedCoveragePass, WorldPoint,
    };
    use ferrite_kernel::coverage_frame::CoverageFrame;
    use ferrite_kernel::coverage_selection::{
        CoverageFootprint, Region, SelectedCoverage, Selection,
    };
    use ferrite_kernel::scale_policy::CoverageScaleRange;
    fn bind(context: &mut RenderContext) {
        context.get_sorted_instructions();
        let region =
            Region::from_rings(&[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]], &[])
                .unwrap();
        let inventory = [180000, 45000]
            .into_iter()
            .enumerate()
            .map(|(id, min)| CoverageFootprint {
                dataset_id: id,
                coverage_id: id as i64,
                region: region.clone(),
                scales: CoverageScaleRange {
                    minimum_denominator: Some(min),
                    optimum_denominator: min / 2,
                    maximum_denominator: min / 4,
                },
            })
            .collect::<Vec<_>>();
        let selection = Selection {
            display_band: 10,
            coverages: (0..2)
                .map(|i| SelectedCoverage {
                    inventory_index: i,
                    selection_band: 10,
                    selected_to_fill_gap: false,
                })
                .collect(),
            uncovered: Region::from_polygons(vec![]).unwrap(),
        };
        let frame = std::sync::Arc::new(
            CoverageFrame::new(&inventory, &selection, &region, [12, 12], 4096).unwrap(),
        );
        let pass = PreparedCoveragePass::prepare(
            context.raw_instructions(),
            frame,
            |_| Ok(InstructionCoverageClass::Dataset(0)),
            |_, source| {
                let crate::PointOriginGeometry::FeaturePoint(p) = source else {
                    panic!()
                };
                Ok(Some([p.x, p.y]))
            },
        )
        .unwrap();
        let prepared = PreparedCoverage::new(
            context.geometry_revision(),
            context.coverage_view_revision(),
            context.instruction_count(),
            vec![pass],
        )
        .unwrap();
        context.set_prepared_coverage(prepared).unwrap();
    }
    fn context() -> RenderContext {
        let mut context = RenderContext::new(Viewport::new(100., 100.));
        context.zoom_to_fit(GeoBounds::new(0., 0., 12., 12.));
        for (name, x) in [("hidden", 1.), ("visible", 11.)] {
            let mut instruction = DrawingInstruction::Point(PointInstruction::new(
                name.into(),
                WorldPoint::new(500., 500.),
            ));
            instruction.set_portrayal_origin(
                PortrayalOrigin::feature_point(WorldPoint::new(x, 1.)).unwrap(),
            );
            context.add_instruction(instruction);
        }
        context
    }
    #[test]
    fn coverage_stays_active_when_date_filtering_is_disabled_and_picking_matches() {
        let mut context = context();
        context.settings.date_dependent = false;
        bind(&mut context);
        assert_eq!(context.date_visibility().0, vec![true, true]);
        assert_eq!(
            context.portrayal_visibility().unwrap(),
            (vec![false, true], 0, 0)
        );
        assert!(!context.coverage_fragment_visible(0, 0, [11., 1.]).unwrap());
        assert!(context.coverage_fragment_visible(1, 0, [1., 1.]).unwrap());
    }
    #[test]
    fn camera_direct_scaler_and_geometry_changes_reject_old_binding() {
        let mut context = context();
        bind(&mut context);
        context.invalidate_coverage_view();
        assert!(context.portrayal_visibility().is_err());
        bind(&mut context);
        context.scaler.set_pixel_ratio(2.);
        assert!(context.portrayal_visibility().is_err());
        bind(&mut context);
        context.add_instruction(DrawingInstruction::Point(PointInstruction::new(
            "new".into(),
            WorldPoint::new(1., 1.),
        )));
        assert!(context.portrayal_visibility().is_err());
    }
}

#[cfg(test)]
mod coverage_overlay_lifetime_tests {
    use super::*;
    use crate::{DrawingInstruction, PointInstruction, PortrayalOrigin, WorldPoint};
    #[test]
    fn sorted_overlay_removal_preserves_chart_commands() {
        let mut context = RenderContext::new(Viewport::new(64., 40.));
        let mut chart = DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0., 0.))
                .with_feature_id(101)
                .with_cell_index(0)
                .with_priority(20),
        );
        chart.set_portrayal_origin(PortrayalOrigin::NonPoint);
        let mut overlay = DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0., 0.))
                .with_feature_id(202)
                .with_priority(-100),
        );
        overlay.set_portrayal_origin(PortrayalOrigin::CoverageExempt);
        context.add_instruction(chart);
        context.add_instruction(overlay);
        assert_eq!(context.get_sorted_instructions()[0].feature_id(), Some(202));
        let revision = context.geometry_revision();
        context.remove_coverage_exempt_instructions();
        assert_ne!(revision, context.geometry_revision());
        assert_eq!(context.instruction_count(), 1);
        assert_eq!(context.raw_instructions()[0].feature_id(), Some(101));
        let origin: PortrayalOrigin =
            bincode::deserialize(&bincode::serialize(&PortrayalOrigin::CoverageExempt).unwrap())
                .unwrap();
        assert_eq!(origin, PortrayalOrigin::CoverageExempt);
    }
    #[test]
    fn required_frame_failure_cannot_fall_back_to_unclipped_rendering_or_picking() {
        let mut context = RenderContext::new(Viewport::new(64., 40.));
        context.require_prepared_coverage();
        assert!(context.prepared_coverage_binding().is_err());
        assert!(context.portrayal_visibility().is_err());
        assert!(context.coverage_fragment_visible(0, 0, [1., 1.]).is_err());
        context.clear_prepared_coverage();
        assert!(context.prepared_coverage_binding().unwrap().is_none());
    }
}

#[cfg(test)]
mod indexed_date_visibility_controls {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    fn original_full_scan(context: &RenderContext) -> (Vec<bool>, usize, usize) {
        if !context.settings.date_dependent {
            return (vec![true; context.instructions.len()], 0, 0);
        }
        let Some(local_offset) =
            chrono::FixedOffset::east_opt(context.settings.local_time_offset_seconds)
        else {
            return (
                vec![true; context.instructions.len()],
                0,
                context
                    .instructions
                    .iter()
                    .filter(|i| !i.time_intervals().is_empty())
                    .count(),
            );
        };
        let selected = if let Some(value) = context.settings.current_datetime.as_deref() {
            ferrite_kernel::parse_viewing_instant(value)
        } else if let Some(value) = context.settings.current_date.as_deref() {
            ferrite_kernel::parse_viewing_date(value).and_then(|date| {
                ferrite_kernel::parse_viewing_instant(&format!(
                    "{}T00:00:00{}",
                    date.format("%Y-%m-%d"),
                    local_offset
                ))
            })
        } else {
            Ok(chrono::Utc::now().fixed_offset())
        };
        let instant = match selected {
            Ok(value) => value,
            Err(_) => {
                return (
                    vec![true; context.instructions.len()],
                    0,
                    context
                        .instructions
                        .iter()
                        .filter(|i| !i.time_intervals().is_empty())
                        .count(),
                )
            }
        };
        let mut hidden = 0;
        let mut diagnostics = 0;
        let visibility = context
            .instructions
            .iter()
            .map(|i| {
                match ferrite_kernel::temporal_intervals_visible_with_offset(
                    i.time_intervals(),
                    &instant,
                    local_offset,
                ) {
                    Ok(v) => {
                        if !v {
                            hidden += 1;
                        }
                        v
                    }
                    Err(_) => {
                        diagnostics += 1;
                        true
                    }
                }
            })
            .collect();
        (visibility, hidden, diagnostics)
    }

    fn annual() -> ferrite_kernel::TemporalInterval {
        ferrite_kernel::TemporalInterval::new(
            Some(
                ferrite_kernel::TemporalBounds::new(
                    Some("----1101".into()),
                    Some("----0331".into()),
                )
                .unwrap(),
            ),
            None,
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap()
    }
    fn context() -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(960., 640.));
        c.settings.current_date = Some("2026-07-15".into());
        for index in 0..128 {
            let mut i = DrawingInstruction::Line(
                LineInstruction::new(vec![
                    WorldPoint::new(index as f64, 0.),
                    WorldPoint::new(index as f64, 1.),
                ])
                .with_priority(127 - index),
            );
            if index == 1 || index == 91 {
                i.set_time_intervals(&[annual()]);
            }
            if index == 91 {
                i.set_portrayal_origin(crate::PortrayalOrigin::CoverageExempt);
            }
            c.add_instruction(i);
        }
        c
    }
    fn check(c: &RenderContext) {
        assert_eq!(c.date_visibility(), original_full_scan(c));
    }
    #[test]
    fn sparse_dirty_and_sorted_entries_preserve_mask_counts_and_source_order() {
        let mut c = context();
        assert!(c.temporal_index_dirty);
        check(&c);
        assert_eq!(c.date_visibility().1, 2);
        c.get_sorted_instructions();
        assert!(!c.temporal_index_dirty);
        assert_eq!(c.temporal_indices.len(), 2);
        check(&c);
        c.settings.current_date = Some("2026-01-15".into());
        check(&c);
        assert_eq!(c.date_visibility().1, 0);
        c.settings.date_dependent = false;
        check(&c);
        assert!(c.date_visibility().0.iter().all(|v| *v));
    }
    #[test]
    fn invalid_view_offset_and_selector_diagnostics_match_original_exactly() {
        let mut c = context();
        c.get_sorted_instructions();
        for offset in [0, 9 * 3600, -7 * 3600, 61, 86400] {
            c.settings.local_time_offset_seconds = offset;
            for date in ["2026-07-15", "bad-date", "2026-02-30"] {
                c.settings.current_date = Some(date.into());
                check(&c);
            }
        }
        c.settings.local_time_offset_seconds = 0;
        c.settings.current_date = Some("2026-07-15".into());
        c.settings.current_datetime = Some("2026-01-15T23:59:59-07:00".into());
        check(&c);
        c.settings.current_datetime = Some("bad-instant".into());
        check(&c);
        c.settings.current_datetime = None;
        let invalid = ferrite_kernel::TemporalInterval::new(
            None,
            Some(
                ferrite_kernel::TemporalBounds::new(Some("250000".into()), Some("130000".into()))
                    .unwrap(),
            ),
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        c.set_time_intervals_from(127, std::slice::from_ref(&invalid));
        check(&c);
        c.get_sorted_instructions();
        check(&c);
        assert_eq!(c.date_visibility().2, 1);
        let mixed = ferrite_kernel::TemporalInterval::new(
            annual().date,
            invalid.time,
            None,
            ferrite_kernel::IntervalClosure::Closed,
        )
        .unwrap();
        c.set_time_intervals_from(127, &[mixed]);
        check(&c);
        c.get_sorted_instructions();
        check(&c);
        assert_eq!(c.date_visibility().2, 1);
    }
    #[test]
    fn mutations_rebuild_ordinals_without_reusing_visibility_or_errors() {
        let mut c = context();
        c.get_sorted_instructions();
        check(&c);
        c.remove_coverage_exempt_instructions();
        assert!(c.temporal_index_dirty);
        check(&c);
        c.get_sorted_instructions();
        assert_eq!(c.temporal_indices.len(), 1);
        check(&c);
        c.set_time_intervals_from(126, &[annual()]);
        check(&c);
        c.get_sorted_instructions();
        check(&c);
        c.truncate_instructions(16);
        check(&c);
        c.get_sorted_instructions();
        check(&c);
        let mut replacement = c.raw_instructions().to_vec();
        replacement.reverse();
        c.set_instructions_from_cache(replacement);
        check(&c);
        c.get_sorted_instructions();
        check(&c);
        c.clear_instructions();
        assert!(c.temporal_indices.is_empty());
        assert!(!c.temporal_index_dirty);
        assert_eq!(c.date_visibility(), (vec![], 0, 0));
        check(&c);
    }
    #[test]
    fn empty_selector_index_keeps_all_visible_including_invalid_view() {
        let mut c = RenderContext::new(Viewport::new(960., 640.));
        c.settings.current_date = Some("bad-date".into());
        for _ in 0..512 {
            c.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
                WorldPoint::new(0., 0.),
                WorldPoint::new(1., 1.),
            ])));
        }
        check(&c);
        c.get_sorted_instructions();
        assert!(c.temporal_indices.is_empty());
        check(&c);
        assert_eq!(c.date_visibility(), (vec![true; 512], 0, 0));
        c.settings.current_date = None;
        c.settings.local_time_offset_seconds = 61;
        check(&c);
        c.settings.date_dependent = false;
        check(&c);
    }
}

#[cfg(test)]
mod static_source_classification_tests {
    use super::*;
    use std::sync::Arc;
    fn point(priority: i32, fixed: bool) -> DrawingInstruction {
        let mut p = crate::PointInstruction::new("test".into(), crate::WorldPoint::new(0., 0.));
        p.priority = crate::DisplayPriority(priority);
        if fixed {
            p.portrayal_origin =
                crate::PortrayalOrigin::augmented_point(crate::PointOriginCrs::Portrayal, [1., 2.])
                    .unwrap();
        }
        DrawingInstruction::Point(p)
    }
    fn oracle(c: &RenderContext) {
        let x = c.static_source_classification();
        for (i, p) in c.raw_instructions().iter().enumerate() {
            assert_eq!(x.is_device_fixed(i), p.portrayal_origin().is_device_fixed());
        }
        assert!(!x.is_device_fixed(c.instruction_count()));
        let expected = c.raw_instructions().iter().any(|p| {
            p.portrayal_origin().requires_view_reprojection()
                || match p {
                    DrawingInstruction::Point(p) => {
                        p.line_placement.is_some()
                            || p.rotation_crs == crate::RotationCrs::Geographic
                            || p.curve_tangent_bearing.is_some()
                    }
                    DrawingInstruction::Text(t) => {
                        t.rotation_crs == crate::RotationCrs::Geographic
                            || t.curve_tangent_bearing.is_some()
                    }
                    _ => false,
                }
        });
        assert_eq!(x.requires_view_reprojection(), expected);
    }
    #[test]
    fn ordinal_sort_and_origin_changes_do_not_reuse_stale_bits() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(point(3, true));
        c.add_instruction(point(1, false));
        let before = c.static_source_classification();
        let revision = c.geometry_revision();
        assert!(before.is_device_fixed(0));
        c.get_sorted_instructions();
        assert_eq!(revision, c.geometry_revision());
        let after = c.static_source_classification();
        assert!(!Arc::ptr_eq(&before, &after));
        assert!(!after.is_device_fixed(0));
        assert!(after.is_device_fixed(1));
        oracle(&c);
        c.set_portrayal_origin_from(0, crate::PortrayalOrigin::NonPoint);
        assert!(!Arc::ptr_eq(&after, &c.static_source_classification()));
        oracle(&c);
        assert!(before.is_device_fixed(0)); // Previous prepared owner stays immutable.
    }
    #[test]
    fn camera_palette_settings_and_coverage_do_not_cache_execution_decisions() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.add_instruction(point(1, true));
        c.get_sorted_instructions();
        let before = c.static_source_classification();
        c.set_bounds(GeoBounds::new(-2., 48., 2., 52.));
        c.set_viewport(1600., 1200.);
        c.remap_colors(&|_| Color::WHITE);
        c.settings.safety_depth = 12.0;
        c.viewing_groups.disable_all();
        c.invalidate_coverage_view();
        assert!(Arc::ptr_eq(&before, &c.static_source_classification()));
        oracle(&c);
        assert!(!Arc::ptr_eq(
            &before,
            &c.empty_for_rebuild().static_source_classification()
        ));
    }
    #[test]
    fn insert_truncate_replace_clear_and_overlay_removal_invalidate() {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        let old = c.static_source_classification();
        c.add_instruction(point(1, true));
        assert!(!Arc::ptr_eq(&old, &c.static_source_classification()));
        let old = c.static_source_classification();
        c.truncate_instructions(0);
        assert!(!Arc::ptr_eq(&old, &c.static_source_classification()));
        c.set_instructions_from_cache(vec![point(0, true)]);
        oracle(&c);
        let old = c.static_source_classification();
        c.set_portrayal_origin_from(0, crate::PortrayalOrigin::CoverageExempt);
        c.remove_coverage_exempt_instructions();
        assert!(!Arc::ptr_eq(&old, &c.static_source_classification()));
        oracle(&c);
        c.add_instruction(point(1, true));
        let old = c.static_source_classification();
        c.clear_instructions();
        assert!(!Arc::ptr_eq(&old, &c.static_source_classification()));
        oracle(&c);
    }
    #[test]
    fn rotation_and_tangent_require_fresh_view_even_without_fixed_sources() {
        let mut p = crate::PointInstruction::new("test".into(), crate::WorldPoint::new(0., 0.));
        p.rotation_crs = crate::RotationCrs::Geographic;
        let mut c = RenderContext::new(Viewport::new(1., 1.));
        c.add_instruction(DrawingInstruction::Point(p));
        assert!(c
            .static_source_classification()
            .requires_view_reprojection());
        oracle(&c);
    }
}

#[cfg(test)]
mod static_instruction_order_identity_controls {
    use super::*;
    use std::sync::Arc;
    fn point(priority: i32) -> DrawingInstruction {
        let mut p = crate::PointInstruction::new("A".into(), crate::WorldPoint::new(0., 0.));
        p.priority = crate::DisplayPriority(priority);
        DrawingInstruction::Point(p)
    }
    #[test]
    fn sort_insert_truncate_replace_clear_cannot_reuse_token() {
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        let old = c.static_instruction_order_identity();
        c.add_instruction(point(3));
        assert!(!Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
        c.add_instruction(point(1));
        let old = c.static_instruction_order_identity();
        let revision = c.geometry_revision();
        c.get_sorted_instructions();
        assert_eq!(revision, c.geometry_revision());
        assert!(!Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
        let old = c.static_instruction_order_identity();
        c.truncate_instructions(1);
        assert!(!Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
        let old = c.static_instruction_order_identity();
        c.set_instructions_from_cache(vec![point(1)]);
        assert!(!Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
        let old = c.static_instruction_order_identity();
        c.clear_instructions();
        assert!(!Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
    }
    #[test]
    fn camera_settings_keep_identity_but_palette_retires_without_classification() {
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        c.add_instruction(point(1));
        c.get_sorted_instructions();
        let old = c.static_instruction_order_identity();
        assert!(c.static_source_classification.get().is_none());
        c.set_bounds(GeoBounds::new(-1., -1., 1., 1.));
        c.set_viewport(20., 20.);
        c.settings.safety_depth = 12.;
        c.invalidate_coverage_view();
        assert!(Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
        c.remap_colors(&|_| Color::WHITE);
        assert!(!Arc::ptr_eq(&old, &c.static_instruction_order_identity()));
        assert!(c.static_source_classification.get().is_none());
        assert!(!Arc::ptr_eq(
            &old,
            &c.empty_for_rebuild().static_instruction_order_identity()
        ));
    }
}

#[cfg(test)]
mod palette_admission_identity_controls {
    use super::*;
    #[test]
    fn alpha_remapping_changes_admission_token_not_geometry_or_order() {
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        let mut line = crate::LineInstruction::new(vec![
            crate::WorldPoint::new(-0., 1.),
            crate::WorldPoint::new(2., 3.),
        ]);
        line.color_token = Some("LINE".into());
        c.add_instruction(DrawingInstruction::Line(line));
        c.get_sorted_instructions();
        let geometry = c.geometry_revision();
        let relation = c.static_line_relation_epoch();
        let area = c.static_area_geometry_epoch();
        for alpha in [1., 0., -1., f32::NAN, f32::INFINITY, 1.] {
            let old = c.static_instruction_order_identity();
            c.remap_colors(&|_| Color::rgba(0., 0., 0., alpha));
            assert!(!std::sync::Arc::ptr_eq(
                &old,
                &c.static_instruction_order_identity()
            ));
            assert_eq!(c.geometry_revision(), geometry);
            assert_eq!(c.static_line_relation_epoch(), relation);
            assert_eq!(c.static_area_geometry_epoch(), area);
            assert!(c.instructions_are_sorted());
            if let DrawingInstruction::Line(line) = &c.raw_instructions()[0] {
                assert_eq!(line.points[0].x.to_bits(), (-0f64).to_bits());
                assert_eq!(
                    line.style.has_visible_stroke(),
                    alpha.is_finite() && alpha > 0.
                );
            } else {
                panic!("original line preserved");
            }
        }
    }
    #[test]
    fn failed_color_lookup_already_retired_old_admission_identity() {
        let mut c = RenderContext::new(Viewport::new(10., 10.));
        let mut line = crate::LineInstruction::new(vec![]);
        line.color_token = Some("LINE".into());
        c.add_instruction(DrawingInstruction::Line(line));
        let old = c.static_instruction_order_identity();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.remap_colors(&|_| panic!("injected lookup failure"))
        }));
        assert!(result.is_err());
        assert!(!std::sync::Arc::ptr_eq(
            &old,
            &c.static_instruction_order_identity()
        ));
    }
}

#[cfg(test)]
mod empty_overlay_presence_tests {
    use super::*;
    use crate::{DrawingInstruction, PointInstruction, PortrayalOrigin, WorldPoint};
    fn command(id: i64, exempt: bool) -> DrawingInstruction {
        let mut c = DrawingInstruction::Point(
            PointInstruction::new("ACHBRT07".into(), WorldPoint::new(0., 0.)).with_feature_id(id),
        );
        c.set_portrayal_origin(if exempt {
            PortrayalOrigin::CoverageExempt
        } else {
            PortrayalOrigin::NonPoint
        });
        c
    }
    fn check(c: &RenderContext) {
        assert!(
            c.may_have_coverage_exempt_instructions()
                || !c
                    .raw_instructions()
                    .iter()
                    .any(|p| matches!(p.portrayal_origin(), PortrayalOrigin::CoverageExempt))
        );
    }
    #[test]
    fn verified_empty_repeated_removal_preserves_bytes_revision_and_sorted_identity() {
        let mut c = RenderContext::new(Viewport::new(64., 40.));
        c.coverage_exempt_empty_fast_path = true;
        for i in 0..256 {
            c.add_instruction(command(i, false));
        }
        c.get_sorted_instructions();
        let before = bincode::serialize(c.raw_instructions()).unwrap();
        let revision = c.geometry_revision();
        let identity = c.static_instruction_order_identity();
        assert!(!c.may_have_coverage_exempt_instructions());
        for _ in 0..8 {
            c.remove_coverage_exempt_instructions();
            check(&c);
        }
        assert_eq!(before, bincode::serialize(c.raw_instructions()).unwrap());
        assert_eq!(revision, c.geometry_revision());
        assert!(c.instructions_are_sorted());
        assert!(std::sync::Arc::ptr_eq(
            &identity,
            &c.static_instruction_order_identity()
        ));
    }
    #[test]
    fn original_opt_out_and_candidate_paths_are_exact_for_all_mutation_routes() {
        let mut original = RenderContext::new(Viewport::new(64., 40.));
        let mut candidate = original.empty_for_rebuild();
        original.coverage_exempt_empty_fast_path = false;
        candidate.coverage_exempt_empty_fast_path = true;
        for c in [&mut original, &mut candidate] {
            c.add_instruction(command(1, false));
            c.add_instruction(command(2, true));
            c.get_sorted_instructions();
            c.remove_coverage_exempt_instructions();
            c.set_portrayal_origin_from(0, PortrayalOrigin::CoverageExempt);
            c.remove_coverage_exempt_instructions();
            c.set_instructions_from_cache(vec![command(3, false)]);
            c.remove_coverage_exempt_instructions();
            c.add_instruction(command(4, true));
            c.truncate_instructions(1);
            c.remove_coverage_exempt_instructions();
        }
        assert_eq!(
            bincode::serialize(original.raw_instructions()).unwrap(),
            bincode::serialize(candidate.raw_instructions()).unwrap()
        );
        assert!(original.may_have_coverage_exempt_instructions());
        assert!(!candidate.may_have_coverage_exempt_instructions());
    }
    #[test]
    fn added_retagged_imported_and_truncated_overlay_cannot_escape_removal() {
        let mut c = RenderContext::new(Viewport::new(64., 40.));
        c.coverage_exempt_empty_fast_path = true;
        c.add_instruction(command(1, false));
        c.add_instruction(command(2, true));
        check(&c);
        c.get_sorted_instructions();
        c.remove_coverage_exempt_instructions();
        assert_eq!(
            c.raw_instructions()
                .iter()
                .map(|i| i.feature_id())
                .collect::<Vec<_>>(),
            vec![Some(1)]
        );
        check(&c);
        c.set_portrayal_origin_from(0, PortrayalOrigin::CoverageExempt);
        check(&c);
        c.remove_coverage_exempt_instructions();
        assert_eq!(c.instruction_count(), 0);
        c.set_instructions_from_cache(vec![command(3, true), command(4, false)]);
        check(&c);
        c.remove_coverage_exempt_instructions();
        assert_eq!(c.instruction_count(), 1);
        check(&c);
        c.add_instruction(command(5, true));
        c.truncate_instructions(1);
        check(&c);
        let rev = c.geometry_revision();
        c.remove_coverage_exempt_instructions();
        assert_eq!(c.geometry_revision(), rev);
        check(&c);
        let child = c.empty_for_rebuild();
        assert!(!child.may_have_coverage_exempt_instructions());
        assert!(child.coverage_exempt_empty_fast_path);
        c.clear_instructions();
        check(&c);
        assert!(!c.may_have_coverage_exempt_instructions());
    }
}

#[cfg(test)]
mod emission_fork_tests {
    use super::*;
    use crate::{LineInstruction, WorldPoint};
    fn line(priority: i32) -> DrawingInstruction {
        DrawingInstruction::Line(
            LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)])
                .with_priority(priority),
        )
    }
    #[test]
    fn fork_shares_geometry_and_identities_without_copying() {
        let mut context = RenderContext::new(Viewport::new(1000., 1000.));
        context.add_instruction(line(2));
        context.add_instruction(line(1));
        context.get_sorted_instructions();
        let order = context.static_instruction_order_identity();
        let fork = context.fork_for_emission();
        assert!(fork.shares_instructions_with(&context));
        assert!(fork.instructions_are_sorted());
        assert_eq!(fork.geometry_revision, context.geometry_revision);
        assert_eq!(
            fork.static_line_relation_epoch(),
            context.static_line_relation_epoch()
        );
        assert!(std::sync::Arc::ptr_eq(
            &fork.static_instruction_order_identity(),
            &order
        ));
    }
    #[test]
    fn mutation_after_fork_copies_and_retires_identity_on_that_owner_only() {
        let mut context = RenderContext::new(Viewport::new(1000., 1000.));
        context.add_instruction(line(1));
        let fork = context.fork_for_emission();
        let revision = fork.geometry_revision;
        context.add_instruction(line(3));
        assert!(!fork.shares_instructions_with(&context));
        assert_eq!(fork.raw_instructions().len(), 1);
        assert_eq!(context.raw_instructions().len(), 2);
        assert_eq!(fork.geometry_revision, revision);
        assert_ne!(context.geometry_revision, revision);
    }
}
