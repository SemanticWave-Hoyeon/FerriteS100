//! Viewport / zoom / pan / world-bounds methods.
//!
//! `update_view_ex` is the workhorse called whenever zoom or pan changes —
//! it pushes the new transform into the renderer, optionally rebuilds the
//! hit-test list, and (unless `preserve_declutter` is set) clears declutter
//! grids. `recalculate_view_bounds` recomputes the geographic bounds shown
//! based on viewport + zoom + pan.

use ferrite_render::GeoBounds;

use crate::ChartApp;

impl ChartApp {
    /// Update the view based on current zoom and pan
    /// - `rebuild_hit_test`: if false, skip rebuilding the hit-test symbol list
    /// - `preserve_declutter`: if true, preserve symbol declutter grids to avoid flickering
    pub(crate) fn update_view_ex(&mut self, rebuild_hit_test: bool, preserve_declutter: bool) {
        let profiling = ferrite_wgpu::profiler::is_profiling_enabled();
        let update_view_start = if profiling {
            Some(std::time::Instant::now())
        } else {
            None
        };

        // Update viewport to use actual chart area (excluding UI panels)
        if let Some(renderer) = &self.renderer {
            let (x, y, w, h) = renderer.ui_state.chart_area;
            if w > 0.0 && h > 0.0 {
                self.render_context.set_viewport_rect(x, y, w, h);
            }
        }

        // Calculate the zoomed and panned bounds
        // Wrap horizontal pan offset modulo 360° so the viewport always stays near
        // the chart data. Combined with ±360° rendering copies, this enables
        // seamless infinite horizontal panning (Earth is round).
        let base_width = self.bounds.max_x - self.bounds.min_x;
        let base_height = self.bounds.max_y - self.bounds.min_y;
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        let center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0 + wrapped_pan_x;
        let center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0 + self.pan_offset.1;

        let zoomed_width = base_width / self.zoom_level;
        let zoomed_height = base_height / self.zoom_level;

        let new_bounds = GeoBounds {
            min_x: center_x - zoomed_width / 2.0,
            max_x: center_x + zoomed_width / 2.0,
            min_y: center_y - zoomed_height / 2.0,
            max_y: center_y + zoomed_height / 2.0,
        };

        self.render_context.zoom_to_fit(new_bounds);

        // Pre-compute color profile and viewing groups before mutable borrow of renderer
        let color_profile = self
            .pc
            .color_profiles
            .profiles
            .get(&self.current_profile_name);
        let visible_vgs = self.get_visible_viewing_groups();

        // Prepare plugin instructions before renderer borrow
        // Always add plugin instructions (route overlays should render even without charts)
        self.render_context
            .truncate_instructions(self.base_instruction_count);
        for instr in self.plugin_system.get_render_instructions() {
            self.render_context.add_instruction(instr);
        }

        if let Some(renderer) = &mut self.renderer {
            // Update zoom level for symbol decluttering and UI
            renderer.set_zoom_level(self.zoom_level);
            renderer.ui_state.zoom_level = self.zoom_level;
            // During animation, preserve declutter state to avoid flickering
            renderer.begin_frame_ex(preserve_declutter);

            // Draw world map coastlines as the lowest layer (before chart data)
            renderer.set_lon_wrap_pixels(360.0 * self.render_context.scaler.scale_x() as f32);
            renderer.add_world_map_lines(&self.render_context.scaler);

            // Chart data + plugin overlay rendering
            renderer.add_instructions_with_symbols(
                &mut self.render_context,
                Some(&mut self.symbol_cache),
                color_profile,
                visible_vgs.as_ref(),
            );
        }

        // Rebuild symbols for hit testing (skip during animation for performance)
        if rebuild_hit_test && self.chart_loaded {
            let hit_test_start = if profiling {
                Some(std::time::Instant::now())
            } else {
                None
            };
            self.build_rendered_symbols();
            if let Some(s) = hit_test_start {
                if let Some(renderer) = &mut self.renderer {
                    renderer.cpu_profiler.record("build_hit_test", s.elapsed());
                }
            }
        }

        if let Some(s) = update_view_start {
            let elapsed = s.elapsed();
            tracing::debug!(
                "[PROFILER] update_view_ex: {:.2}ms (hit_test={})",
                elapsed.as_secs_f64() * 1000.0,
                rebuild_hit_test
            );
            if let Some(renderer) = &mut self.renderer {
                renderer.cpu_profiler.record("update_view", elapsed);
            }
        }
    }

    /// Update the view (rebuilds hit-test symbols, clears declutter grids)
    pub(crate) fn update_view(&mut self) {
        self.update_view_ex(true, false);
        // Sync zoom rebuild level so GPU zoom delta resets to 1.0
        self.zoom_rebuilt_level = self.zoom_level;
    }

    /// Recalculate view bounds/scaler without rebuilding geometry.
    /// Used as a lightweight step before adjusting pan offset during zoom.
    pub(crate) fn recalculate_view_bounds(&mut self) {
        if let Some(renderer) = &self.renderer {
            let (x, y, w, h) = renderer.ui_state.chart_area;
            if w > 0.0 && h > 0.0 {
                self.render_context.set_viewport_rect(x, y, w, h);
            }
        }
        let base_width = self.bounds.max_x - self.bounds.min_x;
        let base_height = self.bounds.max_y - self.bounds.min_y;
        let wrapped_pan_x = self.pan_offset.0 - (self.pan_offset.0 / 360.0).round() * 360.0;
        let center_x = (self.bounds.min_x + self.bounds.max_x) / 2.0 + wrapped_pan_x;
        let center_y = (self.bounds.min_y + self.bounds.max_y) / 2.0 + self.pan_offset.1;
        let zoomed_width = base_width / self.zoom_level;
        let zoomed_height = base_height / self.zoom_level;
        let new_bounds = GeoBounds {
            min_x: center_x - zoomed_width / 2.0,
            max_x: center_x + zoomed_width / 2.0,
            min_y: center_y - zoomed_height / 2.0,
            max_y: center_y + zoomed_height / 2.0,
        };
        self.render_context.zoom_to_fit(new_bounds);
    }
}
