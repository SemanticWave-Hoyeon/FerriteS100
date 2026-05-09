//! World-map background: coastlines + chart-coverage masks.
//!
//! `set_world_map` stores the parsed Natural Earth coastline polylines.
//! `set_world_map_chart_boxes` stores chart bounding rectangles so the
//! world map can be over-painted under loaded charts. `add_world_map_lines`
//! emits both into per-frame line/mask buffers — these draw before chart
//! data so the chart layers cover the world map cleanly.

use ferrite_render::WorldPoint;

use super::WgpuRenderer;
use crate::Vertex2D;

impl WgpuRenderer {
    /// Set Natural Earth world map coastlines for background rendering.
    /// Each inner Vec is a line string: list of [longitude, latitude] pairs.
    pub fn set_world_map(&mut self, coastlines: Vec<Vec<[f64; 2]>>) {
        tracing::info!("World map loaded: {} coastline segments", coastlines.len());
        self.world_map_coastlines = coastlines;
    }

    /// Set chart coverage bounding boxes so world map is masked
    /// where chart data exists (opaque background rectangles).
    pub fn set_world_map_chart_boxes(&mut self, boxes: Vec<(f64, f64, f64, f64)>) {
        self.world_map_chart_boxes = boxes;
    }

    /// Add world map coastline lines and chart-coverage mask rectangles.
    /// Uses separate buffers (not chart line_vertices) so they render
    /// independently in the correct draw order:
    ///   1. World map coastlines (lowest layer)
    ///   2. Opaque background rectangles over chart bboxes (mask coastlines)
    ///   3. Chart data on top (priority-based rendering)
    ///
    /// Renders at lon offsets -360°, 0°, +360° for seamless wrapping.
    pub fn add_world_map_lines(&mut self, scaler: &ferrite_render::Scaler) {
        if self.world_map_coastlines.is_empty() {
            return;
        }

        // Subtle gray color for background coastlines
        let color: [f32; 4] = [0.65, 0.65, 0.65, 1.0];
        let width: f32 = 1.0;

        let vw = scaler.viewport.width;
        let vh = scaler.viewport.height;
        let margin: f32 = 100.0;
        let clip_x_min = -margin;
        let clip_y_min = -margin;
        let clip_x_max = vw + margin;
        let clip_y_max = vh + margin;

        // Render at 3 longitude offsets for seamless wrapping
        let lon_offsets: [f64; 3] = [-360.0, 0.0, 360.0];

        for &lon_offset in &lon_offsets {
            for coastline in &self.world_map_coastlines {
                if coastline.len() < 2 {
                    continue;
                }

                // Quick AABB frustum cull per coastline (shifted by lon_offset)
                let mut ax = f64::MAX;
                let mut ay = f64::MAX;
                let mut bx = f64::MIN;
                let mut by = f64::MIN;
                for pt in coastline.iter() {
                    let lon = pt[0] + lon_offset;
                    if lon < ax {
                        ax = lon;
                    }
                    if pt[1] < ay {
                        ay = pt[1];
                    }
                    if lon > bx {
                        bx = lon;
                    }
                    if pt[1] > by {
                        by = pt[1];
                    }
                }
                if !self.is_aabb_visible(ax, ay, bx, by) {
                    continue;
                }

                let first_lon = coastline[0][0] + lon_offset;
                let mut prev = scaler.world_to_screen(WorldPoint::new(first_lon, coastline[0][1]));

                for pt in &coastline[1..] {
                    let cur_lon = pt[0] + lon_offset;
                    let cur_lat = pt[1];
                    let curr = scaler.world_to_screen(WorldPoint::new(cur_lon, cur_lat));

                    if !prev.x.is_finite()
                        || !prev.y.is_finite()
                        || !curr.x.is_finite()
                        || !curr.y.is_finite()
                    {
                        prev = curr;
                        continue;
                    }

                    if let Some((cx0, cy0, cx1, cy1)) = Self::clip_line_segment(
                        prev.x, prev.y, curr.x, curr.y, clip_x_min, clip_y_min, clip_x_max,
                        clip_y_max,
                    ) {
                        let dx = cx1 - cx0;
                        let dy = cy1 - cy0;
                        let len = (dx * dx + dy * dy).sqrt();

                        if len >= 0.5 {
                            let nx = -dy / len * width * 0.5;
                            let ny = dx / len * width * 0.5;

                            let base_index = self.world_map_line_vertices.len() as u32;

                            self.world_map_line_vertices.push(Vertex2D::new(
                                cx0 - nx,
                                cy0 - ny,
                                color,
                            ));
                            self.world_map_line_vertices.push(Vertex2D::new(
                                cx0 + nx,
                                cy0 + ny,
                                color,
                            ));
                            self.world_map_line_vertices.push(Vertex2D::new(
                                cx1 + nx,
                                cy1 + ny,
                                color,
                            ));
                            self.world_map_line_vertices.push(Vertex2D::new(
                                cx1 - nx,
                                cy1 - ny,
                                color,
                            ));

                            self.world_map_line_indices.push(base_index);
                            self.world_map_line_indices.push(base_index + 1);
                            self.world_map_line_indices.push(base_index + 2);
                            self.world_map_line_indices.push(base_index);
                            self.world_map_line_indices.push(base_index + 2);
                            self.world_map_line_indices.push(base_index + 3);
                        }
                    }

                    prev = curr;
                }
            }

            // Add opaque background rectangles over chart bboxes at this lon offset.
            // These mask world map coastlines under loaded chart areas.
            let bg = self.background_color.to_array();
            for &(min_x, min_y, max_x, max_y) in &self.world_map_chart_boxes {
                let shifted_min_x = min_x + lon_offset;
                let shifted_max_x = max_x + lon_offset;

                // Frustum cull
                if !self.is_aabb_visible(shifted_min_x, min_y, shifted_max_x, max_y) {
                    continue;
                }

                let tl = scaler.world_to_screen(WorldPoint::new(shifted_min_x, max_y));
                let br = scaler.world_to_screen(WorldPoint::new(shifted_max_x, min_y));

                if !tl.x.is_finite() || !tl.y.is_finite() || !br.x.is_finite() || !br.y.is_finite()
                {
                    continue;
                }

                let base = self.world_map_mask_vertices.len() as u32;
                self.world_map_mask_vertices
                    .push(Vertex2D::new(tl.x, tl.y, bg));
                self.world_map_mask_vertices
                    .push(Vertex2D::new(br.x, tl.y, bg));
                self.world_map_mask_vertices
                    .push(Vertex2D::new(br.x, br.y, bg));
                self.world_map_mask_vertices
                    .push(Vertex2D::new(tl.x, br.y, bg));

                self.world_map_mask_indices.push(base);
                self.world_map_mask_indices.push(base + 1);
                self.world_map_mask_indices.push(base + 2);
                self.world_map_mask_indices.push(base);
                self.world_map_mask_indices.push(base + 2);
                self.world_map_mask_indices.push(base + 3);
            }
        }
    }
}
