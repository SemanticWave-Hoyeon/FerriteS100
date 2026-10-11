//! Single actual-camera pure-geometry cache. Never a visibility/material/pick certificate.
use crate::{
    DisplaySettings, LineInstruction, PortrayalPath, Scaler, StaticInstructionOrderIdentity,
    ViewingGroupState, WorldPoint,
};
use std::{collections::HashMap, sync::Arc};
type Runs = Arc<[Arc<[WorldPoint]>]>;
const BYTE_CAP: usize = 8 * 1024 * 1024;
const ENTRY_CAP: usize = 4096;
const ENTRY_BYTE_CAP: usize = 512 * 1024;
#[derive(Debug, Default, Clone, Copy)]
pub struct RetainedPathCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub declines: u64,
    pub retained_payload_bytes: usize,
    pub entries: usize,
}
#[derive(Debug)]
struct Key {
    owner: Arc<StaticInstructionOrderIdentity>,
    revision: u64,
    coverage_revision: u64,
    camera: [u64; 16],
    transform: [u64; 6],
    settings: DisplaySettings,
    groups: ViewingGroupState,
}
#[derive(Debug, Default)]
pub(crate) struct RetainedPathCache {
    key: Option<Key>,
    entries: HashMap<usize, Runs>,
    used: usize,
    work: RetainedPathCacheStats,
}
impl RetainedPathCache {
    pub(crate) fn clear(&mut self) {
        self.key = None;
        self.entries = HashMap::new();
        self.used = 0;
    }
    pub(crate) fn stats(&self) -> RetainedPathCacheStats {
        RetainedPathCacheStats {
            retained_payload_bytes: self.used,
            entries: self.entries.len(),
            ..self.work
        }
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Exact source/camera/policy owners are independently checked"
    )]
    pub(crate) fn resolve(
        &mut self,
        owner: Arc<StaticInstructionOrderIdentity>,
        revision: u64,
        coverage_revision: u64,
        settings: &DisplaySettings,
        groups: &ViewingGroupState,
        policy_bytes: usize,
        ordinal: usize,
        line: &LineInstruction,
        scaler: &Scaler,
    ) -> Option<Runs> {
        // First phase only direct geographic paths. Group size cannot be bounded by its per-arc limit alone.
        let path = line.portrayal_path.as_ref()?;
        if !matches!(
            path,
            PortrayalPath::GeographicArc { .. } | PortrayalPath::GeographicAnnulus { .. }
        ) {
            return None;
        }
        line.points.first()?;
        let camera = scaler.flat_encoded_identity()?;
        let values = [
            scaler.scale_x(),
            scaler.scale_y(),
            scaler.offset_x(),
            scaler.offset_y(),
            scaler.pixels_per_mm(),
            scaler.display_scale,
        ];
        if !values.iter().all(|v| v.is_finite()) || policy_bytes > BYTE_CAP / 2 {
            self.work.declines += 1;
            return None;
        }
        let transform = values.map(f64::to_bits);
        let matches = self.key.as_ref().is_some_and(|k| {
            Arc::ptr_eq(&k.owner, &owner)
                && k.revision == revision
                && k.coverage_revision == coverage_revision
                && k.camera == camera
                && k.transform == transform
                && k.settings == *settings
                && k.groups == *groups
        });
        if !matches {
            self.clear();
            self.used = policy_bytes;
            self.key = Some(Key {
                owner,
                revision,
                coverage_revision,
                camera,
                transform,
                settings: settings.clone(),
                groups: groups.clone(),
            });
        }
        if let Some(runs) = self.entries.get(&ordinal) {
            self.work.hits += 1;
            return Some(Arc::clone(runs));
        }
        self.work.misses += 1;
        // GeoArc <=4097 points; direct annulus <=four bounded arc/radial runs.
        // Compute original pure resolver unchanged: no approximation or dropped segments.
        let points = line
            .render_paths(scaler)
            .map(|run| run.into_owned())
            .collect::<Vec<_>>();
        if points.iter().all(|run| run.len() < 2) {
            self.work.declines += 1;
            return Some(
                points
                    .into_iter()
                    .map(Arc::<[WorldPoint]>::from)
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        let bytes = points.iter().try_fold(128usize, |sum, run| {
            sum.checked_add(
                run.len()
                    .checked_mul(std::mem::size_of::<WorldPoint>())?
                    .checked_add(64)?,
            )
        })?;
        let runs: Runs = points
            .into_iter()
            .map(Arc::<[WorldPoint]>::from)
            .collect::<Vec<_>>()
            .into();
        if bytes > ENTRY_BYTE_CAP
            || self.entries.len() >= ENTRY_CAP
            || self.used.checked_add(bytes)? > BYTE_CAP
        {
            self.work.declines += 1;
            return Some(runs);
        }
        self.used += bytes;
        self.entries.insert(ordinal, Arc::clone(&runs));
        Some(runs)
    }
}
#[cfg(test)]
mod shared_path_tests {
    use super::*;
    use crate::{
        DrawingInstruction, GeoBounds, LineInstruction, RenderContext, ResolvedLinePath, Viewport,
    };
    fn fixture(sweep: f64) -> RenderContext {
        let mut c = RenderContext::new(Viewport::new(800., 600.));
        c.set_retained_path_cache_enabled(true);
        c.set_bounds(GeoBounds::new(179.8, 49.8, 180.2, 50.2));
        let p = WorldPoint::new(179.9, 50.);
        let mut line = LineInstruction::new(vec![p, p]);
        line.portrayal_path = Some(PortrayalPath::GeographicAnnulus {
            center: (p.x, p.y),
            outer: 1000.,
            inner: 500.,
            start: 0.,
            sweep,
        });
        line.style.dash_pattern = vec![8., 5.];
        c.add_instruction(DrawingInstruction::Line(line));
        c
    }
    fn shared(c: &RenderContext) -> Arc<[WorldPoint]> {
        match c.resolved_line_paths(0, &c.scaler).unwrap().next().unwrap() {
            ResolvedLinePath::Shared(p) => p,
            _ => panic!("Expected shared geographic run"),
        }
    }
    fn bits(p: &[WorldPoint]) -> Vec<[u64; 2]> {
        p.iter().map(|p| [p.x.to_bits(), p.y.to_bits()]).collect()
    }
    #[test]
    fn exact_runs_reused_and_dashes_pick_wrap_keep_original_phase() {
        for sweep in [360., -360., 270., -270.] {
            let c = fixture(sweep);
            let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
                unreachable!()
            };
            let original = line
                .render_paths(&c.scaler)
                .map(|p| bits(&p))
                .collect::<Vec<_>>();
            let cached = c
                .resolved_line_paths(0, &c.scaler)
                .unwrap()
                .map(|p| bits(&p))
                .collect::<Vec<_>>();
            assert_eq!(original, cached);
            assert_eq!(cached.len(), if sweep.abs() == 360. { 2 } else { 1 });
            let a = shared(&c);
            let b = shared(&c);
            assert!(Arc::ptr_eq(&a, &b));
            for run in line.render_paths(&c.scaler) {
                for p in run.iter().step_by(13) {
                    let q = c.scaler.world_to_screen(*p);
                    for dx in [-2., 0., 2.] {
                        let query = crate::ScreenPoint::new(q.x + dx, q.y);
                        let old = crate::hit_geometry_wrapped_visible(
                            &c.raw_instructions()[0],
                            &c.scaler,
                            query,
                            3.,
                            None,
                            true,
                        );
                        let new = crate::hit_geometry_wrapped_visible_in_context(
                            &c, 0, query, 3., None, true,
                        );
                        let bits = |h: Option<crate::WrappedGeometryHit>| {
                            h.map(|h| {
                                [
                                    h.hit.distance.to_bits(),
                                    u64::from(h.hit.nearest.x.to_bits()),
                                    u64::from(h.hit.nearest.y.to_bits()),
                                    h.longitude_shift.to_bits(),
                                ]
                            })
                        };
                        assert_eq!(bits(old), bits(new));
                    }
                }
            }
            assert!(c.retained_path_cache_stats().hits > 0);
        }
    }
    #[test]
    fn direct_geographic_arc_invalid_empty_and_degenerate_run_structure_matches_original() {
        for path in [
            PortrayalPath::GeographicArc {
                center: (0., 50.),
                radius_m: -1.,
                start: 0.,
                sweep: 90.,
            },
            PortrayalPath::GeographicArc {
                center: (0., 91.),
                radius_m: 1.,
                start: 0.,
                sweep: 90.,
            },
            PortrayalPath::GeographicArc {
                center: (0., 50.),
                radius_m: f64::NAN,
                start: 0.,
                sweep: 90.,
            },
            PortrayalPath::GeographicArc {
                center: (0., 50.),
                radius_m: 1.,
                start: 0.,
                sweep: 361.,
            },
            PortrayalPath::GeographicArc {
                center: (0., 50.),
                radius_m: 0.,
                start: 0.,
                sweep: 0.,
            },
            PortrayalPath::GeographicArc {
                center: (0., 50.),
                radius_m: 1000.,
                start: 0.,
                sweep: 90.,
            },
        ] {
            let mut c = RenderContext::new(Viewport::new(800., 600.));
            c.set_retained_path_cache_enabled(true);
            c.set_bounds(GeoBounds::new(-0.2, 49.8, 0.2, 50.2));
            let mut line = LineInstruction::new(vec![WorldPoint::new(0., 50.)]);
            line.portrayal_path = Some(path);
            c.add_instruction(DrawingInstruction::Line(line));
            let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
                unreachable!()
            };
            let original = line
                .render_paths(&c.scaler)
                .map(|p| bits(&p))
                .collect::<Vec<_>>();
            assert_eq!(original.len(), 1); // even a rejected GeoArc has one empty Single run
            for _ in 0..2 {
                assert_eq!(
                    original,
                    c.resolved_line_paths(0, &c.scaler)
                        .unwrap()
                        .map(|p| bits(&p))
                        .collect::<Vec<_>>()
                );
            }
            if original.iter().all(|p| p.len() < 2) {
                assert_eq!(c.retained_path_cache_stats().entries, 0);
            }
        }
    }
    #[test]
    fn camera_density_policy_source_and_private_rebuild_invalidate_without_mutating_old() {
        let mut c = fixture(270.);
        let old = shared(&c);
        let again = shared(&c);
        assert!(Arc::ptr_eq(&old, &again));
        c.scaler.set_pixel_ratio(2.);
        let dpi = shared(&c);
        assert!(!Arc::ptr_eq(&old, &dpi));
        c.settings.current_date = Some("2026-10-08".into());
        let date = shared(&c);
        assert!(!Arc::ptr_eq(&dpi, &date));
        c.viewing_groups.set_visible(13030, false);
        let groups = shared(&c);
        assert!(!Arc::ptr_eq(&date, &groups));
        let mut candidate = c.empty_for_rebuild();
        candidate.add_instruction(c.raw_instructions()[0].clone());
        let candidate_run = shared(&candidate);
        assert!(!Arc::ptr_eq(&groups, &candidate_run));
        drop(candidate);
        assert!(Arc::ptr_eq(&groups, &shared(&c))); // abandoned/private publication cannot clear original cache
        c.set_instructions_from_cache(c.raw_instructions().to_vec());
        let source = shared(&c);
        assert!(!Arc::ptr_eq(&groups, &source));
        c.set_retained_path_cache_enabled(false);
        assert_eq!(c.retained_path_cache_stats().retained_payload_bytes, 0);
        let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
            unreachable!()
        };
        assert_eq!(
            c.resolved_line_paths(0, &c.scaler)
                .unwrap()
                .map(|p| bits(&p))
                .collect::<Vec<_>>(),
            line.render_paths(&c.scaler)
                .map(|p| bits(&p))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn full_cache_declines_retention_without_dropping_exact_runs_or_borrowed_ordinary_lines() {
        let c = fixture(270.);
        let DrawingInstruction::Line(line) = &c.raw_instructions()[0] else {
            unreachable!()
        };
        let mut cache = RetainedPathCache::default();
        let owner = c.static_instruction_order_identity();
        let first = cache
            .resolve(
                Arc::clone(&owner),
                c.geometry_revision(),
                0,
                &c.settings,
                &c.viewing_groups,
                1024,
                0,
                line,
                &c.scaler,
            )
            .unwrap();
        // Exercise the hard admission boundary without a slow multi-thousand-geodesic fixture.
        cache.used = BYTE_CAP;
        let overflow = cache
            .resolve(
                owner,
                c.geometry_revision(),
                0,
                &c.settings,
                &c.viewing_groups,
                1024,
                1,
                line,
                &c.scaler,
            )
            .unwrap();
        assert_eq!(
            first.iter().map(|p| bits(p)).collect::<Vec<_>>(),
            overflow.iter().map(|p| bits(p)).collect::<Vec<_>>()
        );
        assert!(!cache.entries.contains_key(&1));
        let stats = cache.stats();
        assert!(stats.retained_payload_bytes <= BYTE_CAP);
        assert!(stats.entries <= ENTRY_CAP);
        assert!(stats.declines > 0);
        let mut ordinary = RenderContext::new(Viewport::new(800., 600.));
        ordinary.set_retained_path_cache_enabled(true);
        ordinary.add_instruction(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ])));
        assert!(matches!(
            ordinary
                .resolved_line_paths(0, &ordinary.scaler)
                .unwrap()
                .next()
                .unwrap(),
            ResolvedLinePath::Original(std::borrow::Cow::Borrowed(_))
        ));
    }
}
