//! Continuous, native-coordinate coverage validity; independent of product encoding.
use anyhow::{ensure, Result};
use geo::{
    kernels::{Kernel, Orientation, RobustKernel},
    Coord,
};
use num_bigint::{BigInt, Sign};
use std::cmp::Ordering;
pub type Point = [f64; 2];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Outside,
    Boundary,
    Inside,
}
#[derive(Debug, Clone)]
pub struct SimpleRing {
    points: Vec<Point>,
    bounds: [f64; 4],
}
// Exactly convert every finite f64 to an integer multiple of2^-1074. At most
// 2098bits per ordinate, independent of geographic origin or point count.
fn integer(v: f64) -> BigInt {
    let bits = v.to_bits();
    let exp = ((bits >> 52) & 2047) as usize;
    let fraction = bits & ((1u64 << 52) - 1);
    let mantissa = if exp == 0 {
        fraction
    } else {
        fraction | (1u64 << 52)
    };
    let n = BigInt::from(mantissa) << if exp == 0 { 0 } else { exp - 1 };
    if bits >> 63 != 0 {
        -n
    } else {
        n
    }
}
fn exact_orientation(a: Point, b: Point, c: Point) -> Ordering {
    let ax = integer(a[0]);
    let ay = integer(a[1]);
    let d =
        (integer(b[0]) - &ax) * (integer(c[1]) - &ay) - (integer(b[1]) - ay) * (integer(c[0]) - ax);
    match d.sign() {
        Sign::Minus => Ordering::Less,
        Sign::NoSign => Ordering::Equal,
        Sign::Plus => Ordering::Greater,
    }
}
fn orientation(a: Point, b: Point, c: Point) -> Ordering {
    // This range keeps adaptive error-free products/tails away from overflow and
    // underflow. Outside it use exact dyadic integers, never an epsilon or repair.
    let safe = [a, b, c]
        .iter()
        .flatten()
        .all(|v| *v == 0. || (v.abs() >= 2f64.powi(-200) && v.abs() <= 2f64.powi(200)));
    if !safe {
        return exact_orientation(a, b, c);
    }
    match RobustKernel::orient2d(
        Coord { x: a[0], y: a[1] },
        Coord { x: b[0], y: b[1] },
        Coord { x: c[0], y: c[1] },
    ) {
        Orientation::Clockwise => Ordering::Less,
        Orientation::Collinear => Ordering::Equal,
        Orientation::CounterClockwise => Ordering::Greater,
    }
}
fn on_segment(a: Point, b: Point, p: Point) -> bool {
    orientation(a, b, p) == Ordering::Equal
        && p[0] >= a[0].min(b[0])
        && p[0] <= a[0].max(b[0])
        && p[1] >= a[1].min(b[1])
        && p[1] <= a[1].max(b[1])
}
fn intersects(a: Point, b: Point, c: Point, d: Point) -> bool {
    let (ab_c, ab_d, cd_a, cd_b) = (
        orientation(a, b, c),
        orientation(a, b, d),
        orientation(c, d, a),
        orientation(c, d, b),
    );
    (ab_c != Ordering::Equal
        && ab_d != Ordering::Equal
        && ab_c != ab_d
        && cd_a != Ordering::Equal
        && cd_b != Ordering::Equal
        && cd_a != cd_b)
        || on_segment(a, b, c)
        || on_segment(a, b, d)
        || on_segment(c, d, a)
        || on_segment(c, d, b)
}
impl SimpleRing {
    pub const MAX_VERTICES: usize = 4096;
    /// Explicit closure required; no holes, repair, simplification or implicit closure.
    pub fn new(points: Vec<Point>) -> Result<Self> {
        ensure!(
            (4..=Self::MAX_VERTICES + 1).contains(&points.len()),
            "Domain ring vertex count outside supported bounds"
        );
        ensure!(
            points.iter().flatten().all(|v| v.is_finite()),
            "Nonfinite domain vertex"
        );
        ensure!(
            points.first() == points.last(),
            "Domain ring is not explicitly closed"
        );
        let n = points.len() - 1;
        for i in 0..n {
            ensure!(points[i] != points[i + 1], "Zero-length domain edge");
            let (previous, current, next) = (points[(i + n - 1) % n], points[i], points[i + 1]);
            ensure!(
                !(orientation(previous, current, next) == Ordering::Equal
                    && (on_segment(previous, current, next)
                        || on_segment(current, next, previous))),
                "Backtracking domain edge"
            );
            for j in i + 1..n {
                if j == i + 1 || (i == 0 && j == n - 1) {
                    continue;
                }
                ensure!(
                    !intersects(points[i], points[i + 1], points[j], points[j + 1]),
                    "Domain ring self-intersects or self-touches"
                );
            }
        }
        ensure!(
            (2..n).any(|i| orientation(points[0], points[1], points[i]) != Ordering::Equal),
            "Zero-area domain ring"
        );
        let mut bounds = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for p in &points {
            bounds[0] = bounds[0].min(p[0]);
            bounds[1] = bounds[1].max(p[0]);
            bounds[2] = bounds[2].min(p[1]);
            bounds[3] = bounds[3].max(p[1]);
        }
        Ok(Self { points, bounds })
    }
    pub fn points(&self) -> &[Point] {
        &self.points
    }
    pub fn bounds(&self) -> [f64; 4] {
        self.bounds
    }
    pub fn classify(&self, p: Point) -> Location {
        if !p.iter().all(|v| v.is_finite())
            || p[0] < self.bounds[0]
            || p[0] > self.bounds[1]
            || p[1] < self.bounds[2]
            || p[1] > self.bounds[3]
        {
            return Location::Outside;
        }
        let mut winding = 0i32;
        for edge in self.points.windows(2) {
            let (a, b) = (edge[0], edge[1]);
            let side = orientation(a, b, p);
            if side == Ordering::Equal
                && p[0] >= a[0].min(b[0])
                && p[0] <= a[0].max(b[0])
                && p[1] >= a[1].min(b[1])
                && p[1] <= a[1].max(b[1])
            {
                return Location::Boundary;
            }
            if a[1] <= p[1] && b[1] > p[1] && side == Ordering::Greater {
                winding += 1;
            }
            if a[1] > p[1] && b[1] <= p[1] && side == Ordering::Less {
                winding -= 1;
            }
        }
        if winding == 0 {
            Location::Outside
        } else {
            Location::Inside
        }
    }
    pub fn contains(&self, p: Point) -> bool {
        self.classify(p) != Location::Outside
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concavity_closed_boundary_and_next_float_do_not_snap() {
        let ring = SimpleRing::new(vec![
            [-2., 48.],
            [2., 48.],
            [2., 52.],
            [0., 50.],
            [-2., 52.],
            [-2., 48.],
        ])
        .unwrap();
        assert_eq!(ring.classify([0., 49.]), Location::Inside);
        assert_eq!(ring.classify([0., 51.]), Location::Outside);
        assert_eq!(ring.classify([1., 51.]), Location::Boundary);
        assert_eq!(
            ring.classify([1., f64::from_bits(51f64.to_bits() - 1)]),
            Location::Inside
        );
        assert_eq!(
            ring.classify([1., f64::from_bits(51f64.to_bits() + 1)]),
            Location::Outside
        );
        let reversed = SimpleRing::new(ring.points.iter().rev().copied().collect()).unwrap();
        for p in [[0., 49.], [0., 51.], [1., 51.]] {
            assert_eq!(ring.classify(p), reversed.classify(p));
        }
    }
    #[test]
    fn invalid_rings_are_not_repaired() {
        for p in [
            vec![[0., 0.], [1., 0.], [0., 1.]],
            vec![[0., 0.], [1., 1.], [0., 1.], [1., 0.], [0., 0.]],
            vec![[0., 0.], [1., 0.], [1., 0.], [0., 1.], [0., 0.]],
            vec![[0., 0.], [1., 0.], [2., 0.], [0., 0.]],
            vec![[0., 0.], [f64::NAN, 1.], [1., 0.], [0., 0.]],
        ] {
            assert!(SimpleRing::new(p).is_err());
        }
    }
    #[test]
    fn exact_fallback_handles_underflow_overflow_and_subnormal_boundary() {
        assert_eq!(
            orientation([0., 0.], [f64::MAX, 0.], [0., f64::MAX]),
            Ordering::Greater
        );
        let t = f64::from_bits(1);
        assert_eq!(orientation([0., 0.], [t, 0.], [0., t]), Ordering::Greater);
        let r = SimpleRing::new(vec![[0., 0.], [1., 0.], [1., 1.], [0., 1.], [0., 0.]]).unwrap();
        assert_eq!(r.classify([t, 0.5]), Location::Inside);
    }
    #[test]
    fn adaptive_sign_matches_independent_integer_determinants() {
        let mut seed = 7u64;
        for _ in 0..2000 {
            let mut points = [[0f64; 2]; 3];
            for p in &mut points {
                for v in p {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    *v = ((seed >> 11) as f64 / ((1u64 << 53) as f64) - 0.5) * 1e8;
                }
            }
            assert_eq!(
                orientation(points[0], points[1], points[2]),
                exact_orientation(points[0], points[1], points[2])
            );
        }
    }
}
