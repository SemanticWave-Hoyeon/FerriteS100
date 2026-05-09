//! Frame lifecycle + view uniforms + pan / zoom / scale state.
//!
//! `begin_frame` resets per-frame buffers (vertices, instances, declutter
//! grids); `update_view_uniforms` writes the current viewport / pan / zoom
//! into the view uniform buffer the shader reads.
//!
//! Pan and zoom are split into a "world transform" (the scaler in
//! `RenderContext`) and a "GPU offset/scale" applied each frame for smooth
//! drag/inertia and zoom animation without rebuilding geometry.

use ferrite_render::ScreenPoint;

use super::WgpuRenderer;
use crate::ViewUniforms;

impl WgpuRenderer {
    pub fn set_profiling_enabled(&mut self, enabled: bool) {
        crate::profiler::set_profiling_enabled(enabled);
        self.gpu_profiler.set_enabled(enabled);
    }

    /// Flush profiler reports (call on shutdown)
    pub fn flush_profiler(&mut self) {
        self.cpu_profiler.flush();
    }

    /// Set animation mode for fast-path rendering during drag/zoom
    #[inline]
    pub fn set_animation_mode(&mut self, animating: bool) {
        self.animation_mode = animating;
        // During animation, use lower LOD
        self.lod_level = if animating { 1 } else { 0 };
    }

    /// Update viewport world bounds for frustum culling
    pub fn update_viewport_bounds(&mut self, scaler: &ferrite_render::Scaler) {
        let (vw, vh) = (scaler.viewport.width, scaler.viewport.height);
        let top_left = scaler.screen_to_world(ScreenPoint { x: 0.0, y: 0.0 });
        let bottom_right = scaler.screen_to_world(ScreenPoint { x: vw, y: vh });
        self.viewport_world_bounds = Some((
            top_left.x.min(bottom_right.x),
            top_left.y.min(bottom_right.y),
            top_left.x.max(bottom_right.x),
            top_left.y.max(bottom_right.y),
        ));
    }

    /// Update view uniforms after resize or zoom
    pub(super) fn update_view_uniforms(&self) {
        let (width, height) = self.state.viewport_size();
        let uniforms = ViewUniforms::with_pan_zoom(
            width,
            height,
            1.0,
            self.screen_pan_offset.0,
            self.screen_pan_offset.1,
            self.screen_zoom_scale,
            self.screen_zoom_pivot.0,
            self.screen_zoom_pivot.1,
        );
        self.state
            .update_view_uniforms(&self.view_buffer, &uniforms);

        // Update wrapping view uniforms for ±360° longitude copies
        if self.lon_wrap_screen_px > 0.0 {
            let left = ViewUniforms::with_pan_zoom(
                width,
                height,
                1.0,
                self.screen_pan_offset.0 - self.lon_wrap_screen_px,
                self.screen_pan_offset.1,
                self.screen_zoom_scale,
                self.screen_zoom_pivot.0,
                self.screen_zoom_pivot.1,
            );
            self.state
                .update_view_uniforms(&self.view_buffer_left, &left);

            let right = ViewUniforms::with_pan_zoom(
                width,
                height,
                1.0,
                self.screen_pan_offset.0 + self.lon_wrap_screen_px,
                self.screen_pan_offset.1,
                self.screen_zoom_scale,
                self.screen_zoom_pivot.0,
                self.screen_zoom_pivot.1,
            );
            self.state
                .update_view_uniforms(&self.view_buffer_right, &right);
        }
    }

    /// Set screen-space pan offset for fast panning during drag.
    /// This only updates the GPU uniform, no vertex rebuild needed.
    #[inline]
    pub fn set_pan_offset(&mut self, dx: f32, dy: f32) {
        self.screen_pan_offset = (dx, dy);
        self.update_view_uniforms();
    }

    /// Add to the current screen-space pan offset.
    /// Returns the new total offset.
    pub fn add_pan_offset(&mut self, dx: f32, dy: f32) -> (f32, f32) {
        self.screen_pan_offset.0 += dx;
        self.screen_pan_offset.1 += dy;
        self.update_view_uniforms();
        self.screen_pan_offset
    }

    /// Get the current screen-space pan offset
    #[inline]
    pub fn get_pan_offset(&self) -> (f32, f32) {
        self.screen_pan_offset
    }

    /// Reset pan offset to zero (call before full rebuild)
    #[inline]
    pub fn reset_pan_offset(&mut self) {
        self.screen_pan_offset = (0.0, 0.0);
        self.screen_zoom_scale = 1.0;
        self.screen_zoom_pivot = (0.0, 0.0);
        self.update_view_uniforms();
    }

    /// Set GPU zoom scale for smooth zooming without geometry rebuild.
    /// The shader scales all geometry around the pivot point.
    #[inline]
    pub fn set_gpu_zoom(&mut self, scale: f32, pivot_x: f32, pivot_y: f32) {
        self.screen_zoom_scale = scale;
        self.screen_zoom_pivot = (pivot_x, pivot_y);
        self.update_view_uniforms();
    }

    /// Get current GPU zoom scale
    #[inline]
    pub fn gpu_zoom_scale(&self) -> f32 {
        self.screen_zoom_scale
    }

    /// Begin a new frame - clears buffers
    pub fn begin_frame(&mut self) {
        self.begin_frame_ex(false);
    }

    /// Begin a new frame with optional preservation of declutter state
    /// If `preserve_declutter` is true, skip screen-space decluttering during animation
    pub fn begin_frame_ex(&mut self, preserve_declutter: bool) {
        // Mark GPU buffers as needing rebuild
        self.gpu_buffers_dirty = true;
        // Invalidate cached symbol GPU buffers (geometry changed)
        self.cached_symbol_buffers.clear();

        // Preserve previous frame counts for pre-allocation (avoids realloc during build)
        let prev_area_v = self.area_vertices.len();
        let prev_area_i = self.area_indices.len();
        let prev_line_v = self.line_vertices.len();
        let prev_line_i = self.line_indices.len();

        self.area_vertices.clear();
        self.area_indices.clear();
        self.line_vertices.clear();
        self.line_indices.clear();
        self.symbol_instances.clear();

        // Reserve capacity based on previous frame (amortized zero reallocs in steady state)
        if prev_area_v > self.area_vertices.capacity() / 2 {
            self.area_vertices.reserve(prev_area_v);
        }
        if prev_area_i > self.area_indices.capacity() / 2 {
            self.area_indices.reserve(prev_area_i);
        }
        if prev_line_v > self.line_vertices.capacity() / 2 {
            self.line_vertices.reserve(prev_line_v);
        }
        if prev_line_i > self.line_indices.capacity() / 2 {
            self.line_indices.reserve(prev_line_i);
        }
        // Clear symbol batches for new frame
        self.symbol_batches.clear();
        // Clear priority ranges for S-101 compliant rendering
        self.area_priority_ranges.clear();
        self.line_priority_ranges.clear();
        self.symbol_priority_ranges.clear();
        self.pattern_vertices.clear();
        self.pattern_indices.clear();
        self.pattern_ranges.clear();
        self.text_labels.clear();
        self.text_collision_grid.clear();
        // World map separate buffers
        self.world_map_line_vertices.clear();
        self.world_map_line_indices.clear();
        self.world_map_mask_vertices.clear();
        self.world_map_mask_indices.clear();

        // During animation (preserve_declutter=true), skip screen-space declutter
        // to prevent symbols from disappearing due to changed screen coordinates
        self.skip_screen_declutter = preserve_declutter;

        // world_dedup must ALWAYS be cleared - it's rebuilt each frame from scratch
        // Only screen-space grids (symbol_grid, sounding_screen_grid) should be preserved
        // during animation to prevent flickering
        self.world_dedup.clear();

        if !preserve_declutter {
            self.symbol_grid.clear();
            self.sounding_screen_grid.clear();
            self.sounding_exact_positions.clear();

            // Adjust grid cell size based on zoom level
            // At low zoom (zoomed out), use larger cells to declutter more aggressively
            // At high zoom (zoomed in), use smaller cells to show more detail
            self.grid_cell_size = (40.0 / self.zoom_level.sqrt() as f32).clamp(20.0, 120.0);

            // Sounding cell size is fixed in screen pixels for consistent density
            // At maximum zoom (>= 45x), show ALL soundings (no filtering)
            if self.zoom_level >= 45.0 {
                self.sounding_cell_size_px = 0.0; // No filtering
            } else {
                // Fixed screen-space cell size for consistent visual density
                // 150px provides good spacing between soundings at most zoom levels
                self.sounding_cell_size_px = 150.0;
            }
        }
    }

    /// Set current zoom level for symbol filtering
    pub fn set_zoom_level(&mut self, zoom: f64) {
        self.zoom_level = zoom;
    }

    /// Set chart compilation scale (e.g., 22000 for 1:22000)
    #[inline]
    pub fn set_compilation_scale(&mut self, scale: u32) {
        self.compilation_scale = scale;
    }

    /// Calculate the current viewing scale based on zoom level
    /// viewing_scale = compilation_scale / zoom_level
    /// Example: At zoom 2.0 with 1:22000 chart -> viewing scale is 1:11000
    #[inline]
    pub fn viewing_scale(&self) -> u32 {
        ((self.compilation_scale as f64) / self.zoom_level.max(0.01)) as u32
    }

    /// Set the screen pixel width of 360° longitude for wrapping.
    /// Call this after scaler is configured: `renderer.set_lon_wrap_pixels(360.0 * scaler.scale_x as f32)`
    pub fn set_lon_wrap_pixels(&mut self, px: f32) {
        self.lon_wrap_screen_px = px;
        // Update left/right view uniform buffers immediately so wrapping draws
        // use the correct offset from the very first frame after chart load.
        self.update_view_uniforms();
    }
}
