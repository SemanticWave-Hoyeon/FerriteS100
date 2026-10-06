//! Resolve S-100 physical line placement on a perspective WGS84 curve.
use crate::{
    globe_curve_clip::{
        project_rhumb_components, project_rhumb_components_prepared, PreparedCurve,
    },
    globe_scene::GlobeMesh,
};
use ferrite_kernel::globe_camera::GlobeCamera;
use ferrite_render::{
    placed_curve_points, sample_curve_position, CurveSample, LineSymbolPlacement, PointInstruction,
};
const PLACEMENT_ROUTE_TEMP_LIMIT: usize = 16 * 1024 * 1024;
fn sample_position(
    placement: &LineSymbolPlacement,
    prepared: Option<&PreparedCurve>,
    s: CurveSample,
) -> Result<(ferrite_render::WorldPoint, f64), String> {
    match prepared {
        Some(curve) => curve.sample_source_position(&placement.points, s),
        None => sample_curve_position(&placement.points, s),
    }
}
fn sample_world(
    placement: &LineSymbolPlacement,
    prepared: Option<&PreparedCurve>,
    s: CurveSample,
) -> Result<ferrite_render::WorldPoint, String> {
    // Bearing is unused during projection/visibility. Compute it only for an
    // actually placed symbol; it has no side effects and cannot return errors.
    match prepared {
        Some(curve) => curve.sample_source_world(&placement.points, s),
        None => sample_curve_position(&placement.points, s).map(|p| p.0),
    }
}
fn at(
    placement: &LineSymbolPlacement,
    camera: &GlobeCamera,
    prepared: Option<&PreparedCurve>,
    s: CurveSample,
) -> Result<CurveSample, String> {
    let p = sample_world(placement, prepared, s)?;
    let g =
        ferrite_kernel::geodesy::GeographicPosition::new(p.y, (p.x + 180.).rem_euclid(360.) - 180.)
            .map_err(|e| e.to_string())?;
    let q = camera
        .clip_ecef(g.to_ecef(0.).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if q[3] <= 0. {
        return Err("Line placement crossed camera eye plane".into());
    }
    let v = camera.viewport();
    Ok(CurveSample {
        screen: [
            (q[0] / q[3] + 1.) * v[0] / 2.,
            (1. - q[1] / q[3]) * v[1] / 2.,
        ],
        ..s
    })
}
fn visible(
    placement: &LineSymbolPlacement,
    camera: &GlobeCamera,
    prepared: Option<&PreparedCurve>,
    s: CurveSample,
) -> Result<bool, String> {
    let p = sample_world(placement, prepared, s)?;
    let g =
        ferrite_kernel::geodesy::GeographicPosition::new(p.y, (p.x + 180.).rem_euclid(360.) - 180.)
            .map_err(|e| e.to_string())?;
    Ok(camera
        .project_visible(g.to_ecef(0.).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?
        .is_some())
}
fn interval(a: CurveSample, b: CurveSample) -> Result<(CurveSample, CurveSample), String> {
    if a.segment == b.segment {
        Ok((a, b))
    } else if b.segment == a.segment + 1 && a.fraction == 1. {
        Ok((
            CurveSample {
                segment: b.segment,
                fraction: 0.,
                ..a
            },
            b,
        ))
    } else {
        Err("Invalid globe placement continuity".into())
    }
}
/// Include Earth visibility before viewport clipping. Transition roots stay on
/// the source rhumb curve, rather than interpolating a 3D chord through Earth.
fn visible_components(
    placement: &LineSymbolPlacement,
    camera: &GlobeCamera,
    prepared: Option<&PreparedCurve>,
    components: &[Vec<CurveSample>],
) -> Result<Vec<Vec<CurveSample>>, String> {
    let mut out: Vec<Vec<CurveSample>> = Vec::new();
    let mut total = 0;
    for path in components {
        let mut connected = false;
        for pair in path.windows(2) {
            let (a, b) = interval(pair[0], pair[1])?;
            let m = at(
                placement,
                camera,
                prepared,
                CurveSample {
                    fraction: (a.fraction + b.fraction) / 2.,
                    ..a
                },
            )?;
            for (mut a, mut b) in [(a, m), (m, b)] {
                let va = visible(placement, camera, prepared, a)?;
                let vb = visible(placement, camera, prepared, b)?;
                if !va && !vb {
                    connected = false;
                    continue;
                }
                if va != vb {
                    let mut lo = a.fraction;
                    let mut hi = b.fraction;
                    for _ in 0..60 {
                        let mid = (lo + hi) / 2.;
                        if mid == lo || mid == hi {
                            break;
                        }
                        if visible(
                            placement,
                            camera,
                            prepared,
                            CurveSample { fraction: mid, ..a },
                        )? == va
                        {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    let root = at(
                        placement,
                        camera,
                        prepared,
                        CurveSample {
                            fraction: if va { lo } else { hi },
                            ..a
                        },
                    )?;
                    if va {
                        b = root;
                    } else {
                        a = root;
                        connected = false;
                    }
                }
                if connected {
                    out.last_mut().unwrap().push(b);
                } else {
                    out.push(vec![a, b]);
                }
                connected = vb;
                total += 2;
                if total > 524288 || out.len() > 4096 {
                    return Err("Globe visible curve placement budget exceeded".into());
                }
            }
        }
    }
    Ok(out)
}
pub fn resolve_globe_line_symbol(
    point: &PointInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
) -> Result<Vec<PointInstruction>, String> {
    resolve_globe_line_symbol_routes(point, camera, pixels_per_mm, false)
}
pub(crate) fn resolve_globe_line_symbol_routes(
    point: &PointInstruction,
    camera: &GlobeCamera,
    pixels_per_mm: f64,
    reuse_routes: bool,
) -> Result<Vec<PointInstruction>, String> {
    let Some(placement) = &point.line_placement else {
        return Ok(vec![point.clone()]);
    };
    placement.validate()?;
    // Allocation is bounded and temporary. Construction failures fall through
    // to the complete original path to preserve its first validation/error.
    let owned = if reuse_routes
        && PreparedCurve::estimated_bytes(placement.points.len()) <= PLACEMENT_ROUTE_TEMP_LIMIT
    {
        PreparedCurve::new(&placement.points).ok()
    } else {
        None
    };
    let prepared = owned.as_ref();
    let (sampled, _) = if prepared.is_some() {
        project_rhumb_components_prepared(
            &placement.points,
            camera,
            0.025,
            5.,
            true,
            262144,
            prepared,
        )?
    } else {
        project_rhumb_components(&placement.points, camera, 0.025, 5., true, 262144)?
    };
    let mut components: Vec<Vec<CurveSample>> = sampled
        .into_iter()
        .map(|p| {
            p.samples
                .into_iter()
                .map(|s| CurveSample {
                    screen: s.screen_px,
                    segment: s.source_segment,
                    fraction: s.source_fraction,
                })
                .collect()
        })
        .collect();
    if placement.visible_parts {
        components = visible_components(placement, camera, prepared, &components)?;
    } else if let (Some(a), Some(b)) = (
        components.first().and_then(|p| p.first()),
        components.last().and_then(|p| p.last()),
    ) {
        if a.segment != 0
            || a.fraction != 0.
            || b.segment != placement.points.len() - 2
            || b.fraction != 1.
        {
            return Err("Whole-curve placement requires an unclipped projection domain".into());
        }
    }
    let v = camera.viewport();
    placed_curve_points(placement, &components, pixels_per_mm, [0., 0., v[0], v[1]])?
        .into_iter()
        .map(|s| {
            let (position, bearing) = sample_position(placement, prepared, s)?;
            Ok(point.resolved_at(position, Some(bearing)))
        })
        .collect()
}
pub fn symbol_mesh(
    point: &PointInstruction,
    camera: &GlobeCamera,
    size: [u32; 2],
    pivot: [f32; 2],
    render_scale: f32,
    pixel_ratio: f64,
    symbol_scale: f32,
) -> Result<GlobeMesh, String> {
    symbol_mesh_routes(
        point,
        camera,
        size,
        pivot,
        render_scale,
        pixel_ratio,
        symbol_scale,
        false,
    )
}
pub(crate) fn symbol_mesh_routes(
    point: &PointInstruction,
    camera: &GlobeCamera,
    size: [u32; 2],
    pivot: [f32; 2],
    render_scale: f32,
    pixel_ratio: f64,
    symbol_scale: f32,
    reuse_routes: bool,
) -> Result<GlobeMesh, String> {
    if point.line_placement.is_none() {
        return crate::globe_billboard::symbol_quad(
            point,
            camera,
            size,
            pivot,
            render_scale,
            pixel_ratio,
            symbol_scale,
        );
    }
    let mut result = GlobeMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    for p in
        resolve_globe_line_symbol_routes(point, camera, 96. / 25.4 * pixel_ratio, reuse_routes)?
    {
        let mesh = crate::globe_billboard::symbol_quad(
            &p,
            camera,
            size,
            pivot,
            render_scale,
            pixel_ratio,
            symbol_scale,
        )?;
        let base = result.vertices.len() as u32;
        result.vertices.extend(mesh.vertices);
        result
            .indices
            .extend(mesh.indices.into_iter().map(|i| base + i));
    }
    result.validate()?;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::geodesy::GeographicPosition;
    use ferrite_render::{LinePlacementMode, WorldPoint};
    #[test]
    fn absolute_mm_uses_actual_perspective_not_flat_chart_scale() {
        for tilt in [0., 45., 70.] {
            for range in [30000., 3000., 150.] {
                for ratio in [1., 2.] {
                    let focus = GeographicPosition::new(70., 179.9).unwrap();
                    let camera =
                        GlobeCamera::orbit(focus, range, 0., tilt, [900., 600.], 45., 1., 1e8)
                            .unwrap();
                    let end = ferrite_kernel::geodesy::direct(focus, 90., range / 5.).unwrap();
                    let p = PointInstruction::new("A".into(), WorldPoint::new(179.9, 70.))
                        .with_line_placement(LineSymbolPlacement {
                            points: vec![
                                WorldPoint::new(179.9, 70.),
                                WorldPoint::new(end.longitude_near(179.9).unwrap(), end.latitude()),
                            ]
                            .into_boxed_slice(),
                            mode: LinePlacementMode::Absolute,
                            offset: 10.,
                            visible_parts: false,
                        });
                    let q = resolve_globe_line_symbol(&p, &camera, 96. / 25.4 * ratio).unwrap();
                    assert_eq!(q.len(), 1);
                    let g = GeographicPosition::new(
                        q[0].position.y,
                        (q[0].position.x + 180.).rem_euclid(360.) - 180.,
                    )
                    .unwrap();
                    let s = camera
                        .project_visible(g.to_ecef(0.).unwrap())
                        .unwrap()
                        .unwrap()
                        .screen_px;
                    assert!(
                        ((s[0] - 450.).hypot(s[1] - 300.) - 10. * 96. / 25.4 * ratio).abs() < 0.05
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod route_reuse_tests {
    use super::*;
    use ferrite_kernel::geodesy::GeographicPosition;
    use ferrite_render::{LinePlacementMode, WorldPoint};
    fn curves() -> Vec<Vec<WorldPoint>> {
        vec![
            vec![WorldPoint::new(-75., 0.), WorldPoint::new(75., 0.)],
            vec![
                WorldPoint::new(179., 70.),
                WorldPoint::new(181., 70.1),
                WorldPoint::new(182., 69.8),
            ],
            vec![WorldPoint::new(30., 89.), WorldPoint::new(30., 90.)],
            vec![WorldPoint::new(10., -70.), WorldPoint::new(12., -69.)],
        ]
    }
    fn bits(v: CurveSample) -> [u64; 4] {
        [
            v.screen[0].to_bits(),
            v.screen[1].to_bits(),
            v.segment as u64,
            v.fraction.to_bits(),
        ]
    }
    #[test]
    fn route_samples_preserve_source_interpolation_bearing_and_invalid_fraction_errors() {
        for points in curves() {
            let prepared = PreparedCurve::new(&points).unwrap();
            assert_eq!(
                prepared.bytes(),
                PreparedCurve::estimated_bytes(points.len())
            );
            for segment in 0..points.len() - 1 {
                for f in [
                    0.,
                    f64::from_bits(1),
                    0.125,
                    0.5,
                    0.875,
                    1.,
                    -0.1,
                    1.1,
                    f64::NAN,
                ] {
                    let s = CurveSample {
                        screen: [0.; 2],
                        segment,
                        fraction: f,
                    };
                    let a = sample_curve_position(&points, s);
                    let b = prepared.sample_source_position(&points, s);
                    match (a, b) {
                        (Ok((p, x)), Ok((q, y))) => assert_eq!(
                            [p.x.to_bits(), p.y.to_bits(), x.to_bits()],
                            [q.x.to_bits(), q.y.to_bits(), y.to_bits()]
                        ),
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        other => panic!("route sample changed outcome: {other:?}"),
                    }
                }
            }
        }
    }
    #[test]
    fn prepared_projection_horizon_roots_and_placement_meshes_preserve_all_bits() {
        for points in curves() {
            let prepared = PreparedCurve::new(&points).unwrap();
            let focus =
                GeographicPosition::new(points[0].y, (points[0].x + 180.).rem_euclid(360.) - 180.)
                    .unwrap();
            for (range, tilt) in [(1e6, 0.), (8e6, 70.)] {
                let camera =
                    GlobeCamera::orbit(focus, range, 45., tilt, [900., 600.], 45., 1., 1e8)
                        .unwrap();
                let a = project_rhumb_components(&points, &camera, 0.025, 5., true, 262144);
                let b = project_rhumb_components_prepared(
                    &points,
                    &camera,
                    0.025,
                    5.,
                    true,
                    262144,
                    Some(&prepared),
                );
                let components = match (a, b) {
                    (Ok((a, n)), Ok((b, m))) => {
                        assert_eq!(n, m);
                        assert_eq!(a.len(), b.len());
                        for (x, y) in a.iter().zip(&b) {
                            assert_eq!(x.initial_phase_px.to_bits(), y.initial_phase_px.to_bits());
                            assert_eq!(x.samples.len(), y.samples.len());
                            for (x, y) in x.samples.iter().zip(&y.samples) {
                                let vals = |s: &crate::globe_lines::ProjectedLineSample| {
                                    [
                                        s.ecef_m[0].to_bits(),
                                        s.ecef_m[1].to_bits(),
                                        s.ecef_m[2].to_bits(),
                                        s.screen_px[0].to_bits(),
                                        s.screen_px[1].to_bits(),
                                        s.forward_depth_m.to_bits(),
                                        s.source_segment as u64,
                                        s.source_fraction.to_bits(),
                                    ]
                                };
                                assert_eq!(vals(x), vals(y));
                            }
                        }
                        a.into_iter()
                            .map(|p| {
                                p.samples
                                    .into_iter()
                                    .map(|s| CurveSample {
                                        screen: s.screen_px,
                                        segment: s.source_segment,
                                        fraction: s.source_fraction,
                                    })
                                    .collect()
                            })
                            .collect::<Vec<Vec<_>>>()
                    }
                    (Err(a), Err(b)) => {
                        assert_eq!(a, b);
                        continue;
                    }
                    _ => panic!("projection changed outcome"),
                };
                for visible_parts in [false, true] {
                    let placement = LineSymbolPlacement {
                        points: points.clone().into_boxed_slice(),
                        mode: LinePlacementMode::Relative,
                        offset: 0.5,
                        visible_parts,
                    };
                    let a = visible_components(&placement, &camera, None, &components);
                    let b = visible_components(&placement, &camera, Some(&prepared), &components);
                    match (a, b) {
                        (Ok(a), Ok(b)) => {
                            assert_eq!(a.len(), b.len());
                            for (x, y) in a.iter().zip(&b) {
                                assert_eq!(x.len(), y.len());
                                assert_eq!(
                                    x.iter().copied().map(bits).collect::<Vec<_>>(),
                                    y.iter().copied().map(bits).collect::<Vec<_>>()
                                );
                            }
                        }
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        _ => panic!("horizon changed outcome"),
                    }
                    let point =
                        PointInstruction::new("A".into(), points[0]).with_line_placement(placement);
                    let a = resolve_globe_line_symbol_routes(&point, &camera, 96. / 25.4, false);
                    let b = resolve_globe_line_symbol_routes(&point, &camera, 96. / 25.4, true);
                    match (a, b) {
                        (Ok(a), Ok(b)) => assert_eq!(
                            serde_json::to_vec(&a).unwrap(),
                            serde_json::to_vec(&b).unwrap()
                        ),
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        _ => panic!("placement changed outcome"),
                    }
                    let a = symbol_mesh_routes(
                        &point,
                        &camera,
                        [20, 24],
                        [10., 12.],
                        1.,
                        1.,
                        1.,
                        false,
                    );
                    let b =
                        symbol_mesh_routes(&point, &camera, [20, 24], [10., 12.], 1., 1., 1., true);
                    match (a, b) {
                        (Ok(a), Ok(b)) => {
                            assert_eq!(a.indices, b.indices);
                            assert_eq!(a.vertices.len(), b.vertices.len());
                            for (x, y) in a.vertices.iter().zip(&b.vertices) {
                                assert_eq!(x.ecef_m.map(f64::to_bits), y.ecef_m.map(f64::to_bits));
                                assert_eq!(x.color.map(f32::to_bits), y.color.map(f32::to_bits));
                            }
                        }
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        _ => panic!("mesh changed outcome"),
                    }
                }
            }
        }
    }
}

struct ProjectionEntry {
    points: Box<[ferrite_render::WorldPoint]>,
    components: std::sync::Arc<Vec<Vec<CurveSample>>>,
    visible: Option<std::sync::Arc<Vec<Vec<CurveSample>>>>,
    prepared: Option<std::sync::Arc<PreparedCurve>>,
    bytes: usize,
    used: u64,
}
pub(crate) struct FrameCurveProjectionCache<'camera> {
    camera: &'camera GlobeCamera,
    reuse_routes: bool,
    pixel_ratio: f64,
    source_revision: u64,
    slots: [Option<ProjectionEntry>; 4],
    budget: usize,
    retained: usize,
    high_water: usize,
    hits: usize,
    misses: usize,
    visibility_hits: usize,
    visibility_misses: usize,
    evictions: usize,
    oversized: usize,
    clock: u64,
}
impl<'camera> FrameCurveProjectionCache<'camera> {
    pub(crate) fn new(
        camera: &'camera GlobeCamera,
        source_revision: u64,
        pixel_ratio: f64,
        budget: usize,
    ) -> Self {
        Self {
            camera,
            reuse_routes: false,
            pixel_ratio,
            source_revision,
            slots: std::array::from_fn(|_| None),
            budget,
            retained: 0,
            high_water: 0,
            hits: 0,
            misses: 0,
            visibility_hits: 0,
            visibility_misses: 0,
            evictions: 0,
            oversized: 0,
            clock: 0,
        }
    }
    pub(crate) fn with_route_reuse(mut self, enabled: bool) -> Self {
        self.reuse_routes = enabled;
        self
    }
    // Only used after project() validated the identical immutable source key.
    fn prepared_for(
        &self,
        projected: &std::sync::Arc<Vec<Vec<CurveSample>>>,
    ) -> Option<std::sync::Arc<PreparedCurve>> {
        self.slots
            .iter()
            .flatten()
            .find(|e| std::sync::Arc::ptr_eq(&e.components, projected))
            .and_then(|e| e.prepared.clone())
    }
    fn project(
        &mut self,
        points: &[ferrite_render::WorldPoint],
    ) -> Result<std::sync::Arc<Vec<Vec<CurveSample>>>, String> {
        self.clock = self.clock.saturating_add(1);
        for entry in self.slots.iter_mut().flatten() {
            if entry.points.len() == points.len()
                && entry
                    .points
                    .iter()
                    .zip(points)
                    .all(|(a, b)| a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits())
            {
                entry.used = self.clock;
                self.hits += 1;
                return Ok(entry.components.clone());
            }
        }
        self.misses += 1;
        let prepared = if self.reuse_routes
            && PreparedCurve::estimated_bytes(points.len())
                <= PLACEMENT_ROUTE_TEMP_LIMIT.saturating_sub(2 * std::mem::size_of::<usize>())
        {
            PreparedCurve::new(points).ok().map(std::sync::Arc::new)
        } else {
            None
        };
        let (sampled, _) = if prepared.is_some() {
            project_rhumb_components_prepared(
                points,
                self.camera,
                0.025,
                5.,
                true,
                262144,
                prepared.as_deref(),
            )?
        } else {
            project_rhumb_components(points, self.camera, 0.025, 5., true, 262144)?
        };
        let components: Vec<Vec<CurveSample>> = sampled
            .into_iter()
            .map(|p| {
                p.samples
                    .into_iter()
                    .map(|s| CurveSample {
                        screen: s.screen_px,
                        segment: s.source_segment,
                        fraction: s.source_fraction,
                    })
                    .collect()
            })
            .collect();
        let bytes = prepared
            .as_ref()
            .map_or(0, |p| p.bytes() + 2 * std::mem::size_of::<usize>())
            + std::mem::size_of::<ProjectionEntry>()
            + 2 * std::mem::size_of::<usize>()
            + points.len() * std::mem::size_of::<ferrite_render::WorldPoint>()
            + std::mem::size_of::<Vec<Vec<CurveSample>>>()
            + components.capacity() * std::mem::size_of::<Vec<CurveSample>>()
            + components
                .iter()
                .map(|p| p.capacity() * std::mem::size_of::<CurveSample>())
                .sum::<usize>();
        let components = std::sync::Arc::new(components);
        if bytes > self.budget {
            self.oversized += 1;
            return Ok(components);
        }
        while self.retained + bytes > self.budget || self.slots.iter().all(Option::is_some) {
            let slot = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.as_ref().map(|e| (i, e.used)))
                .min_by_key(|(_, used)| *used)
                .map(|(i, _)| i)
                .expect("retained entry for eviction");
            self.retained -= self.slots[slot].take().unwrap().bytes;
            self.evictions += 1;
        }
        let slot = self.slots.iter().position(Option::is_none).unwrap();
        self.slots[slot] = Some(ProjectionEntry {
            points: points.to_vec().into_boxed_slice(),
            components: components.clone(),
            visible: None,
            prepared,
            bytes,
            used: self.clock,
        });
        self.retained += bytes;
        self.high_water = self.high_water.max(self.retained);
        Ok(components)
    }
    fn visible(
        &mut self,
        placement: &LineSymbolPlacement,
        projected: &std::sync::Arc<Vec<Vec<CurveSample>>>,
    ) -> Result<std::sync::Arc<Vec<Vec<CurveSample>>>, String> {
        let slot = self.slots.iter().position(|entry| {
            entry.as_ref().is_some_and(|entry| {
                std::sync::Arc::ptr_eq(&entry.components, projected)
                    && entry.points.len() == placement.points.len()
                    && entry.points.iter().zip(&placement.points).all(|(a, b)| {
                        a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits()
                    })
            })
        });
        if let Some(visible) =
            slot.and_then(|i| self.slots[i].as_ref().and_then(|e| e.visible.as_ref()))
        {
            self.visibility_hits += 1;
            return Ok(visible.clone());
        }
        self.visibility_misses += 1;
        let prepared = self.prepared_for(projected);
        let visible = visible_components(placement, self.camera, prepared.as_deref(), projected)?;
        let bytes = 2 * std::mem::size_of::<usize>()
            + std::mem::size_of::<Vec<Vec<CurveSample>>>()
            + visible.capacity() * std::mem::size_of::<Vec<CurveSample>>()
            + visible
                .iter()
                .map(|p| p.capacity() * std::mem::size_of::<CurveSample>())
                .sum::<usize>();
        let visible = std::sync::Arc::new(visible);
        if let Some(slot) = slot {
            if self.slots[slot].as_ref().unwrap().bytes + bytes > self.budget {
                self.oversized += 1;
                return Ok(visible);
            }
            while self.retained + bytes > self.budget {
                let other = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != slot)
                    .filter_map(|(i, e)| e.as_ref().map(|e| (i, e.used)))
                    .min_by_key(|(_, used)| *used)
                    .map(|(i, _)| i)
                    .expect("other entry for visibility budget eviction");
                self.retained -= self.slots[other].take().unwrap().bytes;
                self.evictions += 1;
            }
            let entry = self.slots[slot].as_mut().unwrap();
            entry.visible = Some(visible.clone());
            entry.bytes += bytes;
            self.retained += bytes;
            self.high_water = self.high_water.max(self.retained);
        }
        Ok(visible)
    }
    pub(crate) fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"source_revision":self.source_revision,"route_reuse_enabled":self.reuse_routes,"route_retained_bytes":self.slots.iter().flatten().filter_map(|e|e.prepared.as_ref()).map(|p|p.bytes()+2*std::mem::size_of::<usize>()).sum::<usize>(),"entries":self.slots.iter().filter(|p|p.is_some()).count(),
            "retained_bytes":self.retained,"high_water_bytes":self.high_water,"budget_bytes":self.budget,"hits":self.hits,
            "misses":self.misses,"visibility_hits":self.visibility_hits,"visibility_misses":self.visibility_misses,"evictions":self.evictions,"oversized":self.oversized,"lifetime":"one immutable camera preparation",
            "byte_scope":"conservative retained entry/key/component capacities; original cold transient and driver memory excluded"})
    }
}
fn resolve_globe_line_symbol_cached(
    point: &PointInstruction,
    cache: &mut FrameCurveProjectionCache<'_>,
) -> Result<Vec<PointInstruction>, String> {
    let Some(placement) = &point.line_placement else {
        return Ok(vec![point.clone()]);
    };
    placement.validate()?;
    let sampled = cache.project(&placement.points)?;
    let prepared = cache.prepared_for(&sampled);
    let visible;
    let components = if placement.visible_parts {
        visible = cache.visible(placement, &sampled)?;
        &visible[..]
    } else {
        &sampled[..]
    };
    if !placement.visible_parts {
        if let (Some(a), Some(b)) = (
            components.first().and_then(|p| p.first()),
            components.last().and_then(|p| p.last()),
        ) {
            if a.segment != 0
                || a.fraction != 0.
                || b.segment != placement.points.len() - 2
                || b.fraction != 1.
            {
                return Err("Whole-curve placement requires an unclipped projection domain".into());
            }
        }
    }
    let v = cache.camera.viewport();
    placed_curve_points(
        placement,
        components,
        96. / 25.4 * cache.pixel_ratio,
        [0., 0., v[0], v[1]],
    )?
    .into_iter()
    .map(|s| {
        let (position, bearing) = sample_position(placement, prepared.as_deref(), s)?;
        Ok(point.resolved_at(position, Some(bearing)))
    })
    .collect()
}
pub(crate) fn symbol_mesh_cached(
    point: &PointInstruction,
    cache: &mut FrameCurveProjectionCache<'_>,
    size: [u32; 2],
    pivot: [f32; 2],
    render_scale: f32,
    symbol_scale: f32,
) -> Result<GlobeMesh, String> {
    if point.line_placement.is_none() {
        return symbol_mesh(
            point,
            cache.camera,
            size,
            pivot,
            render_scale,
            cache.pixel_ratio,
            symbol_scale,
        );
    }
    let mut result = GlobeMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    for p in resolve_globe_line_symbol_cached(point, cache)? {
        let mesh = crate::globe_billboard::symbol_quad(
            &p,
            cache.camera,
            size,
            pivot,
            render_scale,
            cache.pixel_ratio,
            symbol_scale,
        )?;
        let base = result.vertices.len() as u32;
        result.vertices.extend(mesh.vertices);
        result
            .indices
            .extend(mesh.indices.into_iter().map(|i| base + i));
    }
    result.validate()?;
    Ok(result)
}

#[cfg(test)]
mod frame_projection_cache_tests {
    use super::*;
    use ferrite_kernel::geodesy::{direct, GeographicPosition};
    use ferrite_render::{LinePlacementMode, WorldPoint};
    fn point(lat: f64, lon: f64, range: f64) -> PointInstruction {
        let focus = GeographicPosition::new(lat, lon).unwrap();
        let end = direct(focus, 90., range / 8.).unwrap();
        PointInstruction::new("A".into(), WorldPoint::new(lon, lat)).with_line_placement(
            LineSymbolPlacement {
                points: vec![
                    WorldPoint::new(lon, lat),
                    WorldPoint::new(end.longitude_near(lon).unwrap(), end.latitude()),
                ]
                .into_boxed_slice(),
                mode: LinePlacementMode::Relative,
                offset: 0.5,
                visible_parts: true,
            },
        )
    }
    fn assert_result(a: Result<GlobeMesh, String>, b: Result<GlobeMesh, String>) {
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.indices, b.indices);
                assert_eq!(a.vertices.len(), b.vertices.len());
                for (a, b) in a.vertices.iter().zip(&b.vertices) {
                    assert_eq!(a.ecef_m.map(f64::to_bits), b.ecef_m.map(f64::to_bits));
                    assert_eq!(a.color.map(f32::to_bits), b.color.map(f32::to_bits));
                }
            }
            (Err(a), Err(b)) => assert_eq!(a, b),
            _ => panic!("Projection cache changed success/error outcome"),
        }
    }
    #[test]
    fn source_projection_reuse_preserves_bits_independent_placement_metadata_and_dpi() {
        let mut hits = 0;
        for (lat, lon) in [(0., 0.), (60., 179.9), (-70., -170.)] {
            for range in [30000., 500.] {
                for tilt in [0., 70.] {
                    for ratio in [1., 2.] {
                        for budget in [0, 128, 16 * 1024 * 1024] {
                            let camera = GlobeCamera::orbit(
                                GeographicPosition::new(lat, lon).unwrap(),
                                range,
                                90.,
                                tilt,
                                [1200., 800.],
                                45.,
                                1.,
                                1e8,
                            )
                            .unwrap();
                            let mut cache =
                                FrameCurveProjectionCache::new(&camera, 42, ratio, budget);
                            for (mode, offset, visible_parts) in [
                                (LinePlacementMode::Relative, 0.5, true),
                                (LinePlacementMode::Absolute, 2., false),
                                (LinePlacementMode::Relative, 0.8, true),
                            ] {
                                let mut p = point(lat, lon, range);
                                p.line_placement.as_mut().unwrap().mode = mode;
                                p.line_placement.as_mut().unwrap().offset = offset;
                                p.line_placement.as_mut().unwrap().visible_parts = visible_parts;
                                p.symbol_ref = "Other".into();
                                p.feature_id = Some(7);
                                p.cell_index = Some(2);
                                p.rotation = 17.;
                                p.local_offset = (0.1, -0.2);
                                assert_result(
                                    symbol_mesh(&p, &camera, [32, 24], [4., 8.], 1., ratio, 1.),
                                    symbol_mesh_cached(&p, &mut cache, [32, 24], [4., 8.], 1., 1.),
                                );
                                let off =
                                    resolve_globe_line_symbol(&p, &camera, 96. / 25.4 * ratio);
                                let on = resolve_globe_line_symbol_cached(&p, &mut cache);
                                match (off, on) {
                                    (Ok(a), Ok(b)) => {
                                        assert_eq!(a.len(), b.len());
                                        for (a, b) in a.iter().zip(&b) {
                                            assert_eq!(a.symbol_ref, b.symbol_ref);
                                            assert_eq!(a.feature_id, b.feature_id);
                                            assert_eq!(a.cell_index, b.cell_index);
                                            assert_eq!(
                                                a.position.x.to_bits(),
                                                b.position.x.to_bits()
                                            );
                                            assert_eq!(
                                                a.position.y.to_bits(),
                                                b.position.y.to_bits()
                                            );
                                            assert_eq!(
                                                a.curve_tangent_bearing.map(f64::to_bits),
                                                b.curve_tangent_bearing.map(f64::to_bits)
                                            );
                                        }
                                    }
                                    (Err(a), Err(b)) => assert_eq!(a, b),
                                    _ => panic!("Metadata outcome changed"),
                                }
                                assert!(cache.retained <= budget);
                                assert!(cache.slots.iter().flatten().count() <= 4);
                            }
                            hits += cache.hits;
                        }
                    }
                }
            }
        }
        assert!(hits > 0);
    }
    #[test]
    fn lru_eviction_oversize_and_camera_lifetime_fallback_preserve_results() {
        let camera = GlobeCamera::orbit(
            GeographicPosition::new(50., 0.).unwrap(),
            30000.,
            0.,
            0.,
            [1200., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let mut cache = FrameCurveProjectionCache::new(&camera, 1, 1., 16 * 1024 * 1024);
        for i in 0..8 {
            let p = point(50., i as f64 * 0.001, 30000.);
            assert_result(
                symbol_mesh(&p, &camera, [32, 24], [4., 8.], 1., 1., 1.),
                symbol_mesh_cached(&p, &mut cache, [32, 24], [4., 8.], 1., 1.),
            );
        }
        assert!(cache.evictions >= 4);
        assert!(cache.retained <= cache.budget);
        let new_camera = GlobeCamera::orbit(
            GeographicPosition::new(50., 0.).unwrap(),
            3000.,
            90.,
            70.,
            [800., 600.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let mut fresh = FrameCurveProjectionCache::new(&new_camera, 2, 3., 0);
        let p = point(50., 0., 3000.);
        assert_result(
            symbol_mesh(&p, &new_camera, [32, 24], [4., 8.], 1., 3., 1.),
            symbol_mesh_cached(&p, &mut fresh, [32, 24], [4., 8.], 1., 1.),
        );
        assert_eq!(fresh.hits, 0);
        assert_eq!(fresh.retained, 0);
        assert_eq!(fresh.oversized, 1);
    }
    #[test]
    fn invalid_placement_is_not_hidden_by_same_curve_cache_hit() {
        let camera = GlobeCamera::orbit(
            GeographicPosition::new(50., 0.).unwrap(),
            30000.,
            0.,
            0.,
            [1200., 800.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let mut cache = FrameCurveProjectionCache::new(&camera, 1, 1., 16 * 1024 * 1024);
        let mut p = point(50., 0., 30000.);
        let _ = resolve_globe_line_symbol_cached(&p, &mut cache);
        let hits = cache.hits;
        p.line_placement.as_mut().unwrap().offset = f64::NAN;
        assert_eq!(
            resolve_globe_line_symbol(&p, &camera, 96. / 25.4).err(),
            resolve_globe_line_symbol_cached(&p, &mut cache).err()
        );
        assert_eq!(cache.hits, hits);
        p.line_placement.as_mut().unwrap().offset = 0.5;
        p.line_placement.as_mut().unwrap().points[0].y = 91.;
        assert_eq!(
            resolve_globe_line_symbol(&p, &camera, 96. / 25.4).err(),
            resolve_globe_line_symbol_cached(&p, &mut cache).err()
        );
    }
}

#[cfg(test)]
mod visible_component_cache_tests {
    use super::*;
    use ferrite_kernel::geodesy::GeographicPosition;
    use ferrite_render::{LinePlacementMode, WorldPoint};
    #[test]
    fn horizon_dateline_and_budget_preserve_every_visible_sample_bit() {
        let mut hits = 0;
        for (lat, lon, range, tilt, points) in [
            (
                60.,
                180.,
                500000.,
                70.,
                vec![WorldPoint::new(179.5, 60.), WorldPoint::new(-179.5, 60.)],
            ),
            (
                0.,
                0.,
                500.,
                45.,
                vec![WorldPoint::new(0., -0.1), WorldPoint::new(0., 0.1)],
            ),
            (
                89.,
                0.,
                3000.,
                70.,
                vec![WorldPoint::new(-1., 89.), WorldPoint::new(1., 89.)],
            ),
        ] {
            let camera = GlobeCamera::orbit(
                GeographicPosition::new(lat, lon).unwrap(),
                range,
                0.,
                tilt,
                [1200., 800.],
                45.,
                1.,
                1e8,
            )
            .unwrap();
            let placement = LineSymbolPlacement {
                points: points.into_boxed_slice(),
                mode: LinePlacementMode::Relative,
                offset: 0.5,
                visible_parts: true,
            };
            for budget in [0, 512, 16 * 1024 * 1024] {
                let mut cache = FrameCurveProjectionCache::new(&camera, 7, 2., budget);
                for _ in 0..2 {
                    let projected = cache.project(&placement.points).unwrap();
                    let expected = visible_components(&placement, &camera, None, &projected);
                    let actual = cache.visible(&placement, &projected);
                    match (expected, actual) {
                        (Ok(a), Ok(b)) => {
                            assert_eq!(a.len(), b.len());
                            for (a, b) in a.iter().zip(b.iter()) {
                                assert_eq!(a.len(), b.len());
                                for (a, b) in a.iter().zip(b) {
                                    assert_eq!(
                                        a.screen.map(f64::to_bits),
                                        b.screen.map(f64::to_bits)
                                    );
                                    assert_eq!(a.segment, b.segment);
                                    assert_eq!(a.fraction.to_bits(), b.fraction.to_bits());
                                }
                            }
                        }
                        (Err(a), Err(b)) => assert_eq!(a, b),
                        _ => panic!("Visible component outcome changed"),
                    }
                    assert!(cache.retained <= budget);
                    assert!(cache.high_water <= budget);
                }
                hits += cache.visibility_hits;
            }
        }
        assert!(hits > 0);
    }
}

#[cfg(test)]
mod combined_route_projection_tests {
    use super::*;
    use ferrite_kernel::geodesy::{direct, GeographicPosition};
    use ferrite_render::{LinePlacementMode, WorldPoint};
    fn same(a: Result<GlobeMesh, String>, b: Result<GlobeMesh, String>) {
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.indices, b.indices);
                assert_eq!(a.vertices.len(), b.vertices.len());
                for (x, y) in a.vertices.iter().zip(&b.vertices) {
                    assert_eq!(x.ecef_m.map(f64::to_bits), y.ecef_m.map(f64::to_bits));
                    assert_eq!(x.color.map(f32::to_bits), y.color.map(f32::to_bits));
                }
            }
            (Err(a), Err(b)) => assert_eq!(a, b),
            _ => panic!("combined state changed outcome"),
        }
    }
    #[test]
    fn all_four_states_preserve_geometry_metadata_and_source_horizon_across_budgets() {
        let mut recorded_routes = false;
        for (lat, lon, range, tilt) in [
            (0., 0., 30000., 0.),
            (70., 179.9, 100000., 45.),
            (89., 30., 30000., 70.),
        ] {
            let focus = GeographicPosition::new(lat, lon).unwrap();
            let camera =
                GlobeCamera::orbit(focus, range, 30., tilt, [1200., 800.], 45., 1., 1e8).unwrap();
            let end = direct(focus, 90., range / 8.).unwrap();
            for budget in [0, 512, 16 * 1024 * 1024] {
                for ratio in [1., 3.] {
                    let mut reuse = FrameCurveProjectionCache::new(&camera, 77, ratio, budget);
                    let mut combo = FrameCurveProjectionCache::new(&camera, 77, ratio, budget)
                        .with_route_reuse(true);
                    for visible_parts in [false, true] {
                        for offset in [0., 0.25, 0.5, 1.] {
                            let point =
                                PointInstruction::new("A".into(), WorldPoint::new(lon, lat))
                                    .with_line_placement(LineSymbolPlacement {
                                        points: vec![
                                            WorldPoint::new(lon, lat),
                                            WorldPoint::new(
                                                end.longitude_near(lon).unwrap(),
                                                end.latitude(),
                                            ),
                                        ]
                                        .into_boxed_slice(),
                                        mode: LinePlacementMode::Relative,
                                        offset,
                                        visible_parts,
                                    });
                            let baseline = || {
                                symbol_mesh_routes(
                                    &point,
                                    &camera,
                                    [24, 32],
                                    [6., 8.],
                                    1.,
                                    ratio,
                                    1.,
                                    false,
                                )
                            };
                            same(
                                baseline(),
                                symbol_mesh_routes(
                                    &point,
                                    &camera,
                                    [24, 32],
                                    [6., 8.],
                                    1.,
                                    ratio,
                                    1.,
                                    true,
                                ),
                            );
                            same(
                                baseline(),
                                symbol_mesh_cached(&point, &mut reuse, [24, 32], [6., 8.], 1., 1.),
                            );
                            same(
                                baseline(),
                                symbol_mesh_cached(&point, &mut combo, [24, 32], [6., 8.], 1., 1.),
                            );
                            assert!(reuse.high_water <= budget && combo.high_water <= budget);
                            let diag = combo.diagnostics();
                            recorded_routes |= diag["route_retained_bytes"].as_u64().unwrap() > 0;
                            let expected = resolve_globe_line_symbol_routes(
                                &point,
                                &camera,
                                96. / 25.4 * ratio,
                                false,
                            );
                            let actual = resolve_globe_line_symbol_cached(&point, &mut combo);
                            match (expected, actual) {
                                (Ok(a), Ok(b)) => assert_eq!(
                                    serde_json::to_vec(&a).unwrap(),
                                    serde_json::to_vec(&b).unwrap()
                                ),
                                (Err(a), Err(b)) => assert_eq!(a, b),
                                _ => panic!("combined resolved metadata changed outcome"),
                            }
                        }
                    }
                }
            }
        }
        assert!(
            recorded_routes,
            "combined policy never retained its route constants"
        );
    }
    #[test]
    fn combined_eviction_and_oversized_fallback_preserve_each_placement() {
        let focus = GeographicPosition::new(0., 0.).unwrap();
        let camera = GlobeCamera::orbit(focus, 30000., 0., 0., [900., 600.], 45., 1., 1e8).unwrap();
        for budget in [0, 512, 16 * 1024 * 1024] {
            let mut cache =
                FrameCurveProjectionCache::new(&camera, 9, 2., budget).with_route_reuse(true);
            for pass in 0..2 {
                for i in 0..8 {
                    let offset = 0.001 * (i as f64);
                    let point =
                        PointInstruction::new(format!("A{pass}"), WorldPoint::new(offset, 0.))
                            .with_line_placement(LineSymbolPlacement {
                                points: vec![
                                    WorldPoint::new(offset, 0.),
                                    WorldPoint::new(offset + 0.015, 0.005),
                                ]
                                .into_boxed_slice(),
                                mode: LinePlacementMode::Absolute,
                                offset: 1.,
                                visible_parts: true,
                            });
                    same(
                        symbol_mesh_routes(
                            &point,
                            &camera,
                            [20, 20],
                            [10., 10.],
                            1.,
                            2.,
                            1.,
                            false,
                        ),
                        symbol_mesh_cached(&point, &mut cache, [20, 20], [10., 10.], 1., 1.),
                    );
                    assert!(cache.high_water <= budget);
                }
            }
            if budget == 16 * 1024 * 1024 {
                assert!(cache.evictions > 0);
                assert!(
                    cache.diagnostics()["route_retained_bytes"]
                        .as_u64()
                        .unwrap()
                        > 0
                );
            } else {
                assert!(cache.oversized > 0);
            }
        }
    }
}
