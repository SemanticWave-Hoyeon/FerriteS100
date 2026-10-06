//! Output-device lattice shared by flat and curved portrayal. A lattice changes
//! motif placement, never the authored motif shape. No period is clamped to a
//! minimum display size: impossible resource requests must be diagnosed.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatternLattice {
    columns: [[f64; 2]; 2],
    inverse: [[f64; 2]; 2],
}
impl PatternLattice {
    /// S-100 vectors use +Y up; physical framebuffer coordinates use +Y down.
    pub fn from_mm(v1: (f32, f32), v2: (f32, f32), pixels_per_mm: f64) -> Result<Self, String> {
        if !pixels_per_mm.is_finite() || pixels_per_mm <= 0. {
            return Err("Pattern device calibration must be finite and positive".into());
        }
        let a = [
            f64::from(v1.0) * pixels_per_mm,
            -f64::from(v1.1) * pixels_per_mm,
        ];
        let b = [
            f64::from(v2.0) * pixels_per_mm,
            -f64::from(v2.1) * pixels_per_mm,
        ];
        if !a.iter().chain(b.iter()).all(|x| x.is_finite()) {
            return Err("Pattern vectors must be finite".into());
        }
        let scale = a[0].abs().max(a[1].abs()).max(b[0].abs()).max(b[1].abs());
        if scale == 0. {
            return Err("Pattern lattice is singular".into());
        }
        // Normalize before determinant calculation to avoid avoidable overflow.
        let an = [a[0] / scale, a[1] / scale];
        let bn = [b[0] / scale, b[1] / scale];
        let det = an[0] * bn[1] - bn[0] * an[1];
        if !det.is_finite() || det.abs() <= 1e-12 {
            return Err("Pattern lattice is singular or too ill-conditioned".into());
        }
        let inverse = [
            [bn[1] / det / scale, -bn[0] / det / scale],
            [-an[1] / det / scale, an[0] / det / scale],
        ];
        if !inverse.iter().flatten().all(|x| x.is_finite()) {
            return Err("Pattern lattice inverse overflow".into());
        }
        Ok(Self {
            columns: [a, b],
            inverse,
        })
    }
    pub fn columns(&self) -> [[f64; 2]; 2] {
        self.columns
    }
    pub fn inverse(&self) -> [[f64; 2]; 2] {
        self.inverse
    }
    pub fn coordinates(&self, physical: [f64; 2]) -> [f64; 2] {
        [
            self.inverse[0][0] * physical[0] + self.inverse[0][1] * physical[1],
            self.inverse[1][0] * physical[0] + self.inverse[1][1] * physical[1],
        ]
    }
    pub fn site(&self, index: [f64; 2]) -> [f64; 2] {
        [
            self.columns[0][0] * index[0] + self.columns[1][0] * index[1],
            self.columns[0][1] * index[0] + self.columns[1][1] * index[1],
        ]
    }
    /// Reduce the authored origin on the CPU, preserving integer-period
    /// equivalence before narrowing the shader constants to f32.
    pub fn phase(&self, origin: [f64; 2]) -> Result<[f64; 2], String> {
        let q = self.coordinates(origin);
        if !origin.iter().chain(q.iter()).all(|x| x.is_finite())
            || q.iter().any(|x| x.abs() > 2_f64.powi(40))
        {
            return Err("Pattern origin exceeds bounded phase precision".into());
        }
        Ok([(-q[0]).rem_euclid(1.), (-q[1]).rem_euclid(1.)])
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PatternCellLimits {
    pub max_dimension: u32,
    pub max_rgba_bytes: usize,
    pub max_motif_copies: usize,
}
impl Default for PatternCellLimits {
    fn default() -> Self {
        Self {
            max_dimension: 4096,
            max_rgba_bytes: 64 * 1024 * 1024,
            max_motif_copies: 4096,
        }
    }
}
/// Plan a periodic cell raster in lattice coordinates. `motif_bounds` are
/// physical pixel bounds relative to the motif's authored reference point,
/// including stroke/filter margins. Copies are placed at integer lattice sites.
#[derive(Debug, Clone)]
pub struct PatternCellPlan {
    pub width: u32,
    pub height: u32,
    pub copies: Vec<[i64; 2]>,
    /// physical relative coordinates -> periodic raster coordinates.
    pub raster_rows: [[f64; 2]; 2],
}
impl PatternCellPlan {
    pub fn new(
        lattice: PatternLattice,
        motif_bounds: [f64; 4],
        quality: f64,
        limits: PatternCellLimits,
    ) -> Result<Self, String> {
        if !quality.is_finite()
            || quality < 1.
            || !motif_bounds.iter().all(|x| x.is_finite())
            || motif_bounds[0] >= motif_bounds[2]
            || motif_bounds[1] >= motif_bounds[3]
        {
            return Err("Invalid pattern motif bounds or raster quality".into());
        }
        let dimension = |a: [f64; 2]| -> Result<u32, String> {
            let n = (a[0].hypot(a[1]) * quality).ceil().max(1.);
            if !n.is_finite() || n > f64::from(limits.max_dimension) {
                return Err("Pattern raster dimension budget exceeded".into());
            }
            Ok(n as u32)
        };
        let width = dimension(lattice.columns[0])?;
        let height = dimension(lattice.columns[1])?;
        let bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("Pattern raster byte overflow")?;
        if bytes > limits.max_rgba_bytes {
            return Err("Pattern raster byte budget exceeded".into());
        }
        let corners = [
            [motif_bounds[0], motif_bounds[1]],
            [motif_bounds[0], motif_bounds[3]],
            [motif_bounds[2], motif_bounds[1]],
            [motif_bounds[2], motif_bounds[3]],
        ];
        let mut low = [f64::INFINITY; 2];
        let mut high = [f64::NEG_INFINITY; 2];
        for p in corners {
            let q = lattice.coordinates(p);
            for axis in 0..2 {
                low[axis] = low[axis].min(q[axis]);
                high[axis] = high[axis].max(q[axis]);
            }
        }
        // Include boundary intersections; the target raster clips off-cell pixels.
        let first = [(-high[0]).ceil(), (-high[1]).ceil()];
        let last = [(1. - low[0]).floor(), (1. - low[1]).floor()];
        if first
            .iter()
            .chain(last.iter())
            .any(|x| !x.is_finite() || x.abs() > 2_f64.powi(40))
        {
            return Err("Pattern copy index overflow".into());
        }
        let columns = (last[0] - first[0] + 1.).max(0.);
        let rows = (last[1] - first[1] + 1.).max(0.);
        if columns * rows > limits.max_motif_copies as f64 {
            return Err("Pattern motif copy budget exceeded".into());
        }
        let mut copies = Vec::with_capacity((columns * rows) as usize);
        // Lexicographic site order is invariant under whole-cell translation,
        // so overlapping motifs retain consistent alpha order across the seam.
        for y in 0..rows as usize {
            for x in 0..columns as usize {
                copies.push([first[0] as i64 + x as i64, first[1] as i64 + y as i64]);
            }
        }
        let inverse = lattice.inverse;
        Ok(Self {
            width,
            height,
            copies,
            raster_rows: [
                [
                    inverse[0][0] * f64::from(width),
                    inverse[0][1] * f64::from(width),
                ],
                [
                    inverse[1][0] * f64::from(height),
                    inverse[1][1] * f64::from(height),
                ],
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }
    #[test]
    fn signed_rotated_sheared_sites_and_dpi_preserve_authored_vectors() {
        for ratio in [1., 1.25, 2., 3.] {
            for (a, b) in [
                ((2., 0.), (1., 3.)),
                ((-2., 0.), (1., -3.)),
                ((1., 2.), (-3., 1.)),
            ] {
                let l = PatternLattice::from_mm(a, b, ratio).unwrap();
                for n in -3..4 {
                    for m in -3..4 {
                        let p = [
                            (f64::from(a.0) * f64::from(n) + f64::from(b.0) * f64::from(m)) * ratio,
                            -(f64::from(a.1) * f64::from(n) + f64::from(b.1) * f64::from(m))
                                * ratio,
                        ];
                        let q = l.coordinates(p);
                        close(q[0], f64::from(n));
                        close(q[1], f64::from(m));
                        assert_eq!(l.site([f64::from(n), f64::from(m)]), p);
                    }
                }
            }
        }
    }
    #[test]
    fn asymmetric_motif_shape_is_restored_after_periodic_cell_prewarp() {
        let l = PatternLattice::from_mm((2., 0.), (1., 3.), 4.).unwrap();
        let plan =
            PatternCellPlan::new(l, [-3., -1., 5., 7.], 2., PatternCellLimits::default()).unwrap();
        for offset in [[-3., -1.], [5., 7.], [0.75, 2.]] {
            let r = [
                plan.raster_rows[0][0] * offset[0] + plan.raster_rows[0][1] * offset[1],
                plan.raster_rows[1][0] * offset[0] + plan.raster_rows[1][1] * offset[1],
            ];
            let restored = l.site([r[0] / f64::from(plan.width), r[1] / f64::from(plan.height)]);
            close(restored[0], offset[0]);
            close(restored[1], offset[1]);
        }
    }
    #[test]
    fn origin_phase_is_fractional_signed_and_period_invariant() {
        let l = PatternLattice::from_mm((2., 0.), (1., 3.), 2.).unwrap();
        let origin = [-13.25, 17.5];
        let phase = l.phase(origin).unwrap();
        for shift in [[4., -9.], [-10000., 34567.]] {
            let p = l.site(shift);
            let q = l.phase([origin[0] + p[0], origin[1] + p[1]]).unwrap();
            close(q[0], phase[0]);
            close(q[1], phase[1]);
        }
        assert!(l.phase([f64::NAN, 0.]).is_err());
        assert!(l.phase([1e20, 0.]).is_err());
    }
    #[test]
    fn copies_cover_all_intersections_and_do_not_clamp_small_periods() {
        let l = PatternLattice::from_mm((0.25, 0.), (0., 0.5), 1.).unwrap();
        let p = PatternCellPlan::new(l, [-0.2, -0.3, 0.4, 0.7], 2., PatternCellLimits::default())
            .unwrap();
        assert_eq!((p.width, p.height), (1, 1));
        assert_eq!(l.columns(), [[0.25, -0.], [0., -0.5]]);
        // Independent site/physical AABB intersection oracle over a larger range.
        for m in -8..9 {
            for n in -8..9 {
                let s = l.site([f64::from(n), f64::from(m)]);
                let intersects = s[0] + 0.4 >= 0.
                    && s[0] - 0.2 <= 0.25
                    && s[1] + 0.7 >= -0.5
                    && s[1] - 0.3 <= 0.;
                assert_eq!(p.copies.contains(&[n.into(), m.into()]), intersects);
            }
        }
    }
    #[test]
    fn invalid_geometry_and_resource_budget_fail_explicitly() {
        for (a, b) in [
            ((0., 0.), (0., 1.)),
            ((1., 2.), (2., 4.)),
            ((f32::NAN, 0.), (0., 1.)),
        ] {
            assert!(PatternLattice::from_mm(a, b, 1.).is_err());
        }
        assert!(PatternLattice::from_mm((1., 0.), (0., 1.), 0.).is_err());
        let l = PatternLattice::from_mm((1., 0.), (0., 1.), 1.).unwrap();
        assert!(PatternCellPlan::new(
            l,
            [-100., -100., 100., 100.],
            2.,
            PatternCellLimits::default()
        )
        .unwrap_err()
        .contains("copy budget"));
        let limits = PatternCellLimits {
            max_dimension: 1,
            ..Default::default()
        };
        assert!(PatternCellPlan::new(l, [-1., -1., 1., 1.], 2., limits)
            .unwrap_err()
            .contains("dimension budget"));
    }
}
