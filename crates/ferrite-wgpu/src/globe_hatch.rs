//! Screen-space physical hatch bands clipped to an already draped surface.
//! Intersections interpolate ECEF and homogeneous clip coordinates together:
//! hatch pixels retain the original triangle depth, winding and polygon holes.
//! The product adapter supplies the resolved AreaCRS origin; this module does
//! not substitute a device origin for LocalGeometry or GlobalGeometry.
use crate::globe_scene::{GlobeMesh, GlobeVertex};
use ferrite_kernel::{
    geodesy::{WGS84_A, WGS84_B},
    globe_camera::GlobeCamera,
};

#[derive(Debug, Clone, Copy)]
pub struct HatchStyle {
    pub origin_px: [f64; 2],
    /// Counterclockwise from the positive x-axis in a y-up portrayal CRS.
    pub angle_degrees: f64,
    pub width_mm: f64,
    pub spacing_mm: f64,
    pub pixels_per_mm: f64,
    pub color: [f32; 4],
}
#[derive(Debug, Clone, Copy)]
pub struct HatchLimits {
    pub max_vertices: usize,
    /// Includes empty triangle/band intersections, bounding CPU work too.
    pub max_band_tests: usize,
}
impl Default for HatchLimits {
    fn default() -> Self {
        Self {
            max_vertices: 262144,
            max_band_tests: 1048576,
        }
    }
}
#[derive(Clone, Copy)]
struct Point {
    ecef: [f64; 3],
    clip: [f64; 4],
    horizon: f64,
}
fn trim(input: &[Point], distance: impl Fn(Point) -> f64) -> Result<Vec<Point>, String> {
    let mut result = Vec::with_capacity(input.len() + 1);
    let Some(mut a) = input.last().copied() else {
        return Ok(result);
    };
    let mut da = distance(a);
    for &b in input {
        let db = distance(b);
        if !da.is_finite() || !db.is_finite() {
            return Err("Hatch clipping overflow".into());
        }
        if (da >= 0.) != (db >= 0.) {
            let t = da / (da - db);
            if !t.is_finite() || !(0. ..=1.).contains(&t) {
                return Err("Invalid hatch intersection".into());
            }
            result.push(Point {
                ecef: std::array::from_fn(|i| a.ecef[i] + t * (b.ecef[i] - a.ecef[i])),
                clip: std::array::from_fn(|i| a.clip[i] + t * (b.clip[i] - a.clip[i])),
                horizon: a.horizon + t * (b.horizon - a.horizon),
            });
        }
        if db >= 0. {
            result.push(b);
        }
        a = b;
        da = db;
    }
    Ok(result)
}
/// Complexity O(T + K + V), where T is input triangles, K attempted bands and
/// V emitted vertices. Fixed-size clipping polygons bound temporary scratch;
/// explicit budgets bound K and V. Failure returns no partially drawn result.
pub fn hatch_mesh(
    source: &GlobeMesh,
    camera: &GlobeCamera,
    style: HatchStyle,
    limits: HatchLimits,
) -> Result<GlobeMesh, String> {
    hatch_mesh_with_dash(source, camera, style, None, limits)
}

fn validate_dash(dash: &ferrite_kernel::DashCycle) -> Result<(), String> {
    if !dash.period.is_finite() || dash.period <= 0. || dash.intervals.len() > 4096 {
        return Err("Invalid globe hatch dash period/budget".into());
    }
    let mut previous = 0.;
    for &(start, end) in &dash.intervals {
        if !start.is_finite()
            || !end.is_finite()
            || start < previous
            || end <= start
            || end > dash.period
        {
            return Err("Noncanonical globe hatch dash intervals".into());
        }
        previous = end;
    }
    Ok(())
}

/// Square ends extend each authored dash by half the physical stroke width.
/// Form their periodic union before clipping, avoiding doubled alpha where
/// neighbouring caps overlap or cross the repeat boundary.
fn square_capped_dash(
    dash: &ferrite_kernel::DashCycle,
    width_mm: f64,
) -> Result<ferrite_kernel::DashCycle, String> {
    validate_dash(dash)?;
    if !width_mm.is_finite() || width_mm <= 0. {
        return Err("Invalid square hatch cap width".into());
    }
    ferrite_kernel::DashCycle::new(
        dash.period,
        dash.intervals
            .iter()
            .map(|&(start, end)| (start - width_mm * 0.5, end - start + width_mm)),
    )
    .map_err(str::to_owned)
}

/// Canonical authored dash cycles are clipped along the hatch direction,
/// independently of the perpendicular band spacing. Butt ends retain depth.
pub fn hatch_mesh_with_dash(
    source: &GlobeMesh,
    camera: &GlobeCamera,
    style: HatchStyle,
    dash: Option<&ferrite_kernel::DashCycle>,
    limits: HatchLimits,
) -> Result<GlobeMesh, String> {
    hatch_mesh_with_cap(source, camera, style, dash, false, limits)
}

/// Round-ended periodic hatch. Arc approximation stays within 0.2 physical pixels.
pub fn hatch_mesh_with_round_dash(
    source: &GlobeMesh,
    camera: &GlobeCamera,
    style: HatchStyle,
    dash: &ferrite_kernel::DashCycle,
    limits: HatchLimits,
) -> Result<GlobeMesh, String> {
    hatch_mesh_with_cap(source, camera, style, Some(dash), true, limits)
}

fn hatch_mesh_with_cap(
    source: &GlobeMesh,
    camera: &GlobeCamera,
    style: HatchStyle,
    dash: Option<&ferrite_kernel::DashCycle>,
    round: bool,
    limits: HatchLimits,
) -> Result<GlobeMesh, String> {
    source.validate()?;
    if !style.origin_px.iter().all(|v| v.is_finite())
        || !style.angle_degrees.is_finite()
        || !style.width_mm.is_finite()
        || style.width_mm <= 0.
        || !style.spacing_mm.is_finite()
        || style.spacing_mm <= 0.
        || !style.pixels_per_mm.is_finite()
        || style.pixels_per_mm <= 0.
        || !style
            .color
            .iter()
            .all(|v| v.is_finite() && (0. ..=1.).contains(v))
        || !(3..=262144).contains(&limits.max_vertices)
        || limits.max_band_tests == 0
    {
        return Err("Invalid globe hatch parameters/budget".into());
    }
    let width = style.width_mm * style.pixels_per_mm;
    let spacing = style.spacing_mm * style.pixels_per_mm;
    if !width.is_finite() || !spacing.is_finite() || spacing <= 0. {
        return Err("Globe hatch physical size overflow".into());
    }
    let angle = style.angle_degrees.rem_euclid(360.).to_radians();
    // y-down screen coordinates reverse the portrayed direction's y component.
    let normal = [angle.sin(), angle.cos()];
    let viewport = camera.viewport();
    let phase = normal[0] * style.origin_px[0] + normal[1] * style.origin_px[1];
    if !phase.is_finite() {
        return Err("Globe hatch origin overflow".into());
    }
    // Reduce the phase before clipping. Large offscreen anchors need not cause
    // large integer band numbers or loss in the band enumeration.
    let phase = phase.rem_euclid(spacing);
    let coordinate = |p: Point| {
        normal[0] * (p.clip[0] + p.clip[3]) * viewport[0] * 0.5
            + normal[1] * (p.clip[3] - p.clip[1]) * viewport[1] * 0.5
            - phase * p.clip[3]
    };
    let along = [angle.cos(), -angle.sin()];
    let dash_period = if let Some(dash) = dash {
        let period = dash.period * style.pixels_per_mm;
        if !period.is_finite() || period <= 0. || dash.intervals.len() > 4096 {
            return Err("Invalid globe hatch dash period/budget".into());
        }
        validate_dash(dash)?;
        Some(period)
    } else {
        None
    };
    let along_origin = along[0] * style.origin_px[0] + along[1] * style.origin_px[1];
    if !along_origin.is_finite() {
        return Err("Globe hatch dash origin overflow".into());
    }
    let along_phase = dash_period.map_or(0., |period| along_origin.rem_euclid(period));
    let along_coordinate = |p: Point| {
        along[0] * (p.clip[0] + p.clip[3]) * viewport[0] * 0.5
            + along[1] * (p.clip[3] - p.clip[1]) * viewport[1] * 0.5
            - along_phase * p.clip[3]
    };
    let round_dash = if round {
        dash.map(round_intervals).transpose()?
    } else {
        None
    };
    let axes = [WGS84_A, WGS84_A, WGS84_B];
    let eye = camera.eye_m();
    let mut result = GlobeMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    let mut tests = 0usize;
    if style.color[3] == 0. {
        return Ok(result);
    }
    for triangle in source.indices.chunks_exact(3) {
        let mut polygon = Vec::with_capacity(3);
        for &index in triangle {
            let ecef = source.vertices[index as usize].ecef_m;
            let horizon = (0..3)
                .map(|i| eye[i] / axes[i] * (ecef[i] / axes[i]))
                .sum::<f64>()
                - 1.;
            polygon.push(Point {
                ecef,
                clip: camera.clip_ecef(ecef).map_err(|e| e.to_string())?,
                horizon,
            });
        }
        for plane in 0..7 {
            polygon = trim(&polygon, |p| {
                let [x, y, z, w] = p.clip;
                match plane {
                    0 => p.horizon,
                    1 => w + x,
                    2 => w - x,
                    3 => w + y,
                    4 => w - y,
                    5 => z,
                    _ => w - z,
                }
            })?;
            if polygon.len() < 3 {
                break;
            }
        }
        if polygon.len() < 3 {
            continue;
        }
        if polygon.iter().any(|p| p.clip[3] <= 0.) {
            return Err("Invalid globe hatch depth".into());
        }
        // Overlapping strokes cover the polygon once, preserving authored alpha.
        if width >= spacing && !round {
            emit_dashed(
                &polygon,
                &mut result,
                &mut tests,
                style,
                dash,
                dash_period,
                limits,
                &along_coordinate,
            )?;
            continue;
        }
        let low = polygon
            .iter()
            .map(|p| coordinate(*p) / p.clip[3])
            .fold(f64::INFINITY, f64::min);
        let high = polygon
            .iter()
            .map(|p| coordinate(*p) / p.clip[3])
            .fold(f64::NEG_INFINITY, f64::max);
        // Nearest-band cells partition overlapping round capsules. Their union
        // equals all bands because a nearest centre minimizes radial distance.
        let band_half = if round {
            width.min(spacing) * 0.5
        } else {
            width * 0.5
        };
        let first = ((low - band_half) / spacing).ceil();
        let last = ((high + band_half) / spacing).floor();
        if !first.is_finite()
            || !last.is_finite()
            || first.abs() > 4503599627370496.
            || last.abs() > 4503599627370496.
        {
            return Err("Globe hatch band range overflow".into());
        }
        let count = (last - first + 1.).max(0.);
        if count > limits.max_band_tests.saturating_sub(tests) as f64 {
            return Err("Globe hatch work budget exceeded".into());
        }
        tests += count as usize;
        for band in 0..count as usize {
            let centre = (first + band as f64) * spacing;
            let clipped = trim(&polygon, |p| {
                coordinate(p) - (centre - band_half) * p.clip[3]
            })?;
            let clipped = trim(&clipped, |p| {
                (centre + band_half) * p.clip[3] - coordinate(p)
            })?;
            if let (Some(intervals), Some(period)) = (&round_dash, dash_period) {
                emit_round_dashed(
                    &clipped,
                    &mut result,
                    &mut tests,
                    style,
                    intervals,
                    period,
                    limits,
                    &along_coordinate,
                    &coordinate,
                    centre,
                )?;
                continue;
            }
            emit_dashed(
                &clipped,
                &mut result,
                &mut tests,
                style,
                dash,
                dash_period,
                limits,
                &along_coordinate,
            )?;
        }
    }
    result.validate()?;
    Ok(result)
}

fn emit_polygon(
    p: &[Point],
    result: &mut GlobeMesh,
    style: HatchStyle,
    limits: HatchLimits,
) -> Result<(), String> {
    if p.len() < 3 {
        return Ok(());
    }
    if result
        .vertices
        .len()
        .checked_add(p.len())
        .is_none_or(|n| n > limits.max_vertices)
        || result.indices.len() + (p.len() - 2) * 3 > 1572864
    {
        return Err("Globe hatch mesh budget exceeded".into());
    }
    let base = result.vertices.len() as u32;
    result.vertices.extend(p.iter().map(|p| GlobeVertex {
        ecef_m: p.ecef,
        color: style.color,
    }));
    for j in 1..p.len() - 1 {
        result
            .indices
            .extend_from_slice(&[base, base + j as u32, base + j as u32 + 1]);
    }
    Ok(())
}
/// Join adjacent intervals and the periodic seam before forming caps. The seam
/// is a continuous pen stroke, not two independently rounded endpoints.
fn round_intervals(dash: &ferrite_kernel::DashCycle) -> Result<Vec<(f64, f64)>, String> {
    validate_dash(dash)?;
    let mut out: Vec<(f64, f64)> = Vec::new();
    for &(a, b) in &dash.intervals {
        if let Some(last) = out.last_mut() {
            if last.1 == a {
                last.1 = b;
                continue;
            }
        }
        out.push((a, b));
    }
    if out.len() > 1 && out[0].0 == 0. && out.last().unwrap().1 == dash.period {
        let last = out.pop().unwrap();
        out[0].0 = last.0 - dash.period;
    }
    Ok(out)
}

fn charge(tests: &mut usize, amount: usize, limits: HatchLimits) -> Result<(), String> {
    *tests = tests
        .checked_add(amount)
        .filter(|n| *n <= limits.max_band_tests)
        .ok_or("Globe round hatch work budget exceeded")?;
    Ok(())
}

fn emit_round_dashed(
    polygon: &[Point],
    result: &mut GlobeMesh,
    tests: &mut usize,
    style: HatchStyle,
    intervals: &[(f64, f64)],
    period: f64,
    limits: HatchLimits,
    along: &impl Fn(Point) -> f64,
    normal: &impl Fn(Point) -> f64,
    centre: f64,
) -> Result<(), String> {
    if polygon.len() < 3 || intervals.is_empty() {
        return Ok(());
    }
    let radius = style.width_mm * style.pixels_per_mm * 0.5;
    // A circumscribed regular polygon differs from the circle by at most 0.2px.
    let sides = ((std::f64::consts::PI / (radius / (radius + 0.2)).acos()).ceil() as usize).max(8);
    if sides > 4096 {
        return Err("Round hatch arc resolution budget exceeded".into());
    }
    let low = polygon
        .iter()
        .map(|p| along(*p) / p.clip[3])
        .fold(f64::INFINITY, f64::min);
    let high = polygon
        .iter()
        .map(|p| along(*p) / p.clip[3])
        .fold(f64::NEG_INFINITY, f64::max);
    for (i, &(a, b)) in intervals.iter().enumerate() {
        charge(tests, 1, limits)?;
        let start = a * style.pixels_per_mm;
        let end = b * style.pixels_per_mm;
        let previous = if i == 0 {
            intervals.last().unwrap().1 * style.pixels_per_mm - period
        } else {
            intervals[i - 1].1 * style.pixels_per_mm
        };
        let next = if i + 1 == intervals.len() {
            intervals[0].0 * style.pixels_per_mm + period
        } else {
            intervals[i + 1].0 * style.pixels_per_mm
        };
        let first = ((low - end - radius) / period).ceil();
        let last = ((high - start + radius) / period).floor();
        if !first.is_finite()
            || !last.is_finite()
            || first.abs() > 4503599627370496.
            || last.abs() > 4503599627370496.
        {
            return Err("Round hatch repeat range overflow".into());
        }
        let count = (last - first + 1.).max(0.);
        if count > limits.max_band_tests.saturating_sub(*tests) as f64 {
            return Err("Globe round hatch repeat budget exceeded".into());
        }
        for repeat in 0..count as usize {
            charge(tests, sides + 2, limits)?;
            let shift = (first + repeat as f64) * period;
            // Gap bisectors form disjoint cells even when round ends overlap;
            // nearest endpoint supplies exactly the union without double alpha.
            let left = shift + (previous + start) * 0.5;
            let right = shift + (end + next) * 0.5;
            let mut clipped = trim(polygon, |p| along(p) - left * p.clip[3])?;
            clipped = trim(&clipped, |p| right * p.clip[3] - along(p))?;
            for j in 0..sides {
                if clipped.len() < 3 {
                    break;
                }
                let angle = 2. * std::f64::consts::PI * j as f64 / sides as f64;
                let (ny, nx) = angle.sin_cos();
                let bound = nx * (shift + if nx >= 0. { end } else { start }) + radius;
                clipped = trim(&clipped, |p| {
                    bound * p.clip[3] - nx * along(p) - ny * (normal(p) - centre * p.clip[3])
                })?;
            }
            emit_polygon(&clipped, result, style, limits)?;
        }
    }
    Ok(())
}

fn emit_dashed(
    polygon: &[Point],
    result: &mut GlobeMesh,
    tests: &mut usize,
    style: HatchStyle,
    dash: Option<&ferrite_kernel::DashCycle>,
    period: Option<f64>,
    limits: HatchLimits,
    coordinate: &impl Fn(Point) -> f64,
) -> Result<(), String> {
    if polygon.len() < 3 {
        return Ok(());
    }
    let (Some(dash), Some(period)) = (dash, period) else {
        return emit_polygon(polygon, result, style, limits);
    };
    let low = polygon
        .iter()
        .map(|p| coordinate(*p) / p.clip[3])
        .fold(f64::INFINITY, f64::min);
    let high = polygon
        .iter()
        .map(|p| coordinate(*p) / p.clip[3])
        .fold(f64::NEG_INFINITY, f64::max);
    for &(start, end) in &dash.intervals {
        // An empty repeat range still consumes CPU work. Charge each interval
        // probe before enumerating fragments, bounding gap-only inputs too.
        if *tests >= limits.max_band_tests {
            return Err("Globe hatch dash work budget exceeded".into());
        }
        *tests += 1;
        let start = start * style.pixels_per_mm;
        let end = end * style.pixels_per_mm;
        let first = ((low - end) / period).ceil();
        let last = ((high - start) / period).floor();
        if !first.is_finite()
            || !last.is_finite()
            || first.abs() > 4503599627370496.
            || last.abs() > 4503599627370496.
        {
            return Err("Globe hatch dash range overflow".into());
        }
        let count = (last - first + 1.).max(0.);
        if count > limits.max_band_tests.saturating_sub(*tests) as f64 {
            return Err("Globe hatch dash work budget exceeded".into());
        }
        *tests += count as usize;
        for repeat in 0..count as usize {
            let offset = (first + repeat as f64) * period;
            let clipped = trim(polygon, |p| coordinate(p) - (offset + start) * p.clip[3])?;
            let clipped = trim(&clipped, |p| (offset + end) * p.clip[3] - coordinate(p))?;
            emit_polygon(&clipped, result, style, limits)?;
        }
    }
    Ok(())
}

/// Resolve a stable point on the authored local geometry, or a common WGS84
/// reference supplied by the renderer. Visibility never changes the origin:
/// hiding an anchor behind the limb must not move the hatch phase to a corner.
pub fn pattern_origin(
    crs: ferrite_render::PatternCrs,
    camera: &GlobeCamera,
    first_geometry_point: ferrite_render::WorldPoint,
    common_reference: ferrite_render::WorldPoint,
) -> Result<[f64; 2], String> {
    if crs == ferrite_render::PatternCrs::Global {
        return Ok([0., camera.viewport()[1]]);
    }
    let point = if crs == ferrite_render::PatternCrs::LocalGeometry {
        first_geometry_point
    } else {
        common_reference
    };
    if !point.x.is_finite() || point.x.abs() > 1e9 {
        return Err("Invalid hatch origin longitude".into());
    }
    let longitude = (point.x + 180.).rem_euclid(360.) - 180.;
    let ecef = ferrite_kernel::geodesy::GeographicPosition::new(point.y, longitude)
        .and_then(|p| p.to_ecef(0.))
        .map_err(|e| e.to_string())?;
    let clip = camera.clip_ecef(ecef).map_err(|e| e.to_string())?;
    if clip[3].abs() <= 1e-9 {
        return Err("Hatch AreaCRS anchor lies on camera plane".into());
    }
    let viewport = camera.viewport();
    let origin = [
        (clip[0] / clip[3] + 1.) * viewport[0] * 0.5,
        (1. - clip[1] / clip[3]) * viewport[1] * 0.5,
    ];
    if !origin.iter().all(|v| v.is_finite()) {
        return Err("Hatch AreaCRS anchor projection overflow".into());
    }
    Ok(origin)
}
/// Render ordered butt/square/round-capped hatch strokes. Unsupported symbol geometry
/// is rejected explicitly, never flattened into the first stroke's solid line.
pub fn drape_hatch_area(
    area: &ferrite_render::AreaInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
    common_reference: ferrite_render::WorldPoint,
) -> Result<GlobeMesh, String> {
    let ferrite_render::AreaFillType::HatchFill {
        color,
        width,
        spacing,
        angle,
    } = area.fill
    else {
        return Err("Not a HatchFill area".into());
    };
    let first = area
        .exterior
        .first()
        .copied()
        .ok_or("Empty hatch exterior")?;
    let origin_px = pattern_origin(area.pattern_crs, camera, first, common_reference)?;
    if !area.fill_opacity.is_finite() || !(0. ..=1.).contains(&area.fill_opacity) {
        return Err("Invalid hatch area opacity".into());
    }
    if !area.hatch_line_style_refs.is_empty() && area.hatch_strokes.is_empty() {
        return Err("Unresolved hatch line styles".into());
    }
    for stroke in &area.hatch_strokes {
        if !stroke.symbols.is_empty() {
            return Err("Hatch repeating symbol geometry is not implemented".into());
        }
        if stroke.style.width_unit != ferrite_render::StrokeUnit::Millimetres
            || !stroke.style.dash_pattern.is_empty()
        {
            return Err("Hatch requires canonical physical stroke units".into());
        }
    }
    let (surface, _) = crate::globe_portrayal::drape_area_geometry(
        &area.exterior,
        &area.interiors,
        camera,
        Default::default(),
    )?;
    let style = HatchStyle {
        origin_px,
        angle_degrees: angle as f64,
        width_mm: width as f64,
        spacing_mm: spacing as f64,
        pixels_per_mm,
        color: color.to_array(),
    };
    if area.hatch_strokes.is_empty() {
        return hatch_mesh(&surface, camera, style, Default::default());
    }
    let mut combined = GlobeMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    for stroke in &area.hatch_strokes {
        if !stroke.style.has_visible_stroke() {
            continue;
        }
        if !stroke.style.offset_mm.is_finite() {
            return Err("Invalid physical hatch offset".into());
        }
        let angle = style.angle_degrees.rem_euclid(360.).to_radians();
        let shift = stroke.style.offset_mm * pixels_per_mm;
        let style = HatchStyle {
            origin_px: [
                style.origin_px[0] - angle.sin() * shift,
                style.origin_px[1] - angle.cos() * shift,
            ],
            width_mm: stroke.style.width as f64,
            color: stroke.style.color.with_alpha(area.fill_opacity).to_array(),
            ..style
        };
        let square_dash = if stroke.style.cap == ferrite_render::CapStyle::Square {
            stroke
                .style
                .dash_cycle
                .as_ref()
                .map(|dash| square_capped_dash(dash, style.width_mm))
                .transpose()?
        } else {
            None
        };
        let mesh = if stroke.style.cap == ferrite_render::CapStyle::Round
            && stroke.style.dash_cycle.is_some()
        {
            hatch_mesh_with_round_dash(
                &surface,
                camera,
                style,
                stroke.style.dash_cycle.as_ref().unwrap(),
                Default::default(),
            )?
        } else {
            hatch_mesh_with_dash(
                &surface,
                camera,
                style,
                square_dash.as_ref().or(stroke.style.dash_cycle.as_ref()),
                Default::default(),
            )?
        };
        combined.append(&mesh)?;
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::{
        geodesy::GeographicPosition,
        globe_coverage_projection::{project_coverage_triangles, CoverageProjectionLimits},
    };
    use ferrite_render::{AreaInstruction, Color, WorldPoint};
    fn camera(heading: f64, tilt: f64) -> GlobeCamera {
        GlobeCamera::orbit(
            GeographicPosition::new(51., 1.).unwrap(),
            50000.,
            heading,
            tilt,
            [240., 180.],
            45.,
            1.,
            1e9,
        )
        .unwrap()
    }
    fn source(c: &GlobeCamera) -> GlobeMesh {
        let ring = |r: f64| {
            vec![
                WorldPoint::new(1. - r, 51. - r),
                WorldPoint::new(1. + r, 51. - r),
                WorldPoint::new(1. + r, 51. + r),
                WorldPoint::new(1. - r, 51. + r),
            ]
        };
        crate::globe_portrayal::drape_area(
            &AreaInstruction::new(ring(0.15))
                .with_interiors(vec![ring(0.035)])
                .with_solid_fill(Color::BLACK),
            c,
            Default::default(),
        )
        .unwrap()
        .0
    }
    fn style(dpi: f64, angle: f64) -> HatchStyle {
        HatchStyle {
            origin_px: [13.2, 171.7],
            angle_degrees: angle,
            width_mm: 0.32,
            spacing_mm: 2.1,
            pixels_per_mm: 3.78 * dpi,
            color: [0.2, 0.3, 0.4, 0.6],
        }
    }
    fn inside_ring(ring: &[[f64; 2]], p: [f64; 2]) -> bool {
        let mut inside = false;
        for i in 0..ring.len() {
            let a = ring[i];
            let b = ring[(i + 1) % ring.len()];
            if (a[1] > p[1]) != (b[1] > p[1])
                && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
            {
                inside = !inside;
            }
        }
        inside
    }
    fn projected(c: &GlobeCamera, m: &GlobeMesh) -> Vec<[[f64; 2]; 3]> {
        m.indices
            .chunks_exact(3)
            .map(|t| {
                std::array::from_fn(|i| {
                    let v = c.clip_ecef(m.vertices[t[i] as usize].ecef_m).unwrap();
                    [(v[0] / v[3] + 1.) * 120., (1. - v[1] / v[3]) * 90.]
                })
            })
            .collect()
    }
    #[test]
    fn round_hatches_match_capsule_union_and_never_double_cover() {
        for (heading, tilt) in [(0., 0.), (90., 60.)] {
            let c = camera(heading, tilt);
            let src = source(&c);
            let source_tris = projected(&c, &src);
            for (dpi, angle, width, spacing) in
                [(1., 0., 1.6, 5.), (2., 35., 1.6, 5.), (1.5, 90., 5., 2.)]
            {
                let s = HatchStyle {
                    width_mm: width,
                    spacing_mm: spacing,
                    ..style(dpi, angle)
                };
                for dash in [
                    ferrite_kernel::DashCycle::new(8., [(5., -4.)]).unwrap(),
                    ferrite_kernel::DashCycle::new(8., [(7., 2.)]).unwrap(),
                    ferrite_kernel::DashCycle::new(8., [(0., 8.)]).unwrap(),
                ] {
                    let mesh =
                        hatch_mesh_with_round_dash(&src, &c, s, &dash, Default::default()).unwrap();
                    let tris = projected(&c, &mesh);
                    let a = angle.to_radians();
                    let radius = width * s.pixels_per_mm * 0.5;
                    let period = dash.period * s.pixels_per_mm;
                    let intervals = round_intervals(&dash).unwrap();
                    let mut checked = 0;
                    for y in (0..180).step_by(3) {
                        for x in (0..240).step_by(3) {
                            let p = [x as f64 + 0.371, y as f64 + 0.613];
                            let source = source_tris.iter().any(|t| inside_ring(t, p));
                            let across = (a.sin() * (p[0] - s.origin_px[0])
                                + a.cos() * (p[1] - s.origin_px[1]))
                                .rem_euclid(spacing * s.pixels_per_mm);
                            let across = across.min(spacing * s.pixels_per_mm - across);
                            let along = (a.cos() * (p[0] - s.origin_px[0])
                                - a.sin() * (p[1] - s.origin_px[1]))
                                .rem_euclid(period);
                            let dx = intervals
                                .iter()
                                .flat_map(|&(start, end)| {
                                    (-1..=1).map(move |repeat| {
                                        let start =
                                            start * s.pixels_per_mm + repeat as f64 * period;
                                        let end = end * s.pixels_per_mm + repeat as f64 * period;
                                        if along < start {
                                            start - along
                                        } else if along > end {
                                            along - end
                                        } else {
                                            0.
                                        }
                                    })
                                })
                                .fold(f64::INFINITY, f64::min);
                            let distance = across.hypot(dx);
                            // The independent exact capsule oracle excludes only the
                            // documented polygon arc tolerance (0.2 physical pixels).
                            if (distance - radius).abs() < 0.21 {
                                continue;
                            }
                            let count = tris.iter().filter(|t| inside_ring(*t, p)).count();
                            assert!(
                                count <= 1,
                                "duplicate alpha at {p:?}, width {width}/spacing {spacing}"
                            );
                            assert_eq!(
                                count == 1,
                                source && distance < radius,
                                "capsule {heading}/{tilt}/{angle}/{dpi}, p {p:?}"
                            );
                            checked += 1;
                        }
                    }
                    assert!(checked > 1000);
                }
            }
        }
    }
    #[test]
    fn round_periodic_seam_and_work_budget_are_explicit() {
        let d = ferrite_kernel::DashCycle::new(8., [(7., 2.)]).unwrap();
        assert_eq!(round_intervals(&d).unwrap(), vec![(-1., 1.)]);
        let c = camera(0., 0.);
        let src = source(&c);
        assert!(hatch_mesh_with_round_dash(
            &src,
            &c,
            style(1., 0.),
            &d,
            HatchLimits {
                max_band_tests: 1,
                ..Default::default()
            }
        )
        .is_err());
    }
    #[test]
    fn physical_hatch_matches_polygon_holes_and_analytic_bands_across_views() {
        for (heading, tilt) in [(0., 0.), (90., 60.)] {
            let c = camera(heading, tilt);
            let src = source(&c);
            let region = project_coverage_triangles(
                &c,
                src.indices
                    .chunks_exact(3)
                    .map(|t| std::array::from_fn(|i| src.vertices[t[i] as usize].ecef_m)),
                CoverageProjectionLimits::default(),
            )
            .unwrap();
            let polygons: Vec<_> = region
                .polygons()
                .iter()
                .map(|p| {
                    (
                        p.exterior()
                            .0
                            .iter()
                            .map(|c| [c.x, c.y])
                            .collect::<Vec<_>>(),
                        p.interiors()
                            .iter()
                            .map(|r| r.0.iter().map(|c| [c.x, c.y]).collect::<Vec<_>>())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            for (dpi, angle) in [(1., 0.), (1.5, 45.), (2., -30.), (3., 90.)] {
                let s = style(dpi, angle);
                let hatch = hatch_mesh(&src, &c, s, Default::default()).unwrap();
                let tris = projected(&c, &hatch);
                assert!(!tris.is_empty());
                let a = angle.to_radians();
                let n = [a.sin(), a.cos()];
                let spacing = s.spacing_mm * s.pixels_per_mm;
                let half = s.width_mm * s.pixels_per_mm * 0.5;
                for y in (0..180).step_by(3) {
                    for x in (0..240).step_by(3) {
                        let p = [x as f64 + 0.371, y as f64 + 0.613];
                        let in_source = polygons.iter().any(|(ext, holes)| {
                            inside_ring(ext, p) && !holes.iter().any(|h| inside_ring(h, p))
                        });
                        let q = (n[0] * (p[0] - s.origin_px[0]) + n[1] * (p[1] - s.origin_px[1]))
                            .rem_euclid(spacing);
                        let expected = in_source && q.min(spacing - q) < half;
                        let actual = tris.iter().any(|t| inside_ring(t, p));
                        assert_eq!(
                            actual, expected,
                            "view {heading}/{tilt}, dpi {dpi}, angle {angle}, pixel {p:?}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn preserves_original_surface_plane_and_transparency() {
        let c = camera(90., 60.);
        let src = source(&c);
        let source = GlobeMesh {
            vertices: src.vertices.clone(),
            indices: src.indices[..3].to_vec(),
        };
        let hatch = hatch_mesh(&source, &c, style(2., 45.), Default::default()).unwrap();
        assert!(!hatch.indices.is_empty());
        let a = source.vertices[source.indices[0] as usize].ecef_m;
        let b = source.vertices[source.indices[1] as usize].ecef_m;
        let d = source.vertices[source.indices[2] as usize].ecef_m;
        let u: [f64; 3] = std::array::from_fn(|i| b[i] - a[i]);
        let v: [f64; 3] = std::array::from_fn(|i| d[i] - a[i]);
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let norm = n.iter().map(|x| x * x).sum::<f64>().sqrt();
        for p in hatch.vertices {
            let distance = (0..3)
                .map(|i| (p.ecef_m[i] - a[i]) * n[i])
                .sum::<f64>()
                .abs()
                / norm;
            assert!(distance < 1e-7, "depth plane changed {distance}");
            assert_eq!(p.color, style(2., 45.).color);
        }
    }
    #[test]
    fn rejects_unbounded_work_and_invalid_physical_parameters() {
        let c = camera(0., 0.);
        let source = source(&c);
        let s = style(1., 0.);
        assert!(hatch_mesh(
            &source,
            &c,
            s,
            HatchLimits {
                max_vertices: 3,
                max_band_tests: 1
            }
        )
        .is_err());
        for bad in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(hatch_mesh(
                &source,
                &c,
                HatchStyle {
                    spacing_mm: bad,
                    ..s
                },
                Default::default()
            )
            .is_err());
            assert!(hatch_mesh(
                &source,
                &c,
                HatchStyle {
                    pixels_per_mm: bad,
                    ..s
                },
                Default::default()
            )
            .is_err());
        }
        assert!(hatch_mesh(
            &source,
            &c,
            HatchStyle {
                spacing_mm: 1e-15,
                width_mm: 1e-16,
                ..s
            },
            Default::default()
        )
        .is_err());
        let empty = hatch_mesh(
            &source,
            &c,
            HatchStyle {
                color: [0., 0., 0., 0.],
                ..s
            },
            Default::default(),
        )
        .unwrap();
        assert!(empty.indices.is_empty());
    }
    #[test]
    fn limb_and_viewport_clipping_keep_only_front_fragments() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            10000000.,
            0.,
            0.,
            [240., 180.],
            45.,
            1.,
            1e9,
        )
        .unwrap();
        let mesh = |coords: &[(f64, f64)]| GlobeMesh {
            vertices: coords
                .iter()
                .map(|&(lat, lon)| GlobeVertex {
                    ecef_m: GeographicPosition::new(lat, lon)
                        .unwrap()
                        .to_ecef(0.)
                        .unwrap(),
                    color: [1.; 4],
                })
                .collect(),
            indices: vec![0, 1, 2],
        };
        let src = mesh(&[(-25., 30.), (25., 30.), (0., 100.)]);
        let h = hatch_mesh(&src, &c, style(1., 35.), Default::default()).unwrap();
        assert!(!h.indices.is_empty());
        let eye = c.eye_m();
        let axes = [WGS84_A, WGS84_A, WGS84_B];
        for v in &h.vertices {
            let horizon = (0..3)
                .map(|i| eye[i] / axes[i] * (v.ecef_m[i] / axes[i]))
                .sum::<f64>()
                - 1.;
            assert!(horizon >= -1e-12);
            let [x, y, z, w] = c.clip_ecef(v.ecef_m).unwrap();
            let tolerance = w.abs() * 1e-12;
            assert!(
                w > 0.
                    && x.abs() <= w + tolerance
                    && y.abs() <= w + tolerance
                    && z >= -tolerance
                    && z <= w + tolerance
            );
        }
        let back = mesh(&[(-5., 170.), (5., 170.), (0., 180.)]);
        assert!(hatch_mesh(&back, &c, style(1., 0.), Default::default())
            .unwrap()
            .indices
            .is_empty());
    }
    #[test]
    fn dash_initial_gaps_and_intervals_match_analytic_fragment_oracle() {
        let c = camera(90., 60.);
        let src = source(&c);
        let style = style(1.5, 45.);
        let dash = ferrite_kernel::DashCycle::new(5., [(1., 1.2), (3., 0.5)]).unwrap();
        let solid = hatch_mesh(&src, &c, style, Default::default()).unwrap();
        let hatch = hatch_mesh_with_dash(&src, &c, style, Some(&dash), Default::default()).unwrap();
        let solid = projected(&c, &solid);
        let actual = projected(&c, &hatch);
        assert!(!actual.is_empty());
        let angle = style.angle_degrees.to_radians();
        let direction = [angle.cos(), -angle.sin()];
        for y in (0..180).step_by(3) {
            for x in (0..240).step_by(3) {
                let p = [x as f64 + 0.371, y as f64 + 0.613];
                let distance = ((direction[0] * (p[0] - style.origin_px[0])
                    + direction[1] * (p[1] - style.origin_px[1]))
                    / style.pixels_per_mm)
                    .rem_euclid(dash.period);
                let expected = solid.iter().any(|t| inside_ring(t, p))
                    && dash
                        .intervals
                        .iter()
                        .any(|&(start, end)| distance > start && distance < end);
                assert_eq!(
                    actual.iter().any(|t| inside_ring(t, p)),
                    expected,
                    "dash pixel {p:?}"
                );
            }
        }
        let invalid = ferrite_kernel::DashCycle {
            period: 5.,
            intervals: vec![(2., 4.), (1., 3.)],
        };
        assert!(hatch_mesh_with_dash(&src, &c, style, Some(&invalid), Default::default()).is_err());
        let empty = ferrite_kernel::DashCycle::new(5., []).unwrap();
        assert!(
            hatch_mesh_with_dash(&src, &c, style, Some(&empty), Default::default())
                .unwrap()
                .indices
                .is_empty()
        );
    }
    #[test]
    fn square_caps_wrap_union_and_reject_malformed_authored_cycles() {
        let dash = ferrite_kernel::DashCycle::new(5., [(0.1, 0.4), (4.5, 0.3)]).unwrap();
        let wrapped = square_capped_dash(&dash, 1.).unwrap();
        assert_eq!(wrapped.intervals.len(), 2);
        for ((a, b), (expected_a, expected_b)) in
            wrapped.intervals.into_iter().zip([(0., 1.), (4., 5.)])
        {
            assert!((a - expected_a).abs() < 1e-12 && (b - expected_b).abs() < 1e-12);
        }
        let overlap = ferrite_kernel::DashCycle::new(5., [(1., 0.5), (2., 0.5)]).unwrap();
        assert_eq!(
            square_capped_dash(&overlap, 2.).unwrap().intervals,
            vec![(0., 3.5)]
        );
        assert_eq!(
            square_capped_dash(&overlap, 5.).unwrap().intervals,
            vec![(0., 5.)]
        );
        let malformed = ferrite_kernel::DashCycle {
            period: 5.,
            intervals: vec![(2., 4.), (1., 3.)],
        };
        assert!(square_capped_dash(&malformed, 1.).is_err());
        assert!(square_capped_dash(&dash, f64::NAN).is_err());
        assert!(square_capped_dash(&dash, 0.).is_err());
        let empty = ferrite_kernel::DashCycle::new(5., []).unwrap();
        assert!(square_capped_dash(&empty, 1.).unwrap().intervals.is_empty());
    }
    #[test]
    fn square_cap_area_matches_independent_periodic_rectangle_oracle() {
        let ring = |r: f64| {
            vec![
                WorldPoint::new(1. - r, 51. - r),
                WorldPoint::new(1. + r, 51. - r),
                WorldPoint::new(1. + r, 51. + r),
                WorldPoint::new(1. - r, 51. + r),
            ]
        };
        for (heading, tilt) in [(0., 0.), (90., 60.)] {
            let c = camera(heading, tilt);
            for (dpi, angle) in [(1., 0.), (1.5, 45.), (2., -30.), (3., 90.)] {
                for (width, authored) in [
                    (0.32, vec![(1., 1.2), (3., 0.5)]),
                    (0.64, vec![(0.1, 0.4), (4.5, 0.3)]),
                    (2., vec![(1., 0.5), (2., 0.5)]),
                ] {
                    let dash = ferrite_kernel::DashCycle::new(5., authored.clone()).unwrap();
                    let mut area = AreaInstruction::new(ring(0.15))
                        .with_interiors(vec![ring(0.035)])
                        .with_hatch_fill(Color::rgba(1., 0., 0., 0.6), width, 3., angle)
                        .with_pattern_crs(ferrite_render::PatternCrs::Global);
                    let mut line =
                        ferrite_render::LineStyle::solid_mm(Color::rgba(1., 0., 0., 0.6), width);
                    line.cap = ferrite_render::CapStyle::Square;
                    line.dash_cycle = Some(dash);
                    area.fill_opacity = 0.5;
                    area.hatch_strokes = vec![ferrite_render::HatchStroke {
                        style: line,
                        interval_length_mm: 0.,
                        symbols: Box::default(),
                    }]
                    .into_boxed_slice();
                    let ppm = 3.78 * dpi;
                    let actual = drape_hatch_area(&area, &c, ppm, WorldPoint::new(0., 0.)).unwrap();
                    assert!(actual.vertices.iter().all(|v| v.color == [1., 0., 0., 0.3]));
                    let actual = projected(&c, &actual);
                    let bands = projected(
                        &c,
                        &hatch_mesh(
                            &source(&c),
                            &c,
                            HatchStyle {
                                origin_px: [0., 180.],
                                angle_degrees: angle as f64,
                                width_mm: width as f64,
                                spacing_mm: 3.,
                                pixels_per_mm: ppm,
                                color: [1., 0., 0., 0.3],
                            },
                            Default::default(),
                        )
                        .unwrap(),
                    );
                    let a = (angle as f64).to_radians();
                    for y in (0..180).step_by(6) {
                        for x in (0..240).step_by(6) {
                            let p = [x as f64 + 0.371, y as f64 + 0.613];
                            let d =
                                ((a.cos() * p[0] - a.sin() * (p[1] - 180.)) / ppm).rem_euclid(5.);
                            // Direct rectangle endpoints in adjacent repeats; does
                            // not use the cap normalizer to derive expectations.
                            let on_dash = authored.iter().any(|&(start, length)| {
                                (-1..=1).any(|repeat| {
                                    d > start + repeat as f64 * 5. - width as f64 * 0.5
                                        && d < start
                                            + length
                                            + repeat as f64 * 5.
                                            + width as f64 * 0.5
                                })
                            });
                            let expected = on_dash && bands.iter().any(|t| inside_ring(t, p));
                            let hits = actual.iter().filter(|t| inside_ring(*t, p)).count();
                            assert_eq!(
                                hits > 0,
                                expected,
                                "square cap {heading}/{tilt}/{dpi}/{angle}/{width} {p:?}"
                            );
                            assert!(hits <= 1, "overlapping caps must not double alpha: {p:?}");
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn pattern_origins_are_stable_for_geometry_and_common_reference() {
        let common = WorldPoint::new(0., 0.);
        let first = WorldPoint::new(1., 51.);
        for c in [camera(0., 0.), camera(90., 60.)] {
            assert_eq!(
                pattern_origin(ferrite_render::PatternCrs::Global, &c, first, common).unwrap(),
                [0., 180.]
            );
            let local =
                pattern_origin(ferrite_render::PatternCrs::LocalGeometry, &c, first, common)
                    .unwrap();
            assert!((local[0] - 120.).abs() < 1e-7 && (local[1] - 90.).abs() < 1e-7);
            let global = pattern_origin(
                ferrite_render::PatternCrs::GlobalGeometry,
                &c,
                first,
                common,
            )
            .unwrap();
            assert_eq!(
                global,
                pattern_origin(
                    ferrite_render::PatternCrs::GlobalGeometry,
                    &c,
                    WorldPoint::new(3., 53.),
                    common
                )
                .unwrap()
            );
            assert_eq!(
                local,
                pattern_origin(
                    ferrite_render::PatternCrs::LocalGeometry,
                    &c,
                    WorldPoint::new(361., 51.),
                    common
                )
                .unwrap()
            );
        }
    }
    #[test]
    fn complete_hatch_area_orders_both_strokes_and_applies_area_alpha_once() {
        let c = camera(0., 0.);
        let ring = vec![
            WorldPoint::new(0.9, 50.9),
            WorldPoint::new(1.1, 50.9),
            WorldPoint::new(1.1, 51.1),
            WorldPoint::new(0.9, 51.1),
        ];
        let mut area = AreaInstruction::new(ring)
            .with_hatch_fill(Color::RED, 0.64, 3., 0.)
            .with_pattern_crs(ferrite_render::PatternCrs::Global);
        let strokes = vec![
            ferrite_render::HatchStroke {
                style: ferrite_render::LineStyle::solid_mm(Color::rgba(1., 0., 0., 0.75), 0.64),
                interval_length_mm: 0.,
                symbols: Box::default(),
            },
            ferrite_render::HatchStroke {
                style: ferrite_render::LineStyle::solid_mm(Color::rgba(0., 1., 0., 0.5), 0.32),
                interval_length_mm: 0.,
                symbols: Box::default(),
            },
        ];
        area.hatch_strokes = strokes.into_boxed_slice();
        area.fill_opacity = 0.4;
        let combined = drape_hatch_area(&area, &c, 3.78, WorldPoint::new(0., 0.)).unwrap();
        let first_green = combined
            .vertices
            .iter()
            .position(|v| v.color[1] == 1.)
            .unwrap();
        assert!(first_green > 0);
        assert!(combined.vertices[..first_green]
            .iter()
            .all(|v| v.color == [1., 0., 0., 0.3]));
        assert!(combined.vertices[first_green..]
            .iter()
            .all(|v| v.color == [0., 1., 0., 0.2]));
        area.hatch_strokes[1].style.cap = ferrite_render::CapStyle::Round;
        let round_solid = drape_hatch_area(&area, &c, 3.78, WorldPoint::new(0., 0.)).unwrap();
        assert_eq!(round_solid.indices, combined.indices);
        assert!(round_solid
            .vertices
            .iter()
            .zip(&combined.vertices)
            .all(|(a, b)| a.ecef_m == b.ecef_m && a.color == b.color));
    }
    #[test]
    fn borrowed_pattern_surface_keeps_original_solid_geometry_bits() {
        let c = camera(90., 60.);
        let ring = vec![
            WorldPoint::new(0.9, 50.9),
            WorldPoint::new(1.1, 50.9),
            WorldPoint::new(1.1, 51.1),
            WorldPoint::new(0.9, 51.1),
        ];
        let area = AreaInstruction::new(ring).with_solid_fill(Color::WHITE);
        let old = crate::globe_portrayal::drape_area(&area, &c, Default::default())
            .unwrap()
            .0;
        let borrowed = crate::globe_portrayal::drape_area_geometry(
            &area.exterior,
            &area.interiors,
            &c,
            Default::default(),
        )
        .unwrap()
        .0;
        assert_eq!(old.indices, borrowed.indices);
        assert_eq!(old.vertices.len(), borrowed.vertices.len());
        for (a, b) in old.vertices.iter().zip(&borrowed.vertices) {
            assert_eq!(a.ecef_m.map(f64::to_bits), b.ecef_m.map(f64::to_bits));
            assert_eq!(a.color, b.color);
        }
    }
}
