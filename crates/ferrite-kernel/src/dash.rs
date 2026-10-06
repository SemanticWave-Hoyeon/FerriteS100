//! Product-neutral canonical repeating dash intervals (Part 9-12.4.1.3).
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashCycle {
    pub period: f64,
    pub intervals: Vec<(f64, f64)>,
}
impl DashCycle {
    /// Input tuples are (start,length), output intervals are (start,end).
    /// Signed lengths draw from start to start+length along the line x-axis.
    /// Intervals crossing a repeat boundary wrap; overlaps form a union.
    pub fn new(
        period: f64,
        dashes: impl IntoIterator<Item = (f64, f64)>,
    ) -> Result<Self, &'static str> {
        if !period.is_finite() || period <= 0. {
            return Err("Dash interval must be finite and positive");
        }
        let mut intervals = Vec::new();
        for (start, length) in dashes {
            if !start.is_finite() || !length.is_finite() || length == 0. {
                return Err(
                    "Dash start/length must be finite; zero-length cap geometry is unsupported",
                );
            }
            // Reversal changes the lower endpoint as well as the extent.
            // Taking abs(length) at the original start would move the stroke.
            let start = if length < 0. { start + length } else { start };
            if !start.is_finite() {
                return Err("Directed dash endpoint overflow");
            }
            let length = length.abs();
            if length >= period {
                intervals.push((0., period));
                continue;
            }
            let start = start.rem_euclid(period);
            let remaining = period - start;
            if length > remaining {
                intervals.extend([(start, period), (0., length - remaining)]);
            } else {
                intervals.push((start, start + length));
            }
        }
        intervals.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let mut union: Vec<(f64, f64)> = Vec::new();
        for (a, b) in intervals {
            if let Some(last) = union.last_mut().filter(|last| a <= last.1) {
                last.1 = last.1.max(b);
            } else {
                union.push((a, b));
            }
        }
        Ok(Self {
            period,
            intervals: union,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn start_offsets_wrap_and_overlaps_are_union() {
        let c = DashCycle::new(10., [(2., 2.), (8., 4.), (3., 2.)]).unwrap();
        assert_eq!(c.intervals, vec![(0., 5.), (8., 10.)]);
        assert_eq!(
            DashCycle::new(10., [(4., 12.)]).unwrap().intervals,
            vec![(0., 10.)]
        );
    }
    #[test]
    fn signed_endpoints_wrap_and_union_like_the_same_undirected_geometry() {
        for period in [1., 5., 22.] {
            for start in [-30., -1., 0., 2., 5.1, 30.] {
                for length in [-40., -3.1, -1., 0.5, 3.1, 40.] {
                    let a = DashCycle::new(period, [(start, length)]).unwrap();
                    let b = DashCycle::new(period, [(start + length, -length)]).unwrap();
                    assert_eq!(a.intervals.len(), b.intervals.len());
                    for ((a, b), (c, d)) in a.intervals.into_iter().zip(b.intervals) {
                        assert!((a - c).abs() < 1e-12 && (b - d).abs() < 1e-12);
                    }
                }
            }
        }
        let ferry = DashCycle::new(22., [(14.2, 2.9), (5.1, -3.1), (19.1, 2.9)]).unwrap();
        // The decimal 19.1+2.9 may wrap a sub-ulp residual past 22.
        // Keep the existing numerical geometry; assess authored spans at tolerance.
        let spans: Vec<_> = ferry
            .intervals
            .iter()
            .filter(|&&(a, b)| b - a > 1e-12)
            .collect();
        assert_eq!(spans.len(), 3);
        assert!((spans[0].0 - 2.).abs() < 1e-12);
        assert_eq!(spans[0].1, 5.1);
        assert!(DashCycle::new(1., [(-f64::MAX, -f64::MAX)]).is_err());
    }
    #[test]
    fn invalid_period_and_lengths_are_reported() {
        assert!(DashCycle::new(0., [(0., 1.)]).is_err());
        assert_eq!(
            DashCycle::new(10., [(0., -1.)]).unwrap().intervals,
            vec![(9., 10.)]
        );
        assert!(DashCycle::new(10., [(0., 0.)]).is_err());
        assert!(DashCycle::new(10., [(f64::NAN, 1.)]).is_err());
    }
}
