//! S-100 Part 9-12.4.1.2 physical line offset, in device coordinates (y down).
//! Source points and their ordering stay unchanged; callers translate decoration.
pub fn screen_line_offsets(
    points: &[[f64; 2]],
    offset_px: f64,
    closed: bool,
) -> Result<Vec<[f64; 2]>, &'static str> {
    if points.len() > 262144
        || !offset_px.is_finite()
        || offset_px.abs() > 4096.
        || points
            .iter()
            .flatten()
            .any(|v| !v.is_finite() || v.abs() > 1e8)
    {
        return Err("Invalid line offset geometry/budget");
    }
    if offset_px == 0. || points.len() < 2 {
        return Ok(vec![[0., 0.]; points.len()]);
    }
    let duplicate_end = closed && points.first() == points.last();
    let n = points.len() - usize::from(duplicate_end);
    if n < 2 {
        return Ok(vec![[0., 0.]; points.len()]);
    }
    let edges = if closed { n } else { n - 1 };
    let mut normals = Vec::with_capacity(edges);
    for i in 0..edges {
        let a = points[i];
        let b = points[(i + 1) % n];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let length = dx.hypot(dy);
        normals.push(if length == 0. {
            None
        } else {
            Some([dy / length, -dx / length])
        });
    }
    let first = normals.iter().find_map(|n| *n);
    let last = normals.iter().rev().find_map(|n| *n);
    if first.is_none() {
        return Ok(vec![[0., 0.]; points.len()]);
    }
    // Two linear passes skip repeated points without quadratic neighbour scans.
    let mut before = vec![None; n];
    let mut direction = if closed { last } else { None };
    for i in 0..n {
        before[i] = direction;
        if i < edges && normals[i].is_some() {
            direction = normals[i];
        }
    }
    let mut after = vec![None; n];
    direction = if closed { first } else { None };
    for i in (0..n).rev() {
        if i < edges && normals[i].is_some() {
            direction = normals[i];
        }
        after[i] = direction;
    }
    let mut output = Vec::with_capacity(points.len());
    for i in 0..n {
        let shift = match (before[i], after[i]) {
            (Some(a), Some(b)) => {
                let denominator = 1. + a[0] * b[0] + a[1] * b[1];
                if denominator <= 1e-12 {
                    return Err("Offset curve reversal requires path partition");
                }
                let shift = [
                    offset_px * (a[0] + b[0]) / denominator,
                    offset_px * (a[1] + b[1]) / denominator,
                ];
                // Explicit supported bound: a large acute miter must be partitioned,
                // never clamped into an incorrectly located parallel curve.
                if shift[0].hypot(shift[1]) > offset_px.abs() * 4. + 1e-9 {
                    return Err("Offset curve miter exceeds supported bound");
                }
                shift
            }
            (Some(a), None) | (None, Some(a)) => [offset_px * a[0], offset_px * a[1]],
            (None, None) => [0., 0.],
        };
        if !shift.iter().all(|v| v.is_finite()) {
            return Err("Nonfinite line offset");
        }
        output.push(shift);
    }
    if duplicate_end {
        output.push(output[0]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positive_offset_is_left_in_y_down_coordinates_at_all_dpi() {
        for degrees in (0..360).step_by(15) {
            let angle = (degrees as f64).to_radians();
            let (s, c) = angle.sin_cos();
            for ppm in [96. / 25.4, 192. / 25.4, 144. / 25.4] {
                for mm in [-2.43, -1., 0., 1.74, 2.43] {
                    let o = screen_line_offsets(
                        &[[10., 20.], [10. + 100. * c, 20. + 100. * s]],
                        mm * ppm,
                        false,
                    )
                    .unwrap();
                    for p in o {
                        assert!((p[0] - mm * ppm * s).abs() < 1e-10);
                        assert!((p[1] + mm * ppm * c).abs() < 1e-10);
                    }
                }
            }
        }
    }
    #[test]
    fn corners_repeated_points_and_closed_ring_keep_perpendicular_distance() {
        let p = [[0., 0.], [10., 0.], [10., 0.], [10., 10.]];
        assert_eq!(
            screen_line_offsets(&p, 2., false).unwrap(),
            vec![[0., -2.], [2., -2.], [2., -2.], [2., 0.]]
        );
        let ring = [[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]];
        assert_eq!(
            screen_line_offsets(&ring, 2., true).unwrap(),
            vec![[-2., -2.], [2., -2.], [2., 2.], [-2., 2.], [-2., -2.]]
        );
    }
    #[test]
    fn direction_reversal_reverses_left_and_source_input_is_unchanged() {
        let points = [[0., 0.], [10., 0.], [10., 10.]];
        let original = points;
        let f = screen_line_offsets(&points, 1., false).unwrap();
        let mut reversed = points;
        reversed.reverse();
        let mut r = screen_line_offsets(&reversed, -1., false).unwrap();
        r.reverse();
        assert_eq!(f, r);
        assert_eq!(points, original);
    }
    #[test]
    fn unsupported_geometry_is_explicit_and_zero_offset_is_exact() {
        assert!(screen_line_offsets(&[[0., 0.], [1., 0.], [0., 0.]], 1., false).is_err());
        assert!(screen_line_offsets(&[[0., 0.], [1., 0.], [0., 0.01]], 1., false).is_err());
        assert!(screen_line_offsets(&[[0., 0.], [1., 0.]], f64::NAN, false).is_err());
        assert!(screen_line_offsets(&[[0., 0.], [1., 0.]], 4097., false).is_err());
        assert_eq!(
            screen_line_offsets(&[[0., 0.], [0., 0.]], 2., false).unwrap(),
            vec![[0., 0.]; 2]
        );
        assert_eq!(
            screen_line_offsets(&[[0., 0.], [1., 0.]], 0., false).unwrap(),
            vec![[0., 0.]; 2]
        );
    }
}
