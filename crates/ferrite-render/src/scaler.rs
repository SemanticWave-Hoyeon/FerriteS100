//! Coordinate Transformation (Scaler)
//!
//! Handles transformation between:
//! - World coordinates (geographic: longitude/latitude in degrees)
//! - Screen coordinates (pixels)
//!
//! Based on S-100 standard's Scaler class.

use crate::{ScreenPoint, WorldPoint};

/// Geographic bounding box
#[derive(Debug, Clone, Copy)]
pub struct GeoBounds {
    pub min_x: f64, // West longitude
    pub min_y: f64, // South latitude
    pub max_x: f64, // East longitude
    pub max_y: f64, // North latitude
}

impl GeoBounds {
    #[inline]
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        GeoBounds {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    #[inline]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    #[inline]
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    #[inline]
    pub fn center(&self) -> WorldPoint {
        WorldPoint::new(
            (self.min_x + self.max_x) / 2.0,
            (self.min_y + self.max_y) / 2.0,
        )
    }

    #[inline]
    pub fn contains(&self, point: WorldPoint) -> bool {
        point.x >= self.min_x
            && point.x <= self.max_x
            && point.y >= self.min_y
            && point.y <= self.max_y
    }

    pub fn expand(&mut self, point: WorldPoint) {
        self.min_x = self.min_x.min(point.x);
        self.min_y = self.min_y.min(point.y);
        self.max_x = self.max_x.max(point.x);
        self.max_y = self.max_y.max(point.y);
    }

    /// Expand by percentage margin
    pub fn expand_by_percent(&mut self, percent: f64) {
        let margin_x = self.width() * percent;
        let margin_y = self.height() * percent;
        self.min_x -= margin_x;
        self.min_y -= margin_y;
        self.max_x += margin_x;
        self.max_y += margin_y;
    }
}

impl Default for GeoBounds {
    fn default() -> Self {
        GeoBounds {
            min_x: f64::MAX,
            min_y: f64::MAX,
            max_x: f64::MIN,
            max_y: f64::MIN,
        }
    }
}

/// Screen viewport
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    #[inline]
    pub fn new(width: f32, height: f32) -> Self {
        Viewport {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }

    #[inline]
    pub fn with_origin(x: f32, y: f32, width: f32, height: f32) -> Self {
        Viewport {
            x,
            y,
            width,
            height,
        }
    }

    #[inline]
    pub fn center(&self) -> ScreenPoint {
        ScreenPoint::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    #[inline]
    pub fn aspect_ratio(&self) -> f32 {
        self.width / self.height
    }
}

/// Coordinate transformation scaler
///
/// Handles conversion between world (geographic) and screen (pixel) coordinates.
/// Uses Mercator-like projection for display.
#[derive(Debug, Clone)]
pub struct Scaler {
    /// Geographic bounds being displayed
    pub geo_bounds: GeoBounds,
    /// Screen viewport
    pub viewport: Viewport,
    /// Scale factor (screen pixels per degree)
    scale_x: f64,
    scale_y: f64,
    /// Offset for centering
    offset_x: f64,
    offset_y: f64,
    /// Display scale (1:N)
    pub display_scale: f64,
    /// Minimum scale (zoom out limit)
    pub min_scale: f64,
    /// Maximum scale (zoom in limit)
    pub max_scale: f64,
}

impl Scaler {
    /// Create new scaler for given bounds and viewport
    pub fn new(geo_bounds: GeoBounds, viewport: Viewport) -> Self {
        let mut scaler = Scaler {
            geo_bounds,
            viewport,
            scale_x: 1.0,
            scale_y: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            display_scale: 1.0,
            min_scale: 100.0,         // 1:100 (very zoomed in)
            max_scale: 100_000_000.0, // 1:100M (very zoomed out)
        };
        scaler.update_transform();
        scaler
    }

    /// Update viewport size
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
        self.update_transform();
    }

    /// Set geographic bounds to display
    pub fn set_bounds(&mut self, bounds: GeoBounds) {
        self.geo_bounds = bounds;
        self.update_transform();
    }

    /// Zoom to fit bounds in viewport
    pub fn zoom_to_fit(&mut self, bounds: GeoBounds) {
        self.geo_bounds = bounds;
        self.update_transform();
    }

    /// Zoom in by factor (> 1 = zoom in)
    pub fn zoom(&mut self, factor: f64, center: ScreenPoint) {
        // Convert center to world
        let world_center = self.screen_to_world(center);

        // Adjust bounds
        let half_width = self.geo_bounds.width() / (2.0 * factor);
        let half_height = self.geo_bounds.height() / (2.0 * factor);

        self.geo_bounds = GeoBounds::new(
            world_center.x - half_width,
            world_center.y - half_height,
            world_center.x + half_width,
            world_center.y + half_height,
        );

        // Clamp scale
        let new_scale = self.display_scale / factor;
        if new_scale >= self.min_scale && new_scale <= self.max_scale {
            self.update_transform();
        }
    }

    /// Pan by screen pixels
    pub fn pan(&mut self, dx: f32, dy: f32) {
        // Convert pixel delta to world delta
        let world_dx = -dx as f64 / self.scale_x;
        let world_dy = dy as f64 / self.scale_y; // Y is inverted

        self.geo_bounds.min_x += world_dx;
        self.geo_bounds.max_x += world_dx;
        self.geo_bounds.min_y += world_dy;
        self.geo_bounds.max_y += world_dy;

        self.update_transform();
    }

    /// Update internal transform parameters
    fn update_transform(&mut self) {
        let geo_width = self.geo_bounds.width();
        let geo_height = self.geo_bounds.height();

        if geo_width <= 0.0 || geo_height <= 0.0 {
            return;
        }

        // Calculate scale to fit bounds in viewport
        let scale_x = self.viewport.width as f64 / geo_width;
        let scale_y = self.viewport.height as f64 / geo_height;

        // Use uniform scale (maintain aspect ratio)
        let scale = scale_x.min(scale_y);
        self.scale_x = scale;
        self.scale_y = scale;

        // Calculate offset to center the display
        let rendered_width = geo_width * scale;
        let rendered_height = geo_height * scale;

        self.offset_x =
            (self.viewport.width as f64 - rendered_width) / 2.0 + self.viewport.x as f64;
        self.offset_y =
            (self.viewport.height as f64 - rendered_height) / 2.0 + self.viewport.y as f64;

        // Calculate display scale (approximate)
        // At equator: 1 degree ≈ 111 km
        let km_per_degree = 111.0;
        let meters_per_pixel = (km_per_degree * 1000.0) / scale;
        self.display_scale = meters_per_pixel * 96.0 / 0.0254; // Assuming 96 DPI
    }

    /// Convert world coordinate to screen coordinate
    #[inline]
    pub fn world_to_screen(&self, world: WorldPoint) -> ScreenPoint {
        let x = (world.x - self.geo_bounds.min_x) * self.scale_x + self.offset_x;
        // Y is inverted (screen Y increases downward)
        let y = (self.geo_bounds.max_y - world.y) * self.scale_y + self.offset_y;

        ScreenPoint::new(x as f32, y as f32)
    }

    /// Convert screen coordinate to world coordinate
    #[inline]
    pub fn screen_to_world(&self, screen: ScreenPoint) -> WorldPoint {
        let x = (screen.x as f64 - self.offset_x) / self.scale_x + self.geo_bounds.min_x;
        let y = self.geo_bounds.max_y - (screen.y as f64 - self.offset_y) / self.scale_y;

        WorldPoint::new(x, y)
    }

    /// Convert distance from pixels to world units (degrees)
    #[inline]
    pub fn screen_to_world_distance(&self, pixels: f32) -> f64 {
        pixels as f64 / self.scale_x
    }

    /// Convert distance from world units to pixels
    #[inline]
    pub fn world_to_screen_distance(&self, degrees: f64) -> f32 {
        (degrees * self.scale_x) as f32
    }

    /// Get current scale factor (pixels per degree)
    #[inline]
    pub fn scale(&self) -> f64 {
        self.scale_x
    }

    /// Get X scale factor (pixels per degree in X direction)
    #[inline]
    pub fn scale_x(&self) -> f64 {
        self.scale_x
    }

    /// Get Y scale factor (pixels per degree in Y direction)
    #[inline]
    pub fn scale_y(&self) -> f64 {
        self.scale_y
    }

    /// Get display scale as string (e.g., "1:50000")
    pub fn display_scale_string(&self) -> String {
        if self.display_scale >= 1_000_000.0 {
            format!("1:{:.1}M", self.display_scale / 1_000_000.0)
        } else if self.display_scale >= 1000.0 {
            format!("1:{:.0}K", self.display_scale / 1000.0)
        } else {
            format!("1:{:.0}", self.display_scale)
        }
    }

    /// Check if a world point is visible in current viewport
    #[inline]
    pub fn is_visible(&self, point: WorldPoint) -> bool {
        self.geo_bounds.contains(point)
    }

    /// Check if a bounding box intersects the viewport
    pub fn intersects(&self, bounds: GeoBounds) -> bool {
        !(bounds.max_x < self.geo_bounds.min_x
            || bounds.min_x > self.geo_bounds.max_x
            || bounds.max_y < self.geo_bounds.min_y
            || bounds.min_y > self.geo_bounds.max_y)
    }
}

impl Default for Scaler {
    fn default() -> Self {
        Scaler::new(
            GeoBounds::new(-180.0, -90.0, 180.0, 90.0),
            Viewport::new(800.0, 600.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_world_to_screen_roundtrip() {
        let scaler = Scaler::new(
            GeoBounds::new(0.0, 0.0, 10.0, 10.0),
            Viewport::new(100.0, 100.0),
        );

        let world = WorldPoint::new(5.0, 5.0);
        let screen = scaler.world_to_screen(world);
        let back = scaler.screen_to_world(screen);

        assert!((back.x - world.x).abs() < 0.001);
        assert!((back.y - world.y).abs() < 0.001);
    }

    #[test]
    fn test_center_point() {
        let bounds = GeoBounds::new(0.0, 0.0, 10.0, 10.0);
        let viewport = Viewport::new(100.0, 100.0);
        let scaler = Scaler::new(bounds, viewport);

        // Center of bounds should map to center of viewport
        let center = WorldPoint::new(5.0, 5.0);
        let screen = scaler.world_to_screen(center);

        assert!((screen.x - 50.0).abs() < 1.0);
        assert!((screen.y - 50.0).abs() < 1.0);
    }
}
