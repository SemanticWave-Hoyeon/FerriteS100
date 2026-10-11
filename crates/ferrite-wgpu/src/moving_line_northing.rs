//! Camera-independent exact WGS84 northing, never a screen-mesh reuse certificate.
use ferrite_render::{FlatProjection, Scaler, ScreenPoint, WorldPoint};
use rustc_hash::FxHashMap;
use std::sync::Arc;
const CAP: usize = 16 * 1024 * 1024;
const ENTRIES: usize = 4096;
#[derive(Clone, Copy)]
struct Slot {
    latitude: u64,
    token: Option<ferrite_kernel::map_camera::PreparedFlatNorthing>,
}
pub(crate) struct Entry {
    slots: Vec<Slot>,
}
impl Entry {
    pub(crate) fn project(&self, index: usize, point: WorldPoint, scaler: &Scaler) -> ScreenPoint {
        // Owned exact latitude bits gate every use, even a same-address mutation
        // or allocator-reused pointer. Fresh longitude/camera arithmetic remains.
        if scaler.projection() == FlatProjection::EllipsoidalMercator {
            if let Some(slot) = self
                .slots
                .get(index)
                .filter(|s| s.latitude == point.y.to_bits())
            {
                if let Some(token) = &slot.token {
                    return scaler.world_to_screen_with_prepared_northing(point, token);
                }
            }
        }
        scaler.world_to_screen(point)
    }
}
pub(crate) struct Cache {
    enabled: bool,
    epoch: Option<u64>,
    entries: FxHashMap<(usize, usize), Arc<Entry>>,
    bytes: usize,
    peak: usize,
    hits: u64,
    cold: u64,
    declines: u64,
}
impl Cache {
    /// Copy only the already sampled policy, not any published arena or GPU resource.
    pub(crate) fn fork_cold(&self) -> Self {
        Self::new(Some(std::ffi::OsStr::new(if self.enabled {
            "1"
        } else {
            "0"
        })))
    }

    pub(crate) fn new(flag: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: flag.and_then(|s| s.to_str()) == Some("1"),
            epoch: None,
            entries: FxHashMap::default(),
            bytes: 0,
            peak: 0,
            hits: 0,
            cold: 0,
            declines: 0,
        }
    }
    pub(crate) fn reset(&mut self) {
        self.entries = FxHashMap::default();
        self.bytes = 0;
        self.epoch = None;
    }
    pub(crate) fn bind_epoch(&mut self, epoch: u64) {
        if self.epoch != Some(epoch) {
            self.reset();
            self.epoch = Some(epoch);
        }
    }
    fn charge(&self, payload: usize) -> Option<usize> {
        self.bytes
            .checked_add(payload)?
            .checked_add(self.entries.capacity().checked_mul(256)?)
    }
    pub(crate) fn prepare(&mut self, points: &[WorldPoint], scaler: &Scaler) -> Option<Arc<Entry>> {
        if !self.enabled
            || self.epoch.is_none()
            || scaler.projection() != FlatProjection::EllipsoidalMercator
        {
            return None;
        }
        let key = (points.as_ptr() as usize, points.len());
        if let Some(entry) = self.entries.get(&key) {
            self.hits = self.hits.saturating_add(1);
            return Some(entry.clone());
        }
        self.cold = self.cold.saturating_add(1);
        let requested = points
            .len()
            .checked_mul(std::mem::size_of::<Slot>())?
            .checked_add(128)?;
        if points.len() < 2 || requested > CAP {
            self.declines += 1;
            return None;
        }
        if self.entries.len() >= ENTRIES || self.charge(requested).is_none_or(|b| b > CAP) {
            // Whole transparent cold fallback/reset, never drop a displayed line.
            self.entries = FxHashMap::default();
            self.bytes = 0;
        }
        if self.entries.try_reserve(1).is_err() || self.charge(requested).is_none_or(|b| b > CAP) {
            self.entries = FxHashMap::default();
            self.bytes = 0;
            self.declines += 1;
            return None;
        }
        let mut slots = Vec::new();
        if slots.try_reserve_exact(points.len()).is_err() {
            self.declines += 1;
            return None;
        }
        let payload = slots
            .capacity()
            .checked_mul(std::mem::size_of::<Slot>())?
            .checked_add(128)?;
        if self.charge(payload).is_none_or(|b| b > CAP) {
            self.declines += 1;
            return None;
        }
        for point in points {
            slots.push(Slot {
                latitude: point.y.to_bits(),
                token: scaler.prepare_flat_northing(point.y).ok(),
            });
        }
        let entry = Arc::new(Entry { slots });
        self.entries.insert(key, entry.clone());
        self.bytes += payload;
        self.peak = self.peak.max(self.charge(0).unwrap_or(CAP));
        Some(entry)
    }
    pub(crate) fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"enabled":self.enabled,"epoch":self.epoch,"entries":self.entries.len(),
            "charged_retained_bytes":self.charge(0),"peak":self.peak,"cap":CAP,
            "hits":self.hits,"cold":self.cold,"declines":self.declines,
            "scope":"exact northing slots only; not projected vertices/RSS/full frame gain"})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn camera(bounds: [f64; 4], viewport: [f32; 2]) -> Scaler {
        let mut s = Scaler::new(
            ferrite_render::GeoBounds::new(bounds[0], bounds[1], bounds[2], bounds[3]),
            ferrite_render::Viewport::new(viewport[0], viewport[1]),
        );
        s.set_projection(FlatProjection::EllipsoidalMercator);
        s
    }
    fn bits(p: ScreenPoint) -> [u32; 2] {
        [p.x.to_bits(), p.y.to_bits()]
    }
    #[test]
    fn moving_camera_and_viewport_exact_f32_matches_independent_legacy() {
        let points = [
            WorldPoint::new(-4., 48.),
            WorldPoint::new(-3., 49.),
            WorldPoint::new(359., -20.),
        ];
        let a = camera([-10., 40., 10., 60.], [1280., 808.]);
        let mut cache = Cache::new(Some("1".as_ref()));
        cache.bind_epoch(1);
        let entry = cache.prepare(&points, &a).unwrap();
        for scaler in [
            a,
            camera([-5., 45., 0., 50.], [800., 600.]),
            camera([350., -30., 370., 10.], [2048., 1024.]),
        ] {
            for (i, p) in points.iter().enumerate() {
                assert_eq!(
                    bits(entry.project(i, *p, &scaler)),
                    bits(scaler.world_to_screen(*p))
                );
            }
        }
    }
    #[test]
    fn exact_changed_latitude_same_owner_never_returns_stale_northing() {
        let mut points = [WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)];
        let s = camera([0., 30., 5., 50.], [800., 600.]);
        let mut c = Cache::new(Some("1".as_ref()));
        c.bind_epoch(1);
        let entry = c.prepare(&points, &s).unwrap();
        points[0].y = 41.;
        points[1].x = 3.;
        let hit = c.prepare(&points, &s).unwrap();
        assert!(Arc::ptr_eq(&entry, &hit));
        for (i, p) in points.iter().enumerate() {
            assert_eq!(bits(hit.project(i, *p, &s)), bits(s.world_to_screen(*p)));
        }
    }
    #[test]
    fn nonfinite_and_invalid_polar_inputs_preserve_legacy_nan_bits() {
        let p = [
            WorldPoint::new(f64::NAN, 40.),
            WorldPoint::new(1., 90.),
            WorldPoint::new(1., f64::INFINITY),
        ];
        let s = camera([0., 30., 5., 50.], [800., 600.]);
        let mut c = Cache::new(Some("1".as_ref()));
        c.bind_epoch(1);
        let e = c.prepare(&p, &s).unwrap();
        for (i, p) in p.iter().enumerate() {
            assert_eq!(bits(e.project(i, *p, &s)), bits(s.world_to_screen(*p)));
        }
    }
    #[test]
    fn new_source_epoch_releases_old_entries_but_owned_view_stays_valid() {
        let p = [WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)];
        let s = camera([0., 30., 5., 50.], [800., 600.]);
        let mut c = Cache::new(Some("1".as_ref()));
        c.bind_epoch(1);
        let a = c.prepare(&p, &s).unwrap();
        c.bind_epoch(2);
        assert!(c.entries.is_empty());
        let b = c.prepare(&p, &s).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(bits(a.project(0, p[0], &s)), bits(s.world_to_screen(p[0])));
    }
    #[test]
    fn disabled_and_actual_oversize_take_original_path_without_retention() {
        let s = camera([0., 30., 5., 50.], [800., 600.]);
        let p = [WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)];
        let mut off = Cache::new(None);
        off.bind_epoch(1);
        assert!(off.prepare(&p, &s).is_none());
        assert!(off.entries.is_empty());
        let mut on = Cache::new(Some("1".as_ref()));
        on.bind_epoch(1);
        let large = vec![p[0]; CAP / std::mem::size_of::<Slot>() + 1];
        assert!(on.prepare(&large, &s).is_none());
        assert!(on.entries.is_empty());
        assert_eq!(on.bytes, 0);
    }
}

#[cfg(test)]
mod projection_controls {
    use super::*;
    #[test]
    fn projection_or_dpi_change_never_reuses_screen_coordinates() {
        let points = [WorldPoint::new(1., 40.), WorldPoint::new(2., 42.)];
        let mut a = Scaler::new(
            ferrite_render::GeoBounds::new(0., 30., 5., 50.),
            ferrite_render::Viewport::new(800., 600.),
        );
        a.set_projection(FlatProjection::EllipsoidalMercator);
        let mut c = Cache::new(Some("1".as_ref()));
        c.bind_epoch(1);
        let e = c.prepare(&points, &a).unwrap();
        a.set_pixel_ratio(2.0);
        let p = e.project(0, points[0], &a);
        let old = a.world_to_screen(points[0]);
        assert_eq!(
            [p.x.to_bits(), p.y.to_bits()],
            [old.x.to_bits(), old.y.to_bits()]
        );
        a.set_projection(FlatProjection::LocalGeographic);
        assert!(c.prepare(&points, &a).is_none());
        let p = e.project(0, points[0], &a);
        let old = a.world_to_screen(points[0]);
        assert_eq!(
            [p.x.to_bits(), p.y.to_bits()],
            [old.x.to_bits(), old.y.to_bits()]
        );
    }
}
