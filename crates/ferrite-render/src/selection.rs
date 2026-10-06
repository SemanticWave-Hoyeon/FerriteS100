use crate::{DrawingInstruction, Scaler, ScreenPoint, WorldPoint};
use ferrite_kernel::{closest_on_path, inside_ring};
#[derive(Debug, Clone, Copy)]
pub struct GeometryHit {
    pub distance: f64,
    pub nearest: ScreenPoint,
}
/// Tests a portrayal's geometry in physical screen pixels, without copying vertices.
/// The renderer must supply only instructions that survived its actual draw filters.
pub fn hit_geometry(
    instruction: &DrawingInstruction,
    scaler: &Scaler,
    query: ScreenPoint,
    radius: f64,
) -> Option<GeometryHit> {
    hit_geometry_visible(instruction, scaler, query, radius, None)
}
/// Hit test the same remaining segment spans that the renderer actually draws.
pub fn hit_geometry_visible(
    instruction: &DrawingInstruction,
    scaler: &Scaler,
    query: ScreenPoint,
    radius: f64,
    spans: Option<&[crate::LineSpan]>,
) -> Option<GeometryHit> {
    if !radius.is_finite() || radius < 0.0 {
        return None;
    }
    let q = [query.x as f64, query.y as f64];
    let project = |p: &WorldPoint| {
        let p = scaler.world_to_screen(*p);
        [p.x as f64, p.y as f64]
    };
    if matches!(instruction,DrawingInstruction::Line(line) if !line.style.has_visible_stroke()) {
        return None;
    }
    let result = match instruction {
        DrawingInstruction::Line(line) => line
            .render_paths(scaler)
            .filter_map(|points| {
                let styled = crate::dash_line_spans(&points, scaler, &line.style, spans);
                let visible = styled.as_deref().or(spans);
                if let Some(visible) = visible {
                    visible
                        .iter()
                        .filter_map(|span| {
                            let (a, b) = span.screen_endpoints(&points, scaler)?;
                            closest_on_path(
                                [[a.x as f64, a.y as f64], [b.x as f64, b.y as f64]],
                                q,
                                false,
                            )
                        })
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                } else {
                    closest_on_path(points.iter().map(project), q, false)
                }
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))?,
        DrawingInstruction::Area(area) => {
            if area.exterior.len() < 3 {
                return None;
            }
            let mut nearest = closest_on_path(area.exterior.iter().map(project), q, true)?;
            for ring in &area.interiors {
                if let Some(hit) = closest_on_path(ring.iter().map(project), q, true) {
                    if hit.0 < nearest.0 {
                        nearest = hit;
                    }
                }
            }
            let inside = inside_ring(area.exterior.iter().map(project), q)
                && !area
                    .interiors
                    .iter()
                    .any(|r| inside_ring(r.iter().map(project), q));
            if inside {
                (0., q)
            } else {
                nearest
            }
        }
        _ => return None,
    };
    (result.0 <= radius).then_some(GeometryHit {
        distance: result.0,
        nearest: ScreenPoint::new(result.1[0] as f32, result.1[1] as f32),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AreaInstruction, GeoBounds, LineInstruction};
    #[test]
    fn line_distance_and_polygon_hole_follow_screen_transform() {
        let scaler = Scaler::new(
            GeoBounds::new(0., 0., 10., 10.),
            crate::Viewport::new(1000., 1000.),
        );
        let world = |x, y| WorldPoint::new(x, y);
        let ring = |a, b| vec![world(a, a), world(b, a), world(b, b), world(a, b)];
        let mut area = AreaInstruction::new(ring(0., 10.));
        area.interiors.push(ring(4., 6.));
        let area = DrawingInstruction::Area(area);
        assert!(hit_geometry(&area, &scaler, scaler.world_to_screen(world(2., 2.)), 5.).is_some());
        assert!(hit_geometry(&area, &scaler, scaler.world_to_screen(world(5., 5.)), 5.).is_none());
        assert!(
            hit_geometry(&area, &scaler, scaler.world_to_screen(world(4., 5.)), 0.01).is_some()
        );
        let line =
            DrawingInstruction::Line(LineInstruction::new(vec![world(0., 5.), world(10., 5.)]));
        let p = scaler.world_to_screen(world(5., 5.));
        assert!(hit_geometry(&line, &scaler, ScreenPoint::new(p.x, p.y + 4.), 5.).is_some());
        assert!(hit_geometry(&line, &scaler, ScreenPoint::new(p.x, p.y + 6.), 5.).is_none());
    }
}

/// Hit the same enabled longitude copies as portrayal, preserving source identity.
#[derive(Debug, Clone, Copy)]
pub struct WrappedGeometryHit {
    pub hit: GeometryHit,
    pub longitude_shift: f64,
}
pub fn hit_geometry_wrapped_visible(
    instruction: &DrawingInstruction,
    scaler: &Scaler,
    query: ScreenPoint,
    radius: f64,
    spans: Option<&[crate::LineSpan]>,
    wrapping: bool,
) -> Option<WrappedGeometryHit> {
    let offsets = [0., -360., 360.];
    offsets[..if wrapping { 3 } else { 1 }]
        .iter()
        .filter_map(|&shift| {
            let pixels = (shift * scaler.scale_x()) as f32;
            let mut hit = hit_geometry_visible(
                instruction,
                scaler,
                ScreenPoint::new(query.x - pixels, query.y),
                radius,
                spans,
            )?;
            hit.nearest.x += pixels;
            Some(WrappedGeometryHit {
                hit,
                longitude_shift: shift,
            })
        })
        .min_by(|a, b| a.hit.distance.total_cmp(&b.hit.distance))
}
#[cfg(test)]
mod wrapped_tests {
    use super::*;
    use crate::{AreaInstruction, GeoBounds, LineInstruction, Viewport};
    #[test]
    fn wrapped_hits_preserve_holes_and_source_segment_visibility() {
        let scaler = Scaler::new(
            GeoBounds::new(350., 0., 370., 10.),
            Viewport::new(960., 640.),
        );
        let ring = |a, b| {
            vec![
                WorldPoint::new(a, a),
                WorldPoint::new(b, a),
                WorldPoint::new(b, b),
                WorldPoint::new(a, b),
            ]
        };
        let mut area = AreaInstruction::new(ring(0., 10.));
        area.interiors.push(ring(4., 6.));
        let a = DrawingInstruction::Area(area);
        let q = scaler.world_to_screen(WorldPoint::new(362., 2.));
        assert!(hit_geometry_visible(&a, &scaler, q, 3., None).is_none());
        let h = hit_geometry_wrapped_visible(&a, &scaler, q, 3., None, true).unwrap();
        assert_eq!(h.longitude_shift, 360.);
        assert_eq!(h.hit.distance, 0.);
        assert!(hit_geometry_wrapped_visible(
            &a,
            &scaler,
            scaler.world_to_screen(WorldPoint::new(365., 5.)),
            3.,
            None,
            true
        )
        .is_none());
        let line = DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 1.),
            WorldPoint::new(10., 1.),
        ]));
        let spans = [crate::LineSpan {
            segment: 0,
            start: 0.,
            end: 0.3,
        }];
        assert!(hit_geometry_wrapped_visible(
            &line,
            &scaler,
            scaler.world_to_screen(WorldPoint::new(362., 1.)),
            3.,
            Some(&spans),
            true
        )
        .is_some());
        assert!(hit_geometry_wrapped_visible(
            &line,
            &scaler,
            scaler.world_to_screen(WorldPoint::new(368., 1.)),
            3.,
            Some(&spans),
            true
        )
        .is_none());
    }
}
#[cfg(test)]
mod invisible_stroke_tests {
    use crate::*;
    #[test]
    fn invisible_strokes_do_not_pick_or_suppress_visible_geometry() {
        let s = Scaler::new(
            GeoBounds::new(0., 0., 10., 10.),
            Viewport::new(1000., 1000.),
        );
        let pts = vec![WorldPoint::new(1., 5.), WorldPoint::new(9., 5.)];
        let low = LineInstruction::new(pts.clone()).with_priority(1);
        for (width, alpha) in [(1., 0.), (0., 1.), (1., f32::NAN)] {
            let mut high = LineInstruction::new(pts.clone()).with_priority(99);
            high.style.width = width;
            high.style.color.a = alpha;
            let q = s.world_to_screen(WorldPoint::new(5., 5.));
            assert!(hit_geometry(&DrawingInstruction::Line(high.clone()), &s, q, 1.).is_none());
            let instructions = [
                DrawingInstruction::Line(low.clone()),
                DrawingInstruction::Line(high),
            ];
            let mut cache = LineSuppressionCache::default();
            let plan = cache.plan(&instructions, 1, None, None);
            assert!(!plan.contains(&0));
        }
    }
}

#[cfg(test)]
mod stroke_profile_tests {
    use crate::*;
    #[test]
    fn authored_opacity_survives_repeated_profile_changes() {
        let mut line = LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 1.)]);
        line.color_token = Some("CHBLK".into());
        line.style.opacity = 0.5;
        line.style.color = Color::BLACK.with_alpha(0.5);
        let mut inst = DrawingInstruction::Line(line);
        for color in [Color::BLUE, Color::GREEN, Color::RED] {
            inst.remap_colors(&|_| color);
            let DrawingInstruction::Line(line) = &inst else {
                panic!()
            };
            assert_eq!(line.style.color.a, 0.5);
            assert_eq!(line.style.color.r, color.r);
            assert_eq!(line.style.color.g, color.g);
            assert_eq!(line.style.color.b, color.b);
        }
    }
}
