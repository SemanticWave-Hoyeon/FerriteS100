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
    /// Process-unique identity for the lifetime of this instruction geometry.
    geometry_revision: u64,
    static_line_relation_epoch:crate::StaticLineRelationEpoch,
    static_area_geometry_epoch:crate::StaticAreaGeometryEpoch,
    /// Immutable command topology; camera, palette and date changes do not rebuild it.
    dependency_graph: std::sync::OnceLock<std::sync::Arc<crate::DrawingDependencyGraph>>,
    dependency_plan_cache_enabled: bool,
    prepared_coverage: Option<std::sync::Arc<crate::PreparedCoverage>>,
    coverage_required: bool,
    coverage_visibility_fusion_enabled: bool,
    coverage_view_revision: u64,
    coverage_scaler_signature: Option<[u64; 15]>,
    scene_spatial: std::sync::OnceLock<std::sync::Arc<crate::SceneSpatialIndex>>,
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
            instructions: Vec::new(),
            feature_ids: HashSet::new(),
            sorted: false,
            geometry_revision: next_geometry_revision(),
            static_line_relation_epoch:crate::StaticLineRelationEpoch::fresh(),
            static_area_geometry_epoch:crate::StaticAreaGeometryEpoch::fresh(),
            dependency_graph: std::sync::OnceLock::new(),
            dependency_plan_cache_enabled: true,
            prepared_coverage: None,
            coverage_required: false,
            coverage_visibility_fusion_enabled: false,
            coverage_view_revision: next_geometry_revision(),
            coverage_scaler_signature: None,
            scene_spatial: std::sync::OnceLock::new(),
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
        next
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

        self.instructions.push(instruction);
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
        self.dependency_graph.take();
        self.scene_spatial.take();
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
    pub fn instructions_are_sorted(&self)->bool {self.sorted}
    pub fn get_sorted_instructions(&mut self) -> &[DrawingInstruction] {
        // Animation must retain the same portrayal order as a stationary view.
        if !self.sorted {
            self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
            // Use stable sort to ensure consistent symbol decluttering
            // Unstable sorts can reorder same-priority instructions differently each frame
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.instructions.sort_by_key(|i| i.render_order());
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
        self.instructions.clear();
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
        self.dependency_graph.take();
        self.scene_spatial.take();
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
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
        for instruction in self.instructions.iter_mut().skip(start) {
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
        }
        for instruction in self.instructions.iter_mut().skip(start) {
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
        for instruction in self.instructions.iter_mut().skip(start) {
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
        self.coverage_visibility_fusion_enabled=enabled;
    }
    pub fn portrayal_visibility(&self) -> crate::error::Result<(Vec<bool>, usize, usize)> {
        let (mut visible, hidden, diagnostics) = self.date_visibility();
        if let Some(coverage) = self.prepared_coverage()? {
            if self.coverage_visibility_fusion_enabled {
                coverage.intersect_visibility(self.geometry_revision,self.coverage_view_revision,&mut visible)?;
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
                self.instructions
                    .iter()
                    .filter(|i| !i.time_intervals().is_empty())
                    .count(),
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
                    self.instructions
                        .iter()
                        .filter(|i| !i.time_intervals().is_empty())
                        .count(),
                )
            }
        };
        let mut hidden = 0;
        let mut diagnostics = 0;
        let visibility = self
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

    /// Remove explicit host overlays even after they interleave with chart commands.
    pub fn remove_coverage_exempt_instructions(&mut self) {
        let before = self.instructions.len();
        self.instructions
            .retain(|i| !matches!(i.portrayal_origin(), crate::PortrayalOrigin::CoverageExempt));
        if before != self.instructions.len() {
            self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
            self.dependency_graph.take();
            self.scene_spatial.take();
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
            self.instructions.truncate(count);
            self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
            self.dependency_graph.take();
            self.scene_spatial.take();
            self.temporal_index_dirty = true;
            self.sorted = false;
        }
    }

    /// Remap all instruction colors using a token-to-color lookup function.
    /// Used for color profile switch (Day/Dusk/Night) without re-running Lua.
    pub fn remap_colors(&mut self, lookup: &dyn Fn(&str) -> Color) {
        for inst in &mut self.instructions {
            inst.remap_colors(lookup);
        }
    }

    /// Independent immutable area topology identity; never geometry ownership.
    pub fn static_area_geometry_epoch(&self)->crate::StaticAreaGeometryEpoch {self.static_area_geometry_epoch}
    pub fn inherit_static_area_geometry_from(&mut self,previous:&Self)->bool {
        if !self.sorted || !previous.sorted || !crate::area_relation_identity::same_area_inputs(&self.instructions,&previous.instructions) {return false}
        self.static_area_geometry_epoch=previous.static_area_geometry_epoch;true
    }

    /// Read-only relation identity, independent from owned geometry lifetime.
    pub fn static_line_relation_epoch(&self)->crate::StaticLineRelationEpoch{self.static_line_relation_epoch}
    /// Once per staged publication. Never digest/pointer equality and never per-frame.
    /// Both contexts must already have the same stable ordinal sorting applied.
    pub fn inherit_static_line_relations_from(&mut self,previous:&Self)->bool{
        if !self.sorted||!previous.sorted||!crate::line_relation_identity::same_static_line_relation_inputs(&self.instructions,&previous.instructions){return false}
        self.static_line_relation_epoch=previous.static_line_relation_epoch;true
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

    /// Get raw instructions slice for cache serialization
    pub fn raw_instructions(&self) -> &[DrawingInstruction] {
        &self.instructions
    }

    /// Set instructions from a pre-built cache (skips viewing group filtering)
    pub fn set_instructions_from_cache(&mut self, instructions: Vec<DrawingInstruction>) {
        self.instructions = instructions;
        self.geometry_revision = next_geometry_revision();
        self.static_line_relation_epoch=crate::StaticLineRelationEpoch::fresh();
        self.static_area_geometry_epoch=crate::StaticAreaGeometryEpoch::fresh();
        self.dependency_graph.take();
        self.scene_spatial.take();
        self.temporal_index_dirty = true;
        self.sorted = false;
        self.feature_ids.clear();
        for inst in &self.instructions {
            if let Some(feature_id) = inst.feature_id() {
                self.feature_ids.insert(feature_id);
            }
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
            g.resolve(&vec![true; 5])
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
