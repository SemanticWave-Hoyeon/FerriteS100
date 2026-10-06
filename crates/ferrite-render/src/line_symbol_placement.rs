//! View-dependent S-100 Part 9/9a line symbol coordinates, independent of products/GPU.
use crate::{PointInstruction, Scaler, WorldPoint};
use ferrite_kernel::{geodesy::GeographicPosition, rhumb::RhumbSegment};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinePlacementMode {
    Relative,
    Absolute,
}
impl LinePlacementMode {
    pub fn from_lua(value: &str) -> Result<Self, String> {
        match value {
            "Relative" => Ok(Self::Relative),
            "Absolute" => Ok(Self::Absolute),
            _ => Err(format!("Invalid line placement mode: {value}")),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineSymbolPlacement {
    pub points: Box<[WorldPoint]>,
    pub mode: LinePlacementMode,
    /// Absolute is a portrayal distance in mm; Relative is a normalized curve distance.
    pub offset: f64,
    pub visible_parts: bool,
}
impl LineSymbolPlacement {
    pub fn validate(&self) -> Result<(), String> {
        if self.points.len() < 2
            || self.points.len() > 262144
            || !self.offset.is_finite()
            || self.offset < 0.
            || (self.mode == LinePlacementMode::Relative && self.offset > 1.)
        {
            return Err("Invalid line symbol placement/path limits".into());
        }
        for p in &self.points {
            geographic(*p)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct CurveSample {
    pub screen: [f64; 2],
    pub segment: usize,
    pub fraction: f64,
}
fn geographic(p: WorldPoint) -> Result<GeographicPosition, String> {
    if !p.x.is_finite() || p.x.abs() > 1e9 {
        return Err("Invalid curve longitude".into());
    }
    GeographicPosition::new(p.y, (p.x + 180.).rem_euclid(360.) - 180.).map_err(|e| e.to_string())
}
pub fn sample_curve_position(
    points: &[WorldPoint],
    s: CurveSample,
) -> Result<(WorldPoint, f64), String> {
    let a = *points
        .get(s.segment)
        .ok_or("Invalid placement source segment")?;
    let b = *points
        .get(s.segment + 1)
        .ok_or("Invalid placement source endpoint")?;
    let route = RhumbSegment::new(geographic(a)?, geographic(b)?).map_err(|e| e.to_string())?;
    let p = route.point(s.fraction).map_err(|e| e.to_string())?;
    Ok((
        WorldPoint::new(
            p.longitude_near(a.x).map_err(|e| e.to_string())?,
            p.latitude(),
        ),
        route.bearing_deg(),
    ))
}
fn mix(a: CurveSample, b: CurveSample, t: f64) -> Result<CurveSample, String> {
    let start = if a.segment == b.segment {
        a.fraction
    } else if b.segment == a.segment + 1 && a.fraction == 1. {
        0.
    } else {
        return Err("Noncontiguous curve samples".into());
    };
    Ok(CurveSample {
        screen: std::array::from_fn(|i| a.screen[i] + (b.screen[i] - a.screen[i]) * t),
        segment: b.segment,
        fraction: start + (b.fraction - start) * t,
    })
}
fn clip(a: [f64; 2], b: [f64; 2], rect: [f64; 4]) -> Option<[f64; 2]> {
    let mut range = [0f64, 1f64];
    for axis in 0..2 {
        let d = b[axis] - a[axis];
        if d == 0. {
            if a[axis] < rect[axis] || a[axis] > rect[axis + 2] {
                return None;
            }
        } else {
            let p = (rect[axis] - a[axis]) / d;
            let q = (rect[axis + 2] - a[axis]) / d;
            range[0] = range[0].max(p.min(q));
            range[1] = range[1].min(p.max(q));
            if range[0] > range[1] {
                return None;
            }
        }
    }
    Some(range)
}
/// Clip geometric centreline to the chart viewport, preserving disconnected runs
/// and source parameters. No phantom line connects a leave/re-enter excursion.
pub fn clip_curve_components(
    components: &[Vec<CurveSample>],
    rect: [f64; 4],
) -> Result<Vec<Vec<CurveSample>>, String> {
    if rect.iter().any(|v| !v.is_finite()) || rect[2] <= rect[0] || rect[3] <= rect[1] {
        return Err("Invalid placement viewport".into());
    }
    let mut out: Vec<Vec<CurveSample>> = Vec::new();
    for samples in components {
        let mut connected = false;
        for pair in samples.windows(2) {
            if let Some([lo, hi]) = clip(pair[0].screen, pair[1].screen, rect) {
                if hi <= lo {
                    connected = false;
                    continue;
                }
                let a = mix(pair[0], pair[1], lo)?;
                let b = mix(pair[0], pair[1], hi)?;
                if connected && lo == 0. {
                    out.last_mut().unwrap().push(b);
                } else {
                    out.push(vec![a, b]);
                }
                connected = hi == 1.;
            } else {
                connected = false;
            }
        }
    }
    Ok(out)
}
/// Resolve positions on the displayed curve's millimetre axis. The caller supplies
/// projection samples and visibility components, not product geometry metadata.
pub fn placed_curve_points(
    placement: &LineSymbolPlacement,
    components: &[Vec<CurveSample>],
    pixels_per_mm: f64,
    rect: [f64; 4],
) -> Result<Vec<CurveSample>, String> {
    placement.validate()?;
    if !pixels_per_mm.is_finite() || pixels_per_mm <= 0. {
        return Err("Invalid placement display calibration".into());
    }
    let clipped;
    let paths = if placement.visible_parts {
        clipped = clip_curve_components(components, rect)?;
        &clipped[..]
    } else {
        &components[..]
    };
    if !placement.visible_parts && paths.len() > 1 {
        return Err("Full curve crosses a singular/depth-clipped projection domain".into());
    }
    let mut out = Vec::new();
    for samples in paths {
        let mut total = 0.;
        for p in samples.windows(2) {
            let n = (p[1].screen[0] - p[0].screen[0]).hypot(p[1].screen[1] - p[0].screen[1]);
            if !n.is_finite() {
                return Err("Non-finite curve length".into());
            }
            total += n;
        }
        if !total.is_finite() {
            return Err("Curve length overflow".into());
        }
        if total == 0. {
            continue;
        }
        let target = match placement.mode {
            LinePlacementMode::Relative => total * placement.offset,
            LinePlacementMode::Absolute => placement.offset * pixels_per_mm,
        };
        if !target.is_finite() {
            return Err("Placement distance overflow".into());
        }
        if target > total {
            continue;
        }
        let mut accum = 0.;
        for p in samples.windows(2) {
            let length = (p[1].screen[0] - p[0].screen[0]).hypot(p[1].screen[1] - p[0].screen[1]);
            if length > 0. && (accum + length >= target) {
                out.push(mix(p[0], p[1], ((target - accum) / length).clamp(0., 1.))?);
                break;
            }
            accum += length;
        }
        if out.len() > 4096 {
            return Err("Line symbol visible-component limit exceeded".into());
        }
    }
    Ok(out)
}
/// Flat sampling uses f64 camera coordinates and source rhumb parameters. Samples
/// are refined for curved legacy geographic projection as well as Mercator.
pub fn resolve_flat_line_symbol(
    point: &PointInstruction,
    scaler: &Scaler,
) -> Result<Vec<PointInstruction>, String> {
    let Some(placement) = &point.line_placement else {
        return Ok(vec![point.clone()]);
    };
    placement.validate()?;
    let t = scaler.flat_transform();
    let center = scaler.geo_bounds.center().x;
    let mut points = Vec::with_capacity(placement.points.len());
    let mut longitude = center;
    for p in &placement.points {
        let g = geographic(*p)?;
        longitude = g.longitude_near(longitude).map_err(|e| e.to_string())?;
        points.push(WorldPoint::new(longitude, p.y));
    }
    let project = |segment: usize, fraction: f64| -> Result<CurveSample, String> {
        let (p, _) = sample_curve_position(
            &points,
            CurveSample {
                screen: [0., 0.],
                segment,
                fraction,
            },
        )?;
        let screen = [
            (p.x - t.geographic_origin[0]) * t.scale[0] + t.offset[0],
            (t.projection.project_y(t.geographic_origin[1]) - t.projection.project_y(p.y))
                * t.scale[1]
                + t.offset[1],
        ];
        if screen.iter().any(|v| !v.is_finite()) {
            return Err("Invalid flat curve projection".into());
        }
        Ok(CurveSample {
            screen,
            segment,
            fraction,
        })
    };
    let mut samples = vec![project(0, 0.)?];
    for segment in 0..points.len() - 1 {
        let mut stack = vec![(project(segment, 0.)?, project(segment, 1.)?, 0u8)];
        while let Some((a, b, depth)) = stack.pop() {
            let mid = project(segment, (a.fraction + b.fraction) / 2.)?;
            let linear = std::array::from_fn::<_, 2, _>(|i| (a.screen[i] + b.screen[i]) / 2.);
            let error = (mid.screen[0] - linear[0]).hypot(mid.screen[1] - linear[1]);
            if error > 0.025 {
                if depth >= 40 || samples.len() + stack.len() + 2 >= 262144 {
                    return Err("Line placement sampling budget exceeded".into());
                }
                stack.push((mid, b, depth + 1));
                stack.push((a, mid, depth + 1));
            } else {
                samples.push(b);
            }
        }
    }
    let v = scaler.viewport;
    let positions = placed_curve_points(
        placement,
        &[samples],
        scaler.pixels_per_mm(),
        [
            v.x as f64,
            v.y as f64,
            (v.x + v.width) as f64,
            (v.y + v.height) as f64,
        ],
    )?;
    positions
        .into_iter()
        .map(|s| {
            let (p, bearing) = sample_curve_position(&points, s)?;
            Ok(point.resolved_at(p, Some(bearing)))
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FlatProjection, GeoBounds, Viewport};
    fn template(mode: LinePlacementMode, offset: f64, visible_parts: bool) -> PointInstruction {
        PointInstruction::new("A".into(), WorldPoint::new(0., 0.)).with_line_placement(
            LineSymbolPlacement {
                points: vec![WorldPoint::new(0., 0.), WorldPoint::new(1., 0.)].into_boxed_slice(),
                mode,
                offset,
                visible_parts,
            },
        )
    }
    #[test]
    fn absolute_distance_is_mm_at_both_densities_and_zoom_levels() {
        for ratio in [1., 2.] {
            for zoom in [1., 10., 200.] {
                let mut s = Scaler::new(
                    GeoBounds::new(-0.1, -0.1, 1.1, 0.1),
                    Viewport::with_origin(43., 37., 1000., 600.),
                );
                s.set_projection(FlatProjection::EllipsoidalMercator);
                s.set_pixel_ratio(ratio);
                s.zoom_to_fit(GeoBounds::new(
                    -0.1 / zoom,
                    -0.1 / zoom,
                    1.1 / zoom,
                    0.1 / zoom,
                ));
                let p = template(LinePlacementMode::Absolute, 10., false);
                let placed = resolve_flat_line_symbol(&p, &s).unwrap();
                assert_eq!(placed.len(), 1);
                let a = s.world_to_screen(p.line_placement.as_ref().unwrap().points[0]);
                let b = s.world_to_screen(placed[0].position);
                assert!(((b.x - a.x) as f64 - 10. * s.pixels_per_mm()).abs() < 0.001);
                assert_eq!(placed[0].curve_tangent_bearing, Some(90.));
                assert!(placed[0].line_placement.is_none());
            }
        }
    }
    #[test]
    fn visible_parts_preserve_reentry_and_offsets_beyond_end_emit_nothing() {
        let p = template(LinePlacementMode::Relative, 0.5, true);
        let a = |x, y, segment, fraction| CurveSample {
            screen: [x, y],
            segment,
            fraction,
        };
        let path = vec![
            a(-10., 20., 0, 0.),
            a(110., 20., 0, 1.),
            a(110., 80., 1, 1.),
            a(-10., 80., 2, 1.),
        ];
        let q = placed_curve_points(
            p.line_placement.as_ref().unwrap(),
            &[path],
            1.,
            [0., 0., 100., 100.],
        )
        .unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].screen, [50., 20.]);
        assert_eq!(q[1].screen, [50., 80.]);
        let p = template(LinePlacementMode::Absolute, 1000., false);
        assert!(placed_curve_points(
            p.line_placement.as_ref().unwrap(),
            &[vec![a(0., 0., 0, 0.), a(100., 0., 0, 1.)]],
            1.,
            [0., 0., 100., 100.]
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn dateline_and_serialization_keep_source_and_runtime_tangent() {
        let p = PointInstruction::new("A".into(), WorldPoint::new(179., 70.)).with_line_placement(
            LineSymbolPlacement {
                points: vec![WorldPoint::new(179., 70.), WorldPoint::new(-179., 70.)]
                    .into_boxed_slice(),
                mode: LinePlacementMode::Relative,
                offset: 0.5,
                visible_parts: false,
            },
        );
        let mut s = Scaler::new(
            GeoBounds::new(178., 69., 182., 71.),
            Viewport::new(800., 600.),
        );
        s.set_projection(FlatProjection::EllipsoidalMercator);
        let b = bincode::serialize(&p).unwrap();
        let p: PointInstruction = bincode::deserialize(&b).unwrap();
        let q = resolve_flat_line_symbol(&p, &s).unwrap();
        assert!((q[0].position.x - 180.).abs() < 1e-10);
        assert!((q[0].position.y - 70.).abs() < 1e-10);
        assert_eq!(q[0].curve_tangent_bearing, Some(90.));
    }
}
