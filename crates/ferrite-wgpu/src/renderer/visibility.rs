//! Frustum culling helpers.
//!
//! Three predicates against `viewport_world_bounds`, each adding a 50%
//! margin so symbols/rings near the edge don't pop in after drag-inertia
//! ends. `is_ring_visible_static` exists in static form because
//! `tile_area_with_pattern/hatch` is called while `&mut self` is already
//! held by the renderer's drawing pipeline.

use ferrite_render::WorldPoint;

use super::WgpuRenderer;

impl WgpuRenderer {
    /// Check if a world point is within the viewport (with margin)
    #[inline]
    pub(super) fn is_point_visible(&self, x: f64, y: f64) -> bool {
        if let Some((min_x, min_y, max_x, max_y)) = self.viewport_world_bounds {
            // Add 50% margin to accommodate GPU pan offset during drag/inertia.
            // Without this, symbols near the viewport edge get culled and then
            // "pop in" when the view rebuilds after drag ends.
            let margin_x = (max_x - min_x) * 0.5;
            let margin_y = (max_y - min_y) * 0.5;
            x >= min_x - margin_x
                && x <= max_x + margin_x
                && y >= min_y - margin_y
                && y <= max_y + margin_y
        } else {
            true // No bounds set, assume visible
        }
    }

    /// Check if a world-space AABB intersects the viewport (with margin for GPU pan/zoom)
    #[inline]
    pub(super) fn is_aabb_visible(
        &self,
        aabb_min_x: f64,
        aabb_min_y: f64,
        aabb_max_x: f64,
        aabb_max_y: f64,
    ) -> bool {
        if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) = self.viewport_world_bounds {
            let margin_x = (vp_max_x - vp_min_x) * 0.5;
            let margin_y = (vp_max_y - vp_min_y) * 0.5;
            // Standard AABB intersection test with margin
            aabb_max_x >= vp_min_x - margin_x
                && aabb_min_x <= vp_max_x + margin_x
                && aabb_max_y >= vp_min_y - margin_y
                && aabb_min_y <= vp_max_y + margin_y
        } else {
            true
        }
    }

    /// Static frustum culling for a ring of world points against viewport bounds.
    /// Used by tile_area_with_pattern/hatch where &self is already mutably borrowed.
    #[inline]
    pub(super) fn is_ring_visible_static(
        ring: &[WorldPoint],
        viewport_world_bounds: Option<(f64, f64, f64, f64)>,
    ) -> bool {
        if let Some((vp_min_x, vp_min_y, vp_max_x, vp_max_y)) = viewport_world_bounds {
            let margin_x = (vp_max_x - vp_min_x) * 0.5;
            let margin_y = (vp_max_y - vp_min_y) * 0.5;
            let mut ax = f64::MAX;
            let mut ay = f64::MAX;
            let mut bx = f64::MIN;
            let mut by = f64::MIN;
            for p in ring {
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
            bx >= vp_min_x - margin_x
                && ax <= vp_max_x + margin_x
                && by >= vp_min_y - margin_y
                && ay <= vp_max_y + margin_y
        } else {
            true
        }
    }
}
