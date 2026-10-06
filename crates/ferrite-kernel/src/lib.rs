//! Product-neutral chart kernel. Product encodings and application UI stay outside.
pub mod geocentric;
pub mod geodesy;
pub mod map_camera;
pub mod rhumb;
pub mod scale_policy;
pub mod triangulation;
pub mod whole_symbol;
pub mod depth_selection;
pub mod coverage_domain;

use anyhow::{ensure, Result};
#[derive(Debug, Clone, Copy)]
pub struct GridGeometry {
    pub width: usize,
    pub height: usize,
    pub origin_x: f64,
    pub origin_y: f64,
    pub spacing_x: f64,
    pub spacing_y: f64,
    pub horizontal_crs: u32,
}
impl GridGeometry {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.width > 0 && self.height > 0, "Empty coverage grid");
        ensure!(
            self.width.checked_mul(self.height).is_some(),
            "Grid size overflow"
        );
        ensure!(
            [self.origin_x, self.origin_y, self.spacing_x, self.spacing_y]
                .iter()
                .all(|n| n.is_finite()),
            "Non-finite grid geometry"
        );
        ensure!(
            self.spacing_x != 0.0 && self.spacing_y != 0.0,
            "Zero grid spacing"
        );
        Ok(())
    }
    pub fn position(&self, column: usize, row: usize) -> Option<(f64, f64)> {
        (column < self.width && row < self.height).then(|| {
            (
                self.origin_x + column as f64 * self.spacing_x,
                self.origin_y + row as f64 * self.spacing_y,
            )
        })
    }
    pub fn nearest(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let c = ((x - self.origin_x) / self.spacing_x + 0.5).floor();
        let r = ((y - self.origin_y) / self.spacing_y + 0.5).floor();
        (c >= 0.0 && r >= 0.0 && c < self.width as f64 && r < self.height as f64)
            .then(|| (c as usize, r as usize))
    }
}
/// Lazy row-major tiles; partial edge windows retain their original grid indices.
pub struct GridWindows {
    width: usize,
    height: usize,
    tile_width: usize,
    tile_height: usize,
    column: usize,
    row: usize,
}
impl Iterator for GridWindows {
    type Item = GridWindow;
    fn next(&mut self) -> Option<GridWindow> {
        if self.row >= self.height {
            return None;
        }
        let w = GridWindow {
            column: self.column,
            row: self.row,
            width: self.tile_width.min(self.width - self.column),
            height: self.tile_height.min(self.height - self.row),
        };
        self.column += w.width;
        if self.column == self.width {
            self.column = 0;
            self.row += w.height;
        }
        Some(w)
    }
}
impl GridGeometry {
    pub fn windows(&self, tile_width: usize, tile_height: usize) -> Result<GridWindows> {
        self.validate()?;
        ensure!(
            tile_width > 0 && tile_height > 0,
            "Tile dimensions must be positive"
        );
        Ok(GridWindows {
            width: self.width,
            height: self.height,
            tile_width,
            tile_height,
            column: 0,
            row: 0,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridWindow {
    pub column: usize,
    pub row: usize,
    pub width: usize,
    pub height: usize,
}
impl GridWindow {
    pub fn validate(&self, g: &GridGeometry) -> Result<()> {
        ensure!(self.width > 0 && self.height > 0, "Empty grid window");
        ensure!(
            self.column
                .checked_add(self.width)
                .is_some_and(|n| n <= g.width)
                && self
                    .row
                    .checked_add(self.height)
                    .is_some_and(|n| n <= g.height),
            "Grid window outside coverage"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct CoverageSample {
    pub value: Option<f32>,
    pub uncertainty: Option<f32>,
}
/// A nearest-node query keeps the source sample and its actual grid position together.
/// Coordinates use the coverage CRS; no spatial interpolation is performed.
#[derive(Debug, Clone, Copy)]
pub struct CoverageQuery {
    pub column: usize,
    pub row: usize,
    pub x: f64,
    pub y: f64,
    pub sample: CoverageSample,
}

/// Source node and the displayed geographic longitude copy remain separate.
#[derive(Debug, Clone, Copy)]
pub struct WrappedCoverageQuery {
    pub query: CoverageQuery,
    pub longitude_shift: f64,
}

#[derive(Debug)]
pub struct CoverageTile {
    pub window: GridWindow,
    pub samples: Vec<CoverageSample>,
}
/// Read a bounded coverage window in row-major order; origin is node (0,0).
/// Missing data must remain absent, never replaced by an estimated numeric value.
pub trait CoverageSource: Send + Sync {
    fn geometry(&self) -> &GridGeometry;
    fn read_window(&self, window: GridWindow) -> Result<CoverageTile>;
    /// Native-CRS continuous validity, evaluated at query position, not node centre.
    fn is_valid_position(&self,x:f64,y:f64)->bool {x.is_finite() && y.is_finite()}
    /// A cell texture alone cannot represent a domain cutting cell interiors.
    fn requires_geometric_mask(&self)->bool {false}
    fn query_nearest(&self, x: f64, y: f64) -> Result<Option<CoverageQuery>> {
        if !self.is_valid_position(x,y) {return Ok(None);}
        let Some((column, row)) = self.geometry().nearest(x, y) else {
            return Ok(None);
        };
        let (node_x, node_y) = self
            .geometry()
            .position(column, row)
            .ok_or_else(|| anyhow::anyhow!("Invalid nearest-node position"))?;
        let tile = self.read_window(GridWindow {
            column,
            row,
            width: 1,
            height: 1,
        })?;
        ensure!(
            tile.samples.len() == 1,
            "Nearest-node read must return exactly one sample"
        );
        Ok(Some(CoverageQuery {
            column,
            row,
            x: node_x,
            y: node_y,
            sample: tile.samples[0],
        }))
    }
    /// Query only the longitude copies actually drawn (0/-360/+360).
    /// Wrapping is defined here for EPSG:4326, never for projected coordinates.
    fn query_nearest_wrapped(
        &self,
        x: f64,
        y: f64,
        wrapping: bool,
    ) -> Result<Option<WrappedCoverageQuery>> {
        ensure!(
            !wrapping || self.geometry().horizontal_crs == 4326,
            "Longitude wrapping requires EPSG:4326"
        );
        let shifts = [0., -360., 360.];
        for &shift in &shifts[..if wrapping { 3 } else { 1 }] {
            if let Some(query) = self.query_nearest(x - shift, y)? {
                return Ok(Some(WrappedCoverageQuery {
                    query,
                    longitude_shift: shift,
                }));
            }
        }
        Ok(None)
    }
    fn sample_nearest(&self, x: f64, y: f64) -> Result<Option<CoverageSample>> {
        Ok(self.query_nearest(x, y)?.map(|query| query.sample))
    }
}

/// Numeric portrayal input retains f64 corrections without changing raw file sample
/// types. Visit exactly width*height samples in row-major order, indexed from zero.
/// Implementations must bound their temporary reads; callers bound the output window.
pub trait NumericCoverageSource: Send + Sync {
    fn numeric_geometry(&self) -> &GridGeometry;
    fn requires_spatial_mask(&self)->bool {false}
    fn visit_window_values(
        &self,
        window: GridWindow,
        visitor: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
    ) -> Result<()>;
}

/// Raw sources need no second f64 sample buffer. Existing 128-row reads are retained.
impl<T: CoverageSource + ?Sized> NumericCoverageSource for T {
    fn numeric_geometry(&self) -> &GridGeometry { self.geometry() }
    fn requires_spatial_mask(&self)->bool {self.requires_geometric_mask()}
    fn visit_window_values(
        &self,
        window: GridWindow,
        visitor: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
    ) -> Result<()> {
        window.validate(self.geometry())?;
        for row in (0..window.height).step_by(128) {
            let part = GridWindow {
                column: window.column, row: window.row + row,
                width: window.width, height: 128.min(window.height - row),
            };
            let tile = self.read_window(part)?;
            ensure!(tile.window == part, "Coverage returned a different window");
            ensure!(tile.samples.len() == part.width * part.height,
                "Coverage tile sample count differs");
            for (index, sample) in tile.samples.iter().enumerate() {
                let p=self.geometry().position(part.column+index%part.width,part.row+index/part.width).ok_or_else(||anyhow::anyhow!("Invalid coverage node"))?;
                visitor(row * window.width + index, if self.is_valid_position(p.0,p.1) {sample.value.map(f64::from)}else{None})?;
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_axis_and_edge_queries() {
        let g = GridGeometry {
            width: 3,
            height: 2,
            origin_x: 10.0,
            origin_y: 20.0,
            spacing_x: -2.0,
            spacing_y: 1.0,
            horizontal_crs: 4326,
        };
        g.validate().unwrap();
        assert_eq!(g.position(2, 1), Some((6.0, 21.0)));
        assert_eq!(g.nearest(6.0, 21.0), Some((2, 1)));
        assert_eq!(g.nearest(3.0, 21.0), None);
        assert_eq!(g.nearest(11.0, 19.5), Some((0, 0)));
        assert_eq!(g.nearest(5.0, 20.0), None);
        assert_eq!(g.nearest(f64::NAN, 20.0), None);
        assert!(GridWindow {
            column: usize::MAX,
            row: 0,
            width: 2,
            height: 1
        }
        .validate(&g)
        .is_err());
    }
}

/// S-100 Part 1, 1-4.5.3.4 interval closures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum IntervalClosure {
    Open,
    Closed,
    LeftClosed,
    RightClosed,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
}
impl IntervalClosure {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "openInterval" => Self::Open,
            "closedInterval" => Self::Closed,
            "geLtInterval" => Self::LeftClosed,
            "gtLeInterval" => Self::RightClosed,
            "gtSemiInterval" => Self::Greater,
            "geSemiInterval" => Self::GreaterEqual,
            "ltSemiInterval" => Self::Less,
            "leSemiInterval" => Self::LessEqual,
            _ => anyhow::bail!("Unknown S-100 interval closure: {s}"),
        })
    }
    pub fn contains(self, value: f64, lower: f64, upper: f64) -> bool {
        if !value.is_finite() {
            return false;
        }
        match self {
            Self::Open => value > lower && value < upper,
            Self::Closed => value >= lower && value <= upper,
            Self::LeftClosed => value >= lower && value < upper,
            Self::RightClosed => value > lower && value <= upper,
            Self::Greater => value > lower,
            Self::GreaterEqual => value >= lower,
            Self::Less => value < upper,
            Self::LessEqual => value <= upper,
        }
    }
}

mod selection;
pub use selection::{closest_on_path, inside_ring};

mod temporal;
pub use temporal::{TemporalBounds, TemporalInterval};

mod date_visibility;
pub use date_visibility::{date_intervals_visible, parse_viewing_date, validate_s100_date};

mod clock_visibility;
pub use clock_visibility::{
    next_temporal_change_after, parse_local_time_offset, parse_viewing_instant,
    temporal_intervals_visible, temporal_intervals_visible_with_offset, ViewingInstant,
};

#[cfg(test)]
mod located_coverage_tests {
    use super::*;
    struct Source {
        grid: GridGeometry,
        samples: Vec<CoverageSample>,
    }
    impl CoverageSource for Source {
        fn geometry(&self) -> &GridGeometry {
            &self.grid
        }
        fn read_window(&self, w: GridWindow) -> Result<CoverageTile> {
            w.validate(&self.grid)?;
            Ok(CoverageTile {
                window: w,
                samples: vec![self.samples[w.row * self.grid.width + w.column]],
            })
        }
    }
    #[test]
    fn query_retains_node_position_and_missing_values_on_negative_axis() {
        let s = Source {
            grid: GridGeometry {
                width: 2,
                height: 1,
                origin_x: 10.,
                origin_y: 20.,
                spacing_x: -2.,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
            samples: vec![
                CoverageSample {
                    value: Some(7.),
                    uncertainty: None,
                },
                CoverageSample {
                    value: None,
                    uncertainty: Some(0.4),
                },
            ],
        };
        let q = s.query_nearest(8.4, 20.24).unwrap().unwrap();
        assert_eq!((q.column, q.row, q.x, q.y), (1, 0, 8., 20.));
        assert!(q.sample.value.is_none());
        assert_eq!(q.sample.uncertainty, Some(0.4));
        let q = s.query_nearest(10.4, 20.).unwrap().unwrap();
        assert_eq!((q.column, q.row), (0, 0));
        assert_eq!(q.sample.value, Some(7.));
        assert!(s.query_nearest(4., 20.).unwrap().is_none());
        assert!(s.query_nearest(f64::NAN, 20.).unwrap().is_none());
        assert_eq!(s.sample_nearest(10., 20.).unwrap().unwrap().value, Some(7.));
    }
    #[test]
    fn geographic_copies_preserve_source_nodes_and_fill_without_projected_crs_wrapping() {
        let mut s = Source {
            grid: GridGeometry {
                width: 2,
                height: 1,
                origin_x: 10.,
                origin_y: 20.,
                spacing_x: -2.,
                spacing_y: 1.,
                horizontal_crs: 4326,
            },
            samples: vec![
                CoverageSample {
                    value: Some(7.),
                    uncertainty: None,
                },
                CoverageSample {
                    value: None,
                    uncertainty: Some(0.4),
                },
            ],
        };
        for shift in [-360., 0., 360.] {
            let q = s
                .query_nearest_wrapped(8.4 + shift, 20.24, true)
                .unwrap()
                .unwrap();
            assert_eq!(q.longitude_shift, shift);
            assert_eq!(
                (q.query.column, q.query.row, q.query.x, q.query.y),
                (1, 0, 8., 20.)
            );
            assert!(q.query.sample.value.is_none());
            assert_eq!(q.query.sample.uncertainty, Some(0.4));
        }
        assert!(s
            .query_nearest_wrapped(368.4, 20., false)
            .unwrap()
            .is_none());
        assert!(s.query_nearest_wrapped(728.4, 20., true).unwrap().is_none());
        assert!(s
            .query_nearest_wrapped(f64::NAN, 20., true)
            .unwrap()
            .is_none());
        s.grid.horizontal_crs = 32630;
        assert!(s.query_nearest_wrapped(8.4, 20., true).is_err());
        assert!(s.query_nearest_wrapped(8.4, 20., false).unwrap().is_some());
    }
}

#[cfg(test)]
mod grid_tile_tests {
    use super::*;
    #[test]
    fn lazy_tiles_cover_every_node_once_with_partial_edges() {
        let g = GridGeometry {
            width: 7,
            height: 5,
            origin_x: 0.,
            origin_y: 0.,
            spacing_x: 1.,
            spacing_y: -1.,
            horizontal_crs: 4326,
        };
        let mut counts = vec![0; 35];
        let windows: Vec<_> = g.windows(3, 2).unwrap().collect();
        assert_eq!(windows.len(), 9);
        assert_eq!((windows[8].width, windows[8].height), (1, 1));
        for w in windows {
            w.validate(&g).unwrap();
            for y in w.row..w.row + w.height {
                for x in w.column..w.column + w.width {
                    counts[y * 7 + x] += 1;
                }
            }
        }
        assert!(counts.iter().all(|&x| x == 1));
        assert!(g.windows(0, 2).is_err());
        assert_eq!(g.windows(usize::MAX, usize::MAX).unwrap().count(), 1);
    }
}

/// Composition stage independent of product adapters and GPU APIs.
/// S-98 Annex A 4.4.1: non-ENC products use ordinary portrayal as overlays
/// when interoperability is off. This does not encode an IC display plane.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum CompositionStage {
    Chart,
    Overlay,
}

mod dash;
pub use dash::DashCycle;

mod stroke;
pub use stroke::{LineSymbolCrs, StrokeCap, StrokeDefinition, StrokeJoin, StrokeSymbol};

/// Product-neutral composition plane. IC order zero is reserved for radar;
/// product drawing planes use nonzero signed orders (S-100 5.2.1 Table 16-2).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct CompositionPlane {
    pub stage: CompositionStage,
    pub order: std::num::NonZeroI32,
}
impl CompositionPlane {
    pub const fn new(stage: CompositionStage, order: std::num::NonZeroI32) -> Self {
        Self { stage, order }
    }
}

mod version;
pub use version::SpecificationVersion;


pub mod surface_bounds;

pub mod spatial_hierarchy;

pub mod coverage_selection;

pub mod coverage_raster;

pub mod coverage_rendering;

pub mod coverage_frame;

pub mod portrayal_position;

pub mod line_offset;


pub mod longitude_extent;

pub mod projection;

pub mod exact_decimal;
pub use exact_decimal::ExactDecimal;

pub mod sequence_update;
