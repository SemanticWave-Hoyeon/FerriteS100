//! Bounded screen-space stroke geometry shared by map and globe projections.
use ferrite_render::{CapStyle, JoinStyle};
use lyon_tessellation::{
    geometry_builder::{GeometryBuilder, GeometryBuilderError, StrokeGeometryBuilder},
    math::point,
    path::Path,
    LineCap, LineJoin, StrokeOptions, StrokeTessellator, StrokeVertex, VertexId, VertexSource,
};
use std::collections::HashMap;
#[derive(Debug, Clone)]
pub struct ScreenStrokeVertex {
    pub position: [f64; 2],
    pub on_path: [f64; 2],
    pub source: (usize, usize, f64),
}
#[derive(Debug, Default)]
pub struct ScreenStrokeMesh {
    pub vertices: Vec<ScreenStrokeVertex>,
    pub indices: Vec<u32>,
}
struct Output<'a> {
    mesh: ScreenStrokeMesh,
    endpoints: &'a HashMap<u32, usize>,
    budget: usize,
    overflow: bool,
}
impl GeometryBuilder for Output<'_> {
    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        if self.mesh.indices.len() + 3 > self.budget * 6 {
            self.overflow = true;
            return;
        }
        self.mesh.indices.extend([a.0, b.0, c.0]);
    }
    fn abort_geometry(&mut self) {
        self.mesh = ScreenStrokeMesh::default();
    }
}
impl StrokeGeometryBuilder for Output<'_> {
    fn add_stroke_vertex(&mut self, v: StrokeVertex) -> Result<VertexId, GeometryBuilderError> {
        if self.mesh.vertices.len() >= self.budget {
            return Err(GeometryBuilderError::TooManyVertices);
        }
        let source = match v.source() {
            VertexSource::Endpoint { id } => {
                let i = self.endpoints[&id.0];
                (i, i, 0.)
            }
            VertexSource::Edge { from, to, t } => {
                (self.endpoints[&from.0], self.endpoints[&to.0], t as f64)
            }
        };
        let a = v.position();
        let b = v.position_on_path();
        let id = VertexId(self.mesh.vertices.len() as u32);
        self.mesh.vertices.push(ScreenStrokeVertex {
            position: [a.x as f64, a.y as f64],
            on_path: [b.x as f64, b.y as f64],
            source,
        });
        Ok(id)
    }
}
/// One connected stroke; dash boundaries are separate runs. Geometry allocation
/// is bounded before producing vertices and triangles, including round joins.
pub fn stroke_screen_path(
    points: &[[f64; 2]],
    width: f64,
    cap: CapStyle,
    join: JoinStyle,
    closed: bool,
    budget: usize,
) -> Result<ScreenStrokeMesh, String> {
    if !width.is_finite()
        || width <= 0.
        || width > 4096.
        || points.len() > 262144
        || budget == 0
        || budget > 262144
        || points
            .iter()
            .flatten()
            .any(|x| !x.is_finite() || x.abs() > 1e8)
    {
        return Err("Invalid screen stroke geometry or budget".into());
    }
    let mut path = Path::builder();
    let mut endpoints = HashMap::new();
    let mut first = true;
    let mut previous = None;
    for (i, p) in points.iter().enumerate() {
        if previous == Some(*p) {
            continue;
        }
        let id = if first {
            first = false;
            path.begin(point(p[0] as f32, p[1] as f32))
        } else {
            path.line_to(point(p[0] as f32, p[1] as f32))
        };
        endpoints.insert(id.0, i);
        previous = Some(*p);
    }
    if first {
        return Ok(ScreenStrokeMesh::default());
    }
    path.end(closed);
    let path = path.build();
    let cap = match cap {
        CapStyle::Butt => LineCap::Butt,
        CapStyle::Round => LineCap::Round,
        CapStyle::Square => LineCap::Square,
    };
    let join = match join {
        JoinStyle::Miter => LineJoin::Miter,
        JoinStyle::Round => LineJoin::Round,
        JoinStyle::Bevel => LineJoin::Bevel,
    };
    let options = StrokeOptions::default()
        .with_miter_limit(StrokeOptions::DEFAULT_MITER_LIMIT)
        .with_line_width(width as f32)
        .with_line_cap(cap)
        .with_line_join(join)
        .with_tolerance(0.125);
    let mut output = Output {
        mesh: ScreenStrokeMesh::default(),
        endpoints: &endpoints,
        budget,
        overflow: false,
    };
    StrokeTessellator::new()
        .tessellate_with_ids(path.id_iter(), &path, None, &options, &mut output)
        .map_err(|e| format!("Stroke tessellation: {e:?}"))?;
    if output.overflow {
        return Err("Screen stroke index budget exceeded".into());
    }
    Ok(output.mesh)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cap_bounds_corner_joins_and_bounded_allocation() {
        for cap in [CapStyle::Butt, CapStyle::Round, CapStyle::Square] {
            let m = stroke_screen_path(
                &[[20., 30.], [80., 30.]],
                10.,
                cap,
                JoinStyle::Miter,
                false,
                1024,
            )
            .unwrap();
            let xmin = m
                .vertices
                .iter()
                .map(|v| v.position[0])
                .fold(f64::INFINITY, f64::min);
            let xmax = m
                .vertices
                .iter()
                .map(|v| v.position[0])
                .fold(f64::NEG_INFINITY, f64::max);
            let extension = if cap == CapStyle::Butt { 0. } else { 5. };
            assert!(
                (xmin - (20. - extension)).abs() < 0.13 && (xmax - (80. + extension)).abs() < 0.13
            );
            assert!(m.vertices.iter().all(|v| v.source.0 < 2 && v.source.1 < 2));
        }
        let mut counts = Vec::new();
        for join in [JoinStyle::Miter, JoinStyle::Round, JoinStyle::Bevel] {
            let m = stroke_screen_path(
                &[[20., 80.], [50., 30.], [80., 80.]],
                10.,
                CapStyle::Butt,
                join,
                false,
                1024,
            )
            .unwrap();
            assert!(!m.indices.is_empty());
            counts.push(m.vertices.len());
        }
        assert!(counts[1] > counts[2]);
        assert!(stroke_screen_path(
            &[[20., 30.], [80., 30.]],
            10.,
            CapStyle::Round,
            JoinStyle::Round,
            false,
            2
        )
        .is_err());
    }
}
