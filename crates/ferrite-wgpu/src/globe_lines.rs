//! Geographic screen-fixed strokes on WGS84. Source geometry is loxodromic;
//! navigation geodesics and local portrayal paths use separate adapters.
pub use crate::globe_curve_clip::{project_rhumb_components, ProjectedLineComponent};
use crate::{
    globe_scene::{GlobeMesh, GlobeVertex},
    screen_stroke::stroke_screen_path,
};
use ferrite_kernel::{geodesy::GeographicPosition, globe_camera::GlobeCamera, rhumb::RhumbSegment};
use ferrite_render::{dash_projected_line_spans_clipped, LineInstruction, WorldPoint};
#[derive(Debug, Clone)]
pub struct ProjectedLineSample {
    pub ecef_m: [f64; 3],
    pub screen_px: [f64; 2],
    pub forward_depth_m: f64,
    pub source_segment: usize,
    pub source_fraction: f64,
}
#[derive(Debug, Default, Clone)]
pub struct GlobeLineStats {
    pub samples: usize,
    pub frustum_culled: bool,
    pub horizon_culled: bool,
    pub refinements: usize,
    pub runs: usize,
    pub triangles: usize,
}
fn sample(
    p: GeographicPosition,
    c: &GlobeCamera,
    segment: usize,
    fraction: f64,
) -> Result<ProjectedLineSample, String> {
    let e = p.to_ecef(0.).map_err(|e| e.to_string())?;
    let clip = c.clip_ecef(e).map_err(|e| e.to_string())?;
    if clip[3] <= 0. {
        return Err(
            "Geographic stroke crosses camera eye plane; requires homogeneous path partition"
                .into(),
        );
    }
    let v = c.viewport();
    Ok(ProjectedLineSample {
        ecef_m: e,
        screen_px: [
            (clip[0] / clip[3] + 1.) * v[0] / 2.,
            (1. - clip[1] / clip[3]) * v[1] / 2.,
        ],
        forward_depth_m: clip[3],
        source_segment: segment,
        source_fraction: fraction,
    })
}
fn position(p: WorldPoint) -> Result<GeographicPosition, String> {
    if !p.x.is_finite() || p.x.abs() > 1e9 {
        return Err("Invalid globe line longitude".into());
    }
    GeographicPosition::new(p.y, (p.x + 180.).rem_euclid(360.) - 180.).map_err(|e| e.to_string())
}
/// Subdivide source rhumb segments using both screen deviation and ECEF chord
/// error. Sample source parameters survive subdivision for later suppression.
pub fn project_rhumb_line(
    points: &[WorldPoint],
    camera: &GlobeCamera,
    screen_error: f64,
    chord_error: f64,
    budget: usize,
) -> Result<(Vec<ProjectedLineSample>, usize), String> {
    if points.len() > budget
        || budget > 262144
        || budget < 2
        || !screen_error.is_finite()
        || screen_error <= 0.
        || !chord_error.is_finite()
        || chord_error <= 0.
    {
        return Err("Invalid globe line sampling limits".into());
    }
    if points.len() < 2 {
        return Ok((Vec::new(), 0));
    }
    let mut output = Vec::new();
    let mut refinements = 0;
    for (seg, pair) in points.windows(2).enumerate() {
        let route =
            RhumbSegment::new(position(pair[0])?, position(pair[1])?).map_err(|e| e.to_string())?;
        let a = sample(route.point(0.).map_err(|e| e.to_string())?, camera, seg, 0.)?;
        let b = sample(route.point(1.).map_err(|e| e.to_string())?, camera, seg, 1.)?;
        if seg == 0 {
            output.push(a.clone());
        }
        let mut pending = vec![(0., 1., a, b, 0u8)];
        while let Some((t0, t1, a, b, depth)) = pending.pop() {
            let mid = (t0 + t1) / 2.;
            let m = sample(
                route.point(mid).map_err(|e| e.to_string())?,
                camera,
                seg,
                mid,
            )?;
            let linear: [f64; 3] = std::array::from_fn(|i| (a.ecef_m[i] + b.ecef_m[i]) / 2.);
            let chord = (0..3)
                .map(|i| (m.ecef_m[i] - linear[i]).powi(2))
                .sum::<f64>()
                .sqrt();
            let clip = camera.clip_ecef(linear).map_err(|e| e.to_string())?;
            let v = camera.viewport();
            let s = [
                (clip[0] / clip[3] + 1.) * v[0] / 2.,
                (1. - clip[1] / clip[3]) * v[1] / 2.,
            ];
            let pixel = (m.screen_px[0] - s[0]).hypot(m.screen_px[1] - s[1]);
            if chord > chord_error || pixel > screen_error {
                if depth >= 32
                    || mid == t0
                    || mid == t1
                    || output.len() + pending.len() + 2 >= budget
                {
                    return Err("Globe line subdivision budget/resolution exceeded".into());
                }
                refinements += 1;
                pending.push((mid, t1, m.clone(), b, depth + 1));
                pending.push((t0, mid, a, m, depth + 1));
            } else {
                if output.len() >= budget {
                    return Err("Globe line sample budget exceeded".into());
                }
                output.push(b);
            }
        }
    }
    Ok((output, refinements))
}
fn interpolate(
    a: &ProjectedLineSample,
    b: &ProjectedLineSample,
    screen_fraction: f64,
) -> ProjectedLineSample {
    let f = screen_fraction.clamp(0., 1.);
    let t = f * a.forward_depth_m / (b.forward_depth_m * (1. - f) + a.forward_depth_m * f);
    ProjectedLineSample {
        ecef_m: std::array::from_fn(|i| a.ecef_m[i] + t * (b.ecef_m[i] - a.ecef_m[i])),
        screen_px: std::array::from_fn(|i| a.screen_px[i] + f * (b.screen_px[i] - a.screen_px[i])),
        forward_depth_m: a.forward_depth_m + t * (b.forward_depth_m - a.forward_depth_m),
        source_segment: b.source_segment,
        source_fraction: if a.source_segment == b.source_segment {
            a.source_fraction + t * (b.source_fraction - a.source_fraction)
        } else {
            t * b.source_fraction
        },
    }
}
/// Coincident-curve suppression in ellipsoidal Mercator parameter space.
/// The caller supplies actual execution eligibility (date/groups/resources/
/// dependencies). Visibility is checked before a higher-priority curve hides
/// another curve. Returned fractions parameterize source rhumb segments.
pub fn geographic_line_suppression(
    instructions: &[ferrite_render::DrawingInstruction],
    eligible: &[bool],
    meridian: f64,
) -> Result<std::sync::Arc<ferrite_render::LineSuppressionPlan>, String> {
    if instructions.len() != eligible.len() || !meridian.is_finite() {
        return Err("Invalid globe suppression input".into());
    }
    let mut transformed = Vec::with_capacity(instructions.len());
    for (i, instruction) in instructions.iter().enumerate() {
        let mut output = LineInstruction::new(Vec::new());
        if eligible[i] {
            if let ferrite_render::DrawingInstruction::Line(line) = instruction {
                output = line.clone();
                output.scale_range = Default::default();
                let mut previous = meridian;
                for point in &mut output.points {
                    let geo = position(*point)?;
                    let longitude = geo.longitude_near(previous).map_err(|e| e.to_string())?;
                    previous = longitude;
                    let uv = ferrite_kernel::geodesy::Mercator::World
                        .project(geo)
                        .map_err(|e| e.to_string())?;
                    *point = WorldPoint::new(
                        ferrite_kernel::geodesy::WGS84_A * longitude.to_radians(),
                        uv[1],
                    );
                }
            }
        }
        transformed.push(ferrite_render::DrawingInstruction::Line(output));
    }
    Ok(
        ferrite_render::LineSuppressionCache::default().plan_with_visibility(
            &transformed,
            0,
            None,
            None,
            Some(eligible),
        ),
    )
}
// Keep camera-independent Mercator paths while execution visibility changes.
// A newly revealed path or a changed dateline lift triggers a fresh preparation.
pub(crate) struct PreparedSuppression {
    transformed: Vec<ferrite_render::DrawingInstruction>,
    prepared: Vec<bool>,
    first_lifts: Vec<Option<f64>>,
    errors: Vec<Option<String>>,
    compiled: Option<ferrite_render::PreparedLineSuppression>,
    cache: ferrite_render::LineSuppressionCache,
    pub(crate) bytes: usize,
}
impl PreparedSuppression {
    pub(crate) fn new(
        instructions: &[ferrite_render::DrawingInstruction],
        eligible: &[bool],
        meridian: f64,
    ) -> Result<Option<Self>, String> {
        if instructions.len() != eligible.len() || !meridian.is_finite() {
            return Err("Invalid globe suppression input".into());
        }
        let base_bytes = instructions.len()
            * (std::mem::size_of::<ferrite_render::DrawingInstruction>()
                + std::mem::size_of::<Option<f64>>()
                + std::mem::size_of::<Option<String>>()
                + 1);
        let points = instructions
            .iter()
            .zip(eligible)
            .map(|(item, enabled)| match item {
                ferrite_render::DrawingInstruction::Line(line) if *enabled => line.points.len(),
                _ => 0,
            })
            .sum::<usize>();
        if base_bytes + points * std::mem::size_of::<WorldPoint>() > 64 * 1024 * 1024 {
            return Ok(None);
        }
        let mut transformed = Vec::with_capacity(instructions.len());
        let mut first_lifts = Vec::with_capacity(instructions.len());
        let mut errors = Vec::with_capacity(instructions.len());
        for (item, enabled) in instructions.iter().zip(eligible) {
            let mut output = LineInstruction::new(Vec::new());
            let mut first = None;
            let projected = (|| -> Result<(), String> {
                if *enabled {
                    if let ferrite_render::DrawingInstruction::Line(line) = item {
                        output.style.width = if line.style.has_visible_stroke() {
                            1.
                        } else {
                            0.
                        };
                        output.priority = line.priority;
                        output.display_plane = line.display_plane;
                        output.suppressible = line.suppressible;
                        output.points = Vec::with_capacity(line.points.len());
                        let mut previous = meridian;
                        for point in &line.points {
                            let geo = position(*point)?;
                            let longitude =
                                geo.longitude_near(previous).map_err(|e| e.to_string())?;
                            first.get_or_insert(longitude);
                            previous = longitude;
                            let uv = ferrite_kernel::geodesy::Mercator::World
                                .project(geo)
                                .map_err(|e| e.to_string())?;
                            output.points.push(WorldPoint::new(
                                ferrite_kernel::geodesy::WGS84_A * longitude.to_radians(),
                                uv[1],
                            ));
                        }
                    }
                }
                Ok(())
            })();
            errors.push(projected.err());
            first_lifts.push(first);
            transformed.push(ferrite_render::DrawingInstruction::Line(output));
        }
        let bytes = base_bytes
            + points * std::mem::size_of::<WorldPoint>()
            + errors.iter().flatten().map(|e| e.capacity()).sum::<usize>();
        if bytes > 64 * 1024 * 1024 {
            return Ok(None);
        }
        let compiled =
            ferrite_render::PreparedLineSuppression::compile(&transformed, 32 * 1024 * 1024);
        Ok(Some(Self {
            compiled,
            transformed,
            prepared: eligible.to_vec(),
            first_lifts,
            errors,
            cache: Default::default(),
            bytes,
        }))
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        self.bytes + self.compiled.as_ref().map_or(0, |c| c.retained_bytes())
    }
    pub(crate) fn matches(
        &self,
        instructions: &[ferrite_render::DrawingInstruction],
        eligible: &[bool],
        meridian: f64,
    ) -> Result<bool, String> {
        if !meridian.is_finite()
            || eligible.len() != self.prepared.len()
            || instructions.len() != eligible.len()
        {
            return Ok(false);
        }
        for (i, enabled) in eligible.iter().enumerate() {
            if !enabled {
                continue;
            }
            if !self.prepared[i] {
                return Ok(false);
            }
            if self.errors[i].is_some() {
                continue;
            }
            if let ferrite_render::DrawingInstruction::Line(line) = &instructions[i] {
                if let Some(point) = line.points.first() {
                    if Some(
                        position(*point)?
                            .longitude_near(meridian)
                            .map_err(|e| e.to_string())?,
                    ) != self.first_lifts[i]
                    {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
    pub(crate) fn plan(
        &mut self,
        eligible: &[bool],
    ) -> Result<std::sync::Arc<ferrite_render::LineSuppressionPlan>, String> {
        if eligible.len() != self.errors.len() {
            return Err("Invalid globe suppression input".into());
        }
        for (enabled, error) in eligible.iter().zip(&self.errors) {
            if *enabled {
                if let Some(error) = error {
                    return Err(error.clone());
                }
            }
        }
        if let Some(compiled) = &self.compiled {
            return compiled
                .plan(eligible)
                .map(std::sync::Arc::new)
                .map_err(str::to_string);
        }
        let plan =
            self.cache
                .plan_with_visibility(&self.transformed, 0, None, None, Some(eligible));
        let bytes = plan.fully_suppressed.capacity() * 16
            + plan.partial.capacity() * 64
            + plan
                .partial
                .values()
                .map(|v| v.capacity() * std::mem::size_of::<ferrite_render::LineSpan>())
                .sum::<usize>();
        if bytes > 16 * 1024 * 1024 {
            self.cache.clear();
        }
        Ok(plan)
    }
}
/// Geographic inline strokes with calibrated physical widths, continuous dash
/// phase, and authored caps/joins. Unsupported deferred/style-reference geometry
/// returns a diagnostic instead of substituting a solid geographic line.
pub fn drape_line(
    line: &LineInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
) -> Result<(GlobeMesh, GlobeLineStats), String> {
    drape_line_with_spans(line, camera, pixels_per_mm, None)
}
/// Visible intervals are source rhumb fractions, as returned by
/// geographic_line_suppression. Dash phase follows the full unsuppressed path.
pub fn drape_line_with_spans(
    line: &LineInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
    visible: Option<&[ferrite_render::LineSpan]>,
) -> Result<(GlobeMesh, GlobeLineStats), String> {
    drape_line_with_spans_and_culling(line, camera, pixels_per_mm, visible, true)
}
/// Differential rendering and hosts can disable whole-path culling. No segment
/// is individually discarded: visible runs retain the full component dash phase.
pub fn drape_line_with_spans_and_culling(
    line: &LineInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
    visible: Option<&[ferrite_render::LineSpan]>,
    frustum_culling: bool,
) -> Result<(GlobeMesh, GlobeLineStats), String> {
    drape_line_prepared(
        line,
        camera,
        pixels_per_mm,
        visible,
        frustum_culling,
        None,
        true,
    )
}
pub(crate) fn drape_line_prepared(
    line: &LineInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
    visible: Option<&[ferrite_render::LineSpan]>,
    frustum_culling: bool,
    prepared: Option<&crate::globe_curve_clip::PreparedCurve>,
    cache_surface_bounds: bool,
) -> Result<(GlobeMesh, GlobeLineStats), String> {
    drape_line_prepared_reusing(
        line,
        camera,
        pixels_per_mm,
        visible,
        frustum_culling,
        prepared,
        cache_surface_bounds,
        false,
    )
}
pub(crate) fn drape_line_prepared_reusing(
    line: &LineInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
    visible: Option<&[ferrite_render::LineSpan]>,
    frustum_culling: bool,
    prepared: Option<&crate::globe_curve_clip::PreparedCurve>,
    cache_surface_bounds: bool,
    reuse_scratch: bool,
) -> Result<(GlobeMesh, GlobeLineStats), String> {
    let mut mesh = GlobeMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    let mut stats = GlobeLineStats::default();
    if !pixels_per_mm.is_finite() || pixels_per_mm <= 0. {
        return Err("Invalid globe display calibration".into());
    }
    if matches!(line.portrayal_path.as_ref(), Some(ferrite_render::PortrayalPath::GeographicArc { .. })) {
        if visible.is_some() {
            return Err("Geographic arc suppression needs authored curve parameters".into());
        }
        let generated = crate::globe_geographic_arc::materialize(line, camera)?
            .expect("matched geographic arc");
        // The generated curve belongs to this camera. A cached source route for
        // the original instruction (whose points contain only its centre) cannot
        // stand in for it. All ordinary line operations below remain unchanged.
        return drape_line_prepared_reusing(&generated, camera, pixels_per_mm,
            None, frustum_culling, None, false, reuse_scratch);
    }
    if line.style_ref.is_some() || line.screen_ray.is_some() || line.portrayal_path.is_some() {
        return Err("Deferred/local/reference line needs its projection adapter".into());
    }
    if !line.style.has_visible_stroke() || line.points.len() < 2 {
        return Ok((mesh, stats));
    }
    let color = line.style.color.to_array();
    if color
        .iter()
        .any(|x| !x.is_finite() || !(0. ..=1.).contains(x))
    {
        return Err("Invalid globe stroke color".into());
    }
    if color[3] != 1. {
        return Err("Translucent stroke requires per-path overlap coverage".into());
    }
    if line.points.len() > 262144 {
        return Err("Clipped curve aggregate sample budget exceeded".into());
    }
    if let Some(spans) = visible {
        if spans.len() > 262144 {
            return Err("Globe suppression span budget exceeded".into());
        }
        for span in spans {
            if !span.start.is_finite()
                || !span.end.is_finite()
                || span.start < 0.
                || span.end > 1.
                || span.start >= span.end
                || span
                    .segment
                    .checked_add(1)
                    .is_none_or(|i| i >= line.points.len())
            {
                return Err("Invalid globe visible span".into());
            }
        }
    }
    let offset_px = line.style.offset_mm * pixels_per_mm;
    if !offset_px.is_finite() || offset_px.abs() > 4096. {
        return Err("Invalid physical line offset".into());
    }
    // Use the canonical dash validator even for an invisible path, so culling
    // does not mask invalid style data. This empty validation path emits nothing.
    dash_projected_line_spans_clipped(&[], pixels_per_mm, &line.style, &[], 0.)?;
    let width = line.style.physical_width(pixels_per_mm as f32) as f64;
    let sphere = if let Some(source) = prepared.filter(|_| cache_surface_bounds) {
        source.surface_sphere()
    } else {
        crate::globe_curve_clip::geographic_curve_sphere(&line.points)?
    };
    if frustum_culling && offset_px == 0. && width > 0. && width <= 4096. {
        // Polar-meridian handling retains the existing path partition adapter.
        if let Some((centre, radius)) = sphere {
            // Same conservative guard as screen ROI: miter limit4, caps,
            // width rounding and antialiasing fit within four widths +2px.
            if camera
                .sphere_outside_frustum(centre, radius, width * 4. + 2.)
                .map_err(|e| e.to_string())?
            {
                stats.frustum_culled = true;
                return Ok((mesh, stats));
            }
        }
    }
    let horizon = ferrite_kernel::globe_visibility::SurfaceHorizon::from_eye(camera.eye_m())
        .map_err(|e| e.to_string())?;
    let tolerance = horizon.numerical_tolerance_m();
    let plane_bound = sphere
        .map(|(centre, radius)| {
            horizon
                .signed_distance_m(centre)
                .map(|d| (d - radius, d + radius))
        })
        .transpose()
        .map_err(|e| e.to_string())?;
    if plane_bound.is_some_and(|(_, max)| max < -tolerance) {
        stats.horizon_culled = true;
        return Ok((mesh, stats));
    }
    // Full visible bounds keep the common path free of new interval/sample
    // allocations. A horizon intersection needs exact authored source cuts.
    let mut horizon_spans = None;
    let mut horizon_cuts = std::collections::HashMap::<usize, Vec<f64>>::new();
    if !plane_bound.is_some_and(|(min, _)| min >= tolerance) {
        let mut spans = Vec::new();
        let mut work = 0;
        for (segment, pair) in line.points.windows(2).enumerate() {
            let route = RhumbSegment::new(position(pair[0])?, position(pair[1])?)
                .map_err(|e| e.to_string())?;
            let (intervals, n) = horizon
                .rhumb_intervals(route, 262144usize.saturating_sub(work))
                .map_err(|e| e.to_string())?;
            work += n;
            for interval in intervals {
                if spans.len() >= 262144 {
                    return Err("Globe horizon span budget exceeded".into());
                }
                spans.push(ferrite_render::LineSpan {
                    segment,
                    start: interval.start,
                    end: interval.end,
                });
                for t in [interval.start, interval.end] {
                    if t > 0. && t < 1. {
                        horizon_cuts.entry(segment).or_default().push(t);
                    }
                }
            }
        }
        for cuts in horizon_cuts.values_mut() {
            cuts.sort_by(f64::total_cmp);
            cuts.dedup();
        }
        horizon_spans = Some(spans);
    }
    let combined_spans = if let Some(horizon_spans) = &horizon_spans {
        if let Some(visible) = visible {
            let mut by_segment = std::collections::HashMap::<usize, Vec<_>>::new();
            for span in visible {
                by_segment.entry(span.segment).or_default().push(span);
            }
            let mut result = Vec::new();
            for h in horizon_spans {
                if let Some(source) = by_segment.get(&h.segment) {
                    for s in source {
                        let start = h.start.max(s.start);
                        let end = h.end.min(s.end);
                        if start < end {
                            if result.len() >= 262144 {
                                return Err("Globe combined visibility budget exceeded".into());
                            }
                            result.push(ferrite_render::LineSpan {
                                segment: h.segment,
                                start,
                                end,
                            });
                        }
                    }
                }
            }
            Some(result)
        } else {
            None
        }
    } else {
        None
    };
    let visible = combined_spans
        .as_deref()
        .or(horizon_spans.as_deref())
        .or(visible);
    let phase_accuracy = line.style.dash_cycle.is_some() || !line.style.dash_pattern.is_empty();
    let (mut components, refinements) =
        crate::globe_curve_clip::project_rhumb_components_prepared_reusing(
            &line.points,
            camera,
            0.25,
            5.,
            phase_accuracy,
            262144,
            prepared,
            reuse_scratch,
        )?;
    // Insert true WGS84 root positions into the *full* forward-depth path.
    // Hidden prefixes remain for dash phase; source suppression is intersected
    // after projection rather than restarting the dash at each horizon cut.
    let mut total_samples = components.iter().map(|r| r.samples.len()).sum::<usize>();
    if !horizon_cuts.is_empty() {
        for component in &mut components {
            let mut samples = Vec::with_capacity(component.samples.len());
            for pair in component.samples.windows(2) {
                if samples.is_empty() {
                    samples.push(pair[0].clone());
                }
                let segment = pair[1].source_segment;
                let start = if pair[0].source_segment == segment {
                    pair[0].source_fraction
                } else {
                    0.
                };
                if let Some(cuts) = horizon_cuts.get(&segment) {
                    for t in cuts {
                        if *t > start && *t < pair[1].source_fraction {
                            if total_samples >= 262144 {
                                return Err("Globe horizon sample budget exceeded".into());
                            }
                            let route = RhumbSegment::new(
                                position(line.points[segment])?,
                                position(line.points[segment + 1])?,
                            )
                            .map_err(|e| e.to_string())?;
                            samples.push(sample(
                                route.point(*t).map_err(|e| e.to_string())?,
                                camera,
                                segment,
                                *t,
                            )?);
                            total_samples += 1;
                        }
                    }
                }
                samples.push(pair[1].clone());
            }
            component.samples = samples;
        }
    }
    stats.samples = total_samples;
    stats.refinements = refinements;
    for component in components {
        let samples = component.samples;
        let screen: Vec<_> = samples.iter().map(|s| s.screen_px).collect();
        let offsets = if offset_px == 0. {
            None
        } else {
            Some(
                ferrite_kernel::line_offset::screen_line_offsets(
                    &screen,
                    offset_px,
                    screen.len() > 2 && screen.first() == screen.last(),
                )
                .map_err(str::to_string)?,
            )
        };
        let shift_at = |segment: usize, fraction: f64| -> [f64; 2] {
            offsets
                .as_ref()
                .map(|o| {
                    std::array::from_fn(|axis| {
                        o[segment][axis] + fraction * (o[segment + 1][axis] - o[segment][axis])
                    })
                })
                .unwrap_or([0., 0.])
        };
        let projected_visible = if let Some(spans) = visible {
            let mut by_segment: std::collections::HashMap<usize, Vec<_>> =
                std::collections::HashMap::new();
            for span in spans {
                by_segment.entry(span.segment).or_default().push(*span);
            }
            let mut result = Vec::new();
            for (segment, pair) in samples.windows(2).enumerate() {
                let end = &pair[1];
                let start = if pair[0].source_segment == end.source_segment {
                    pair[0].source_fraction
                } else {
                    0.
                };
                let length = end.source_fraction - start;
                if length <= 0. {
                    continue;
                }
                if let Some(spans) = by_segment.get(&end.source_segment) {
                    for s in spans {
                        let a = s.start.max(start);
                        let b = s.end.min(end.source_fraction);
                        if b > a {
                            let screen_fraction = |t: f64| {
                                // Preserve authored subdivision boundaries exactly,
                                // rather than reconstructing them by depth arithmetic.
                                if t == start {
                                    return 0.;
                                }
                                if t == end.source_fraction {
                                    return 1.;
                                }
                                let t = (t - start) / length;
                                t * end.forward_depth_m
                                    / ((1. - t) * pair[0].forward_depth_m + t * end.forward_depth_m)
                            };
                            if result.len() >= 262144 {
                                return Err("Globe projected suppression budget exceeded".into());
                            }
                            result.push(ferrite_render::LineSpan {
                                segment,
                                start: screen_fraction(a),
                                end: screen_fraction(b),
                            });
                        }
                    }
                }
            }
            Some(result)
        } else {
            None
        };
        let width = line.style.physical_width(pixels_per_mm as f32) as f64;
        let margin = width * 4. + offset_px.abs() * 4. + 2.;
        let viewport = camera.viewport();
        // Restrict tessellation/dash emission to the viewport guard, while keeping
        // full forward-component arc lengths for phase and source parameter mapping.
        let mut roi = Vec::new();
        let mut by_segment =
            std::collections::HashMap::<usize, Vec<ferrite_render::LineSpan>>::new();
        if let Some(visible) = &projected_visible {
            for s in visible {
                by_segment.entry(s.segment).or_default().push(*s);
            }
        }
        for (segment, pair) in screen.windows(2).enumerate() {
            if let Some((start, end)) = clip_screen_segment(
                pair[0],
                pair[1],
                [-margin, -margin, viewport[0] + margin, viewport[1] + margin],
            ) {
                if projected_visible.is_some() {
                    if let Some(spans) = by_segment.get(&segment) {
                        for s in spans {
                            let a = start.max(s.start);
                            let b = end.min(s.end);
                            if b > a {
                                roi.push(ferrite_render::LineSpan {
                                    segment,
                                    start: a,
                                    end: b,
                                });
                            }
                        }
                    }
                } else {
                    roi.push(ferrite_render::LineSpan {
                        segment,
                        start,
                        end,
                    });
                }
            }
        }
        let spans = dash_projected_line_spans_clipped(
            &screen,
            pixels_per_mm,
            &line.style,
            &roi,
            component.initial_phase_px,
        )?
        .or(Some(roi));
        let mut runs: Vec<Vec<ProjectedLineSample>> = Vec::new();
        let mut run_offsets: Option<Vec<Vec<[f64; 2]>>> = offsets.as_ref().map(|_| Vec::new());
        if let Some(spans) = spans {
            let mut previous_span: Option<ferrite_render::LineSpan> = None;
            for s in spans {
                let a = interpolate(&samples[s.segment], &samples[s.segment + 1], s.start);
                let b = interpolate(&samples[s.segment], &samples[s.segment + 1], s.end);
                let connected = previous_span.is_some_and(|p| {
                    (p.segment == s.segment && p.end == s.start)
                        || (p.segment + 1 == s.segment && p.end == 1. && s.start == 0.)
                });
                if let Some(run) = runs.last_mut().filter(|_| connected) {
                    run.push(b);
                    if let Some(shifts) = &mut run_offsets {
                        shifts.last_mut().unwrap().push(shift_at(s.segment, s.end));
                    }
                } else {
                    runs.push(vec![a, b]);
                    if let Some(shifts) = &mut run_offsets {
                        shifts.push(vec![
                            shift_at(s.segment, s.start),
                            shift_at(s.segment, s.end),
                        ]);
                    }
                }
                previous_span = Some(s);
            }
        } else {
            if let Some(shifts) = &mut run_offsets {
                shifts.push(offsets.clone().unwrap());
            }
            runs.push(samples);
        }
        let width = line.style.physical_width(pixels_per_mm as f32) as f64;
        for (run_index, mut run) in runs.into_iter().enumerate() {
            let v = camera.viewport();
            let margin = width * 4. + offset_px.abs() * 4. + 2.;
            if (0..2).any(|axis| {
                run.iter().all(|s| s.screen_px[axis] < -margin)
                    || run.iter().all(|s| s.screen_px[axis] > v[axis] + margin)
            }) {
                continue;
            }
            let closed =
                run.len() > 2 && run.first().unwrap().screen_px == run.last().unwrap().screen_px;
            if closed {
                run.pop();
                if let Some(shifts) = &mut run_offsets {
                    shifts[run_index].pop();
                }
            }
            let mut points: Vec<_> = run.iter().map(|s| s.screen_px).collect();
            if let Some(shifts) = &run_offsets {
                for (point, shift) in points.iter_mut().zip(&shifts[run_index]) {
                    point[0] += shift[0];
                    point[1] += shift[1];
                }
            }
            let geometry = stroke_screen_path(
                &points,
                width,
                line.style.cap,
                line.style.join,
                closed,
                262144 - mesh.vertices.len(),
            )?;
            let base = mesh.vertices.len() as u32;
            stats.runs += 1;
            for p in geometry.vertices {
                let (a, b, f) = p.source;
                let anchor = interpolate(&run[a], &run[b], f);
                // Use the f64 projected anchor rather than the tessellator's rounded point.
                let offset = [
                    p.position[0] - anchor.screen_px[0],
                    p.position[1] - anchor.screen_px[1],
                ];
                let ecef_m = camera
                    .offset_pixels(anchor.ecef_m, offset)
                    .map_err(|e| e.to_string())?;
                mesh.vertices.push(GlobeVertex { ecef_m, color });
            }
            mesh.indices
                .extend(geometry.indices.into_iter().map(|i| i + base));
        }
    } // projected depth component
    stats.triangles = mesh.indices.len() / 3;
    mesh.validate()?;
    Ok((mesh, stats))
}
/// Liang-Barsky clipping in finite screen coordinates, after near/far partition.
fn clip_screen_segment(a: [f64; 2], b: [f64; 2], rect: [f64; 4]) -> Option<(f64, f64)> {
    let mut start: f64 = 0.;
    let mut end: f64 = 1.;
    for axis in 0..2 {
        let delta = b[axis] - a[axis];
        if delta == 0. {
            if a[axis] < rect[axis] || a[axis] > rect[axis + 2] {
                return None;
            }
            continue;
        }
        let p = (rect[axis] - a[axis]) / delta;
        let q = (rect[axis + 2] - a[axis]) / delta;
        start = start.max(p.min(q));
        end = end.min(p.max(q));
        if start >= end {
            return None;
        }
    }
    Some((start, end))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn horizon_clipping_intersects_source_suppression_without_bridging_the_gap() {
        use ferrite_render::{Color, LineSpan, LineStyle};
        let camera = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            20_000_000.,
            0.,
            0.,
            [640., 480.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let line = LineInstruction::new(vec![
            WorldPoint::new(-100., 0.),
            WorldPoint::new(0., 0.),
            WorldPoint::new(100., 0.),
        ])
        .with_style(LineStyle::solid_mm(Color::RED, 1.6));
        let spans = [
            LineSpan {
                segment: 0,
                start: 0.,
                end: 0.9,
            },
            LineSpan {
                segment: 1,
                start: 0.1,
                end: 1.,
            },
        ];
        let (mesh, stats) =
            drape_line_with_spans(&line, &camera, 3.779527559055118, Some(&spans)).unwrap();
        assert!(!mesh.indices.is_empty() && stats.runs == 2);
        let project = |ecef| {
            let c = camera.clip_ecef(ecef).unwrap();
            [(c[0] / c[3] + 1.) * 320., (1. - c[1] / c[3]) * 240.]
        };
        let limit = |lon| {
            project(
                GeographicPosition::new(0., lon)
                    .unwrap()
                    .to_ecef(0.)
                    .unwrap(),
            )[0]
        };
        let left = limit(-10.);
        let right = limit(10.);
        for triangle in mesh.indices.chunks_exact(3) {
            let x: Vec<_> = triangle
                .iter()
                .map(|i| project(mesh.vertices[*i as usize].ecef_m)[0])
                .collect();
            assert!(x.iter().all(|v| *v <= left + 1e-4) || x.iter().all(|v| *v >= right - 1e-4));
        }
        assert!(
            drape_line_with_spans(&line, &camera, 3.779527559055118, Some(&[]))
                .unwrap()
                .0
                .indices
                .is_empty()
        );
        let hidden =
            LineInstruction::new(vec![WorldPoint::new(160., 0.), WorldPoint::new(170., 0.)])
                .with_style(LineStyle::solid_mm(Color::RED, 1.6));
        let (mesh, stats) = drape_line(&hidden, &camera, 3.779527559055118).unwrap();
        assert!(mesh.indices.is_empty() && stats.horizon_culled);
    }
    #[test]
    fn retained_surface_bounds_preserve_culling_strokes_and_dash_phase() {
        for points in [
            vec![WorldPoint::new(-0.1, 0.), WorldPoint::new(0.1, 0.)],
            vec![WorldPoint::new(50., 20.), WorldPoint::new(50.01, 20.01)],
            vec![WorldPoint::new(179.9, 70.), WorldPoint::new(-179.9, 70.01)],
            vec![WorldPoint::new(30., 89.9), WorldPoint::new(30., 90.)],
        ] {
            let prepared = crate::globe_curve_clip::PreparedCurve::new(&points).unwrap();
            assert_eq!(
                prepared.surface_sphere(),
                crate::globe_curve_clip::geographic_curve_sphere(&points).unwrap()
            );
            for (lat, lon) in [(0., 0.), (70., 180.), (89.9, 30.)] {
                let camera = GlobeCamera::orbit(
                    GeographicPosition::new(lat, lon).unwrap(),
                    30000.,
                    0.,
                    0.,
                    [900., 600.],
                    45.,
                    1.,
                    1e8,
                )
                .unwrap();
                for dashed in [false, true] {
                    let mut line = LineInstruction::new(points.clone());
                    if dashed {
                        line.style.dash_pattern = vec![1., 1.];
                    }
                    let a = drape_line_prepared(&line, &camera, 96. / 25.4, None, true, None, true)
                        .unwrap();
                    let b = drape_line_prepared(
                        &line,
                        &camera,
                        96. / 25.4,
                        None,
                        true,
                        Some(&prepared),
                        true,
                    )
                    .unwrap();
                    assert_eq!(a.0.indices, b.0.indices);
                    assert_eq!(a.1.frustum_culled, b.1.frustum_culled);
                    assert_eq!(a.0.vertices.len(), b.0.vertices.len());
                    for (x, y) in a.0.vertices.iter().zip(&b.0.vertices) {
                        assert_eq!(x.ecef_m, y.ecef_m);
                        assert_eq!(x.color, y.color);
                    }
                }
            }
        }
    }

    #[test]
    fn whole_path_culling_keeps_invalid_data_visible_as_errors() {
        let camera = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            30000.,
            0.,
            0.,
            [900., 600.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let line = LineInstruction::new(vec![
            WorldPoint::new(50., 20.),
            WorldPoint::new(50.01, 20.01),
        ]);
        let span = ferrite_render::LineSpan {
            segment: usize::MAX,
            start: 0.,
            end: 1.,
        };
        for enabled in [false, true] {
            assert!(drape_line_with_spans_and_culling(
                &line,
                &camera,
                96. / 25.4,
                Some(&[span]),
                enabled
            )
            .is_err());
            let mut bad = line.clone();
            bad.style.dash_pattern = vec![1.];
            assert!(
                drape_line_with_spans_and_culling(&bad, &camera, 96. / 25.4, None, enabled)
                    .is_err()
            );
            bad = line.clone();
            bad.points[1].y = f64::NAN;
            assert!(
                drape_line_with_spans_and_culling(&bad, &camera, 96. / 25.4, None, enabled)
                    .is_err()
            );
        }
    }

    #[test]
    fn physical_offset_keeps_dash_phase_and_projects_left_at_multiple_scales() {
        for altitude in [30000., 60000.] {
            let camera = GlobeCamera::orbit(
                GeographicPosition::new(0., 0.).unwrap(),
                altitude,
                0.,
                0.,
                [900., 600.],
                45.,
                1.,
                1e8,
            )
            .unwrap();
            for ppm in [96. / 25.4, 192. / 25.4, 144. / 25.4] {
                for dashed in [false, true] {
                    let mut line = LineInstruction::new(vec![
                        WorldPoint::new(-0.01, 0.),
                        WorldPoint::new(0.01, 0.),
                    ]);
                    line.style =
                        ferrite_render::LineStyle::solid_mm(ferrite_render::Color::default(), 0.32);
                    if dashed {
                        line.style.dash_cycle =
                            Some(ferrite_kernel::DashCycle::new(8., [(1., 4.)]).unwrap());
                    }
                    let original = line.points.clone();
                    let base = drape_line(&line, &camera, ppm).unwrap();
                    let extent = |mesh: &GlobeMesh| {
                        let mut lo = f64::INFINITY;
                        let mut hi = f64::NEG_INFINITY;
                        for vertex in &mesh.vertices {
                            let clip = camera.clip_ecef(vertex.ecef_m).unwrap();
                            let y = (1. - clip[1] / clip[3]) * 300.;
                            lo = lo.min(y);
                            hi = hi.max(y);
                        }
                        (lo, hi)
                    };
                    let before = extent(&base.0);
                    for offset in [-2.43, -1., 1.74, 2.43] {
                        line.style.offset_mm = offset;
                        let shifted = drape_line(&line, &camera, ppm).unwrap();
                        let after = extent(&shifted.0);
                        assert_eq!(base.0.indices, shifted.0.indices);
                        assert_eq!(base.1.runs, shifted.1.runs);
                        assert!(
                            ((after.0 + after.1 - before.0 - before.1) / 2. + offset * ppm).abs()
                                < 0.002
                        );
                        assert!((after.1 - after.0 - before.1 + before.0).abs() < 0.002);
                        assert_eq!(line.points, original);
                    }
                }
            }
        }
    }
    #[test]
    fn dateline_rhumb_stroke_preserves_geography_and_physical_width() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(70., 180.).unwrap(),
            300000.,
            90.,
            60.,
            [1000., 800.],
            45.,
            1.,
            1e9,
        )
        .unwrap();
        let mut l = LineInstruction::new(vec![
            WorldPoint::new(179., 70.),
            WorldPoint::new(-179., 70.),
        ]);
        l.style.width = 10.;
        let (samples, _) = project_rhumb_line(&l.points, &c, 0.25, 5., 262144).unwrap();
        assert!(samples.len() > 2);
        for s in &samples {
            let p = ferrite_kernel::geocentric::from_ecef(s.ecef_m).unwrap();
            assert!((p.surface.latitude() - 70.).abs() < 1e-10);
        }
        let (m, stats) = drape_line(&l, &c, 4.).unwrap();
        assert!(stats.triangles > 0);
        let expected = crate::screen_stroke::stroke_screen_path(
            &samples.iter().map(|p| p.screen_px).collect::<Vec<_>>(),
            10.,
            l.style.cap,
            l.style.join,
            false,
            262144,
        )
        .unwrap();
        assert_eq!(expected.vertices.len(), m.vertices.len());
        for (v, w) in m.vertices.iter().zip(expected.vertices) {
            let clip = c.clip_ecef(v.ecef_m).unwrap();
            assert!(clip[3] > 0.);
            let screen = [
                (clip[0] / clip[3] + 1.) * 500.,
                (1. - clip[1] / clip[3]) * 400.,
            ];
            assert!((screen[0] - w.position[0]).hypot(screen[1] - w.position[1]) < 1e-6);
        }

        l.style.dash_pattern = vec![10., 10.];
        let (_, d) = drape_line(&l, &c, 4.).unwrap();
        assert!(d.runs > 1);
    }
}

#[cfg(test)]
mod suppression_tests {
    use super::*;
    use ferrite_render::{DisplayPriority, DrawingInstruction};
    #[test]
    fn higher_rhumb_half_hides_only_coincident_interval_without_resetting_dash() {
        let route = RhumbSegment::new(
            GeographicPosition::new(60., 179.).unwrap(),
            GeographicPosition::new(65., -179.).unwrap(),
        )
        .unwrap();
        let middle = route.point(0.5).unwrap();
        let mut low = LineInstruction::new(vec![
            WorldPoint::new(179., 60.),
            WorldPoint::new(middle.longitude(), middle.latitude()),
            WorldPoint::new(-179., 65.),
        ]);
        low.priority = DisplayPriority(1);
        low.style.dash_pattern = vec![20., 10.];
        let mut high = LineInstruction::new(vec![
            WorldPoint::new(middle.longitude(), middle.latitude()),
            WorldPoint::new(-179., 65.),
        ]);
        high.priority = DisplayPriority(2);
        let instructions = vec![
            DrawingInstruction::Line(low.clone()),
            DrawingInstruction::Line(high),
        ];
        let plan = geographic_line_suppression(&instructions, &[true, true], 180.).unwrap();
        let spans = plan.spans(0).unwrap();
        assert_eq!(spans.len(), 1);
        assert!(spans[0].segment == 0 && spans[0].start == 0. && spans[0].end == 1.);
        let mut parallel_low = LineInstruction::new(vec![
            WorldPoint::new(179., 70.),
            WorldPoint::new(-179., 70.),
        ]);
        parallel_low.priority = DisplayPriority(1);
        let mut parallel_high = LineInstruction::new(vec![
            WorldPoint::new(180., 70.),
            WorldPoint::new(-179., 70.),
        ]);
        parallel_high.priority = DisplayPriority(2);
        let partial = geographic_line_suppression(
            &[
                DrawingInstruction::Line(parallel_low),
                DrawingInstruction::Line(parallel_high),
            ],
            &[true, true],
            180.,
        )
        .unwrap();
        assert!((partial.spans(0).unwrap()[0].end - 0.5).abs() < 1e-12);
        let visible = geographic_line_suppression(&instructions, &[true, false], 180.).unwrap();
        assert!(!visible.contains(&0) && visible.spans(0).is_none());
        let c = GlobeCamera::orbit(
            GeographicPosition::new(62.5, 180.).unwrap(),
            1000000.,
            90.,
            45.,
            [1000., 800.],
            45.,
            1.,
            1e9,
        )
        .unwrap();
        let (all, _) = drape_line(&low, &c, 4.).unwrap();
        let (half, _) = drape_line_with_spans(&low, &c, 4., Some(spans)).unwrap();
        assert!(!half.indices.is_empty() && half.indices.len() < all.indices.len());
    }
}

#[cfg(test)]
mod prepared_suppression_tests {
    use super::*;
    use ferrite_render::{DisplayPriority, DrawingInstruction};
    #[test]
    fn prepared_paths_match_cold_visibility_and_dateline_plans() {
        let mut low = LineInstruction::new(vec![
            WorldPoint::new(179., 60.),
            WorldPoint::new(-179., 60.),
        ]);
        low.priority = DisplayPriority(1);
        let mut high = low.clone();
        high.priority = DisplayPriority(5);
        let mut other = high.clone();
        other.points[0].y = 61.;
        other.points[1].y = 61.;
        let items = vec![
            DrawingInstruction::Line(low),
            DrawingInstruction::Line(high),
            DrawingInstruction::Line(other),
        ];
        let mut p = PreparedSuppression::new(&items, &[true, true, false], 180.)
            .unwrap()
            .unwrap();
        for eligible in [
            [true, true, false],
            [true, false, false],
            [false, true, false],
        ] {
            assert!(p.matches(&items, &eligible, 179.).unwrap());
            assert_eq!(
                *p.plan(&eligible).unwrap(),
                *geographic_line_suppression(&items, &eligible, 179.).unwrap()
            );
        }
        assert!(!p.matches(&items, &[true, true, true], 180.).unwrap());
        assert!(!p.matches(&items, &[true, true, false], -179.).unwrap());
    }
}

#[cfg(test)]
mod prepared_error_tests {
    use super::*;
    use ferrite_render::DrawingInstruction;
    #[test]
    fn hidden_bad_source_does_not_fail_until_it_executes() {
        let items = vec![DrawingInstruction::Line(LineInstruction::new(vec![
            WorldPoint::new(0., 91.),
            WorldPoint::new(1., 91.),
        ]))];
        let mut prepared = PreparedSuppression::new(&items, &[true], 0.)
            .unwrap()
            .unwrap();
        assert_eq!(
            *prepared.plan(&[false]).unwrap(),
            *geographic_line_suppression(&items, &[false], 0.).unwrap()
        );
        assert_eq!(
            prepared.plan(&[true]).unwrap_err(),
            geographic_line_suppression(&items, &[true], 0.).unwrap_err()
        );
    }
}
