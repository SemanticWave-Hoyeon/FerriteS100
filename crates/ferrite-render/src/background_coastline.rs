//! Bounded coastline chunks for contextual background rendering. This is not ENC coverage.
#[derive(Debug, Clone)]
pub struct CoastlineChunk {
    pub points: Vec<[f64; 2]>,
    pub bounds: [f64; 4],
}
#[derive(Debug, Clone, Default)]
pub struct BackgroundCoastlines {
    chunks: Vec<CoastlineChunk>,
    segments: usize,
}
impl BackgroundCoastlines {
    /// Cache AABBs once, retaining every input segment exactly once. Adjacent
    /// chunks share one endpoint, never connect independent components.
    pub fn new(lines: Vec<Vec<[f64; 2]>>) -> Result<Self, String> {
        let mut out = Self::default();
        for line in lines {
            if line
                .iter()
                .any(|p| !p.iter().all(|v| v.is_finite()) || p[0].abs() > 180. || p[1].abs() > 90.)
            {
                return Err("Invalid background coastline coordinate".into());
            }
            if line.len() < 2 {
                continue;
            }
            out.segments += line.len() - 1;
            for start in (0..line.len() - 1).step_by(128) {
                let end = (start + 129).min(line.len());
                let points = line[start..end].to_vec();
                let mut bounds = [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ];
                for p in &points {
                    bounds[0] = bounds[0].min(p[0]);
                    bounds[1] = bounds[1].min(p[1]);
                    bounds[2] = bounds[2].max(p[0]);
                    bounds[3] = bounds[3].max(p[1]);
                }
                out.chunks.push(CoastlineChunk { points, bounds });
            }
        }
        Ok(out)
    }
    pub fn is_empty(&self) -> bool {
        self.segments == 0
    }
    pub fn segments(&self) -> usize {
        self.segments
    }
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
    /// Offset is already an explicit longitude copy: do not wrap again here.
    pub fn visible_chunks(
        &self,
        view: [f64; 4],
        offset: f64,
    ) -> impl Iterator<Item = &CoastlineChunk> {
        self.chunks.iter().filter(move |c| {
            let b = c.bounds;
            b[0] + offset <= view[2]
                && b[2] + offset >= view[0]
                && b[1] <= view[3]
                && b[3] >= view[1]
        })
    }
    pub fn use_detailed(scale_x: f64) -> bool {
        scale_x.is_finite() && scale_x.abs() >= 30.
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunking_preserves_all_segments_and_disconnected_components() {
        let a: Vec<_> = (0..400).map(|i| [-100. + i as f64 / 10., 10.]).collect();
        let b = vec![[100., 20.], [101., 21.]];
        let expected: Vec<_> = [a.clone(), b.clone()]
            .iter()
            .flat_map(|l| l.windows(2).map(|w| [w[0], w[1]]))
            .collect();
        let c = BackgroundCoastlines::new(vec![a, b]).unwrap();
        let actual: Vec<_> = c
            .visible_chunks([-180., -90., 180., 90.], 0.)
            .flat_map(|l| l.points.windows(2).map(|w| [w[0], w[1]]))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(c.segments(), 400);
        assert_eq!(c.chunk_count(), 5);
        assert!(c.chunks.iter().all(|c| c.points.len() <= 129));
    }
    #[test]
    fn query_matches_exhaustive_segment_bounds_including_longitude_copies() {
        let lines: Vec<Vec<_>> = (0..30)
            .map(|j| {
                (0..500)
                    .map(|i| [-179. + i as f64 * 0.7, -70. + j as f64 * 4.])
                    .collect()
            })
            .collect();
        let c = BackgroundCoastlines::new(lines.clone()).unwrap();
        for off in [-360., 0., 360.] {
            for view in [
                [-2., 45., 3., 51.],
                [179., -90., 185., 90.],
                [-181., -10., -178., 10.],
            ] {
                let touches = |a: [f64; 2], b: [f64; 2]| {
                    a[0].min(b[0]) + off <= view[2]
                        && a[0].max(b[0]) + off >= view[0]
                        && a[1].min(b[1]) <= view[3]
                        && a[1].max(b[1]) >= view[1]
                };
                let expected: Vec<_> = lines
                    .iter()
                    .flat_map(|l| l.windows(2))
                    .filter(|w| touches(w[0], w[1]))
                    .map(|w| [w[0], w[1]])
                    .collect();
                let actual: Vec<_> = c
                    .visible_chunks(view, off)
                    .flat_map(|l| l.points.windows(2))
                    .filter(|w| touches(w[0], w[1]))
                    .map(|w| [w[0], w[1]])
                    .collect();
                assert_eq!(actual, expected);
            }
        }
        assert!(c.visible_chunks([-2., 45., 3., 51.], 0.).count() < c.chunk_count() / 10);
    }
    #[test]
    fn malformed_data_is_rejected_and_lod_uses_physical_scale() {
        for p in [[f64::NAN, 0.], [181., 0.], [0., 91.]] {
            assert!(BackgroundCoastlines::new(vec![vec![[0., 0.], p]]).is_err());
        }
        assert!(!BackgroundCoastlines::use_detailed(29.99));
        assert!(BackgroundCoastlines::use_detailed(30.));
        assert!(!BackgroundCoastlines::use_detailed(f64::NAN));
    }
}
