//! Conservative screen-space selection broad phase. The exact portrayal hit
//! predicates remain authoritative for dash gaps, suppressed spans and holes.
use crate::{DrawingInstruction, LineSpan, Scaler, ScreenPoint};
use rstar::{RTree, RTreeObject, AABB};
#[derive(Debug, Clone)]
struct Envelope {
    ordinal: usize,
    bounds: AABB<[f64; 2]>,
}
impl RTreeObject for Envelope {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.bounds
    }
}
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SelectionIndexStats {
    pub primitives: usize,
    pub indexed: usize,
    pub fallback: usize,
    pub projected_vertices: usize,
    /// Payload only, excluding R-tree node/allocator overhead. No per-vertex cache.
    pub minimum_payload_bytes: usize,
    pub build_seconds: f64,
}
#[derive(Debug, Default)]
pub struct SelectionIndex {
    tree: RTree<Envelope>,
    instructions: Vec<usize>,
    fallback: Vec<usize>,
    signature: Option<[u64; 14]>,
    stats: SelectionIndexStats,
}
fn signature(s: &Scaler) -> [u64; 14] {
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
    ]
    .map(f64::to_bits)
}
impl SelectionIndex {
    pub fn clear(&mut self) {
        self.tree = RTree::new();
        self.instructions.clear();
        self.fallback.clear();
        self.signature = None;
        self.stats = SelectionIndexStats::default();
    }
    pub fn matches_scaler(&self, s: &Scaler) -> bool {
        self.signature == Some(signature(s))
    }
    pub fn statistics(&self) -> &SelectionIndexStats {
        &self.stats
    }
    /// One envelope per currently displayed instruction, preserving renderer order.
    /// Include supplied suppression endpoints even if a caller extrapolates a span.
    pub fn rebuild<'a>(
        &mut self,
        instructions: &[DrawingInstruction],
        displayed: &[usize],
        scaler: &Scaler,
        spans: impl Fn(usize) -> Option<&'a [LineSpan]>,
    ) {
        self.rebuild_resolved(instructions, displayed, scaler, spans, |_| None);
    }
    pub fn rebuild_in_context<'a>(
        &mut self,
        context: &crate::RenderContext,
        displayed: &[usize],
        spans: impl Fn(usize) -> Option<&'a [LineSpan]>,
    ) {
        self.rebuild_resolved(
            context.raw_instructions(),
            displayed,
            &context.scaler,
            spans,
            |index| context.resolved_line_paths(index, &context.scaler),
        );
    }
    fn rebuild_resolved<'a, 'b>(
        &mut self,
        instructions: &'b [DrawingInstruction],
        displayed: &[usize],
        scaler: &Scaler,
        spans: impl Fn(usize) -> Option<&'a [LineSpan]>,
        resolve: impl Fn(usize) -> Option<crate::ResolvedLinePaths<'b>>,
    ) {
        let started = std::time::Instant::now();
        self.clear();
        self.instructions.extend_from_slice(displayed);
        let mut entries = Vec::with_capacity(displayed.len());
        let mut projected = 0;
        for (ordinal, &index) in displayed.iter().enumerate() {
            let mut min = [f64::INFINITY; 2];
            let mut max = [f64::NEG_INFINITY; 2];
            let mut valid = true;
            let mut count = 0;
            let mut add = |q: crate::ScreenPoint| {
                count += 1;
                projected += 1;
                let q = [q.x as f64, q.y as f64];
                if !q.iter().all(|x| x.is_finite()) {
                    valid = false;
                    return;
                }
                for a in 0..2 {
                    min[a] = min[a].min(q[a]);
                    max[a] = max[a].max(q[a]);
                }
            };
            match instructions.get(index) {
                Some(DrawingInstruction::Line(line)) => {
                    for path in resolve(index).unwrap_or_else(|| line.render_paths(scaler)) {
                        for &p in path.iter() {
                            add(scaler.world_to_screen(p));
                        }
                        // Include finite extrapolated caller spans as well as
                        // ordinary suppression fractions. Use the exact screen
                        // interpolation used by drawing and picking.
                        if let Some(spans) = spans(index) {
                            for span in spans {
                                if let Some((a, b)) = span.screen_endpoints(&path, scaler) {
                                    add(a);
                                    add(b);
                                }
                            }
                        }
                    }
                }
                Some(DrawingInstruction::Area(area)) => {
                    for ring in std::iter::once(&area.exterior).chain(&area.interiors) {
                        for &p in ring {
                            add(scaler.world_to_screen(p));
                        }
                    }
                }
                _ => valid = false,
            }
            if valid && count > 0 {
                // f32 projection and interpolated span endpoints can round at envelope
                // edges. Pad conservatively; broad-phase false positives are harmless.
                for axis in 0..2 {
                    let pad =
                        8. * f32::EPSILON as f64 * min[axis].abs().max(max[axis].abs()).max(1.)
                            + 1e-4;
                    min[axis] -= pad;
                    max[axis] += pad;
                }
                entries.push(Envelope {
                    ordinal,
                    bounds: AABB::from_corners(min, max),
                });
            } else {
                self.fallback.push(ordinal);
            }
        }
        self.tree = RTree::bulk_load(entries);
        self.signature = Some(signature(scaler));
        self.stats = SelectionIndexStats {
            primitives: displayed.len(),
            indexed: self.tree.size(),
            fallback: self.fallback.len(),
            projected_vertices: projected,
            minimum_payload_bytes: self.instructions.len() * std::mem::size_of::<usize>()
                + self.fallback.len() * std::mem::size_of::<usize>()
                + self.tree.size() * std::mem::size_of::<Envelope>(),
            build_seconds: started.elapsed().as_secs_f64(),
        };
    }
    /// Query in the exact same shifted f32 coordinate used by wrapped hit testing.
    /// A different transform returns all primitives until a fresh index is built.
    pub fn candidates(
        &self,
        scaler: &Scaler,
        query: ScreenPoint,
        radius: f64,
        wrapping: bool,
    ) -> Vec<usize> {
        if !radius.is_finite() || radius < 0. || !query.x.is_finite() || !query.y.is_finite() {
            return Vec::new();
        }
        if !self.matches_scaler(scaler) {
            return self.instructions.clone();
        }
        let shifts = [0., -360., 360.];
        let mut ordinals = self.fallback.clone();
        // Dense overlap provides little pruning. Bound traversal and skip sorting;
        // the exact full scan is the same authoritative fallback as a stale view.
        let limit = (self.instructions.len() / 8).max(16);
        if ordinals.len() >= limit {
            return self.instructions.clone();
        }
        for &shift in &shifts[..if wrapping { 3 } else { 1 }] {
            let pixels = (shift * scaler.scale_x()) as f32;
            let q = [(query.x - pixels) as f64, query.y as f64];
            if !q.iter().all(|x| x.is_finite()) {
                return self.instructions.clone();
            }
            let bounds = AABB::from_corners(
                [q[0] - radius, q[1] - radius],
                [q[0] + radius, q[1] + radius],
            );
            let result = self.tree.locate_in_envelope_intersecting_int(&bounds, |e| {
                ordinals.push(e.ordinal);
                if ordinals.len() >= limit {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            });
            if matches!(result, std::ops::ControlFlow::Break(())) {
                return self.instructions.clone();
            }
        }
        ordinals.sort_unstable();
        ordinals.dedup();
        ordinals.into_iter().map(|o| self.instructions[o]).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AreaInstruction, Color, GeoBounds, LineInstruction, LineStyle, Viewport, WorldPoint,
    };
    fn scaler() -> Scaler {
        Scaler::new(
            GeoBounds::new(-10., -10., 10., 10.),
            Viewport::with_origin(40., 25., 900., 700.),
        )
    }
    #[test]
    fn candidates_preserve_renderer_order_holes_dashes_and_wrapped_copies() {
        let ring = |a, b| {
            vec![
                WorldPoint::new(a, a),
                WorldPoint::new(b, a),
                WorldPoint::new(b, b),
                WorldPoint::new(a, b),
            ]
        };
        let mut area = AreaInstruction::new(ring(-9., 9.));
        area.interiors.push(ring(-2., 2.));
        let mut line =
            LineInstruction::new(vec![WorldPoint::new(-9., 0.), WorldPoint::new(9., 0.)]);
        line.style = LineStyle::solid(Color::BLACK, 1.);
        line.style.dash_pattern = vec![7., 5.];
        let instructions = vec![
            DrawingInstruction::Area(area),
            DrawingInstruction::Line(line),
            DrawingInstruction::Line(LineInstruction::new(vec![
                WorldPoint::new(7., 7.),
                WorldPoint::new(9., 9.),
            ])),
        ];
        let displayed = [2, 0, 1];
        let spans = [LineSpan {
            segment: 0,
            start: 0.1,
            end: 0.6,
        }];
        for density in [1., 2., 3.] {
            let mut s = scaler();
            s.set_pixel_ratio(density);
            let mut index = SelectionIndex::default();
            index.rebuild(&instructions, &displayed, &s, |i| {
                if i == 1 {
                    Some(&spans)
                } else {
                    None
                }
            });
            for x in -10..=10 {
                for y in -10..=10 {
                    for shift in [-360., 0., 360.] {
                        let q = s.world_to_screen(WorldPoint::new(x as f64 + shift, y as f64));
                        let candidates = index.candidates(&s, q, 3., true);
                        let exact = |ids: &[usize]| {
                            ids.iter()
                                .filter_map(|&i| {
                                    crate::hit_geometry_wrapped_visible(
                                        &instructions[i],
                                        &s,
                                        q,
                                        3.,
                                        if i == 1 { Some(&spans) } else { None },
                                        true,
                                    )
                                    .map(|hit| {
                                        (i, hit.hit.nearest, hit.hit.distance, hit.longitude_shift)
                                    })
                                })
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(exact(&displayed), exact(&candidates));
                    }
                }
            }
        }
    }
    #[test]
    fn stale_dpi_bounds_and_viewport_return_full_order_until_rebuild() {
        let mut s = scaler();
        let i = vec![DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 1.),
        ]))];
        let mut index = SelectionIndex::default();
        index.rebuild(&i, &[0], &s, |_| None);
        for change in 0..3 {
            let mut next = s.clone();
            match change {
                0 => next.set_pixel_ratio(2.),
                1 => next.pan(50., 0.),
                _ => next.set_viewport(Viewport::new(2000., 1000.)),
            };
            assert!(!index.matches_scaler(&next));
            assert_eq!(
                index.candidates(&next, ScreenPoint::new(-1e5, -1e5), 1., false),
                vec![0]
            );
        }
        s.set_pixel_ratio(2.);
        index.rebuild(&i, &[0], &s, |_| None);
        assert!(index.matches_scaler(&s));
        index.clear();
        assert!(!index.matches_scaler(&s));
    }
    #[test]
    fn malformed_geometry_falls_back_without_unbounded_grid_or_tree_panics() {
        let s = scaler();
        let i = vec![
            DrawingInstruction::Line(LineInstruction::new(vec![WorldPoint::new(f64::NAN, 0.)])),
            DrawingInstruction::Line(LineInstruction::new(Vec::new())),
        ];
        let mut index = SelectionIndex::default();
        index.rebuild(&i, &[1, 0, 999], &s, |_| None);
        assert_eq!(
            index.candidates(&s, ScreenPoint::new(1e9, 1e9), 0., true),
            vec![1, 0, 999]
        );
        assert_eq!(index.statistics().fallback, 3);
        assert!(index
            .candidates(&s, ScreenPoint::new(0., 0.), -1., false)
            .is_empty());
    }
    #[test]
    fn extrapolated_visibility_endpoints_are_conservatively_indexed() {
        let s = scaler();
        let i = vec![DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 0.),
            WorldPoint::new(1., 0.),
        ]))];
        let span = [LineSpan {
            segment: 0,
            start: 2.,
            end: 3.,
        }];
        let mut index = SelectionIndex::default();
        index.rebuild(&i, &[0], &s, |_| Some(&span));
        let q = s.world_to_screen(WorldPoint::new(2.5, 0.));
        assert_eq!(index.candidates(&s, q, 0., false), vec![0]);
        assert!(crate::hit_geometry_visible(&i[0], &s, q, 1., Some(&span)).is_some());
    }
    #[test]
    fn bounded_payload_does_not_grow_with_zoom_or_large_polygon_vertex_count() {
        let mut points = Vec::new();
        for n in 0..10_000 {
            let a = n as f64 / 10_000. * std::f64::consts::TAU;
            points.push(WorldPoint::new(a.cos(), a.sin()));
        }
        let i = vec![DrawingInstruction::Area(AreaInstruction::new(points))];
        let mut index = SelectionIndex::default();
        let mut payload = None;
        for z in [0.005, 1., 25., 200.] {
            let s = Scaler::new(
                GeoBounds::new(-10. / z, -10. / z, 10. / z, 10. / z),
                Viewport::new(2000., 1000.),
            );
            index.rebuild(&i, &[0], &s, |_| None);
            let bytes = index.statistics().minimum_payload_bytes;
            assert!(bytes < 100);
            assert_eq!(*payload.get_or_insert(bytes), bytes);
            assert_eq!(index.statistics().projected_vertices, 10_000);
        }
    }
    #[test]
    fn dense_fallback_preserves_order_and_exact_hits_including_distant_nonhits() {
        let s = scaler();
        let mut instructions: Vec<_> = (0..64)
            .map(|_| {
                DrawingInstruction::Area(AreaInstruction::new(vec![
                    WorldPoint::new(-1., -1.),
                    WorldPoint::new(1., -1.),
                    WorldPoint::new(1., 1.),
                    WorldPoint::new(-1., 1.),
                ]))
            })
            .collect();
        instructions.push(DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(8., 8.),
            WorldPoint::new(9., 9.),
        ])));
        let ids: Vec<_> = (0..instructions.len()).rev().collect();
        let mut index = SelectionIndex::default();
        index.rebuild(&instructions, &ids, &s, |_| None);
        let q = s.world_to_screen(WorldPoint::new(0., 0.));
        let candidates = index.candidates(&s, q, 0.1, false);
        assert_eq!(candidates, ids);
        let hits = |ids: &[usize]| {
            ids.iter()
                .filter_map(|&i| {
                    crate::hit_geometry(&instructions[i], &s, q, 0.1)
                        .map(|h| (i, h.distance, h.nearest))
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(hits(&ids), hits(&candidates));
        assert_eq!(hits(&candidates).len(), 64);
    }
}
