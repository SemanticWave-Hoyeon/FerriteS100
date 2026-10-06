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

    /// Compare a measured UI rectangle in physical pixels. Invalid/empty layout
    /// never matches; sub-hundredth-pixel rounding noise does not force rebuilds.
    pub fn matches_physical_rect(&self, rect: (f32, f32, f32, f32)) -> bool {
        let values = [rect.0, rect.1, rect.2, rect.3];
        values.iter().all(|v| v.is_finite())
            && rect.2 > 0.
            && rect.3 > 0.
            && [self.x, self.y, self.width, self.height]
                .into_iter()
                .zip(values)
                .all(|(a, b)| (a - b).abs() <= 0.01)
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

/// Internal flat-view coordinates are angular Mercator coordinates (metre
/// coordinates divided by WGS84 a and converted to degrees). Geographic APIs
/// still return longitude/latitude. The two axes share the same unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize)]
pub enum FlatProjection {
    #[default]
    LocalGeographic,
    EllipsoidalMercator,
}
impl FlatProjection {
    // A finite camera limit, not a claim that Mercator represents the poles.
    pub const CAMERA_LATITUDE_LIMIT: f64 = 89.5;
    pub fn project_y(self, latitude: f64) -> f64 {
        if self == Self::LocalGeographic {
            return latitude;
        }
        use ferrite_kernel::geodesy::{GeographicPosition, Mercator, WGS84_A};
        GeographicPosition::new(latitude, 0.)
            .and_then(|p| Mercator::World.project(p))
            .map(|p| (p[1] / WGS84_A).to_degrees())
            .unwrap_or(f64::NAN)
    }
    pub fn unproject_y(self, northing: f64) -> f64 {
        if self == Self::LocalGeographic {
            return northing;
        }
        use ferrite_kernel::geodesy::{Mercator, WGS84_A};
        Mercator::World
            .unproject([0., northing.to_radians() * WGS84_A])
            .map(|p| p.latitude())
            .unwrap_or(f64::NAN)
    }
    pub fn projected_bounds(self, mut b: GeoBounds) -> Option<GeoBounds> {
        if self == Self::EllipsoidalMercator {
            b.min_y = b.min_y.max(-Self::CAMERA_LATITUDE_LIMIT);
            b.max_y = b.max_y.min(Self::CAMERA_LATITUDE_LIMIT);
        }
        let p = GeoBounds::new(
            b.min_x,
            self.project_y(b.min_y),
            b.max_x,
            self.project_y(b.max_y),
        );
        ([p.min_x, p.min_y, p.max_x, p.max_y]
            .iter()
            .all(|v| v.is_finite())
            && p.width() > 0.
            && p.height() > 0.)
            .then_some(p)
    }
    pub fn geographic_bounds(self, p: GeoBounds) -> Option<GeoBounds> {
        let b = GeoBounds::new(
            p.min_x,
            self.unproject_y(p.min_y),
            p.max_x,
            self.unproject_y(p.max_y),
        );
        ([b.min_x, b.min_y, b.max_x, b.max_y]
            .iter()
            .all(|v| v.is_finite())
            && b.width() > 0.
            && b.height() > 0.)
            .then_some(b)
    }
    pub fn view_bounds(self, base: GeoBounds, zoom: f64, pan: [f64; 2]) -> Option<GeoBounds> {
        if !zoom.is_finite() || zoom <= 0. || pan.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let p = self.projected_bounds(base)?;
        let width = p.width() / zoom;
        let mut height = p.height() / zoom;
        let cx = p.center().x + pan[0];
        let mut cy = p.center().y + pan[1];
        if self == Self::EllipsoidalMercator {
            let limit = self.project_y(Self::CAMERA_LATITUDE_LIMIT);
            height = height.min(2. * limit);
            cy = cy.clamp(-limit + height / 2., limit - height / 2.);
        }
        self.geographic_bounds(GeoBounds::new(
            cx - width / 2.,
            cy - height / 2.,
            cx + width / 2.,
            cy + height / 2.,
        ))
    }
    pub fn pan_between(self, base: GeoBounds, view: GeoBounds) -> Option<[f64; 2]> {
        let b = self.projected_bounds(base)?.center();
        let v = self.projected_bounds(view)?.center();
        Some([v.x - b.x, v.y - b.y])
    }
    pub fn pan_to(self, base: GeoBounds, center: WorldPoint) -> Option<[f64; 2]> {
        let b = self.projected_bounds(base)?.center();
        let y = self.project_y(center.y);
        (center.x.is_finite() && y.is_finite()).then_some([center.x - b.x, y - b.y])
    }
}
/// Snapshot for cached projected geometry. Mixing projection families is never
/// an affine navigation operation; the caller must rebuild in that case.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct FlatTransform {
    pub projection: FlatProjection,
    pub scale: [f64; 2],
    pub offset: [f64; 2],
    pub geographic_origin: [f64; 2],
}
/// Coordinate transformation scaler
///
/// Handles conversion between world (geographic) and screen (pixel) coordinates.
/// Geographic input; explicitly selected flat projection for display.
#[derive(Debug, Clone)]
pub struct Scaler {
    projection: FlatProjection,
    camera: ferrite_kernel::map_camera::MapCamera,
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
    /// Physical pixels per UI point (96 DPI baseline).
    pixel_ratio: f64,
    /// Minimum scale (zoom out limit)
    pub min_scale: f64,
    /// Maximum scale (zoom in limit)
    pub max_scale: f64,
}

impl Scaler {
    /// Create new scaler for given bounds and viewport
    pub fn new(geo_bounds: GeoBounds, viewport: Viewport) -> Self {
        let mut scaler = Scaler {
            projection: FlatProjection::LocalGeographic,
            camera: ferrite_kernel::map_camera::MapCamera::Flat(
                ferrite_kernel::map_camera::FlatMapCamera::new(
                    ferrite_kernel::map_camera::AngularProjection::Geographic,
                    [0., 0.],
                    [1., 1.],
                    [0., 0.],
                )
                .expect("constant camera transform"),
            ),
            geo_bounds,
            viewport,
            scale_x: 1.0,
            scale_y: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            display_scale: 1.0,
            pixel_ratio: 1.0,
            min_scale: 100.0,         // 1:100 (very zoomed in)
            max_scale: 100_000_000.0, // 1:100M (very zoomed out)
        };
        scaler.update_transform();
        scaler
    }

    pub fn projection(&self) -> FlatProjection {
        self.projection
    }
    pub fn set_projection(&mut self, projection: FlatProjection) {
        self.projection = projection;
        if let Some(p) = projection.projected_bounds(self.geo_bounds) {
            if let Some(b) = projection.geographic_bounds(p) {
                self.geo_bounds = b;
            }
        }
        self.update_transform();
    }
    /// Snapshot of the actual camera plus physical viewport/DPI. No qualification.
    pub fn flat_encoded_identity(&self) -> Option<[u64; 16]> {
        let ferrite_kernel::map_camera::MapCamera::Flat(camera) = &self.camera;
        let c = camera.encoded_identity();
        Some([c[0],c[1],c[2],c[3],c[4],c[5],c[6],
            self.geo_bounds.min_x.to_bits(),self.geo_bounds.min_y.to_bits(),self.geo_bounds.max_x.to_bits(),self.geo_bounds.max_y.to_bits(),
            u64::from(self.viewport.x.to_bits()),u64::from(self.viewport.y.to_bits()),u64::from(self.viewport.width.to_bits()),u64::from(self.viewport.height.to_bits()),self.pixel_ratio.to_bits()])
    }
    pub fn flat_transform(&self) -> FlatTransform {
        FlatTransform {
            projection: self.projection,
            scale: [self.scale_x, self.scale_y],
            offset: [self.offset_x, self.offset_y],
            geographic_origin: [self.geo_bounds.min_x, self.geo_bounds.max_y],
        }
    }
    pub fn pixels_per_mm(&self) -> f64 {
        96.0 * self.pixel_ratio / 25.4
    }

    pub fn set_pixel_ratio(&mut self, ratio: f64) {
        if ratio.is_finite() && ratio > 0.0 {
            self.pixel_ratio = ratio;
            self.update_transform();
        }
    }

    /// Update viewport size
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
        self.update_transform();
    }

    /// Set geographic bounds to display
    pub fn set_bounds(&mut self, bounds: GeoBounds) {
        let Some(p) = self.projection.projected_bounds(bounds) else {
            return;
        };
        let Some(bounds) = self.projection.geographic_bounds(p) else {
            return;
        };
        self.geo_bounds = bounds;
        self.update_transform();
    }

    /// Zoom to fit bounds in viewport
    pub fn zoom_to_fit(&mut self, bounds: GeoBounds) {
        let Some(p) = self.projection.projected_bounds(bounds) else {
            return;
        };
        let Some(bounds) = self.projection.geographic_bounds(p) else {
            return;
        };
        self.geo_bounds = bounds;
        self.update_transform();
    }

    /// Zoom in by factor (> 1 = zoom in)
    pub fn zoom(&mut self, factor: f64, center: ScreenPoint) {
        // Convert center to world
        let world_center = self.screen_to_world(center);

        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let new_scale = self.display_scale / factor;
        if new_scale < self.min_scale || new_scale > self.max_scale {
            return;
        }
        if self.projection == FlatProjection::EllipsoidalMercator {
            let p = self.projection.projected_bounds(self.geo_bounds).unwrap();
            let q = self.projection.project_y(world_center.y);
            if let Some(b) = self.projection.geographic_bounds(GeoBounds::new(
                world_center.x - (world_center.x - p.min_x) / factor,
                q - (q - p.min_y) / factor,
                world_center.x + (p.max_x - world_center.x) / factor,
                q + (p.max_y - q) / factor,
            )) {
                self.geo_bounds = b;
                self.update_transform();
            }
            return;
        }
        // Preserve the world location underneath the requested zoom pivot.
        let left = (world_center.x - self.geo_bounds.min_x) / factor;
        let right = (self.geo_bounds.max_x - world_center.x) / factor;
        let bottom = (world_center.y - self.geo_bounds.min_y) / factor;
        let top = (self.geo_bounds.max_y - world_center.y) / factor;
        self.geo_bounds = GeoBounds::new(
            world_center.x - left,
            world_center.y - bottom,
            world_center.x + right,
            world_center.y + top,
        );
        self.update_transform();
    }

    /// Pan by screen pixels
    pub fn pan(&mut self, dx: f32, dy: f32) {
        // Convert pixel delta to world delta
        let world_dx = -dx as f64 / self.scale_x;
        let world_dy = dy as f64 / self.scale_y; // Y is inverted

        self.geo_bounds.min_x += world_dx;
        self.geo_bounds.max_x += world_dx;
        let p = self.projection.projected_bounds(self.geo_bounds).unwrap();
        if let Some(b) = self.projection.geographic_bounds(GeoBounds::new(
            self.geo_bounds.min_x,
            p.min_y + world_dy,
            self.geo_bounds.max_x,
            p.max_y + world_dy,
        )) {
            self.geo_bounds = b;
        }

        self.update_transform();
    }

    /// Update internal transform parameters
    fn update_transform(&mut self) {
        let geo_width = self.geo_bounds.width();
        let geo_height = self.projection.project_y(self.geo_bounds.max_y)
            - self.projection.project_y(self.geo_bounds.min_y);

        if !geo_width.is_finite()
            || !geo_height.is_finite()
            || geo_width <= 0.0
            || geo_height <= 0.0
        {
            return;
        }

        // Calculate scale to fit bounds in viewport
        // Local equirectangular projection: east-west degrees shrink with latitude.
        let longitude_factor = if self.projection == FlatProjection::EllipsoidalMercator {
            1.
        } else {
            self.geo_bounds
                .center()
                .y
                .to_radians()
                .cos()
                .abs()
                .max(1e-6)
        };
        let scale_x = self.viewport.width as f64 / (geo_width * longitude_factor);
        let scale_y = self.viewport.height as f64 / geo_height;

        // Use uniform scale (maintain aspect ratio)
        let scale = scale_x.min(scale_y);
        self.scale_x = scale * longitude_factor;
        self.scale_y = scale;

        // Calculate offset to center the display
        let rendered_width = geo_width * self.scale_x;
        let rendered_height = geo_height * scale;

        self.offset_x =
            (self.viewport.width as f64 - rendered_width) / 2.0 + self.viewport.x as f64;
        self.offset_y =
            (self.viewport.height as f64 - rendered_height) / 2.0 + self.viewport.y as f64;

        // Calculate display scale (approximate)
        // At equator: 1 degree ≈ 111 km
        let km_per_degree = 111.319490793;
        let mut meters_per_pixel = (km_per_degree * 1000.0) / scale;
        if self.projection == FlatProjection::EllipsoidalMercator {
            use ferrite_kernel::geodesy::WGS84_F;
            let q = (self.projection.project_y(self.geo_bounds.min_y)
                + self.projection.project_y(self.geo_bounds.max_y))
                / 2.;
            let phi = self.projection.unproject_y(q).to_radians();
            let e2 = WGS84_F * (2. - WGS84_F);
            let k = (1. - e2 * phi.sin().powi(2)).sqrt() / phi.cos();
            meters_per_pixel /= k;
        }
        self.display_scale = meters_per_pixel * 96.0 * self.pixel_ratio / 0.0254;
        use ferrite_kernel::map_camera::{AngularProjection, FlatMapCamera, MapCamera};
        let projection = match self.projection {
            FlatProjection::LocalGeographic => AngularProjection::Geographic,
            FlatProjection::EllipsoidalMercator => AngularProjection::EllipsoidalMercator,
        };
        if let Ok(camera) = FlatMapCamera::new(
            projection,
            [self.geo_bounds.min_x, self.geo_bounds.max_y],
            [self.scale_x, self.scale_y],
            [self.offset_x, self.offset_y],
        ) {
            self.camera = MapCamera::Flat(camera);
        }
    }

    /// Convert world coordinate to screen coordinate
    #[inline]
    pub fn world_to_screen(&self, world: WorldPoint) -> ScreenPoint {
        let p = self.world_to_screen_f64(world);
        ScreenPoint::new(p[0] as f32, p[1] as f32)
    }

    /// Unrounded projection for tessellation error checks. Display vertices
    /// still use world_to_screen's existing f32 conversion.
    pub fn world_to_screen_f64(&self, world: WorldPoint) -> [f64; 2] {
        let p = self
            .camera
            .project([world.x, world.y])
            .ok()
            .flatten()
            .unwrap_or([f64::NAN; 2]);
        p
    }

    /// Convert screen coordinate to world coordinate
    #[inline]
    pub fn screen_to_world(&self, screen: ScreenPoint) -> WorldPoint {
        let p = self
            .camera
            .unproject([screen.x as f64, screen.y as f64])
            .ok()
            .flatten()
            .unwrap_or([f64::NAN; 2]);
        WorldPoint::new(p[0], p[1])
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

    /// Get X offset (pixels)
    #[inline]
    pub fn offset_x(&self) -> f64 {
        self.offset_x
    }

    /// Get Y offset (pixels)
    #[inline]
    pub fn offset_y(&self) -> f64 {
        self.offset_y
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
    #[test]
    fn display_scale_is_independent_of_pixel_density() {
        let b = GeoBounds::new(-1.15, 50.75, -1.05, 50.85);
        let a = Scaler::new(b, Viewport::new(800.0, 600.0));
        let mut retina = Scaler::new(b, Viewport::new(1600.0, 1200.0));
        retina.set_pixel_ratio(2.0);
        assert!((a.display_scale - retina.display_scale).abs() < 1e-6);
    }
    #[test]
    fn rejected_zoom_preserves_bounds_and_transform() {
        let mut s = Scaler::default();
        let old = s.geo_bounds;
        s.zoom(1e20, ScreenPoint::new(100.0, 200.0));
        assert_eq!(s.geo_bounds.min_x, old.min_x);
        assert_eq!(s.geo_bounds.max_y, old.max_y);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    #[test]
    fn physical_rect_change_detects_each_axis() {
        let v = Viewport::with_origin(20., 60., 800., 600.);
        assert!(v.matches_physical_rect((20., 60., 800., 600.)));
        for r in [
            (21., 60., 800., 600.),
            (20., 61., 800., 600.),
            (20., 60., 801., 600.),
            (20., 60., 800., 601.),
        ] {
            assert!(!v.matches_physical_rect(r));
        }
    }
    #[test]
    fn physical_rect_rejects_invalid_layout_and_tolerates_roundoff() {
        let v = Viewport::new(800., 600.);
        assert!(v.matches_physical_rect((0.005, 0., 800., 600.)));
        assert!(!v.matches_physical_rect((0., 0., 0., 600.)));
        assert!(!v.matches_physical_rect((f32::NAN, 0., 800., 600.)));
    }
}

/// Exact affine bridge between two current flat scalers. The GPU applies this
/// to geographic anchors; fixed millimetre symbol offsets remain unscaled.
#[derive(Debug, Clone, Copy)]
pub struct ScreenAffine {
    pub scale: [f32; 2],
    pub translation: [f32; 2],
}
impl ScreenAffine {
    pub fn between(source: &Scaler, target: &Scaler) -> Option<Self> {
        Self::between_transform(source.flat_transform(), target)
    }
    pub fn between_transform(source: FlatTransform, target: &Scaler) -> Option<Self> {
        if source.projection != target.projection {
            return None;
        }
        let [sx, sy] = source.scale;
        let [ox, oy] = source.offset;
        let [min_x, max_y] = source.geographic_origin;
        if sx <= 0. || sy <= 0. {
            return None;
        }
        let scale = [target.scale_x() / sx, target.scale_y() / sy];
        let t = target.world_to_screen(WorldPoint::new(min_x, max_y));
        let translation = [t.x as f64 - ox * scale[0], t.y as f64 - oy * scale[1]];
        if !scale.iter().chain(&translation).all(|v| v.is_finite())
            || scale.iter().any(|v| *v <= 0.)
        {
            return None;
        }
        let view = Self {
            scale: scale.map(|v| v as f32),
            translation: translation.map(|v| v as f32),
        };
        (view
            .scale
            .iter()
            .chain(&view.translation)
            .all(|v| v.is_finite())
            && view.scale.iter().all(|v| *v > 0.))
        .then_some(view)
    }
    pub fn apply(&self, p: ScreenPoint) -> ScreenPoint {
        ScreenPoint::new(
            p.x * self.scale[0] + self.translation[0],
            p.y * self.scale[1] + self.translation[1],
        )
    }
}
pub fn anchored_zoom_bounds_projected(
    projection: FlatProjection,
    base: GeoBounds,
    viewport: Viewport,
    zoom: f64,
    anchor: WorldPoint,
    pivot: ScreenPoint,
    seed_latitude: f64,
) -> Option<GeoBounds> {
    if projection == FlatProjection::LocalGeographic {
        return anchored_zoom_bounds(base, viewport, zoom, anchor, pivot, seed_latitude);
    }
    let bounds = projection.view_bounds(base, zoom, [0., 0.])?;
    let mut scaler = Scaler::new(bounds, viewport);
    scaler.set_projection(projection);
    let at = scaler.screen_to_world(pivot);
    let pan = projection.pan_to(bounds, anchor)?;
    let current = projection.pan_to(bounds, at)?;
    let result = projection.view_bounds(bounds, 1., [pan[0] - current[0], pan[1] - current[1]])?;
    scaler.set_bounds(result);
    let actual = scaler.world_to_screen(anchor);
    ((actual.x - pivot.x).abs() < 0.02 && (actual.y - pivot.y).abs() < 0.02).then_some(result)
}
/// Solve the latitude-dependent flat view around an invariant world/screen
/// anchor. Newton steps have bounded iterations; an unresolvable camera leaves
/// the application on its last valid view instead of emitting nonfinite state.
pub fn anchored_zoom_bounds(
    base: GeoBounds,
    viewport: Viewport,
    zoom: f64,
    anchor: WorldPoint,
    pivot: ScreenPoint,
    seed_latitude: f64,
) -> Option<GeoBounds> {
    if ![
        base.width(),
        base.height(),
        zoom,
        viewport.width as f64,
        viewport.height as f64,
    ]
    .iter()
    .all(|v| v.is_finite() && *v > 0.)
        || ![
            anchor.x,
            anchor.y,
            pivot.x as f64,
            pivot.y as f64,
            seed_latitude,
        ]
        .iter()
        .all(|v| v.is_finite())
    {
        return None;
    }
    let width = base.width() / zoom;
    let height = base.height() / zoom;
    let at = |latitude: f64| {
        Scaler::new(
            GeoBounds::new(
                -width / 2.,
                latitude - height / 2.,
                width / 2.,
                latitude + height / 2.,
            ),
            viewport,
        )
    };
    let error = |latitude: f64| {
        let s = at(latitude);
        let w = s.screen_to_world(pivot);
        (w.y - anchor.y, s)
    };
    let mut latitude = seed_latitude;
    for _ in 0..32 {
        let (residual, s) = error(latitude);
        if !residual.is_finite() {
            return None;
        }
        if residual.abs() * s.scale_y().abs() < 0.002 {
            let w = s.screen_to_world(pivot);
            let longitude = anchor.x - w.x;
            let bounds = GeoBounds::new(
                longitude - width / 2.,
                latitude - height / 2.,
                longitude + width / 2.,
                latitude + height / 2.,
            );
            let actual = Scaler::new(bounds, viewport).world_to_screen(anchor);
            return ((actual.x - pivot.x).abs() < 0.02 && (actual.y - pivot.y).abs() < 0.02)
                .then_some(bounds);
        }
        let delta = 1e-5;
        let derivative = (error(latitude + delta).0 - error(latitude - delta).0) / (2. * delta);
        if !derivative.is_finite() || derivative.abs() < 1e-10 {
            return None;
        }
        let mut step = residual / derivative;
        let mut next = latitude - step;
        for _ in 0..16 {
            if error(next).0.abs() < residual.abs() {
                break;
            }
            step *= 0.5;
            next = latitude - step;
        }
        if next == latitude {
            return None;
        }
        latitude = next;
    }
    None
}
#[cfg(test)]
mod anchored_camera_tests {
    use super::*;
    #[test]
    fn zoom_anchor_and_fast_rebuild_mapping_agree_for_off_centre_repeated_scrolls() {
        for latitude in [-70., 0., 48.65, 80.] {
            for density in [1., 2., 3.] {
                let viewport = Viewport::with_origin(
                    75. * density,
                    40. * density,
                    1400. * density,
                    900. * density,
                );
                let base = GeoBounds::new(-2., latitude - 2., 2., latitude + 2.);
                let mut old = Scaler::new(base, viewport);
                for (zoom, x, y) in [
                    (1.15, 0.2, 0.8),
                    (0.7, 0.7, 0.15),
                    (25., 0.15, 0.75),
                    (200., 0.8, 0.2),
                    (2., 0.6, 0.65),
                ] {
                    let pivot = ScreenPoint::new(
                        viewport.x + viewport.width * x,
                        viewport.y + viewport.height * y,
                    );
                    let anchor = old.screen_to_world(pivot);
                    let bounds = anchored_zoom_bounds(
                        base,
                        viewport,
                        zoom,
                        anchor,
                        pivot,
                        old.geo_bounds.center().y,
                    )
                    .unwrap();
                    let target = Scaler::new(bounds, viewport);
                    let affine = ScreenAffine::between(&old, &target).unwrap();
                    let actual = target.world_to_screen(anchor);
                    assert!((actual.x - pivot.x).abs() < 0.03 && (actual.y - pivot.y).abs() < 0.03);
                    for offset in [(0., 0.), (0.001, 0.001), (-0.002, 0.003)] {
                        let w = WorldPoint::new(anchor.x + offset.0, anchor.y + offset.1);
                        let fast = affine.apply(old.world_to_screen(w));
                        let rebuilt = target.world_to_screen(w);
                        assert!(
                            (fast.x - rebuilt.x).abs() < 0.2 && (fast.y - rebuilt.y).abs() < 0.2,
                            "{fast:?} != {rebuilt:?}"
                        );
                    }
                    old = target;
                }
            }
        }
    }
    #[test]
    fn invalid_anchor_camera_inputs_are_rejected() {
        let base = GeoBounds::new(0., 0., 1., 1.);
        let view = Viewport::new(1000., 700.);
        for zoom in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(anchored_zoom_bounds(
                base,
                view,
                zoom,
                WorldPoint::new(0.5, 0.5),
                ScreenPoint::new(20., 30.),
                0.5
            )
            .is_none());
        }
    }
}

#[cfg(test)]
mod mercator_camera_tests {
    use super::*;
    use ferrite_kernel::geodesy::{direct, GeographicPosition};
    fn mercator(bounds: GeoBounds, viewport: Viewport) -> Scaler {
        let mut s = Scaler::new(bounds, viewport);
        s.set_projection(FlatProjection::EllipsoidalMercator);
        s
    }
    #[test]
    fn mercator_preserves_rhumb_midpoint_conformality_and_ground_scale() {
        use ferrite_kernel::rhumb::RhumbSegment;
        let projection = FlatProjection::EllipsoidalMercator;
        for lat in [-80., -45., 0., 48.65, 80.] {
            let viewport = Viewport::new(100000., 100000.);
            let s = mercator(
                GeoBounds::new(9.999, lat - 0.001, 10.001, lat + 0.001),
                viewport,
            );
            let focus = s.screen_to_world(viewport.center());
            let origin = GeographicPosition::new(focus.y, focus.x).unwrap();
            let p = s.world_to_screen(focus);
            let lengths: Vec<_> = [0., 45., 90., 135.]
                .into_iter()
                .map(|az| {
                    let q = direct(origin, az, 1.).unwrap();
                    let q = s.world_to_screen(WorldPoint::new(q.longitude(), q.latitude()));
                    ((q.x - p.x) as f64).hypot((q.y - p.y) as f64)
                })
                .collect();
            let expected = s.pixels_per_mm() * 1000. / s.display_scale;
            for d in lengths {
                assert!((d / expected - 1.).abs() < 1e-4, "{lat}: {d} vs {expected}");
            }
            let a = GeographicPosition::new(lat - 0.5, 9.).unwrap();
            let b = GeographicPosition::new(lat + 0.5, 11.).unwrap();
            let curve = RhumbSegment::new(a, b).unwrap();
            let y0 = projection.project_y(a.latitude());
            let y1 = projection.project_y(b.latitude());
            for f in [0.1, 0.5, 0.9] {
                let q = curve.point(f).unwrap();
                let ratio = (q.longitude() - a.longitude()) / (b.longitude() - a.longitude());
                assert!(
                    (projection.project_y(q.latitude()) - (y0 + (y1 - y0) * ratio)).abs() < 1e-10
                );
            }
        }
    }
    #[test]
    fn mercator_affine_anchor_pan_and_projection_change_are_consistent() {
        let projection = FlatProjection::EllipsoidalMercator;
        for lat in [-80., 0., 48.65, 80.] {
            let base = GeoBounds::new(170., lat - 1., 174., lat + 1.);
            let viewport = Viewport::with_origin(30., 45., 1000., 800.);
            let before = mercator(base, viewport);
            for zoom in [0.8, 1.7, 25., 200.] {
                let pivot = ScreenPoint::new(150., 650.);
                let anchor = before.screen_to_world(pivot);
                let bounds = anchored_zoom_bounds_projected(
                    projection, base, viewport, zoom, anchor, pivot, lat,
                )
                .unwrap();
                let mut after = before.clone();
                after.set_bounds(bounds);
                let a = after.world_to_screen(anchor);
                assert!((a.x - pivot.x).abs() < 0.02 && (a.y - pivot.y).abs() < 0.02);
                let affine = ScreenAffine::between(&before, &after).unwrap();
                for point in [
                    WorldPoint::new(170.2, lat - 0.4),
                    anchor,
                    WorldPoint::new(173.2, lat + 0.5),
                ] {
                    let a = affine.apply(before.world_to_screen(point));
                    let b = after.world_to_screen(point);
                    assert!((a.x - b.x).abs() < 0.05 && (a.y - b.y).abs() < 0.05);
                }
                let pan = projection.pan_between(base, bounds).unwrap();
                let rebuilt = projection.view_bounds(base, zoom, pan).unwrap();
                assert!((rebuilt.max_y - bounds.max_y).abs() < 1e-10);
            }
            let mut pan = before.clone();
            let p = WorldPoint::new(172., lat);
            let a = pan.world_to_screen(p);
            pan.pan(40., -25.);
            let b = pan.world_to_screen(p);
            assert!((b.x - a.x - 40.).abs() < 0.02 && (b.y - a.y + 25.).abs() < 0.02);
            let legacy = Scaler::new(base, viewport);
            assert!(ScreenAffine::between(&legacy, &before).is_none());
        }
    }
}
