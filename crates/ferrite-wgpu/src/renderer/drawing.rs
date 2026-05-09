//! Convert `DrawingInstruction`s into per-frame vertex/index buffers.
//!
//! This is the largest module in the renderer because it covers every
//! S-101 drawing primitive: areas (with solid/pattern/hatch fills),
//! lines (with dashes / clipping / suppression), symbols (with
//! decluttering / world-coord dedup), and text labels.
//!
//! Bookkeeping pieces also live here:
//! - Triangulation cache (`ensure_triangulated` + helpers): earcut runs
//!   once per polygon and is keyed on world-coord vertices.
//! - Line-suppression cache: one prepass per `add_instructions*` call
//!   identifies lines hidden behind higher-priority lines (S-100 4.8.3).
//! - Symbol classification cache: `classify_symbol` results memoised by
//!   interned `SymbolId`.
//! - Missing-symbol tracking: per-id warning + per-frame counters so the
//!   absence of a symbol surfaces as a logged warning, not a red square
//!   (CLAUDE.md "no placeholder colors" rule).

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet};

use ferrite_portrayal_catalog::ColorProfile;
use ferrite_render::{
    intern_symbol, Color, DrawingInstruction, RenderContext, SymbolId, WorldPoint,
};

use super::{WgpuRenderer, SCREEN_PX_PER_MM};
use crate::pipeline::{PatternVertex, TextureVertex};
use crate::profiler::ScopeTimer;
use crate::renderer_internals::{
    classify_symbol, point_in_ring, CachedTriangulation, PatternTexture, SymbolInstance,
    SymbolTexture, TextLabel, SYM_NAV_AID, SYM_SAFETY, SYM_SOUNDING,
};
use crate::{SymbolCache, Vertex2D};

impl WgpuRenderer {
    /// Clear triangulation cache (call when chart data changes)
    pub fn clear_triangulation_cache(&mut self) {
        self.triangulation_cache.clear();
        self.cached_suppressed_lines = None; // Invalidate when chart data changes
    }

    /// Pre-compute triangulations for all area instructions.
    /// Call after chart load to avoid cold-path stalls during first render frame.
    pub fn precompute_triangulations(
        &mut self,
        instructions: &[ferrite_render::DrawingInstruction],
    ) {
        let mut count = 0;
        for instr in instructions {
            if let ferrite_render::DrawingInstruction::Area(area) = instr {
                if self.ensure_triangulated(area).is_some() {
                    count += 1;
                }
            }
        }
        tracing::info!("Pre-computed {} area triangulations", count);
    }

    /// Clear symbol textures (call when color profile changes)
    pub fn clear_symbol_textures(&mut self) {
        self.symbol_textures.clear();
    }

    /// Pack symbol instances into contiguous vertex/index arrays for single-buffer rendering.
    /// Produces packed_symbol_vertices, packed_symbol_indices, and packed_symbol_ranges.
    pub(super) fn pack_symbol_batch_range(&mut self, start: usize, end: usize) {
        self.packed_symbol_vertices.clear();
        self.packed_symbol_indices.clear();
        self.packed_symbol_ranges.clear();

        // Screen-space guard bounds for symbol culling
        let (vp_w, vp_h) = self.state.viewport_size();
        let sym_guard = 200.0_f32;
        let sym_min_x = -sym_guard;
        let sym_min_y = -sym_guard;
        let sym_max_x = vp_w + sym_guard;
        let sym_max_y = vp_h + sym_guard;

        // First, group by symbol_id using the existing batches map
        self.symbol_batches.clear();
        for instance in &self.symbol_instances[start..end] {
            if let Some(tex) = self.symbol_textures.get(&instance.symbol_id) {
                let display_scale = instance.scale / tex.render_scale * self.symbol_scale;
                let half_w = (tex.width as f32 * display_scale) / 2.0;
                let half_h = (tex.height as f32 * display_scale) / 2.0;
                let pivot_x = tex.pivot_in_texture.0 * display_scale;
                let pivot_y = tex.pivot_in_texture.1 * display_scale;
                let rotation = instance.rotation.to_radians();
                let cos_r = rotation.cos();
                let sin_r = rotation.sin();

                let transform = |dx: f32, dy: f32| -> (f32, f32) {
                    let px = dx + half_w - pivot_x;
                    let py = dy + half_h - pivot_y;
                    let rx = px * cos_r - py * sin_r;
                    let ry = px * sin_r + py * cos_r;
                    (instance.screen_x + rx, instance.screen_y + ry)
                };

                let (x0, y0) = transform(-half_w, -half_h);
                let (x1, y1) = transform(half_w, -half_h);
                let (x2, y2) = transform(half_w, half_h);
                let (x3, y3) = transform(-half_w, half_h);

                // Skip symbols entirely outside viewport guard bounds
                let all_left = x0 < sym_min_x && x1 < sym_min_x && x2 < sym_min_x && x3 < sym_min_x;
                let all_right =
                    x0 > sym_max_x && x1 > sym_max_x && x2 > sym_max_x && x3 > sym_max_x;
                let all_top = y0 < sym_min_y && y1 < sym_min_y && y2 < sym_min_y && y3 < sym_min_y;
                let all_bottom =
                    y0 > sym_max_y && y1 > sym_max_y && y2 > sym_max_y && y3 > sym_max_y;
                if all_left || all_right || all_top || all_bottom {
                    continue;
                }

                let batch = self
                    .symbol_batches
                    .entry(instance.symbol_id)
                    .or_insert_with(|| (Vec::new(), Vec::new()));
                let base = batch.0.len() as u32;
                batch.0.push(TextureVertex::new(x0, y0, 0.0, 0.0));
                batch.0.push(TextureVertex::new(x1, y1, 1.0, 0.0));
                batch.0.push(TextureVertex::new(x2, y2, 1.0, 1.0));
                batch.0.push(TextureVertex::new(x3, y3, 0.0, 1.0));
                batch
                    .1
                    .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }

        // Now pack all batches into contiguous arrays
        for (&sym_id, (verts, idxs)) in &self.symbol_batches {
            let vertex_offset = self.packed_symbol_vertices.len() as u32;
            let index_start = self.packed_symbol_indices.len() as u32;

            self.packed_symbol_vertices.extend_from_slice(verts);
            // Offset indices by vertex_offset
            for &idx in idxs {
                self.packed_symbol_indices.push(idx + vertex_offset);
            }

            let index_count = idxs.len() as u32;
            self.packed_symbol_ranges
                .push((sym_id, index_start, index_count));
        }
    }

    /// Add drawing instructions from render context
    pub fn add_instructions(&mut self, context: &mut RenderContext) {
        self.add_instructions_with_symbols(context, None, None, None);
    }

    /// Add drawing instructions with symbol rendering support
    /// Uses S-101 compliant priority grouping for correct render order
    ///
    /// # Arguments
    /// * `context` - The render context containing drawing instructions
    /// * `symbol_cache` - Optional symbol cache for SVG rendering
    /// * `color_profile` - Optional color profile for symbol coloring
    /// * `visible_viewing_groups` - Optional set of viewing group IDs that should be visible.
    ///   If None, all viewing groups are visible. Used for Display Mode filtering.
    pub fn add_instructions_with_symbols(
        &mut self,
        context: &mut RenderContext,
        mut symbol_cache: Option<&mut SymbolCache>,
        color_profile: Option<&ColorProfile>,
        visible_viewing_groups: Option<&std::collections::HashSet<u32>>,
    ) {
        let profiling = crate::profiler::is_profiling_enabled();
        let total_timer = if profiling {
            Some(ScopeTimer::new("add_instructions_total"))
        } else {
            None
        };

        // Set animation mode and sort instructions, then extract what we need
        context.set_animation_mode(self.animation_mode);
        // get_sorted_instructions() sorts in-place on first call, then returns &slice
        // Clone scaler (cheap: a few f64 fields) to avoid borrow conflict with &mut self methods
        let scaler = context.scaler.clone();
        let instructions = context.get_sorted_instructions();
        let _instruction_count = instructions.len();

        // Update viewport bounds for frustum culling
        self.update_viewport_bounds(&scaler);

        // =====================================================================
        // S-100 Part 9-11.1.9: Line suppression pre-pass
        // =====================================================================
        // When multiple features share the same curve geometry, only the
        // highest-priority LineInstruction is rendered. Lines marked as
        // unsuppressible (LineInstructionUnsuppressed) always render.
        //
        // Build a map from curve geometry hash -> highest priority that claims it.
        // A curve is identified by hashing all its world-coordinate points, so two
        // line instructions referencing the same spatial curve produce the same key.
        // =====================================================================
        // Line suppression: use cached set if instructions haven't changed
        // =====================================================================
        let suppression_timer = if profiling {
            Some(ScopeTimer::new("line_suppression"))
        } else {
            None
        };
        let inst_ptr = instructions.as_ptr() as usize;
        let inst_len = instructions.len();

        // Check if cached suppression set is still valid (same instruction slice)
        let cache_hit = matches!(
            &self.cached_suppressed_lines,
            Some((cached_ptr, cached_len, _)) if *cached_ptr == inst_ptr && *cached_len == inst_len
        );
        if !cache_hit {
            let set = Self::compute_line_suppression(instructions);
            self.cached_suppressed_lines = Some((inst_ptr, inst_len, set));
        }
        // Clone the Arc-like reference for use in the loop (FxHashSet clone is cheap for small sets)
        let suppressed_lines = self.cached_suppressed_lines.as_ref().unwrap().2.clone();
        if let Some(t) = suppression_timer {
            self.cpu_profiler.record("line_suppression", t.elapsed());
        }

        // Pre-compute world→screen transform once for all areas
        let area_transform = Self::scaler_transform(&scaler);

        // Track skipped counts for debugging
        let mut _culled_count = 0usize;

        // Per-type timing accumulators
        let mut area_time = std::time::Duration::ZERO;
        let mut line_time = std::time::Duration::ZERO;
        let mut symbol_time = std::time::Duration::ZERO;
        let mut text_time = std::time::Duration::ZERO;
        let mut area_count = 0u32;
        let mut line_count = 0u32;
        let mut symbol_count = 0u32;
        let mut text_count = 0u32;

        // S-101 Priority tracking: track index ranges per (display_plane, priority)
        let mut current_priority: Option<i32> = None;
        let mut current_plane: u8 = 0; // 0=UnderRadar, 1=OverRadar
        let mut area_start_idx = 0usize;
        let mut line_start_idx = 0usize;
        let mut symbol_start_idx = 0usize;

        // Pre-compute viewing scale once (constant during entire instruction loop)
        let viewing_scale = self.viewing_scale();

        // When zoomed out far beyond the chart's compilation scale, point symbols
        // and text become visually meaningless (the entire chart covers only a few
        // pixels). Suppress them to avoid stray symbols on the world map view.
        // Threshold: 5× the compilation scale (e.g., 1:22000 chart → hide symbols
        // beyond 1:110000).
        let suppress_point_text = viewing_scale > self.compilation_scale.saturating_mul(5);

        for (inst_idx, instruction) in instructions.iter().enumerate() {
            // Display Mode filtering: skip instructions not in visible viewing groups
            if let Some(visible) = visible_viewing_groups {
                let vg = instruction.viewing_group().0;
                if !visible.contains(&vg) {
                    // Allow sounding viewing group (33010) through when show_soundings is enabled
                    if !(self.show_soundings && vg == 33010) {
                        continue;
                    }
                }
            }

            let inst_priority = instruction.priority().0;
            let inst_plane = match instruction.display_plane() {
                ferrite_render::DisplayPlane::UnderRadar => 0u8,
                ferrite_render::DisplayPlane::OverRadar => 1u8,
            };

            // Check if priority or display plane changed - record ranges for previous group
            if let Some(prev_priority) = current_priority {
                if prev_priority != inst_priority || current_plane != inst_plane {
                    // Record area range if any areas were added for previous group
                    if self.area_indices.len() > area_start_idx {
                        self.area_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            area_start_idx,
                            self.area_indices.len(),
                        ));
                    }
                    area_start_idx = self.area_indices.len();

                    // Record line range if any lines were added for previous group
                    if self.line_indices.len() > line_start_idx {
                        self.line_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            line_start_idx,
                            self.line_indices.len(),
                        ));
                    }
                    line_start_idx = self.line_indices.len();

                    // Record symbol range if any symbols were added for previous group
                    if self.symbol_instances.len() > symbol_start_idx {
                        self.symbol_priority_ranges.push((
                            current_plane,
                            prev_priority,
                            symbol_start_idx,
                            self.symbol_instances.len(),
                        ));
                    }
                    symbol_start_idx = self.symbol_instances.len();
                }
            }
            current_priority = Some(inst_priority);
            current_plane = inst_plane;

            // S-100 Scale-dependent visibility: skip instructions outside their scale range
            if !instruction.scale_range().is_visible_at(viewing_scale) {
                continue;
            }

            let inst_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };

            match instruction {
                DrawingInstruction::Area(area) => {
                    // LOD: Skip small areas when zoomed out (animation mode)
                    if self.animation_mode && self.lod_level > 0 {
                        // Skip areas with few points during animation
                        if area.exterior.len() < 10 {
                            _culled_count += 1;
                            continue;
                        }
                    }

                    // Pattern fills: tile symbols inside the polygon area
                    if let ferrite_render::AreaFillType::Pattern {
                        ref symbol_ref,
                        v1,
                        v2,
                    } = area.fill
                    {
                        // Render pattern overlay only when enabled
                        if self.ui_state.settings.show_shallow_pattern {
                            if let (Some(cache), Some(profile)) =
                                (symbol_cache.as_mut(), color_profile)
                            {
                                self.tile_area_with_pattern(
                                    area,
                                    symbol_ref,
                                    v1,
                                    v2,
                                    &scaler,
                                    cache,
                                    profile,
                                    inst_priority,
                                );
                            }
                        }
                    } else if let ferrite_render::AreaFillType::HatchFill {
                        color,
                        width,
                        spacing,
                        angle,
                    } = &area.fill
                    {
                        self.tile_area_with_hatch(
                            area,
                            *color,
                            *width,
                            *spacing,
                            *angle,
                            &scaler,
                            inst_priority,
                        );
                    } else {
                        self.add_area_cached(area, area_transform);
                    }
                    if let Some(s) = inst_start {
                        area_time += s.elapsed();
                        area_count += 1;
                    }
                }
                DrawingInstruction::Line(line) => {
                    // S-100 Part 9-11.1.9: Skip suppressed lines (lower-priority
                    // suppressible lines on curves already claimed by higher priority)
                    if suppressed_lines.contains(&inst_idx) {
                        _culled_count += 1;
                        continue;
                    }
                    // LOD: Skip short lines when zoomed out
                    if self.animation_mode && self.lod_level > 0 && line.points.len() < 5 {
                        _culled_count += 1;
                        continue;
                    }
                    self.add_line(line, &scaler);
                    if let Some(s) = inst_start {
                        line_time += s.elapsed();
                        line_count += 1;
                    }
                }
                DrawingInstruction::Point(point) => {
                    // Suppress point symbols when zoomed out far beyond chart scale
                    if suppress_point_text {
                        continue;
                    }

                    // Frustum culling: skip points outside viewport
                    if !self.is_point_visible(point.position.x, point.position.y) {
                        _culled_count += 1;
                        continue;
                    }

                    // Pre-classify via cached flags (intern is fast: read-lock only)
                    let sym_id = intern_symbol(&point.symbol_ref);
                    let flags = self.get_symbol_flags(sym_id, &point.symbol_ref);
                    let is_sounding = flags & SYM_SOUNDING != 0;

                    // Soundings: respect show_soundings toggle
                    if is_sounding && !self.show_soundings {
                        continue;
                    }

                    // LOD: Skip non-essential symbols during animation
                    if self.animation_mode && self.lod_level > 0 {
                        // Keep only important symbols (soundings, nav aids)
                        if !is_sounding && (flags & SYM_NAV_AID == 0) {
                            _culled_count += 1;
                            continue;
                        }
                    }

                    // Try to render the symbol. We require both the cache and an
                    // active color profile — without a profile, SVG color tokens
                    // cannot be resolved and the rendered symbol would be wrong.
                    let rendered = match (symbol_cache.as_mut(), color_profile) {
                        (Some(cache), Some(profile)) => {
                            self.try_add_symbol(point, &scaler, cache, profile, sym_id)
                        }
                        _ => false,
                    };

                    // CLAUDE.md: "no placeholder colors, no fallbacks". If the symbol
                    // cannot render, surface it as a real error (logged once per id)
                    // and skip — never paint a hardcoded red square.
                    if !rendered {
                        self.note_unrendered_symbol(sym_id, &point.symbol_ref);
                    }
                    if let Some(s) = inst_start {
                        symbol_time += s.elapsed();
                        symbol_count += 1;
                    }
                }
                DrawingInstruction::Text(text) => {
                    // Suppress text when zoomed out far beyond chart scale
                    if suppress_point_text {
                        continue;
                    }

                    // LOD: Skip text during animation for performance
                    if self.animation_mode {
                        continue;
                    }

                    // Frustum culling: skip text outside viewport
                    if !self.is_point_visible(text.position.x, text.position.y) {
                        continue;
                    }

                    // Convert world position to screen coordinates
                    let screen = scaler.world_to_screen(text.position);

                    // S-100 Part 9a-11.2.2.4: FontSize is in typographic points (pt).
                    // S-101 Lua rules emit values like 10 (= 10pt standard body text).
                    // Convert points → pixels: pts * (DPI / 72), where 1pt = 1/72 inch.
                    let dpi_scale = self.state.window.scale_factor() as f32;
                    let screen_dpi = 96.0 * dpi_scale;
                    let font_size_px = (text.font_size * screen_dpi / 72.0).clamp(6.0, 40.0);

                    // Apply offset (in mm from Lua LocalOffset, convert to pixels)
                    let offset_x = text.offset.x * SCREEN_PX_PER_MM * dpi_scale;
                    let offset_y = text.offset.y * SCREEN_PX_PER_MM * dpi_scale;
                    let sx = screen.x + offset_x;
                    let sy = screen.y + offset_y;

                    // Estimate text bounding box for collision avoidance
                    let est_width = font_size_px * 0.6 * text.text.len() as f32;
                    let est_height = font_size_px * 1.3;

                    // Apply alignment offset for collision box
                    let box_x = match text.h_align {
                        ferrite_render::HAlign::Left => sx,
                        ferrite_render::HAlign::Center => sx - est_width * 0.5,
                        ferrite_render::HAlign::Right => sx - est_width,
                    };
                    let box_y = match text.v_align {
                        ferrite_render::VAlign::Top => sy,
                        ferrite_render::VAlign::Middle => sy - est_height * 0.5,
                        ferrite_render::VAlign::Bottom => sy - est_height,
                    };

                    // Collision avoidance: skip if overlapping existing text
                    if !self
                        .text_collision_grid
                        .try_place(box_x, box_y, est_width, est_height)
                    {
                        continue;
                    }

                    self.text_labels.push(TextLabel {
                        screen_x: sx,
                        screen_y: sy,
                        text: text.text.clone(),
                        font_size: font_size_px,
                        color: [text.color.r, text.color.g, text.color.b, text.color.a],
                        bold: text.bold,
                        italic: text.italic,
                        h_align: text.h_align,
                        v_align: text.v_align,
                    });
                    if let Some(s) = inst_start {
                        text_time += s.elapsed();
                        text_count += 1;
                    }
                }
            }
        }

        // Log per-type instruction timing
        if profiling {
            self.cpu_profiler.record("inst_area", area_time);
            self.cpu_profiler.record("inst_line", line_time);
            self.cpu_profiler.record("inst_symbol", symbol_time);
            self.cpu_profiler.record("inst_text", text_time);
            tracing::debug!(
                "[PROFILER] Instructions: area={} ({:.2}ms), line={} ({:.2}ms), symbol={} ({:.2}ms), text={} ({:.2}ms)",
                area_count, area_time.as_secs_f64() * 1000.0,
                line_count, line_time.as_secs_f64() * 1000.0,
                symbol_count, symbol_time.as_secs_f64() * 1000.0,
                text_count, text_time.as_secs_f64() * 1000.0,
            );
        }

        if let Some(t) = total_timer {
            let elapsed = t.elapsed();
            self.cpu_profiler.record("add_instructions_total", elapsed);
            tracing::debug!(
                "[PROFILER] add_instructions_total: {:.2}ms (areas: {}v/{}i, lines: {}v/{}i, symbols: {}, texts: {})",
                elapsed.as_secs_f64() * 1000.0,
                self.area_vertices.len(), self.area_indices.len(),
                self.line_vertices.len(), self.line_indices.len(),
                self.symbol_instances.len(), self.text_labels.len(),
            );
        }

        // Record final priority ranges
        if let Some(final_priority) = current_priority {
            if self.area_indices.len() > area_start_idx {
                self.area_priority_ranges.push((
                    current_plane,
                    final_priority,
                    area_start_idx,
                    self.area_indices.len(),
                ));
            }
            if self.line_indices.len() > line_start_idx {
                self.line_priority_ranges.push((
                    current_plane,
                    final_priority,
                    line_start_idx,
                    self.line_indices.len(),
                ));
            }
            if self.symbol_instances.len() > symbol_start_idx {
                self.symbol_priority_ranges.push((
                    current_plane,
                    final_priority,
                    symbol_start_idx,
                    self.symbol_instances.len(),
                ));
            }
        }
    }

    /// Fast O(1) cache key for area geometry based on slice pointer + length.
    /// Since instructions are borrowed from a stable slice, the exterior Vec's data pointer
    /// uniquely identifies the polygon geometry (same data = same pointer).
    fn area_geometry_key(area: &ferrite_render::AreaInstruction) -> i64 {
        // Use data pointer as unique identifier (stable while instructions slice is alive)
        let ptr = area.exterior.as_ptr() as usize;
        let len = area.exterior.len();
        // Combine pointer and length into a single i64 key
        // Pointer is unique per allocation, length adds extra discrimination
        (ptr as i64) ^ ((len as i64) << 48)
    }

    /// Clean a ring of world points: remove consecutive duplicates and closing duplicate
    fn clean_world_ring(points: &[WorldPoint], epsilon: f64) -> Vec<f64> {
        let mut cleaned: Vec<f64> = Vec::with_capacity(points.len() * 2);
        for p in points {
            if !p.x.is_finite() || !p.y.is_finite() {
                continue;
            }
            // Skip consecutive duplicates
            if cleaned.len() >= 2 {
                let prev_x = cleaned[cleaned.len() - 2];
                let prev_y = cleaned[cleaned.len() - 1];
                if (p.x - prev_x).abs() <= epsilon && (p.y - prev_y).abs() <= epsilon {
                    continue;
                }
            }
            cleaned.push(p.x);
            cleaned.push(p.y);
        }
        // Remove closing duplicate
        if cleaned.len() >= 6 {
            let n = cleaned.len();
            if (cleaned[0] - cleaned[n - 2]).abs() < epsilon * 10.0
                && (cleaned[1] - cleaned[n - 1]).abs() < epsilon * 10.0
            {
                cleaned.truncate(n - 2);
            }
        }
        cleaned
    }

    /// Compute signed area of a ring stored as [x0,y0,x1,y1,...] pairs
    fn ring_signed_area_flat(vertices: &[f64]) -> f64 {
        let n = vertices.len() / 2;
        if n < 3 {
            return 0.0;
        }
        let mut area = 0.0;
        for i in 0..n {
            let j = (i + 1) % n;
            area +=
                (vertices[j * 2] - vertices[i * 2]) * (vertices[j * 2 + 1] + vertices[i * 2 + 1]);
        }
        area
    }

    /// Get or compute cached triangulation for an area polygon.
    /// Triangulation is done in world coordinates so it only needs to run once per unique polygon.
    /// Ensure triangulation is cached for this area, returning the cache key.
    /// Returns None if the area cannot be triangulated.
    fn ensure_triangulated(&mut self, area: &ferrite_render::AreaInstruction) -> Option<i64> {
        let cache_key = Self::area_geometry_key(area);

        // Check cache first
        if self.triangulation_cache.contains_key(&cache_key) {
            return Some(cache_key);
        }

        // World-coordinate epsilon (degrees, ~0.01m precision)
        let epsilon = 1e-8;

        // Clean exterior ring in world coordinates
        let ext_verts = Self::clean_world_ring(&area.exterior, epsilon);
        if ext_verts.len() < 6 {
            return None; // < 3 points
        }

        let exterior_area = Self::ring_signed_area_flat(&ext_verts);
        let exterior_is_cw = exterior_area > 0.0;

        let mut vertices = ext_verts;
        let mut hole_indices: Vec<usize> = Vec::new();

        // Handle interior rings (holes)
        for hole in &area.interiors {
            let mut hole_verts = Self::clean_world_ring(hole, epsilon);
            if hole_verts.len() < 6 {
                continue;
            }

            let hole_area = Self::ring_signed_area_flat(&hole_verts);
            let hole_is_cw = hole_area > 0.0;
            if hole_is_cw == exterior_is_cw {
                // Reverse the hole ring
                let n = hole_verts.len() / 2;
                for i in 0..n / 2 {
                    let j = n - 1 - i;
                    hole_verts.swap(i * 2, j * 2);
                    hole_verts.swap(i * 2 + 1, j * 2 + 1);
                }
            }

            let hole_start = vertices.len() / 2;
            hole_indices.push(hole_start);
            vertices.extend_from_slice(&hole_verts);
        }

        // Triangulate in world coordinates
        let total_vertex_count = vertices.len() / 2;
        let indices = match earcutr::earcut(&vertices, &hole_indices, 2) {
            Ok(idx) if idx.len() >= 3 => idx
                .into_iter()
                .filter(|&i| i < total_vertex_count)
                .collect::<Vec<_>>(),
            _ => {
                // Fan triangulation fallback (exterior only)
                let n = vertices.len().min(area.exterior.len() * 2) / 2;
                if n < 3 {
                    return None;
                }
                let mut fan = Vec::with_capacity((n - 2) * 3);
                for i in 1..(n - 1) {
                    fan.push(0);
                    fan.push(i);
                    fan.push(i + 1);
                }
                fan
            }
        };

        if indices.len() < 3 {
            return None;
        }

        // Compute world AABB for frustum culling
        let mut aabb_min_x = f64::MAX;
        let mut aabb_min_y = f64::MAX;
        let mut aabb_max_x = f64::MIN;
        let mut aabb_max_y = f64::MIN;
        let vc = vertices.len() / 2;
        for i in 0..vc {
            let x = vertices[i * 2];
            let y = vertices[i * 2 + 1];
            if x < aabb_min_x {
                aabb_min_x = x;
            }
            if y < aabb_min_y {
                aabb_min_y = y;
            }
            if x > aabb_max_x {
                aabb_max_x = x;
            }
            if y > aabb_max_y {
                aabb_max_y = y;
            }
        }

        let cached = CachedTriangulation {
            indices,
            world_vertices: vertices,
            world_aabb: (aabb_min_x, aabb_min_y, aabb_max_x, aabb_max_y),
        };

        self.triangulation_cache.insert(cache_key, cached);
        Some(cache_key)
    }

    /// Pre-computed world→screen transform parameters (avoids per-area scaler lookups)
    #[inline]
    fn scaler_transform(scaler: &ferrite_render::Scaler) -> (f64, f64, f64, f64, f64, f64) {
        (
            scaler.scale_x(),
            scaler.scale_y(),
            scaler.offset_x(),
            scaler.offset_y(),
            scaler.geo_bounds.min_x,
            scaler.geo_bounds.max_y,
        )
    }

    fn add_area_cached(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        transform: (f64, f64, f64, f64, f64, f64),
    ) {
        // Get fill color — pattern/centroid/hatch fills are overlays, not solid fills
        let color = match &area.fill {
            ferrite_render::AreaFillType::Solid(c) => c.to_array(),
            ferrite_render::AreaFillType::Pattern { .. }
            | ferrite_render::AreaFillType::HatchFill { .. }
            | ferrite_render::AreaFillType::CentroidSymbol(_) => return,
        };

        let cache_key = Self::area_geometry_key(area);

        // Fast path: triangulation already cached — use cached AABB for O(1) frustum culling
        // (avoids re-scanning entire exterior ring just to compute AABB)
        if let Some(cached) = self.triangulation_cache.get(&cache_key) {
            // Frustum culling with cached AABB
            if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) = self.viewport_world_bounds {
                let (ax, ay, bx, by) = cached.world_aabb;
                let margin_x = (vp_max_x - vp_min_x) * 0.5;
                let margin_y = (vp_max_y - vp_min_y) * 0.5;
                if bx < vp_min_x - margin_x
                    || ax > vp_max_x + margin_x
                    || by < vp_min_y - margin_y
                    || ay > vp_max_y + margin_y
                {
                    return;
                }
            }

            let total_vertex_count = cached.world_vertices.len() / 2;
            let wv_ptr = cached.world_vertices.as_ptr();
            let wv_len = cached.world_vertices.len();
            let idx_ptr = cached.indices.as_ptr();
            let idx_len = cached.indices.len();
            // SAFETY: triangulation_cache is not modified during the loops below,
            // and these pointers remain valid because we don't mutate the cache.
            let wv = unsafe { std::slice::from_raw_parts(wv_ptr, wv_len) };
            let indices = unsafe { std::slice::from_raw_parts(idx_ptr, idx_len) };

            let base_index = self.area_vertices.len() as u32;
            let (scale_x, scale_y, offset_x, offset_y, min_x, max_y) = transform;

            self.area_vertices.reserve(total_vertex_count);
            self.area_vertices.extend((0..total_vertex_count).map(|i| {
                let wx = wv[i * 2];
                let wy = wv[i * 2 + 1];
                let sx = ((wx - min_x) * scale_x + offset_x) as f32;
                let sy = ((max_y - wy) * scale_y + offset_y) as f32;
                Vertex2D::new(sx, sy, color)
            }));

            self.area_indices.reserve(idx_len);
            self.area_indices
                .extend(indices.iter().map(|&i| base_index + i as u32));

            return;
        }

        // Cold path: first-time triangulation — fall back to ring-scan culling
        if !Self::is_ring_visible_static(&area.exterior, self.viewport_world_bounds) {
            return;
        }

        // Ensure triangulation is cached
        if self.ensure_triangulated(area).is_none() {
            return;
        }

        // Recurse once: now the cache is populated, fast path will handle it
        self.add_area_cached(area, transform);
    }

    /// Fill an area polygon with a tiled pattern texture (S-100 standard).
    ///
    /// Uses GPU texture repeat mode (like OpenS100's D2D1_EXTEND_MODE_WRAP):
    /// triangulates the polygon and assigns UV coordinates with optional shear
    /// for parallelogram tiling (S-100 Part 9a: v1/v2 lattice vectors).
    #[allow(clippy::too_many_arguments)]
    fn tile_area_with_pattern(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        symbol_ref: &str,
        v1: (f32, f32),
        v2: (f32, f32),
        scaler: &ferrite_render::Scaler,
        symbol_cache: &mut SymbolCache,
        color_profile: &ColorProfile,
        priority: i32,
    ) {
        // Frustum culling: quick AABB check on exterior ring
        if !Self::is_ring_visible_static(&area.exterior, self.viewport_world_bounds) {
            return;
        }

        // Apply HiDPI scale factor so pattern matches physical mm on screen
        let dpi_scale = self.state.window.scale_factor() as f32;
        let mm_to_px = SCREEN_PX_PER_MM * dpi_scale;

        // S-100: v1 is the horizontal period, v2 defines the row offset
        // Texture tile size = |v1| width × |v2.y| height (rectangular tile)
        // Parallelogram offset = v2.x (horizontal shift per row)
        let v1_len = (v1.0 * v1.0 + v1.1 * v1.1).sqrt();
        let spacing_x_px = (v1_len * mm_to_px).max(4.0);
        let spacing_y_px = (v2.1.abs() * mm_to_px).max(4.0);
        // Shear ratio: how much each row shifts horizontally (in UV units)
        let shear = if v2.1.abs() > 0.001 { v2.0 / v2.1 } else { 0.0 };

        // Ensure pattern texture exists in GPU cache
        let pat_key = format!("{}_pat", symbol_ref);
        if !self.pattern_textures.contains_key(&pat_key) {
            let geom = match symbol_cache.get_symbol_for_pattern(
                symbol_ref,
                color_profile,
                spacing_x_px,
                spacing_y_px,
                mm_to_px,
            ) {
                Some(g) => g,
                None => {
                    tracing::warn!("Pattern fill symbol '{}' not found", symbol_ref);
                    return;
                }
            };
            let tex_w = geom.width;
            let tex_h = geom.height;
            let (texture, view) = self.state.create_texture_from_rgba(
                &geom.pixels,
                tex_w,
                tex_h,
                &format!("pattern_{}", symbol_ref),
            );
            let bind_group = self
                .pipelines
                .create_pattern_bind_group(&self.state.device, &view);
            self.pattern_textures.insert(
                pat_key.clone(),
                PatternTexture {
                    texture,
                    bind_group,
                    width: tex_w,
                    height: tex_h,
                },
            );
        }

        // inv_tile_size: use actual texture pixel dimensions for seamless tiling
        let pat_tex = self.pattern_textures.get(&pat_key).unwrap();
        let inv_tx = 1.0 / pat_tex.width as f32;
        let inv_ty = 1.0 / pat_tex.height as f32;

        // Triangulate the polygon — build earcut coords directly (no intermediate Vec)
        let mut coords: Vec<f64> = Vec::with_capacity(
            (area.exterior.len() + area.interiors.iter().map(|h| h.len()).sum::<usize>()) * 2,
        );
        let mut exterior_count = 0usize;
        for p in &area.exterior {
            let s = scaler.world_to_screen(*p);
            if s.x.is_finite() && s.y.is_finite() {
                coords.push(s.x as f64);
                coords.push(s.y as f64);
                exterior_count += 1;
            }
        }
        if exterior_count < 3 {
            return;
        }

        let mut hole_indices: Vec<usize> = Vec::with_capacity(area.interiors.len());
        for hole in &area.interiors {
            let hole_start = coords.len() / 2;
            let mut hole_count = 0usize;
            for p in hole {
                let s = scaler.world_to_screen(*p);
                if s.x.is_finite() && s.y.is_finite() {
                    coords.push(s.x as f64);
                    coords.push(s.y as f64);
                    hole_count += 1;
                }
            }
            if hole_count >= 3 {
                hole_indices.push(hole_start);
            } else {
                // Remove invalid hole points
                coords.truncate(hole_start * 2);
            }
        }

        let indices = earcutr::earcut(&coords, &hole_indices, 2).unwrap_or_default();

        if indices.is_empty() {
            return;
        }

        // S-100: parallelogram shear ratio (dimensionless).
        // shear = v2.x / v2.y: for each pixel of Y movement, X shifts by shear pixels.
        // The shader computes: u = (pos.x - shear * pos.y) * inv_tx
        let shear_screen = shear;

        // Triangle guard bounds: skip triangles with extreme off-screen vertices
        let extreme_guard = 16000.0_f32;
        let bnd_min_x = -extreme_guard;
        let bnd_min_y = -extreme_guard;
        let bnd_max_x = extreme_guard;
        let bnd_max_y = extreme_guard;

        let base_index = self.pattern_vertices.len() as u32;
        let total_points = coords.len() / 2;
        for i in 0..total_points {
            let x = coords[i * 2] as f32;
            let y = coords[i * 2 + 1] as f32;
            self.pattern_vertices
                .push(PatternVertex::new(x, y, inv_tx, inv_ty, shear_screen));
        }

        let idx_start = self.pattern_indices.len();
        // Skip triangles with any vertex far off-screen to prevent ray artifacts
        for tri in indices.chunks(3) {
            if tri.len() < 3 {
                break;
            }
            let (i0, i1, i2) = (tri[0], tri[1], tri[2]);
            let x0 = coords[i0 * 2] as f32;
            let y0 = coords[i0 * 2 + 1] as f32;
            let x1 = coords[i1 * 2] as f32;
            let y1 = coords[i1 * 2 + 1] as f32;
            let x2 = coords[i2 * 2] as f32;
            let y2 = coords[i2 * 2 + 1] as f32;
            if x0 >= bnd_min_x
                && x0 <= bnd_max_x
                && y0 >= bnd_min_y
                && y0 <= bnd_max_y
                && x1 >= bnd_min_x
                && x1 <= bnd_max_x
                && y1 >= bnd_min_y
                && y1 <= bnd_max_y
                && x2 >= bnd_min_x
                && x2 <= bnd_max_x
                && y2 >= bnd_min_y
                && y2 <= bnd_max_y
            {
                self.pattern_indices.push(base_index + i0 as u32);
                self.pattern_indices.push(base_index + i1 as u32);
                self.pattern_indices.push(base_index + i2 as u32);
            }
        }
        let idx_end = self.pattern_indices.len();

        let plane = match area.display_plane {
            ferrite_render::DisplayPlane::OverRadar => 1u8,
            _ => 0u8,
        };
        self.pattern_ranges
            .push((plane, priority, idx_start, idx_end, pat_key));
    }

    /// S-100 Part 9a hatch fill: render parallel lines inside a polygon area.
    /// Lines are drawn at the specified angle, spacing, and width within the
    /// polygon boundary using line-polygon clipping.
    #[allow(clippy::too_many_arguments)]
    fn tile_area_with_hatch(
        &mut self,
        area: &ferrite_render::AreaInstruction,
        color: Color,
        width: f32,
        spacing_mm: f32,
        angle_deg: f32,
        scaler: &ferrite_render::Scaler,
        _priority: i32,
    ) {
        // Frustum culling: quick AABB check on exterior ring
        if !Self::is_ring_visible_static(&area.exterior, self.viewport_world_bounds) {
            return;
        }

        let dpi_scale = self.state.window.scale_factor() as f32;
        let spacing_px = (spacing_mm * SCREEN_PX_PER_MM * dpi_scale).max(2.0);
        let line_width = (width * SCREEN_PX_PER_MM * dpi_scale).max(0.5);
        let color_arr = color.to_array();

        // Convert polygon exterior to screen coordinates (no clamping — clip_line_to_polygon handles bounds)
        let screen_ring: Vec<(f32, f32)> = area
            .exterior
            .iter()
            .map(|p| {
                let s = scaler.world_to_screen(*p);
                (s.x, s.y)
            })
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .collect();

        if screen_ring.len() < 3 {
            return;
        }

        // Compute bounding box
        let (min_x, min_y, max_x, max_y) = screen_ring.iter().fold(
            (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
            |(mn_x, mn_y, mx_x, mx_y), &(x, y)| {
                (mn_x.min(x), mn_y.min(y), mx_x.max(x), mx_y.max(y))
            },
        );

        // Angle in radians (S-100: 0 = horizontal, CCW positive)
        let angle_rad = angle_deg.to_radians();
        let cos_a = angle_rad.cos();
        let sin_a = angle_rad.sin();

        // Direction perpendicular to the hatch lines (used for spacing)
        let perp_x = -sin_a;
        let perp_y = cos_a;

        // Project bounding box corners onto the perpendicular axis to find range
        let corners = [
            (min_x, min_y),
            (max_x, min_y),
            (max_x, max_y),
            (min_x, max_y),
        ];
        let mut proj_min = f32::MAX;
        let mut proj_max = f32::MIN;
        for &(cx, cy) in &corners {
            let proj = cx * perp_x + cy * perp_y;
            if proj < proj_min {
                proj_min = proj;
            }
            if proj > proj_max {
                proj_max = proj;
            }
        }

        // Diagonal length for extending lines across the entire bounding box
        let diag = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2)).sqrt();

        // Pre-compute viewport bounds for segment culling (hoisted out of loop)
        let hatch_margin = line_width * 2.0 + 50.0;
        let (vp_w, vp_h) = self.state.viewport_size();
        let hatch_clip_min_x = -hatch_margin;
        let hatch_clip_min_y = -hatch_margin;
        let hatch_clip_max_x = vp_w + hatch_margin;
        let hatch_clip_max_y = vp_h + hatch_margin;
        let half_line_width = line_width * 0.5;

        // Generate hatch lines at regular spacing
        let mut d = proj_min;
        while d <= proj_max {
            // Line center point on the perpendicular axis
            let cx = perp_x * d;
            let cy = perp_y * d;

            // Line endpoints extending in the hatch direction across the bbox
            let lx0 = cx - cos_a * diag;
            let ly0 = cy - sin_a * diag;
            let lx1 = cx + cos_a * diag;
            let ly1 = cy + sin_a * diag;

            // Clip this line segment to the polygon using intersection tests
            let segments = Self::clip_line_to_polygon(lx0, ly0, lx1, ly1, &screen_ring);
            for (sx, sy, ex, ey) in segments {
                // Reject hatch segments outside viewport + margin
                if sx < hatch_clip_min_x
                    || sx > hatch_clip_max_x
                    || sy < hatch_clip_min_y
                    || sy > hatch_clip_max_y
                    || ex < hatch_clip_min_x
                    || ex > hatch_clip_max_x
                    || ey < hatch_clip_min_y
                    || ey > hatch_clip_max_y
                {
                    continue;
                }
                // Render as a line quad
                let ldx = ex - sx;
                let ldy = ey - sy;
                let len = (ldx * ldx + ldy * ldy).sqrt();
                if len < 0.001 {
                    continue;
                }
                let nx = -ldy / len * half_line_width;
                let ny = ldx / len * half_line_width;

                let base_index = self.line_vertices.len() as u32;
                self.line_vertices
                    .push(Vertex2D::new(sx - nx, sy - ny, color_arr));
                self.line_vertices
                    .push(Vertex2D::new(sx + nx, sy + ny, color_arr));
                self.line_vertices
                    .push(Vertex2D::new(ex + nx, ey + ny, color_arr));
                self.line_vertices
                    .push(Vertex2D::new(ex - nx, ey - ny, color_arr));

                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 1);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index);
                self.line_indices.push(base_index + 2);
                self.line_indices.push(base_index + 3);
            }

            d += spacing_px;
        }
    }

    /// Clip a line segment to a polygon, returning visible sub-segments.
    /// Uses scanline intersection: find all intersection points of the line
    /// with polygon edges, sort them along the line, then emit inside segments.
    fn clip_line_to_polygon(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        ring: &[(f32, f32)],
    ) -> Vec<(f32, f32, f32, f32)> {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let line_len_sq = dx * dx + dy * dy;
        if line_len_sq < 1e-10 {
            return Vec::new();
        }

        // Find parametric t values where line intersects each polygon edge
        let mut t_values: Vec<f32> = Vec::with_capacity(8);
        let n = ring.len();
        for i in 0..n {
            let j = (i + 1) % n;
            let (ex0, ey0) = ring[i];
            let (ex1, ey1) = ring[j];

            let edx = ex1 - ex0;
            let edy = ey1 - ey0;

            let denom = dx * edy - dy * edx;
            if denom.abs() < 1e-10 {
                continue; // Parallel
            }

            let t = ((ex0 - x0) * edy - (ey0 - y0) * edx) / denom;
            let u = ((ex0 - x0) * dy - (ey0 - y0) * dx) / denom;

            if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&t) {
                t_values.push(t);
            }
        }

        if t_values.is_empty() {
            // Line might be entirely inside or outside
            let mid_x = (x0 + x1) * 0.5;
            let mid_y = (y0 + y1) * 0.5;
            if point_in_ring(mid_x, mid_y, ring) {
                return vec![(x0, y0, x1, y1)];
            }
            return Vec::new();
        }

        t_values.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Remove near-duplicates
        t_values.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

        // Emit segments between consecutive intersection pairs that are inside
        let mut segments = Vec::with_capacity(t_values.len() / 2 + 1);
        let start_inside = point_in_ring(x0, y0, ring);

        let mut prev_t = 0.0_f32;
        let mut inside = start_inside;

        for &t in &t_values {
            if inside {
                let seg_x0 = x0 + prev_t * dx;
                let seg_y0 = y0 + prev_t * dy;
                let seg_x1 = x0 + t * dx;
                let seg_y1 = y0 + t * dy;
                segments.push((seg_x0, seg_y0, seg_x1, seg_y1));
            }
            inside = !inside;
            prev_t = t;
        }

        // Handle remaining segment to end
        if inside {
            let seg_x0 = x0 + prev_t * dx;
            let seg_y0 = y0 + prev_t * dy;
            segments.push((seg_x0, seg_y0, x1, y1));
        }

        segments
    }

    /// Compute line suppression set (S-100 Part 9-11.1.9).
    /// When multiple features share the same curve geometry, only the
    /// highest-priority LineInstruction is rendered.
    fn compute_line_suppression(instructions: &[DrawingInstruction]) -> FxHashSet<usize> {
        let mut curve_max_priority: FxHashMap<u64, i32> = FxHashMap::default();
        let mut line_entries: Vec<(usize, u64, i32)> = Vec::new();
        let mut has_suppressible = false;

        for (idx, inst) in instructions.iter().enumerate() {
            if let DrawingInstruction::Line(line) = inst {
                if line.points.len() < 2 {
                    continue;
                }
                let key = Self::curve_geometry_hash(&line.points);
                let priority = line.priority.0;
                let entry = curve_max_priority.entry(key).or_insert(priority);
                if priority > *entry {
                    *entry = priority;
                }
                if line.suppressible {
                    line_entries.push((idx, key, priority));
                    has_suppressible = true;
                }
            }
        }

        if has_suppressible {
            let mut suppressed = FxHashSet::default();
            for &(idx, key, priority) in &line_entries {
                if let Some(&max_pri) = curve_max_priority.get(&key) {
                    if priority < max_pri {
                        suppressed.insert(idx);
                    }
                }
            }
            suppressed
        } else {
            FxHashSet::default()
        }
    }

    /// Compute a hash of curve geometry for S-100 line suppression.
    /// Two line instructions referencing the same spatial curve will have
    /// identical world-coordinate point sequences and thus the same hash.
    fn curve_geometry_hash(points: &[WorldPoint]) -> u64 {
        let mut hasher = rustc_hash::FxHasher::default();
        for p in points {
            p.x.to_bits().hash(&mut hasher);
            p.y.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Add line instruction
    /// Cohen-Sutherland outcode for line clipping
    #[inline]
    fn cs_outcode(x: f32, y: f32, x_min: f32, y_min: f32, x_max: f32, y_max: f32) -> u8 {
        let mut code = 0u8;
        if x < x_min {
            code |= 1;
        }
        // LEFT
        else if x > x_max {
            code |= 2;
        } // RIGHT
        if y < y_min {
            code |= 4;
        }
        // TOP
        else if y > y_max {
            code |= 8;
        } // BOTTOM
        code
    }

    /// Clip a line segment to a rectangle using Cohen-Sutherland.
    /// Returns Some((x0,y0,x1,y1)) if any portion is visible, None if fully outside.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn clip_line_segment(
        mut x0: f32,
        mut y0: f32,
        mut x1: f32,
        mut y1: f32,
        x_min: f32,
        y_min: f32,
        x_max: f32,
        y_max: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        let mut code0 = Self::cs_outcode(x0, y0, x_min, y_min, x_max, y_max);
        let mut code1 = Self::cs_outcode(x1, y1, x_min, y_min, x_max, y_max);

        loop {
            if (code0 | code1) == 0 {
                // Both inside
                return Some((x0, y0, x1, y1));
            }
            if (code0 & code1) != 0 {
                // Both on same outside side
                return None;
            }
            // Pick the point that is outside
            let code_out = if code0 != 0 { code0 } else { code1 };
            let (x, y);
            if code_out & 8 != 0 {
                // Below
                x = x0 + (x1 - x0) * (y_max - y0) / (y1 - y0);
                y = y_max;
            } else if code_out & 4 != 0 {
                // Above
                x = x0 + (x1 - x0) * (y_min - y0) / (y1 - y0);
                y = y_min;
            } else if code_out & 2 != 0 {
                // Right
                y = y0 + (y1 - y0) * (x_max - x0) / (x1 - x0);
                x = x_max;
            } else {
                // Left
                y = y0 + (y1 - y0) * (x_min - x0) / (x1 - x0);
                x = x_min;
            }
            if code_out == code0 {
                x0 = x;
                y0 = y;
                code0 = Self::cs_outcode(x0, y0, x_min, y_min, x_max, y_max);
            } else {
                x1 = x;
                y1 = y;
                code1 = Self::cs_outcode(x1, y1, x_min, y_min, x_max, y_max);
            }
        }
    }

    fn add_line(
        &mut self,
        line: &ferrite_render::LineInstruction,
        scaler: &ferrite_render::Scaler,
    ) {
        let points = &line.points;
        if points.len() < 2 {
            return;
        }

        // Frustum culling: compute world AABB and skip if entirely off-screen
        {
            let mut ax = f64::MAX;
            let mut ay = f64::MAX;
            let mut bx = f64::MIN;
            let mut by = f64::MIN;
            for p in points.iter() {
                if p.x < ax {
                    ax = p.x;
                }
                if p.y < ay {
                    ay = p.y;
                }
                if p.x > bx {
                    bx = p.x;
                }
                if p.y > by {
                    by = p.y;
                }
            }
            if !self.is_aabb_visible(ax, ay, bx, by) {
                return;
            }
        }

        let color = line.style.color.to_array();
        let width = line.style.width;

        // Screen-space clip bounds with generous margin for line width
        let vw = scaler.viewport.width;
        let vh = scaler.viewport.height;
        let margin = width * 2.0 + 50.0; // extra margin for thick lines
        let clip_x_min = -margin;
        let clip_y_min = -margin;
        let clip_x_max = vw + margin;
        let clip_y_max = vh + margin;

        // Screen-space length limit for short polylines (≤10 points).
        // Light sector/bearing/route lines are few-vertex features that become enormous
        // "rays" at high zoom. If any single segment exceeds 25% of viewport height,
        // skip the entire line. Dense polylines (coastlines, contours with many vertices)
        // are unaffected since they represent real geometry.
        if points.len() <= 10 {
            let max_seg = vh.min(vw) * 0.25;
            let max_seg_sq = max_seg * max_seg;
            for i in 0..points.len() - 1 {
                let s0 = scaler.world_to_screen(points[i]);
                let s1 = scaler.world_to_screen(points[i + 1]);
                let dx = s1.x - s0.x;
                let dy = s1.y - s0.y;
                if dx * dx + dy * dy > max_seg_sq {
                    return;
                }
            }
        }

        // Direct iteration: no Vec<ScreenPoint> allocation.
        // Transform consecutive world points to screen, clip, and emit quads inline.
        let mut prev = scaler.world_to_screen(points[0]);
        for p in &points[1..] {
            let curr = scaler.world_to_screen(*p);

            // Skip segments with NaN/Inf coordinates
            if !prev.x.is_finite()
                || !prev.y.is_finite()
                || !curr.x.is_finite()
                || !curr.y.is_finite()
            {
                prev = curr;
                continue;
            }

            // Clip to viewport bounds for clean edges
            if let Some((cx0, cy0, cx1, cy1)) = Self::clip_line_segment(
                prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max, clip_y_max,
            ) {
                let dx = cx1 - cx0;
                let dy = cy1 - cy0;
                let len = (dx * dx + dy * dy).sqrt();

                if len >= 0.001 {
                    let nx = -dy / len * width * 0.5;
                    let ny = dx / len * width * 0.5;

                    let base_index = self.line_vertices.len() as u32;

                    self.line_vertices
                        .push(Vertex2D::new(cx0 - nx, cy0 - ny, color));
                    self.line_vertices
                        .push(Vertex2D::new(cx0 + nx, cy0 + ny, color));
                    self.line_vertices
                        .push(Vertex2D::new(cx1 + nx, cy1 + ny, color));
                    self.line_vertices
                        .push(Vertex2D::new(cx1 - nx, cy1 - ny, color));

                    self.line_indices.push(base_index);
                    self.line_indices.push(base_index + 1);
                    self.line_indices.push(base_index + 2);
                    self.line_indices.push(base_index);
                    self.line_indices.push(base_index + 2);
                    self.line_indices.push(base_index + 3);
                }
            }

            prev = curr;
        }
    }

    /// Try to render point as SVG symbol, returns true if successful
    /// Get or compute symbol classification flags for a SymbolId.
    /// Cached permanently (symbol names never change).
    #[inline]
    fn get_symbol_flags(&mut self, symbol_id: SymbolId, symbol_str: &str) -> u8 {
        if let Some(&flags) = self.symbol_class_cache.get(&symbol_id) {
            return flags;
        }
        let flags = classify_symbol(symbol_str);
        self.symbol_class_cache.insert(symbol_id, flags);
        flags
    }

    fn try_add_symbol(
        &mut self,
        point: &ferrite_render::PointInstruction,
        scaler: &ferrite_render::Scaler,
        symbol_cache: &mut SymbolCache,
        color_profile: &ColorProfile,
        pre_interned_id: SymbolId,
    ) -> bool {
        let symbol_str = &point.symbol_ref;
        if symbol_str.is_empty() {
            return false;
        }

        // Use pre-interned SymbolId (avoids redundant read-lock)
        let symbol_id = pre_interned_id;

        // Get symbol geometry from cache (this will render via resvg if not cached)
        // Use reference to avoid cloning the pixel buffer
        let geom = match symbol_cache.get_symbol(symbol_str, color_profile) {
            Some(g) => g,
            None => return false,
        };

        // Create GPU texture if not already cached (using interned SymbolId for O(1) lookup)
        if !self.symbol_textures.contains_key(&symbol_id) {
            let (_texture, view) = self.state.create_texture_from_rgba(
                &geom.pixels,
                geom.width,
                geom.height,
                &format!("symbol_{}", symbol_str),
            );

            let bind_group = self
                .pipelines
                .create_texture_bind_group(&self.state.device, &view);

            let pivot_in_tex = geom.pivot_in_texture();
            self.symbol_textures.insert(
                symbol_id,
                SymbolTexture {
                    texture: _texture,
                    bind_group,
                    width: geom.width,
                    height: geom.height,
                    pivot_in_texture: pivot_in_tex,
                    render_scale: geom.render_scale,
                },
            );

            tracing::debug!(
                "Created GPU texture for symbol '{}': {}x{}, pivot_in_tex: ({:.2}, {:.2})",
                symbol_str,
                geom.width,
                geom.height,
                pivot_in_tex.0,
                pivot_in_tex.1
            );
        }

        // Convert screen position
        let screen = scaler.world_to_screen(point.position);

        // === STAGE 1: World-coordinate deduplication ===
        // Remove exact duplicates from multiple charts at the same geographic position
        // Use high precision (6 decimal places ≈ 0.1 meter) for deduplication
        let world_x_key = (point.position.x * 1_000_000.0) as i64;
        let world_y_key = (point.position.y * 1_000_000.0) as i64;
        // Use interned symbol ID as hash (already unique per symbol type)
        let symbol_hash = symbol_id.0 as u64;
        let world_key = (world_x_key, world_y_key, symbol_hash);

        if self.world_dedup.contains(&world_key) {
            // Exact duplicate from another chart - skip
            return true;
        }
        self.world_dedup.insert(world_key);

        // === STAGE 2: World-coordinate-based decluttering ===
        // Uses world coordinates divided by pixel-equivalent cell sizes for stable grids.
        // Unlike screen-space grids, world-coordinate grids produce identical results
        // regardless of pan offset, eliminating symbol pop-in/pop-out during drag.
        //
        // Cell sizes are computed as: screen_cell_size_px / scale_factor
        // This gives the same visual density as screen-space but is pan-stable.

        // Classify symbol types via cached bitflags (O(1) lookup vs repeated starts_with)
        let flags = self.get_symbol_flags(symbol_id, symbol_str);
        let is_nav_aid = flags & SYM_NAV_AID != 0;
        let is_safety_hazard = flags & SYM_SAFETY != 0;
        let is_sounding = flags & SYM_SOUNDING != 0;

        // Compute world-space cell sizes from screen-space pixel sizes
        let scale_x = scaler.scale_x().abs();
        let scale_y = scaler.scale_y().abs();

        // Safety hazard symbols are NEVER decluttered.
        // S-100 does not define symbol decluttering (only sounding collision via champion).
        // Hiding safety symbols (wrecks, obstructions, dangers) would violate navigation safety.
        // Scale-dependent visibility is handled by ScaleMinimum/ScaleMaximum from Lua rules.

        // Skip decluttering during animation to prevent symbols from disappearing
        // Only world-coordinate deduplication (Stage 1) applies during drag/inertia
        if !is_safety_hazard && !self.skip_screen_declutter && scale_x > 1e-10 && scale_y > 1e-10 {
            // Soundings: S-100 collision avoidance (champion = shallowest wins for safety)
            // Two-stage approach:
            // 1. sounding_exact_positions: tracks exact world positions to allow all digits of same sounding
            // 2. sounding_screen_grid: world-based grid to filter out visually nearby soundings
            // At high zoom (cell_size == 0), skip grid filtering and show all soundings
            if is_sounding && self.sounding_cell_size_px > 0.1 {
                // World-space key for exact position (all digits of one sounding share this)
                let exact_key = (world_x_key, world_y_key);

                // Check if we've already allowed a sounding at this exact world position
                if self.sounding_exact_positions.contains(&exact_key) {
                    // This is another digit of an already-allowed sounding - let it through
                    // (skip grid check)
                } else {
                    // First time seeing this exact position - check world-based grid
                    let world_cell_x = self.sounding_cell_size_px as f64 / scale_x;
                    let world_cell_y = self.sounding_cell_size_px as f64 / scale_y;
                    let sounding_grid_x = (point.position.x / world_cell_x).floor() as i32;
                    let sounding_grid_y = (point.position.y / world_cell_y).floor() as i32;
                    let sounding_grid_key = (sounding_grid_x, sounding_grid_y);

                    // Get current sounding's depth (default to MAX if not set)
                    let current_depth = point.depth().unwrap_or(f64::MAX);

                    if let Some(&(old_exact_key, old_depth)) =
                        self.sounding_screen_grid.get(&sounding_grid_key)
                    {
                        // Another sounding already claimed this cell
                        // For SAFETY: keep the SHALLOWEST (lowest numerical depth) sounding
                        if current_depth < old_depth {
                            // This sounding is shallower - replace the old one
                            self.sounding_exact_positions.remove(&old_exact_key);
                            self.sounding_screen_grid
                                .insert(sounding_grid_key, (exact_key, current_depth));
                            self.sounding_exact_positions.insert(exact_key);
                        } else {
                            // Existing sounding is shallower or equal - skip this one
                            return true;
                        }
                    } else {
                        // Cell is empty - this sounding claims it
                        self.sounding_screen_grid
                            .insert(sounding_grid_key, (exact_key, current_depth));
                        self.sounding_exact_positions.insert(exact_key);
                    }
                }
            }
            // Non-safety, non-sounding symbols: visual declutter (non-standard optimization)
            else if !is_sounding {
                let effective_cell_size = if is_nav_aid {
                    if self.zoom_level >= 5.0 {
                        15.0
                    } else {
                        self.grid_cell_size
                    }
                } else {
                    self.grid_cell_size
                };

                let world_cell_x = effective_cell_size as f64 / scale_x;
                let world_cell_y = effective_cell_size as f64 / scale_y;
                let grid_x = (point.position.x / world_cell_x).floor() as i32;
                let grid_y = (point.position.y / world_cell_y).floor() as i32;
                let grid_key = (grid_x, grid_y);

                if self.symbol_grid.contains(&grid_key) {
                    return true;
                }
                self.symbol_grid.insert(grid_key);
            }
        }

        // Add symbol instance for rendering (uses interned SymbolId - 4 bytes vs 24+ for String)
        self.symbol_instances.push(SymbolInstance {
            symbol_id,
            screen_x: screen.x,
            screen_y: screen.y,
            scale: point.scale,
            // S-100 geographic CRS: rotation is clockwise from north (0°=up).
            // Renderer rotation matrix is clockwise from +X (east) in screen-space (Y-down).
            // Conversion: screen_angle = geo_angle - 90°
            rotation: if point.rotation != 0.0 {
                point.rotation - 90.0
            } else {
                0.0
            },
        });

        true
    }

    /// Record a point instruction that could not be rendered as a symbol.
    /// Empty `symbol_ref` indicates a portrayal-rule bug; missing/un-renderable
    /// symbols indicate a Portrayal Catalogue gap. Both are logged once per id
    /// so they surface in normal logs without spamming every frame.
    fn note_unrendered_symbol(&mut self, sym_id: SymbolId, symbol_ref: &str) {
        if symbol_ref.is_empty() {
            self.empty_symbol_ref_count = self.empty_symbol_ref_count.saturating_add(1);
            return;
        }
        if self.missing_symbol_ids.insert(sym_id) {
            tracing::warn!(
                "Symbol '{}' could not be rendered (missing SVG or unresolved color tokens) — \
                 check Portrayal Catalogue completeness",
                symbol_ref
            );
        }
    }

    /// Number of unique symbol ids that failed to render this session.
    pub fn missing_symbol_count(&self) -> usize {
        self.missing_symbol_ids.len()
    }

    /// Number of point instructions emitted without a symbol_ref this session.
    /// A non-zero count indicates a portrayal-rule bug.
    pub fn empty_symbol_ref_count(&self) -> u32 {
        self.empty_symbol_ref_count
    }
}
