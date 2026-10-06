//! Product-neutral, allocation-free f64 screen triangle/rectangle intersection.
#[derive(Clone, Debug)]
pub struct ClippedTriangle {
    vertices: [[f64; 2]; 8],
    len: usize,
}
impl ClippedTriangle {
    pub fn points(&self) -> &[[f64; 2]] {
        &self.vertices[..self.len]
    }
}
pub fn clip_triangle_to_rect(
    triangle: [[f64; 2]; 3],
    rect: [f64; 4],
) -> Result<ClippedTriangle, &'static str> {
    if triangle
        .iter()
        .flatten()
        .chain(rect.iter())
        .any(|x| !x.is_finite())
        || rect[0] >= rect[2]
        || rect[1] >= rect[3]
    {
        return Err("Invalid triangle clipping input");
    }
    let mut input = [[0.; 2]; 8];
    input[..3].copy_from_slice(&triangle);
    let mut len = 3;
    for (axis, bound, lower) in [
        (0, rect[0], true),
        (0, rect[2], false),
        (1, rect[1], true),
        (1, rect[3], false),
    ] {
        if len == 0 {
            break;
        }
        let inside = |p: [f64; 2]| {
            if lower {
                p[axis] >= bound
            } else {
                p[axis] <= bound
            }
        };
        let mut output = [[0.; 2]; 8];
        let mut count = 0;
        let mut previous = input[len - 1];
        for current in input[..len].iter().copied() {
            if inside(previous) != inside(current) {
                // Canonical endpoint order gives shared edges identical intersections.
                let (a, b) = if previous[axis] < current[axis] {
                    (previous, current)
                } else {
                    (current, previous)
                };
                let delta = b[axis] - a[axis];
                if !delta.is_finite() || delta <= 0. {
                    return Err("Triangle clipping overflow");
                }
                let t = ((bound - a[axis]) / delta).clamp(0., 1.);
                let mut q = [a[0] * (1. - t) + b[0] * t, a[1] * (1. - t) + b[1] * t];
                q[axis] = bound;
                if q.iter().any(|x| !x.is_finite()) {
                    return Err("Triangle clipping overflow");
                }
                if count >= 8 {
                    return Err("Triangle clipping vertex bound exceeded");
                }
                output[count] = q;
                count += 1;
            }
            if inside(current) {
                if count >= 8 {
                    return Err("Triangle clipping vertex bound exceeded");
                }
                output[count] = current;
                count += 1;
            }
            previous = current;
        }
        input = output;
        len = count;
    }
    // Interpolation can move a previous boundary a fraction of an ulp.
    for p in &mut input[..len] {
        p[0] = p[0].clamp(rect[0], rect[2]);
        p[1] = p[1].clamp(rect[1], rect[3]);
    }
    Ok(ClippedTriangle {
        vertices: input,
        len,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn contains(tri: [[f64; 2]; 3], p: [f64; 2]) -> bool {
        let crosses = (0..3)
            .map(|i| {
                let a = tri[i];
                let b = tri[(i + 1) % 3];
                (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
            })
            .collect::<Vec<_>>();
        crosses.iter().all(|x| *x >= -1e-6) || crosses.iter().all(|x| *x <= 1e-6)
    }
    #[test]
    fn preserves_large_covering_triangles_and_shared_edges() {
        let rect = [30., 45., 1030., 845.];
        for size in [2e4, 1e6, 1e9] {
            for tri in [
                [[-size, -size], [size, -size], [0., size]],
                [[-size, 400.], [500., -size], [size, size]],
            ] {
                let c = clip_triangle_to_rect(tri, rect).unwrap();
                assert!(c.len >= 3 && c.len <= 7);
                assert!(c.points().iter().all(|p| p[0] >= rect[0]
                    && p[0] <= rect[2]
                    && p[1] >= rect[1]
                    && p[1] <= rect[3]));
                for x in (35..1030).step_by(31) {
                    for y in (50..845).step_by(29) {
                        let p = [x as f64, y as f64];
                        let found = (1..c.len - 1).any(|i| {
                            contains([c.vertices[0], c.vertices[i], c.vertices[i + 1]], p)
                        });
                        assert_eq!(found, contains(tri, p));
                    }
                }
            }
        }
        let a = clip_triangle_to_rect([[-1e6, -1e6], [1e6, 1e6], [1e6, -1e6]], rect).unwrap();
        let b = clip_triangle_to_rect([[1e6, 1e6], [-1e6, -1e6], [-1e6, 1e6]], rect).unwrap();
        let ea: Vec<_> = a
            .points()
            .iter()
            .copied()
            .filter(|p| (p[0] - p[1]).abs() < 1e-8)
            .collect();
        let eb: Vec<_> = b
            .points()
            .iter()
            .copied()
            .filter(|p| (p[0] - p[1]).abs() < 1e-8)
            .collect();
        assert_eq!(ea.len(), 2);
        assert!(ea.iter().all(|p| eb.contains(p)));
    }
    #[test]
    fn rejects_invalid_input_and_omits_outside_triangles() {
        let t = [[0., 0.], [1., 0.], [0., 1.]];
        assert_eq!(
            clip_triangle_to_rect(t, [10., 10., 20., 20.])
                .unwrap()
                .points()
                .len(),
            0
        );
        assert!(clip_triangle_to_rect([[f64::NAN, 0.], t[1], t[2]], [0., 0., 1., 1.]).is_err());
        assert!(clip_triangle_to_rect(t, [1., 0., 0., 1.]).is_err());
    }
}
