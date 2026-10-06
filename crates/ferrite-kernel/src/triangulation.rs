//! Checked polygon triangulation in caller-selected planar coordinates.
//! Geographic projection, display styles and GPU ownership stay outside.
use anyhow::{bail, ensure, Result};
/// Removes only exact consecutive/closing duplicates. Invalid or collapsed
/// holes are errors, never silently erased by an epsilon simplifier.
pub fn normalize_ring(points: &[[f64; 2]]) -> Result<Vec<f64>> {
    ensure!(
        points.len() <= 1_048_576,
        "Polygon ring vertex budget exceeded"
    );
    let mut out = Vec::with_capacity(points.len() * 2);
    for p in points {
        ensure!(p.iter().all(|v| v.is_finite()), "Non-finite polygon ring");
        if out.len() >= 2 && out[out.len() - 2..] == *p {
            continue;
        }
        out.extend_from_slice(p);
    }
    if out.len() >= 4 && out[..2] == out[out.len() - 2..] {
        out.truncate(out.len() - 2);
    }
    ensure!(out.len() >= 6, "Collapsed polygon ring");
    Ok(out)
}
fn sum(values: impl Iterator<Item = f64>) -> f64 {
    let (mut total, mut correction) = (0., 0.);
    for value in values {
        let y = value - correction;
        let t = total + y;
        correction = (t - total) - y;
        total = t;
    }
    total
}
fn area(ring: &[f64]) -> f64 {
    let n = ring.len() / 2;
    let o = [ring[0], ring[1]];
    sum((0..n).map(|i| {
        let j = (i + 1) % n;
        (ring[2 * i] - o[0]) * (ring[2 * j + 1] - o[1])
            - (ring[2 * j] - o[0]) * (ring[2 * i + 1] - o[1])
    }))
}
/// Rejects errors, incomplete index triples and area loss instead of producing
/// an exterior fan or filtering individual triangle indices. Area agreement is
/// a numerical coverage check, not proof that arbitrary input rings are simple.
pub fn triangulate(coords: &[f64], holes: &[usize]) -> Result<Vec<usize>> {
    ensure!(
        coords.len() % 2 == 0 && coords.len() >= 6 && coords.len() / 2 <= 1_048_576,
        "Invalid polygon coordinate count"
    );
    ensure!(
        coords.iter().all(|v| v.is_finite()),
        "Non-finite polygon coordinates"
    );
    let n = coords.len() / 2;
    ensure!(
        holes.windows(2).all(|w| w[0] < w[1]) && holes.iter().all(|h| *h > 0 && *h < n),
        "Invalid polygon hole offsets"
    );
    let ends: Vec<_> = std::iter::once(0)
        .chain(holes.iter().copied())
        .chain(std::iter::once(n))
        .collect();
    ensure!(
        ends.windows(2).all(|w| w[1] - w[0] >= 3),
        "Collapsed polygon ring"
    );
    // Translate before earcut to retain small local features at large eastings.
    let origin = [coords[0], coords[1]];
    let local: Vec<_> = coords
        .chunks_exact(2)
        .flat_map(|p| [p[0] - origin[0], p[1] - origin[1]])
        .collect();
    ensure!(
        local.iter().all(|v| v.is_finite()),
        "Polygon coordinate span overflow"
    );
    let areas: Vec<_> = ends
        .windows(2)
        .map(|w| area(&local[w[0] * 2..w[1] * 2]).abs())
        .collect();
    ensure!(
        areas.iter().all(|a| a.is_finite() && *a > 0.),
        "Degenerate polygon ring"
    );
    let expected = areas[0] - sum(areas[1..].iter().copied());
    ensure!(
        expected > 0. && expected.is_finite(),
        "Polygon holes exhaust exterior"
    );
    let indices = earcutr::earcut(&local, holes, 2)
        .map_err(|e| anyhow::anyhow!("Polygon triangulation failed: {e}"))?;
    ensure!(
        indices.len() >= 3 && indices.len() % 3 == 0 && indices.iter().all(|i| *i < n),
        "Invalid or empty polygon triangle result"
    );
    let covered = sum(indices.chunks_exact(3).map(|t| {
        let a = [local[t[0] * 2], local[t[0] * 2 + 1]];
        let b = [local[t[1] * 2], local[t[1] * 2 + 1]];
        let c = [local[t[2] * 2], local[t[2] * 2 + 1]];
        ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs()
    }));
    let bound = 128. * f64::EPSILON * (n + indices.len()) as f64 * sum(areas.iter().copied());
    if !covered.is_finite() || (covered - expected).abs() > bound {
        bail!("Polygon triangle coverage mismatch: expected={expected}, covered={covered}");
    }
    Ok(indices)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concave_polygon_keeps_hole_and_exact_small_vertices_at_large_origin() {
        let outer = [
            [0., 0.],
            [10., 0.],
            [10., 4.],
            [4., 4.],
            [4., 10.],
            [0., 10.],
            [0., 0.],
        ];
        let hole = [[1., 1.], [2., 1.], [2., 2.], [1., 2.], [1., 1.]];
        for origin in [0., 20_000_000.] {
            let move_ring = |r: &[[f64; 2]]| {
                r.iter()
                    .map(|p| [p[0] + origin, p[1] + origin])
                    .collect::<Vec<_>>()
            };
            let mut coords = normalize_ring(&move_ring(&outer)).unwrap();
            let h = coords.len() / 2;
            coords.extend(normalize_ring(&move_ring(&hole)).unwrap());
            let tris = triangulate(&coords, &[h]).unwrap();
            let contains = |q: [f64; 2]| {
                tris.chunks_exact(3).any(|t| {
                    let side = |i, j| {
                        (coords[t[j] * 2] - coords[t[i] * 2]) * (q[1] - coords[t[i] * 2 + 1])
                            - (coords[t[j] * 2 + 1] - coords[t[i] * 2 + 1])
                                * (q[0] - coords[t[i] * 2])
                    };
                    let s = [side(0, 1), side(1, 2), side(2, 0)];
                    s.iter().all(|v| *v >= 0.) || s.iter().all(|v| *v <= 0.)
                })
            };
            assert!(contains([origin + 3., origin + 3.]));
            assert!(!contains([origin + 1.5, origin + 1.5]));
            assert!(!contains([origin + 7., origin + 7.]));
        }
        assert_eq!(
            normalize_ring(&[[0., 0.], [1e-10, 0.], [1e-10, 1e-10], [0., 1e-10]])
                .unwrap()
                .len(),
            8
        );
    }
    #[test]
    fn malformed_or_collapsed_holes_never_turn_into_exterior_fill() {
        assert!(normalize_ring(&[[0., 0.], [0., 0.], [1., 1.]]).is_err());
        assert!(normalize_ring(&[[0., 0.], [f64::NAN, 1.], [1., 1.]]).is_err());
        assert!(triangulate(&[0., 0., 4., 4., 0., 4., 4., 0.], &[]).is_err());
        assert!(triangulate(&[0., 0., 4., 0., 4., 4., 0., 4.], &[3]).is_err());
        assert!(triangulate(
            &[0., 0., 4., 0., 4., 4., 0., 4., 10., 10., 11., 10., 11., 11., 10., 11.],
            &[4]
        )
        .is_err());
    }
}
