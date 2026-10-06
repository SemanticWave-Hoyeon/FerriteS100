//! Product-neutral planar picking primitives. Coordinates and tolerance share units.
pub fn closest_on_path(
    points: impl IntoIterator<Item = [f64; 2]>,
    query: [f64; 2],
    closed: bool,
) -> Option<(f64, [f64; 2])> {
    if !query.iter().all(|x| x.is_finite()) {
        return None;
    }
    let mut iter = points.into_iter();
    let first = iter.next()?;
    if !first.iter().all(|x| x.is_finite()) {
        return None;
    }
    let mut previous = first;
    let mut best = (f64::INFINITY, first);
    let mut segments = 0;
    let mut visit = |a: [f64; 2], b: [f64; 2]| {
        let d = [b[0] - a[0], b[1] - a[1]];
        let length = d[0] * d[0] + d[1] * d[1];
        let t = if length > 0.0 {
            (((query[0] - a[0]) * d[0] + (query[1] - a[1]) * d[1]) / length).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let p = [a[0] + t * d[0], a[1] + t * d[1]];
        let distance = (query[0] - p[0]).hypot(query[1] - p[1]);
        if distance < best.0 {
            best = (distance, p);
        }
    };
    for point in iter {
        if !point.iter().all(|x| x.is_finite()) {
            return None;
        }
        visit(previous, point);
        previous = point;
        segments += 1;
    }
    if closed {
        visit(previous, first);
        segments += 1;
    }
    (segments > 0).then_some(best)
}
/// Even/odd containment, with the closing segment included for open input rings.
/// Boundaries are handled separately by closest_on_path and caller tolerance.
pub fn inside_ring(points: impl IntoIterator<Item = [f64; 2]>, query: [f64; 2]) -> bool {
    if !query.iter().all(|x| x.is_finite()) {
        return false;
    }
    let mut iter = points.into_iter();
    let Some(first) = iter.next() else {
        return false;
    };
    if !first.iter().all(|x| x.is_finite()) {
        return false;
    }
    let mut previous = first;
    let mut inside = false;
    let mut count = 1;
    let mut cross = |a: [f64; 2], b: [f64; 2]| {
        if (a[1] > query[1]) != (b[1] > query[1])
            && query[0] < (b[0] - a[0]) * (query[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
    };
    for point in iter {
        if !point.iter().all(|x| x.is_finite()) {
            return false;
        }
        cross(previous, point);
        previous = point;
        count += 1;
    }
    cross(previous, first);
    count >= 3 && inside
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closest_segment_handles_endpoints_duplicates_and_closure() {
        assert_eq!(
            closest_on_path([[0., 0.], [10., 0.]], [4., 3.], false),
            Some((3., [4., 0.]))
        );
        assert_eq!(
            closest_on_path([[0., 0.], [0., 0.]], [3., 4.], false),
            Some((5., [0., 0.]))
        );
        assert_eq!(
            closest_on_path([[0., 0.], [10., 0.]], [-3., 4.], false),
            Some((5., [0., 0.]))
        );
        assert_eq!(closest_on_path([[0., 0.]], [0., 0.], false), None);
        assert!(closest_on_path([[0., 0.], [f64::NAN, 0.]], [0., 0.], false).is_none());
        assert_eq!(
            closest_on_path([[0., 0.], [10., 0.], [10., 10.]], [5., 5.], true)
                .unwrap()
                .0,
            0.
        );
    }
    #[test]
    fn concave_ring_and_open_ring_containment() {
        let ring = [
            [0., 0.],
            [10., 0.],
            [10., 4.],
            [4., 4.],
            [4., 10.],
            [0., 10.],
        ];
        assert!(inside_ring(ring, [2., 8.]));
        assert!(!inside_ring(ring, [8., 8.]));
        assert!(!inside_ring(ring, [11., 2.]));
        assert!(!inside_ring(ring, [f64::NAN, 2.]));
    }
}
