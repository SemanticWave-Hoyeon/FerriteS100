//! One feature-container composition, using a common centroid-cell partition.
//! Datum reference keys and transformation evidence are supplied by the host; this
//! module does not infer a correction from an IHO datum classification code.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::depth_selection::{
    DepthAdjustmentProvider, DepthCandidate, DepthReference, DepthSource, SelectedDepth,
    ShoalestDepth,
};
use ferrite_kernel::{CoverageSource, GridGeometry, GridWindow, NumericCoverageSource};

pub struct DatumCoverage<'a> {
    pub coverage: &'a dyn CoverageSource,
    pub reference: DepthReference,
}

/// Temporary selection storage is capped at 65536 cells; source reads are no larger
/// than that block. Output dimensions must fit the SourceGrid integer coordinate limit.
/// Unsupported common partitions fail explicitly instead of sampling coarse cell centres.
pub struct ConservativeCoverage<'a, P: DepthAdjustmentProvider> {
    sources: Vec<DatumCoverage<'a>>,
    geometry: GridGeometry,
    target: DepthReference,
    provider: &'a P,
}
fn lcm(a: usize, b: usize) -> Result<usize> {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        let r = x % y;
        x = y;
        y = r;
    }
    a.checked_div(x)
        .and_then(|v| v.checked_mul(b))
        .context("Common grid dimension overflow")
}
fn source_axis(index: usize, output: usize, input: usize, reversed: bool) -> usize {
    let index = index / (output / input);
    if reversed {
        input - 1 - index
    } else {
        index
    }
}
// Closed encoded cell intervals implement commonPointRule=low at shared
// boundaries. Exact expansion comparisons avoid inventing a fuzzy boundary band.
pub(crate) fn cells_at_axis(
    origin: f64,
    spacing: f64,
    length: usize,
    point: f64,
) -> Result<[Option<usize>; 2]> {
    if !point.is_finite() {
        return Ok([None, None]);
    }
    let encoded = crate::boundary(point, 1., 0.)?;
    let boundary = |index: usize| {
        crate::boundary(
            origin,
            spacing,
            if spacing > 0. {
                index as f64 - 0.5
            } else {
                (length - index) as f64 - 0.5
            },
        )
    };
    let (mut low, mut high) = (0, length + 1);
    while low < high {
        let middle = low + (high - low) / 2;
        if crate::compare_boundary(boundary(middle)?, encoded)? == std::cmp::Ordering::Less {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    if low > length {
        return Ok([None, None]);
    }
    let equal = crate::compare_boundary(boundary(low)?, encoded)? == std::cmp::Ordering::Equal;
    let canonical = [low.checked_sub(1), (equal && low < length).then_some(low)];
    Ok(canonical.map(|c| c.map(|c| if spacing > 0. { c } else { length - 1 - c })))
}
impl<'a, P: DepthAdjustmentProvider> ConservativeCoverage<'a, P> {
    pub fn new(
        sources: Vec<DatumCoverage<'a>>,
        target: DepthReference,
        provider: &'a P,
    ) -> Result<Self> {
        let first = *sources
            .first()
            .context("No depth instances to compose")?
            .coverage
            .geometry();
        let extent = crate::grid_extent(&first)?;
        let (mut width, mut height) = (first.width, first.height);
        for source in &sources {
            let g = source.coverage.geometry();
            ensure!(
                g.horizontal_crs == first.horizontal_crs,
                "Mismatched horizontal CRS"
            );
            let other = crate::grid_extent(g)?;
            for k in 0..4 {
                ensure!(
                    crate::same_boundary(extent[k], other[k])?,
                    "Depth composition requires identical encoded cell-grid extents"
                );
            }
            width = lcm(width, g.width)?;
            height = lcm(height, g.height)?;
            ensure!(
                width <= 1 << 20 && height <= 1 << 20,
                "Common depth grid exceeds supported exact SourceGrid coordinate limit"
            );
        }
        let spacing_x = first.spacing_x / (width / first.width) as f64;
        let spacing_y = first.spacing_y / (height / first.height) as f64;
        let geometry = if width == first.width && height == first.height {
            first
        } else {
            GridGeometry {
                width,
                height,
                origin_x: first.origin_x - first.spacing_x * 0.5 + spacing_x * 0.5,
                origin_y: first.origin_y - first.spacing_y * 0.5 + spacing_y * 0.5,
                spacing_x,
                spacing_y,
                horizontal_crs: first.horizontal_crs,
            }
        };
        let common = crate::grid_extent(&geometry)?;
        for k in 0..4 {
            ensure!(
                crate::same_boundary(extent[k], common[k])?,
                "Common depth grid cannot represent original encoded extent exactly"
            );
        }
        Ok(Self {
            sources,
            geometry,
            target,
            provider,
        })
    }
    fn node(&self, source: usize, column: usize, row: usize) -> (usize, usize) {
        let g = self.sources[source].coverage.geometry();
        (
            source_axis(
                column,
                self.geometry.width,
                g.width,
                g.spacing_x.is_sign_positive() != self.geometry.spacing_x.is_sign_positive(),
            ),
            source_axis(
                row,
                self.geometry.height,
                g.height,
                g.spacing_y.is_sign_positive() != self.geometry.spacing_y.is_sign_positive(),
            ),
        )
    }
    /// Evaluate original closed source-cell domains at the supplied position.
    /// CommonPointRule=low compares all incident original cells (at most four per
    /// instance); artificial LCM partition boundaries do not create extra values.
    /// Corrections are evaluated at this position. Numeric raster generation samples
    /// them at common-cell centres; nonconstant transformations require a separate
    /// approximation/error contract before an App can claim display-query equality.
    /// A location belongs to the coverage only when an original closed cell
    /// and that source's own continuous validity domain both contain it.
    /// This distinguishes a valid fill location from absence of coverage.
    pub fn covers_position(&self, x: f64, y: f64) -> Result<bool> {
        if !x.is_finite() || !y.is_finite() {
            return Ok(false);
        }
        for source in &self.sources {
            if !source.coverage.is_valid_position(x, y) {
                continue;
            }
            let g = source.coverage.geometry();
            if cells_at_axis(g.origin_x, g.spacing_x, g.width, x)?
                .iter()
                .any(Option::is_some)
                && cells_at_axis(g.origin_y, g.spacing_y, g.height, y)?
                    .iter()
                    .any(Option::is_some)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn query_nearest(&self, x: f64, y: f64) -> Result<Option<SelectedDepth>> {
        if !x.is_finite() || !y.is_finite() {
            return Ok(None);
        }
        let mut selected = ShoalestDepth::new(self.target, x, y)?;
        for (instance, source) in self.sources.iter().enumerate() {
            if !source.coverage.is_valid_position(x, y) {
                continue;
            }
            let g = source.coverage.geometry();
            let columns = cells_at_axis(g.origin_x, g.spacing_x, g.width, x)?;
            let rows = cells_at_axis(g.origin_y, g.spacing_y, g.height, y)?;
            for column in columns.into_iter().flatten() {
                for row in rows.into_iter().flatten() {
                    let part = GridWindow {
                        column,
                        row,
                        width: 1,
                        height: 1,
                    };
                    let tile = source.coverage.read_window(part)?;
                    ensure!(
                        tile.window == part && tile.samples.len() == 1,
                        "Invalid source point read"
                    );
                    let node = g.position(column, row).context("Invalid source node")?;
                    let sample = tile.samples[0];
                    selected.consider(
                        DepthCandidate {
                            source: DepthSource {
                                instance,
                                column,
                                row,
                            },
                            reference: source.reference,
                            x: node.0,
                            y: node.1,
                            raw_depth: sample.value.map(f64::from),
                            uncertainty: sample.uncertainty.map(f64::from),
                        },
                        self.provider,
                    )?;
                }
            }
        }
        selected.finish()
    }
}
impl<P: DepthAdjustmentProvider> NumericCoverageSource for ConservativeCoverage<'_, P> {
    fn requires_spatial_mask(&self) -> bool {
        self.sources
            .iter()
            .any(|s| s.coverage.requires_geometric_mask())
    }
    fn numeric_geometry(&self) -> &GridGeometry {
        &self.geometry
    }
    fn visit_window_values(
        &self,
        window: GridWindow,
        visitor: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
    ) -> Result<()> {
        window.validate(&self.geometry)?;
        const MAX_BLOCK: usize = 65536;
        ensure!(
            window.width <= MAX_BLOCK,
            "Depth composition window too wide; tile it first"
        );
        let rows = (MAX_BLOCK / window.width).min(128);
        for start in (0..window.height).step_by(rows) {
            let height = rows.min(window.height - start);
            let mut selections = Vec::with_capacity(window.width * height);
            for row in 0..height {
                for column in 0..window.width {
                    let p = self
                        .geometry
                        .position(window.column + column, window.row + start + row)
                        .context("Invalid common grid position")?;
                    selections.push(ShoalestDepth::new(self.target, p.0, p.1)?);
                }
            }
            for (instance, source) in self.sources.iter().enumerate() {
                let (a, b) = self.node(instance, window.column, window.row + start);
                let (c, d) = self.node(
                    instance,
                    window.column + window.width - 1,
                    window.row + start + height - 1,
                );
                let part = GridWindow {
                    column: a.min(c),
                    row: b.min(d),
                    width: a.abs_diff(c) + 1,
                    height: b.abs_diff(d) + 1,
                };
                let tile = source.coverage.read_window(part)?;
                ensure!(
                    tile.window == part && tile.samples.len() == part.width * part.height,
                    "Invalid source composition window"
                );
                for (index, selection) in selections.iter_mut().enumerate() {
                    let (column, row) = self.node(
                        instance,
                        window.column + index % window.width,
                        window.row + start + index / window.width,
                    );
                    let p = self
                        .geometry
                        .position(
                            window.column + index % window.width,
                            window.row + start + index / window.width,
                        )
                        .context("Invalid domain comparison position")?;
                    if !source.coverage.is_valid_position(p.0, p.1) {
                        continue;
                    }
                    let sample = tile.samples[(row - part.row) * part.width + column - part.column];
                    let node = source
                        .coverage
                        .geometry()
                        .position(column, row)
                        .context("Invalid source node")?;
                    selection.consider(
                        DepthCandidate {
                            source: DepthSource {
                                instance,
                                column,
                                row,
                            },
                            reference: source.reference,
                            x: node.0,
                            y: node.1,
                            raw_depth: sample.value.map(f64::from),
                            uncertainty: sample.uncertainty.map(f64::from),
                        },
                        self.provider,
                    )?;
                }
            }
            for (index, selection) in selections.into_iter().enumerate() {
                visitor(
                    start * window.width + index,
                    selection.finish()?.map(|s| s.adjusted_depth),
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::depth_selection::{DepthAdjustment, IdentityDepthAdjustment};
    use ferrite_kernel::{CoverageSample, CoverageTile};
    struct Grid {
        geometry: GridGeometry,
        values: Vec<Option<f32>>,
    }
    impl CoverageSource for Grid {
        fn geometry(&self) -> &GridGeometry {
            &self.geometry
        }
        fn read_window(&self, w: GridWindow) -> Result<CoverageTile> {
            w.validate(&self.geometry)?;
            let mut samples = Vec::new();
            for row in w.row..w.row + w.height {
                for col in w.column..w.column + w.width {
                    samples.push(CoverageSample {
                        value: self.values[row * self.geometry.width + col],
                        uncertainty: Some(0.125),
                    });
                }
            }
            Ok(CoverageTile { window: w, samples })
        }
    }
    struct Adjust;
    impl DepthAdjustmentProvider for Adjust {
        fn adjustment(
            &self,
            from: DepthReference,
            to: DepthReference,
            _: f64,
            _: f64,
        ) -> Result<Option<DepthAdjustment>> {
            ensure!(to == DepthReference(1), "Unexpected target");
            Ok(Some(DepthAdjustment {
                correction_metres: if from == to { 0. } else { -4. },
                provenance: from.0,
            }))
        }
    }
    fn grids() -> (Grid, Grid) {
        (
            Grid {
                geometry: GridGeometry {
                    width: 3,
                    height: 2,
                    origin_x: 0.5,
                    origin_y: 0.5,
                    spacing_x: 1.,
                    spacing_y: 1.,
                    horizontal_crs: 4326,
                },
                values: vec![Some(10.); 6],
            },
            Grid {
                geometry: GridGeometry {
                    width: 4,
                    height: 2,
                    origin_x: 2.625,
                    origin_y: 1.5,
                    spacing_x: -0.75,
                    spacing_y: -1.,
                    horizontal_crs: 4326,
                },
                values: [Some(12.), Some(14.), None, Some(8.)].repeat(2),
            },
        )
    }
    #[test]
    fn mixed_resolution_reverse_axes_preserve_cell_boundaries_and_query_winner() {
        let (a, b) = grids();
        let provider = Adjust;
        let c = ConservativeCoverage::new(
            vec![
                DatumCoverage {
                    coverage: &a,
                    reference: DepthReference(1),
                },
                DatumCoverage {
                    coverage: &b,
                    reference: DepthReference(2),
                },
            ],
            DepthReference(1),
            &provider,
        )
        .unwrap();
        assert_eq!((c.geometry.width, c.geometry.height), (12, 2));
        let expected = [4., 4., 4., 10., 10., 10., 10., 10., 10., 8., 8., 8.];
        for row in 0..2 {
            for (col, expected) in expected.into_iter().enumerate() {
                let p = c.geometry.position(col, row).unwrap();
                let q = c.query_nearest(p.0, p.1).unwrap().unwrap();
                assert_eq!(q.adjusted_depth, expected);
                let instance = if expected == 10. { 0 } else { 1 };
                assert_eq!(q.candidate.source.instance, instance);
                assert_eq!(q.candidate.uncertainty, Some(0.125));
                let raw_grid = if instance == 0 { &a } else { &b };
                let source = q.candidate.source;
                assert_eq!(
                    (q.candidate.x, q.candidate.y),
                    raw_grid
                        .geometry
                        .position(source.column, source.row)
                        .unwrap()
                );
            }
        }
        let mut actual = Vec::new();
        c.visit_window_values(
            GridWindow {
                column: 2,
                row: 0,
                width: 8,
                height: 2,
            },
            &mut |index, value| {
                assert_eq!(index, actual.len());
                actual.push(value.unwrap());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(actual, expected[2..10].repeat(2));
        assert!(c.query_nearest(-1., 0.).unwrap().is_none());
    }
    #[test]
    fn unknown_datum_fails_both_query_and_portrayal_instead_of_painter_order() {
        let (a, b) = grids();
        let identity = IdentityDepthAdjustment;
        let c = ConservativeCoverage::new(
            vec![
                DatumCoverage {
                    coverage: &a,
                    reference: DepthReference(1),
                },
                DatumCoverage {
                    coverage: &b,
                    reference: DepthReference(2),
                },
            ],
            DepthReference(1),
            &identity,
        )
        .unwrap();
        assert!(c.query_nearest(0.125, 0.5).is_err());
        let mut visited = 0;
        assert!(c
            .visit_window_values(
                GridWindow {
                    column: 0,
                    row: 0,
                    width: 12,
                    height: 2
                },
                &mut |_, _| {
                    visited += 1;
                    Ok(())
                }
            )
            .is_err());
        assert_eq!(visited, 0); // current block does not publish a partial selection
                                // Only NoData in the other datum: no transformation is needed for absent values.
        assert_eq!(
            c.query_nearest(1., 0.5).unwrap().unwrap().adjusted_depth,
            10.
        );
    }
    #[test]
    fn mismatched_extent_and_unrepresentable_common_partition_rejected() {
        let (a, mut b) = grids();
        b.geometry.origin_x += 0.001;
        assert!(ConservativeCoverage::new(
            vec![
                DatumCoverage {
                    coverage: &a,
                    reference: DepthReference(1)
                },
                DatumCoverage {
                    coverage: &b,
                    reference: DepthReference(2)
                },
            ],
            DepthReference(1),
            &Adjust
        )
        .is_err());
        assert!(lcm(usize::MAX, 2).is_err());
    }
    #[test]
    fn one_source_keeps_original_geometry_and_numeric_values() {
        let (a, _) = grids();
        let identity = IdentityDepthAdjustment;
        let c = ConservativeCoverage::new(
            vec![DatumCoverage {
                coverage: &a,
                reference: DepthReference(1),
            }],
            DepthReference(1),
            &identity,
        )
        .unwrap();
        assert_eq!(c.geometry.origin_x.to_bits(), a.geometry.origin_x.to_bits());
        assert_eq!(
            c.geometry.spacing_x.to_bits(),
            a.geometry.spacing_x.to_bits()
        );
        let mut composed = Vec::new();
        let mut raw = Vec::new();
        let window = GridWindow {
            column: 0,
            row: 0,
            width: 3,
            height: 2,
        };
        c.visit_window_values(window, &mut |i, v| {
            composed.push((i, v));
            Ok(())
        })
        .unwrap();
        a.visit_window_values(window, &mut |i, v| {
            raw.push((i, v));
            Ok(())
        })
        .unwrap();
        assert_eq!(composed, raw);
    }
}

#[cfg(test)]
mod common_point_tests {
    use super::*;
    use ferrite_kernel::depth_selection::{DepthAdjustment, IdentityDepthAdjustment};
    use ferrite_kernel::{CoverageSample, CoverageTile};
    struct Grid {
        g: GridGeometry,
        values: Vec<f32>,
    }
    impl CoverageSource for Grid {
        fn geometry(&self) -> &GridGeometry {
            &self.g
        }
        fn read_window(&self, w: GridWindow) -> Result<CoverageTile> {
            w.validate(&self.g)?;
            Ok(CoverageTile {
                window: w,
                samples: (w.row..w.row + w.height)
                    .flat_map(|row| {
                        (w.column..w.column + w.width).map(move |col| CoverageSample {
                            value: Some(self.values[row * self.g.width + col]),
                            uncertainty: None,
                        })
                    })
                    .collect(),
            })
        }
    }
    #[test]
    fn exact_shared_boundaries_use_low_not_axis_or_draw_order() {
        let provider = IdentityDepthAdjustment;
        for reverse in [false, true] {
            let grid = Grid {
                g: GridGeometry {
                    width: 2,
                    height: 1,
                    origin_x: if reverse { 1.5 } else { 0.5 },
                    origin_y: 0.5,
                    spacing_x: if reverse { -1. } else { 1. },
                    spacing_y: 1.,
                    horizontal_crs: 4326,
                },
                values: if reverse {
                    vec![5., 20.]
                } else {
                    vec![20., 5.]
                },
            };
            let c = ConservativeCoverage::new(
                vec![DatumCoverage {
                    coverage: &grid,
                    reference: DepthReference(1),
                }],
                DepthReference(1),
                &provider,
            )
            .unwrap();
            assert_eq!(
                c.query_nearest(1., 0.5).unwrap().unwrap().adjusted_depth,
                5.
            );
            assert_eq!(
                c.query_nearest(f64::from_bits(1f64.to_bits() - 1), 0.5)
                    .unwrap()
                    .unwrap()
                    .adjusted_depth,
                20.
            );
            assert_eq!(
                c.query_nearest(f64::from_bits(1f64.to_bits() + 1), 0.5)
                    .unwrap()
                    .unwrap()
                    .adjusted_depth,
                5.
            );
            assert_eq!(
                c.query_nearest(0., 0.5).unwrap().unwrap().adjusted_depth,
                20.
            );
            assert_eq!(
                c.query_nearest(2., 0.5).unwrap().unwrap().adjusted_depth,
                5.
            );
            assert!(c.query_nearest(-f64::MIN_POSITIVE, 0.5).unwrap().is_none());
        }
        let grid = Grid {
            g: GridGeometry {
                width: 2,
                height: 2,
                origin_x: 0.5,
                origin_y: 0.5,
                spacing_x: 1.,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
            values: vec![20., 3., 5., 10.],
        };
        let c = ConservativeCoverage::new(
            vec![DatumCoverage {
                coverage: &grid,
                reference: DepthReference(1),
            }],
            DepthReference(1),
            &provider,
        )
        .unwrap();
        let q = c.query_nearest(1., 1.).unwrap().unwrap();
        assert_eq!(q.adjusted_depth, 3.);
        assert_eq!((q.candidate.source.column, q.candidate.source.row), (1, 0));
    }
    #[test]
    fn artificial_partition_boundary_is_not_an_original_cell_boundary() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Provider(AtomicUsize);
        impl DepthAdjustmentProvider for Provider {
            fn adjustment(
                &self,
                _: DepthReference,
                _: DepthReference,
                x: f64,
                y: f64,
            ) -> Result<Option<DepthAdjustment>> {
                assert_eq!((x, y), (0.25, 0.5));
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(Some(DepthAdjustment {
                    correction_metres: x,
                    provenance: 1,
                }))
            }
        }
        let a = Grid {
            g: GridGeometry {
                width: 3,
                height: 1,
                origin_x: 0.5,
                origin_y: 0.5,
                spacing_x: 1.,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
            values: vec![10.; 3],
        };
        let b = Grid {
            g: GridGeometry {
                width: 4,
                height: 1,
                origin_x: 0.375,
                origin_y: 0.5,
                spacing_x: 0.75,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
            values: vec![20.; 4],
        };
        let provider = Provider(AtomicUsize::new(0));
        let c = ConservativeCoverage::new(
            vec![
                DatumCoverage {
                    coverage: &a,
                    reference: DepthReference(1),
                },
                DatumCoverage {
                    coverage: &b,
                    reference: DepthReference(2),
                },
            ],
            DepthReference(1),
            &provider,
        )
        .unwrap();
        assert_eq!(
            c.query_nearest(0.25, 0.5).unwrap().unwrap().adjusted_depth,
            10.25
        );
        assert_eq!(provider.0.load(Ordering::Relaxed), 2);
    }
}
