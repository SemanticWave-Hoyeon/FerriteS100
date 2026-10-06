//! Geographic anchors with screen-fixed physical portrayal quads.
use crate::globe_scene::{GlobeMesh, GlobeVertex};
use ferrite_kernel::globe_camera::GlobeCamera;
use ferrite_render::PointInstruction;
/// Texture raster pixels and pivot follow the existing SVG raster cache.
pub fn symbol_quad(
    point: &PointInstruction,
    camera: &GlobeCamera,
    size: [u32; 2],
    pivot: [f32; 2],
    render_scale: f32,
    pixel_ratio: f64,
    symbol_scale: f32,
) -> Result<GlobeMesh, String> {
    let empty = || GlobeMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    if !point.position.x.is_finite()
        || !point.position.y.is_finite()
        || !point.rotation.is_finite()
        || !point.scale.is_finite()
        || point.scale <= 0.
        || !render_scale.is_finite()
        || render_scale <= 0.
        || !pixel_ratio.is_finite()
        || pixel_ratio <= 0.
        || !symbol_scale.is_finite()
        || symbol_scale <= 0.
        || pivot.iter().any(|x| !x.is_finite())
        || [point.local_offset.0, point.local_offset.1]
            .iter()
            .any(|x| !x.is_finite())
    {
        return Err("Invalid globe symbol dimensions/anchor".into());
    }
    let Some(source) = crate::globe_device_point::GlobePointAnchor::resolve(&point.portrayal_origin, point.position, camera, 96. / 25.4 * pixel_ratio)? else { return Ok(empty()); };
    let anchor = source.ecef_m;
    let projected = source.screen_px;
    let factor = point.scale as f64 / render_scale as f64 * symbol_scale as f64 * pixel_ratio;
    let rotation = ferrite_render::screen_rotation(point, |bearing| source.project_bearing(camera,bearing))?;
    let (sin, cos) = (rotation as f64).to_radians().sin_cos();
    let mm = 96. / 25.4 * pixel_ratio;
    let corners = [
        [0., 0.],
        [size[0] as f64, 0.],
        [size[0] as f64, size[1] as f64],
        [0., size[1] as f64],
    ];
    let offsets: Vec<_> = corners
        .iter()
        .map(|p| {
            let x = (p[0] - pivot[0] as f64) * factor;
            let y = (p[1] - pivot[1] as f64) * factor;
            [
                x * cos - y * sin + point.local_offset.0 as f64 * mm,
                x * sin + y * cos - point.local_offset.1 as f64 * mm,
            ]
        })
        .collect();
    let v = camera.viewport();
    if (0..2).any(|a| {
        offsets.iter().all(|p| projected[a] + p[a] < 0.)
            || offsets.iter().all(|p| projected[a] + p[a] > v[a])
    }) {
        return Ok(empty());
    }
    let uv = [[0., 0.], [1., 0.], [1., 1.], [0., 1.]];
    let mut vertices = Vec::new();
    for (i, o) in offsets.iter().enumerate() {
        let ecef_m = camera
            .offset_pixels(anchor, *o)
            .map_err(|e| e.to_string())?;
        vertices.push(GlobeVertex {
            ecef_m,
            color: [uv[i][0], uv[i][1], 0., 1.],
        });
    }
    let mesh = GlobeMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
    };
    mesh.validate()?;
    Ok(mesh)
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::geodesy::GeographicPosition;
    use ferrite_render::WorldPoint;
    #[test]
    fn tilted_quad_keeps_authored_pivot_rotation_offset_and_size() {
        for tilt in [0., 45., 70.] {
            let g = GeographicPosition::new(70., 179.9).unwrap();
            let c = GlobeCamera::orbit(g, 30000., 0., tilt, [1000., 800.], 45., 1., 1e8).unwrap();
            let mut p = PointInstruction::new("test".into(), WorldPoint::new(179.9, 70.));
            p.rotation = 90.;
            p.local_offset = (2., 3.);
            let m = symbol_quad(&p, &c, [20, 10], [5., 2.], 2., 2., 1.).unwrap();
            assert_eq!(m.indices.len(), 6);
            let q = c.clip_ecef(m.vertices[0].ecef_m).unwrap();
            let x = (q[0] / q[3] + 1.) * 500.;
            let y = (1. - q[1] / q[3]) * 400.;
            assert!((x - (500. + 2. + 2. * 96. / 25.4 * 2.)).abs() < 1e-7);
            assert!((y - (400. - 5. - 3. * 96. / 25.4 * 2.)).abs() < 1e-7);
        }
    }
    #[test]
    fn far_side_symbol_does_not_authorize_a_parent() {
        let c = GlobeCamera::orbit(
            GeographicPosition::new(0., 0.).unwrap(),
            1e6,
            0.,
            0.,
            [800., 600.],
            45.,
            1.,
            1e8,
        )
        .unwrap();
        let p = PointInstruction::new("test".into(), WorldPoint::new(180., 0.));
        assert!(symbol_quad(&p, &c, [20, 10], [0., 0.], 2., 1., 1.)
            .unwrap()
            .indices
            .is_empty());
    }
}
