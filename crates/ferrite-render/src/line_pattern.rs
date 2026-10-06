//! Repeating dash intervals shared by rendering and picking.
use crate::{LineSpan, LineStyle, Scaler, StrokeUnit, WorldPoint};
use ferrite_kernel::DashCycle;
use std::borrow::Cow;
/// Phase survives vertices and suppressed intervals. Explicit S-100 cycles use mm.
pub fn dash_line_spans(
    points: &[WorldPoint],
    scaler: &Scaler,
    style: &LineStyle,
    suppression: Option<&[LineSpan]>,
) -> Option<Vec<LineSpan>> {
    if style.dash_cycle.is_none() && style.dash_pattern.is_empty() {
        return None;
    }
    let projected: Vec<_> = points
        .iter()
        .map(|p| {
            let s = scaler.world_to_screen(*p);
            [s.x as f64, s.y as f64]
        })
        .collect();
    match dash_projected_line_spans(&projected, scaler.pixels_per_mm(), style, suppression) {
        Ok(spans) => spans,
        Err(error) => {
            tracing::warn!("{error}");
            Some(Vec::new())
        }
    }
}
/// Projection-independent dash phase, shared by planar and perspective paths.
/// Screen coordinates and display calibration are physical pixels. Suppression
/// never resets phase; all output spans refer to the original sampled segments.
pub fn dash_projected_line_spans(
    points: &[[f64; 2]],
    pixels_per_mm: f64,
    style: &LineStyle,
    suppression: Option<&[LineSpan]>,
) -> Result<Option<Vec<LineSpan>>, String> {
    dash_projected_line_spans_impl(points, pixels_per_mm, style, suppression, None)
}
/// Emit only supplied visible intervals, while advancing phase along the full
/// projected component. Off-screen/suppressed length never resets the pattern.
/// Disconnected near/far-plane components can specify an explicit phase origin.
pub fn dash_projected_line_spans_clipped(
    points: &[[f64; 2]],
    pixels_per_mm: f64,
    style: &LineStyle,
    visible: &[LineSpan],
    initial_phase_px: f64,
) -> Result<Option<Vec<LineSpan>>, String> {
    if !initial_phase_px.is_finite()
        || visible.iter().any(|s| {
            s.segment >= points.len().saturating_sub(1)
                || !s.start.is_finite()
                || !s.end.is_finite()
                || s.start < 0.
                || s.end > 1.
                || s.start >= s.end
        })
    {
        return Err("Invalid clipped dash intervals or phase origin".into());
    }
    dash_projected_line_spans_impl(
        points,
        pixels_per_mm,
        style,
        Some(visible),
        Some(initial_phase_px),
    )
}
fn dash_projected_line_spans_impl(
    points: &[[f64; 2]],
    pixels_per_mm: f64,
    style: &LineStyle,
    suppression: Option<&[LineSpan]>,
    clipped_phase: Option<f64>,
) -> Result<Option<Vec<LineSpan>>, String> {
    if points.len() > 262144
        || !pixels_per_mm.is_finite()
        || pixels_per_mm <= 0.
        || points.iter().flatten().any(|x| !x.is_finite())
    {
        return Err("Invalid projected dash path or display calibration".into());
    }
    let (cycle, unit) = if let Some(cycle) = &style.dash_cycle {
        if !cycle.period.is_finite()
            || cycle.period <= 0.
            || cycle.intervals.iter().any(|(a, b)| {
                !a.is_finite() || !b.is_finite() || *a < 0. || b <= a || *b > cycle.period
            })
            || cycle.intervals.windows(2).any(|p| p[0].1 > p[1].0)
        {
            tracing::warn!("Invalid stored dash cycle");
            return Err("Invalid or unresolved projected dash cycle".into());
        }
        (Cow::Borrowed(cycle), pixels_per_mm)
    } else {
        if style.dash_pattern.is_empty() {
            return Ok(None);
        }
        if style.dash_pattern.len() % 2 != 0
            || style
                .dash_pattern
                .iter()
                .any(|x| !x.is_finite() || *x <= 0.)
        {
            tracing::warn!("Invalid legacy dash pattern");
            return Err("Invalid or unresolved projected dash cycle".into());
        }
        let mut start = 0.;
        let mut dashes = Vec::new();
        for pair in style.dash_pattern.chunks_exact(2) {
            dashes.push((start, pair[0] as f64));
            start += pair[0] as f64 + pair[1] as f64;
        }
        let Ok(c) = DashCycle::new(start, dashes) else {
            return Err("Invalid or unresolved projected dash cycle".into());
        };
        (
            Cow::Owned(c),
            match style.width_unit {
                StrokeUnit::PhysicalPixels => 1.,
                StrokeUnit::Millimetres => pixels_per_mm,
            },
        )
    };
    let period = cycle.period * unit;
    if !period.is_finite() || period <= 0. {
        return Err("Invalid or unresolved projected dash cycle".into());
    }
    if cycle.intervals.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if cycle.intervals.len() == 1 && cycle.intervals[0] == (0., cycle.period) {
        return Ok(None);
    }
    let intervals: Vec<_> = cycle
        .intervals
        .iter()
        .map(|(a, b)| (a * unit, b * unit))
        .collect();
    let mut phase = clipped_phase.unwrap_or(0.).rem_euclid(period);
    let clipped = clipped_phase.map(|_| canonical_spans(suppression.unwrap_or(&[])));
    let mut clipped_cursor = 0;
    let mut result = Vec::new();
    for (segment, pair) in points.windows(2).enumerate() {
        let length = (pair[1][0] - pair[0][0]).hypot(pair[1][1] - pair[0][1]);
        if !length.is_finite() || length < 1e-8 {
            continue;
        }
        if let Some(visible) = &clipped {
            while clipped_cursor < visible.len() && visible[clipped_cursor].segment < segment {
                clipped_cursor += 1;
            }
            let mut at = clipped_cursor;
            while at < visible.len() && visible[at].segment == segment {
                let window = visible[at];
                at += 1;
                let left_distance = window.start * length;
                let right_distance = window.end * length;
                let mut local = left_distance;
                let mut local_phase = (phase + left_distance.rem_euclid(period)).rem_euclid(period);
                while local < right_distance {
                    let remaining = right_distance - local;
                    let advance = (period - local_phase).min(remaining);
                    if !advance.is_finite() || advance <= 0. || local + advance <= local {
                        return Err("Clipped dash exceeds numeric resolution".into());
                    }
                    let next_local = if advance == remaining {
                        right_distance
                    } else {
                        local + advance
                    };
                    let next_phase = local_phase + advance;
                    for &(start, end) in &intervals {
                        let a = start.max(local_phase);
                        let b = end.min(next_phase);
                        if b > a {
                            if result.len() >= 262144 {
                                return Err("Projected dash span budget exceeded".into());
                            }
                            let left = if a == local_phase {
                                local
                            } else {
                                local + a - local_phase
                            };
                            let right = if b == next_phase {
                                next_local
                            } else {
                                local + b - local_phase
                            };
                            result.push(LineSpan {
                                segment,
                                start: if left == left_distance {
                                    window.start
                                } else {
                                    left / length
                                },
                                end: if right == right_distance {
                                    window.end
                                } else {
                                    right / length
                                },
                            });
                        }
                    }
                    local = next_local;
                    local_phase = if advance >= period - local_phase {
                        0.
                    } else {
                        local_phase + advance
                    };
                }
            }
            phase = (phase + length.rem_euclid(period)).rem_euclid(period);
            continue;
        }
        let mut local = 0.;
        while local < length {
            let advance = (period - phase).min(length - local);
            if !advance.is_finite() || advance <= 0. || local + advance <= local {
                tracing::warn!("Dash interval exceeds numeric resolution");
                return Err("Invalid or unresolved projected dash cycle".into());
            }
            // Keep source-vertex boundaries exact. Adding a short length to
            // a large phase then subtracting it can move the end outside [0,1]
            // and split a continuous dash differently on different platforms.
            let remaining = length - local;
            let next_local = if advance == remaining {
                length
            } else {
                local + advance
            };
            let next_phase = phase + advance;
            for &(start, end) in &intervals {
                let left = start.max(phase);
                let right = end.min(next_phase);
                if right > left {
                    if result.len() >= 262144 {
                        return Err("Projected dash span budget exceeded".into());
                    }
                    result.push(LineSpan {
                        segment,
                        start: if left == phase {
                            local
                        } else {
                            local + (left - phase)
                        } / length,
                        end: if right == next_phase {
                            next_local
                        } else {
                            local + (right - phase)
                        } / length,
                    });
                }
            }
            local = next_local;
            phase = if advance >= period - phase {
                0.
            } else {
                phase + advance
            };
        }
    }
    Ok(Some(if clipped.is_some() {
        result
    } else if let Some(visible) = suppression {
        intersect_spans(&result, visible)
    } else {
        result
    }))
}
fn canonical_spans(spans: &[LineSpan]) -> Cow<'_, [LineSpan]> {
    let valid = |s: &LineSpan| {
        s.start.is_finite() && s.end.is_finite() && s.start >= 0. && s.end <= 1. && s.end > s.start
    };
    if spans.iter().all(valid)
        && spans.windows(2).all(|p| {
            p[0].segment < p[1].segment || (p[0].segment == p[1].segment && p[0].end <= p[1].start)
        })
    {
        return Cow::Borrowed(spans);
    }
    let mut sorted: Vec<_> = spans
        .iter()
        .copied()
        .filter(|s| s.start.is_finite() && s.end.is_finite())
        .map(|mut s| {
            s.start = s.start.max(0.);
            s.end = s.end.min(1.);
            s
        })
        .filter(valid)
        .collect();
    sorted.sort_by(|a, b| {
        a.segment
            .cmp(&b.segment)
            .then(a.start.total_cmp(&b.start))
            .then(a.end.total_cmp(&b.end))
    });
    let mut union: Vec<LineSpan> = Vec::new();
    for s in sorted {
        if let Some(last) = union
            .last_mut()
            .filter(|last| last.segment == s.segment && s.start <= last.end)
        {
            last.end = last.end.max(s.end);
        } else {
            union.push(s);
        }
    }
    Cow::Owned(union)
}
fn intersect_spans(dash: &[LineSpan], visible: &[LineSpan]) -> Vec<LineSpan> {
    let visible = canonical_spans(visible);
    let (mut a, mut b) = (0, 0);
    let mut result = Vec::new();
    while a < dash.len() && b < visible.len() {
        let (x, y) = (dash[a], visible[b]);
        if x.segment < y.segment {
            a += 1;
            continue;
        }
        if y.segment < x.segment {
            b += 1;
            continue;
        }
        let start = x.start.max(y.start);
        let end = x.end.min(y.end);
        if end > start {
            result.push(LineSpan {
                segment: x.segment,
                start,
                end,
            });
        }
        if x.end <= y.end {
            a += 1;
        } else {
            b += 1;
        }
    }
    result
}
#[cfg(test)]
mod light_line_tests {
    use crate::*;
    #[test]
    fn fixed_ray_length_and_dash_picking_survive_zoom_and_density() {
        for ratio in [1., 2.] {
            for zoom in [0.5, 1., 2.] {
                let mut scaler = Scaler::new(
                    GeoBounds::new(-5. / zoom, -5. / zoom, 5. / zoom, 5. / zoom),
                    Viewport::new(1000., 1000.),
                );
                scaler.set_pixel_ratio(ratio);
                let mut line =
                    LineInstruction::new(vec![WorldPoint::new(0., 0.), WorldPoint::new(0., 0.)]);
                line.screen_ray = Some(ScreenRay {
                    direction: 90.,
                    length_mm: 25.,
                    geographic_direction: true,
                });
                line.style = LineStyle::solid_mm(Color::BLACK, 0.32);
                line.style.dash_pattern = vec![3.6, 1.8];
                let p = line.render_points(&scaler);
                let a = scaler.world_to_screen(p[0]);
                let b = scaler.world_to_screen(p[1]);
                let expected = 25. * 96. / 25.4 * ratio;
                assert!((((b.x - a.x).hypot(b.y - a.y)) as f64 - expected).abs() < 0.001);
                let instr = DrawingInstruction::Line(line);
                let px = 96. / 25.4 * ratio;
                assert!(hit_geometry(
                    &instr,
                    &scaler,
                    ScreenPoint::new(a.x + (2. * px) as f32, a.y),
                    0.01
                )
                .is_some());
                assert!(hit_geometry(
                    &instr,
                    &scaler,
                    ScreenPoint::new(a.x + (4.2 * px) as f32, a.y),
                    0.01
                )
                .is_none());
            }
        }
    }
    #[test]
    fn dash_phase_continues_through_vertices_and_suppressed_parts() {
        let scaler = Scaler::new(
            GeoBounds::new(0., 0., 10., 10.),
            Viewport::new(1000., 1000.),
        );
        let p = |x| scaler.screen_to_world(ScreenPoint::new(x, 500.));
        let points = [p(100.), p(104.), p(110.)];
        let style = LineStyle::dashed(Color::BLACK, 1., vec![3., 2.]);
        let spans = dash_line_spans(&points, &scaler, &style, None).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].segment, 0);
        assert!((spans[0].end - 0.75).abs() < 1e-6);
        assert_eq!(spans[1].segment, 1);
        assert!((spans[1].start - 1. / 6.).abs() < 1e-6);
        assert!((spans[1].end - 4. / 6.).abs() < 1e-6);
        let remaining = [LineSpan {
            segment: 1,
            start: 0.5,
            end: 1.,
        }];
        let spans = dash_line_spans(&points, &scaler, &style, Some(&remaining)).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, 0.5);
        assert!((spans[0].end - 4. / 6.).abs() < 1e-6);
    }
}
#[cfg(test)]
mod explicit_cycle_tests {
    use super::*;
    use crate::{
        hit_geometry, Color, DrawingInstruction, GeoBounds, LineInstruction, ScreenPoint, Viewport,
    };
    #[test]
    fn repeating_multiple_offsets_match_known_on_and_off_positions() {
        for ratio in [1., 2.] {
            let mut s = Scaler::new(
                GeoBounds::new(-1., -1., 1., 1.),
                Viewport::new(1000., 1000.),
            );
            s.set_pixel_ratio(ratio);
            let origin = WorldPoint::new(0., 0.);
            let a = s.world_to_screen(origin);
            let point = |mm| {
                s.screen_to_world(ScreenPoint::new(a.x + (mm * s.pixels_per_mm()) as f32, a.y))
            };
            let mut line = LineInstruction::new(vec![origin, point(3.), point(25.)]);
            line.style = LineStyle::solid_mm(Color::BLACK, 0.32);
            line.style.dash_cycle = Some(DashCycle::new(10., [(2., 2.), (6., 1.)]).unwrap());
            let inst = DrawingInstruction::Line(line);
            for (x, hit) in [
                (1., false),
                (2.5, true),
                (3.5, true),
                (4.5, false),
                (6.5, true),
                (8., false),
                (12.5, true),
                (16.5, true),
                (21., false),
                (23., true),
            ] {
                assert_eq!(
                    hit_geometry(&inst, &s, s.world_to_screen(point(x)), 0.01).is_some(),
                    hit,
                    "{x}mm ratio{ratio}"
                );
            }
        }
    }
    #[test]
    fn sorted_intersection_matches_independent_interval_membership() {
        let dash = [
            LineSpan {
                segment: 0,
                start: 0.1,
                end: 0.4,
            },
            LineSpan {
                segment: 0,
                start: 0.6,
                end: 0.9,
            },
            LineSpan {
                segment: 1,
                start: 0.,
                end: 1.,
            },
        ];
        let visible = [
            LineSpan {
                segment: 1,
                start: 0.2,
                end: 0.8,
            },
            LineSpan {
                segment: 0,
                start: 0.3,
                end: 0.7,
            },
            LineSpan {
                segment: 0,
                start: 0.2,
                end: 0.35,
            },
        ];
        let actual = intersect_spans(&dash, &visible);
        for segment in 0..=2 {
            for i in 0..1000 {
                let x = (i as f64 + 0.5) / 1000.;
                let member = |spans: &[LineSpan]| {
                    spans
                        .iter()
                        .any(|s| s.segment == segment && x >= s.start && x < s.end)
                };
                assert_eq!(member(&actual), member(&dash) && member(&visible));
            }
        }
    }
}

#[cfg(test)]
mod projected_dash_tests {
    use super::*;
    #[test]
    fn dash_crossing_vertex_has_exact_segment_endpoints() {
        let mut style = LineStyle::default();
        style.dash_cycle = Some(DashCycle::new(100., [(0., 90.)]).unwrap());
        // A short vertical segment after a long segment loses low bits if
        // its endpoint is computed by adding then subtracting the phase.
        let spans = dash_projected_line_spans(
            &[
                [0., 0.],
                [37.37720700718782, 0.],
                [37.37720700718782, 0.2807663837793567],
            ],
            1.,
            &style,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(spans.len(), 2);
        for s in spans {
            assert_eq!((s.start, s.end), (0., 1.));
        }
    }
    #[test]
    fn calibrated_phase_survives_subdivision_and_empty_cycle_is_valid() {
        let mut style = LineStyle::default();
        style.dash_cycle = Some(DashCycle::new(10., [(2., 3.)]).unwrap());
        let spans = dash_projected_line_spans(&[[0., 0.], [12., 0.], [40., 0.]], 2., &style, None)
            .unwrap()
            .unwrap();
        let total: f64 = spans
            .iter()
            .map(|s| (s.end - s.start) * if s.segment == 0 { 12. } else { 28. })
            .sum();
        assert!((total - 12.).abs() < 1e-10);
        let visible = [LineSpan {
            segment: 1,
            start: 0.25,
            end: 0.75,
        }];
        let clipped = dash_projected_line_spans(
            &[[0., 0.], [12., 0.], [40., 0.]],
            2.,
            &style,
            Some(&visible),
        )
        .unwrap()
        .unwrap();
        assert!(clipped
            .iter()
            .all(|s| s.segment == 1 && s.start >= 0.25 && s.end <= 0.75));
        style.dash_cycle = Some(DashCycle::new(10., []).unwrap());
        assert!(
            dash_projected_line_spans(&[[0., 0.], [40., 0.]], 2., &style, None)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(dash_projected_line_spans(&[[0., 0.], [40., 0.]], 0., &style, None).is_err());
    }
}

#[cfg(test)]
mod clipped_tests {
    use super::*;
    #[test]
    fn clipped_dash_advances_hidden_billion_pixels_without_allocating_hidden_cycles() {
        let points = [[0., 0.], [1e9 + 3., 0.], [1e9 + 3., 123.]];
        let mut style = LineStyle::default();
        style.dash_pattern = vec![10., 10.];
        style.width_unit = StrokeUnit::PhysicalPixels;
        let roi = [
            LineSpan {
                segment: 0,
                start: 1. - 1000. / (1e9 + 3.),
                end: 1.,
            },
            LineSpan {
                segment: 1,
                start: 20. / 123.,
                end: 80. / 123.,
            },
        ];
        let spans = dash_projected_line_spans_clipped(&points, 4., &style, &roi, 0.)
            .unwrap()
            .unwrap();
        assert!(spans.len() > 40 && spans.len() < 100);
        for distance in [21., 25., 31., 35., 41., 45., 51., 55., 61., 65., 71., 75.] {
            let f = distance / 123.;
            let actual = spans
                .iter()
                .any(|s| s.segment == 1 && f > s.start && f < s.end);
            assert_eq!(actual, (3. + distance) % 20. < 10.);
        }
        assert!(dash_projected_line_spans_clipped(&points, 4., &style, &roi, f64::NAN).is_err());
    }
    #[test]
    fn clipped_and_full_dash_agree_on_masked_intervals_and_phase_shift() {
        let points = [[0., 0.], [47., 0.], [47., 91.]];
        let mut style = LineStyle::default();
        style.dash_cycle = Some(DashCycle::new(11., [(2., 4.)]).unwrap());
        let roi = [
            LineSpan {
                segment: 0,
                start: 0.2,
                end: 0.8,
            },
            LineSpan {
                segment: 1,
                start: 0.3,
                end: 0.95,
            },
        ];
        let full = dash_projected_line_spans(&points, 2., &style, Some(&roi))
            .unwrap()
            .unwrap();
        let clipped = dash_projected_line_spans_clipped(&points, 2., &style, &roi, 0.)
            .unwrap()
            .unwrap();
        assert_eq!(full.len(), clipped.len());
        for (a, b) in full.iter().zip(&clipped) {
            assert_eq!(a.segment, b.segment);
            assert!((a.start - b.start).abs() < 1e-14 && (a.end - b.end).abs() < 1e-14);
        }
        let shifted = dash_projected_line_spans_clipped(&points, 2., &style, &roi, -7.)
            .unwrap()
            .unwrap();
        assert_ne!(clipped, shifted);
    }
}
