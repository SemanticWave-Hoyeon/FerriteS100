//! Camera-dependent tessellation of a retained WGS84 radius arc.
//! Vertices lie on the authored geodesic circle. The existing globe stroke
//! adapter joins them on the surface, preserving physical style and clipping.
use ferrite_kernel::{
    geodesy::{GeodesicRadiusArc, GeographicPosition},
    globe_camera::GlobeCamera,
    rhumb::RhumbSegment,
};
use ferrite_render::{LineInstruction, PortrayalPath, WorldPoint};

const MAX_VERTICES: usize = 4097;
const SCREEN_ERROR: f64 = 0.0625;
const SURFACE_ERROR_M: f64 = 1.;

pub(crate) fn supports(line: &LineInstruction) -> bool {
    line.style_ref.is_none()
        && line.screen_ray.is_none()
        && line.style.color.a == 1.
        && (line.portrayal_path.is_none()
            || (!line.suppressible
                && matches!(
                    line.portrayal_path.as_ref(),
                    Some(PortrayalPath::GeographicArc { .. })
                )))
}

fn screen(camera: &GlobeCamera, ecef: [f64; 3]) -> Result<Option<[f64; 2]>, String> {
    let clip = camera.clip_ecef(ecef).map_err(|e| e.to_string())?;
    if clip[3] <= 0. {
        return Ok(None);
    }
    let viewport = camera.viewport();
    let point = [
        (clip[0] / clip[3] + 1.) * viewport[0] / 2.,
        (1. - clip[1] / clip[3]) * viewport[1] / 2.,
    ];
    if !point.iter().all(|v| v.is_finite()) {
        return Err("Non-finite geographic arc projection".into());
    }
    Ok(Some(point))
}

/// This is a bounded numerical error policy at three interior fractions, not
/// an analytic global error theorem. Unmet quality returns a diagnostic.
pub(crate) fn tessellate(
    arc: GeodesicRadiusArc,
    camera: &GlobeCamera,
) -> Result<Vec<WorldPoint>, String> {
    tessellate_recording(arc, camera, |_, _| {})
}
fn tessellate_recording(
    arc: GeodesicRadiusArc,
    camera: &GlobeCamera,
    mut record: impl FnMut(f64, GeographicPosition),
) -> Result<Vec<WorldPoint>, String> {
    let seeds = arc.sample(5., MAX_VERTICES).map_err(|e| e.to_string())?;
    let segments = seeds.len() - 1;
    let mut output = vec![WorldPoint::new(seeds[0].longitude(), seeds[0].latitude())];
    record(0., seeds[0]);
    for (index, pair) in seeds.windows(2).enumerate() {
        let mut pending = vec![(
            index as f64 / segments as f64,
            (index + 1) as f64 / segments as f64,
            pair[0],
            pair[1],
            0u8,
        )];
        while let Some((start, end, a, b, depth)) = pending.pop() {
            let route = RhumbSegment::new(a, b).map_err(|e| e.to_string())?;
            let mut split = false;
            let mut midpoint = None;
            for fraction in [0.25, 0.5, 0.75] {
                let authored = arc
                    .position_at(start + (end - start) * fraction)
                    .map_err(|e| e.to_string())?;
                let joined = route.point(fraction).map_err(|e| e.to_string())?;
                let actual = authored.to_ecef(0.).map_err(|e| e.to_string())?;
                let approximate = joined.to_ecef(0.).map_err(|e| e.to_string())?;
                let distance = (0..3)
                    .map(|i| (actual[i] - approximate[i]).powi(2))
                    .sum::<f64>()
                    .sqrt();
                let projected_error = match (screen(camera, actual)?, screen(camera, approximate)?)
                {
                    (Some(a), Some(b)) => (a[0] - b[0]).hypot(a[1] - b[1]),
                    (None, None) => 0.,
                    _ => f64::INFINITY,
                };
                split |= distance > SURFACE_ERROR_M || projected_error > SCREEN_ERROR;
                if fraction == 0.5 {
                    midpoint = Some(authored);
                }
            }
            if split {
                let middle = (start + end) / 2.;
                // Pending endpoints and unvisited seed endpoints also consume
                // the aggregate budget; no silent truncation or coarser fallback.
                let remaining_seeds = segments - index - 1;
                if depth >= 20
                    || middle == start
                    || middle == end
                    || output.len() + pending.len() + remaining_seeds + 2 >= MAX_VERTICES
                {
                    return Err(
                        "Geographic globe arc subdivision budget/resolution exceeded".into(),
                    );
                }
                let mid = midpoint.unwrap();
                pending.push((middle, end, mid, b, depth + 1));
                pending.push((start, middle, a, mid, depth + 1));
            } else {
                if output.len() >= MAX_VERTICES {
                    return Err("Geographic globe arc vertex budget exceeded".into());
                }
                record(end, b);
                output.push(WorldPoint::new(b.longitude(), b.latitude()));
            }
        }
    }
    Ok(output)
}

pub(crate) fn materialize(
    line: &LineInstruction,
    camera: &GlobeCamera,
) -> Result<Option<LineInstruction>, String> {
    let Some(PortrayalPath::GeographicArc {
        center,
        radius_m,
        start,
        sweep,
    }) = line.portrayal_path.as_ref()
    else {
        return Ok(None);
    };
    if line.suppressible {
        return Err("Coincident geographic arc suppression needs authored curve parameters".into());
    }
    let center = GeographicPosition::new(center.1, center.0).map_err(|e| e.to_string())?;
    let arc =
        GeodesicRadiusArc::new(center, *radius_m, *start, *sweep).map_err(|e| e.to_string())?;
    let mut generated = line.clone();
    generated.points = tessellate(arc, camera)?;
    generated.portrayal_path = None;
    Ok(Some(generated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_render::{Color, LineStyle};
    #[test]
    fn globe_arc_keeps_metric_vertices_source_style_and_refines_for_close_view() {
        let center = GeographicPosition::new(50., 179.9).unwrap();
        for sweep in [-270., 270., 360.] {
            let arc = GeodesicRadiusArc::new(center, 50_000., 35., sweep).unwrap();
            let mut counts = vec![];
            for distance in [200_000., 1000.] {
                let camera = GlobeCamera::orbit(
                    arc.position_at(0.5).unwrap(),
                    distance,
                    0.,
                    0.,
                    [960., 640.],
                    45.,
                    0.1,
                    1e8,
                )
                .unwrap();
                let mut line = LineInstruction::new(vec![WorldPoint::new(179.9, 50.)])
                    .with_style(LineStyle::solid_mm(Color::BLUE, 0.32))
                    .with_feature_id(42)
                    .with_cell_index(3);
                line.suppressible = false;
                line.portrayal_path = Some(PortrayalPath::GeographicArc {
                    center: (179.9, 50.),
                    radius_m: 50_000.,
                    start: 35.,
                    sweep,
                });
                assert!(supports(&line));
                let original = line.clone();
                let generated = materialize(&line, &camera).unwrap().unwrap();
                let mut parameters = vec![];
                let independent_points =
                    tessellate_recording(arc, &camera, |t, geo| parameters.push((t, geo))).unwrap();
                assert_eq!(generated.points, independent_points);
                // Dense fractions are deliberately different from the three
                // fractions used to decide subdivision. Validate the numerical
                // policy between every accepted pair, including the dateline.
                for pair in parameters.windows(2) {
                    let route = RhumbSegment::new(pair[0].1, pair[1].1).unwrap();
                    for k in 1..16 {
                        let f = k as f64 / 16.;
                        let actual = ferrite_kernel::geodesy::direct(
                            center,
                            35. + sweep * (pair[0].0 + (pair[1].0 - pair[0].0) * f),
                            50_000.,
                        )
                        .unwrap()
                        .to_ecef(0.)
                        .unwrap();
                        let joined = route.point(f).unwrap().to_ecef(0.).unwrap();
                        let distance = (0..3)
                            .map(|i| (actual[i] - joined[i]).powi(2))
                            .sum::<f64>()
                            .sqrt();
                        assert!(
                            distance <= SURFACE_ERROR_M + 1e-6,
                            "surface error {distance}"
                        );
                        let a = screen(&camera, actual).unwrap().unwrap();
                        let b = screen(&camera, joined).unwrap().unwrap();
                        let error = (a[0] - b[0]).hypot(a[1] - b[1]);
                        assert!(error <= SCREEN_ERROR + 1e-5, "pixel error {error}");
                    }
                }
                assert_eq!(generated.style.width, line.style.width);
                assert_eq!(generated.feature_id, line.feature_id);
                assert_eq!(generated.cell_index, line.cell_index);
                assert_eq!(line.points, original.points);
                assert!(line.portrayal_path.is_some() && generated.portrayal_path.is_none());
                assert!(generated.points.len() > 2 && generated.points.len() <= MAX_VERTICES);
                assert_eq!(
                    generated.points[0],
                    WorldPoint::new(
                        arc.position_at(0.).unwrap().longitude(),
                        arc.position_at(0.).unwrap().latitude()
                    )
                );
                if sweep == 360. {
                    assert_eq!(generated.points.first(), generated.points.last());
                }
                for point in &generated.points {
                    let geo = GeographicPosition::new(point.y, point.x).unwrap();
                    assert!(
                        (ferrite_kernel::geodesy::inverse(center, geo)
                            .unwrap()
                            .distance_m
                            - 50_000.)
                            .abs()
                            < 1e-5
                    );
                }
                let (mesh, _) = crate::globe_lines::drape_line(&line, &camera, 96. / 25.4).unwrap();
                assert!(!mesh.indices.is_empty());
                counts.push(generated.points.len());
            }
            assert!(
                counts[1] > counts[0],
                "close view did not refine: {counts:?}"
            );
        }
    }
}
