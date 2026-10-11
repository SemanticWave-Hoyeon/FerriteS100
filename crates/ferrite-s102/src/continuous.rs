//! Retained original-source candidates for continuous domain selection.
//! No composed centroid value is stored. All colours are classified in f64 on the
//! host; the GPU compares integer ranks only. Corrections are explicitly constant.
use crate::{
    BathymetryCoverage, BathymetryPortrayal, ConservativeCoverage, DatumCoverage, InstanceDomain,
};
use anyhow::{ensure, Context, Result};
use ferrite_kernel::depth_selection::{
    DepthAdjustment, DepthAdjustmentProvider, DepthCandidate, DepthReference, DepthSource,
    SelectedDepth, ShoalestDepth,
};
use ferrite_kernel::{
    CoverageSource, CoverageTile, GridGeometry, GridWindow, NumericCoverageSource,
};
use ferrite_render::{GeoBounds, RasterGrid};

pub const MAX_SOURCES: usize = 32;
pub const MAX_DOMAIN_VERTICES: usize = 4096;
pub const MAX_CANDIDATE_NODES: usize = 2 * 1024 * 1024;
pub const MAX_ATLAS_WORDS: usize = 4 * 1024 * 1024;
pub const SELECTOR_MAGIC: u32 = 0x53444331;
pub const SELECTOR_OFFSET: u32 = 1048576;
const HEADER: usize = 16;
const SOURCE_HEADER: usize = 16;

/// Host input: spatial constancy is part of this type's contract. This is not
/// transformation evidence inferred from a vertical-datum classification code.
#[derive(Clone)]
pub struct ConstantDatumAdjustments {
    target: DepthReference,
    corrections: Vec<(DepthReference, DepthAdjustment)>,
}
impl ConstantDatumAdjustments {
    pub fn new(
        target: DepthReference,
        corrections: Vec<(DepthReference, DepthAdjustment)>,
    ) -> Result<Self> {
        let mut references = std::collections::HashSet::new();
        for (reference, adjustment) in &corrections {
            ensure!(
                *reference != target && references.insert(reference.0),
                "Duplicate or identity correction override"
            );
            ensure!(
                adjustment.correction_metres.is_finite(),
                "Nonfinite constant correction"
            );
        }
        Ok(Self {
            target,
            corrections,
        })
    }
}
impl DepthAdjustmentProvider for ConstantDatumAdjustments {
    fn adjustment(
        &self,
        from: DepthReference,
        to: DepthReference,
        _: f64,
        _: f64,
    ) -> Result<Option<DepthAdjustment>> {
        ensure!(to == self.target, "Constant correction target mismatch");
        Ok(if from == to {
            Some(DepthAdjustment {
                correction_metres: 0.,
                provenance: 0,
            })
        } else {
            self.corrections
                .iter()
                .find(|(reference, _)| *reference == from)
                .map(|(_, correction)| *correction)
        })
    }
}
struct CapturedSource {
    geometry: GridGeometry,
    domain: InstanceDomain,
    tile: CoverageTile,
    selected: Vec<Option<SelectedDepth>>,
    colours: Vec<[u8; 4]>,
    ranks: Vec<u32>,
}
/// A bounded packet for one common-grid window plus original-cell boundary halo.
/// Exact CPU query uses the original robust domain and closed-cell predicates.
/// GPU qualification is separate: it must not claim predicate parity with f64.
pub struct ContinuousDepthTile {
    pub grid: RasterGrid,
    pub size: [u32; 2],
    pub bounds: GeoBounds,
    sources: Vec<CapturedSource>,
    policy: ConstantDatumAdjustments,
}
/// RGBA8Unorm texels are lossless little-endian u32 words, not an RGBA image.
/// Consumers must use the selector shader and must never premultiply this payload.
pub struct ContinuousSelectorAtlas {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
    pub max_coordinate_rounding: f64,
    pub max_domain_coordinate: f64,
    pub candidate_nodes: usize,
    pub domain_vertices: usize,
}
fn canonical_window(g: &GridGeometry, w: GridWindow) -> GridWindow {
    GridWindow {
        column: if g.spacing_x > 0. {
            w.column
        } else {
            g.width - w.column - w.width
        },
        row: if g.spacing_y < 0. {
            w.row
        } else {
            g.height - w.row - w.height
        },
        ..w
    }
}
fn original_window(g: &GridGeometry, w: GridWindow) -> GridWindow {
    canonical_window(g, w)
}
fn relative_index(w: GridWindow, column: usize, row: usize) -> Result<usize> {
    ensure!(
        column >= w.column && row >= w.row && column - w.column < w.width && row - w.row < w.height,
        "Continuous query outside retained original-cell halo"
    );
    Ok((row - w.row) * w.width + column - w.column)
}
fn compare(a: &SelectedDepth, b: &SelectedDepth) -> std::cmp::Ordering {
    // IEEE signed zeros compare equal, matching ShoalestDepth's semantic tie rule.
    a.adjusted_depth
        .partial_cmp(&b.adjusted_depth)
        .unwrap()
        .then_with(|| a.candidate.source.cmp(&b.candidate.source))
}
impl ContinuousDepthTile {
    pub fn capture(
        coverages: &[BathymetryCoverage],
        policy: ConstantDatumAdjustments,
        window: GridWindow,
        portrayal: &BathymetryPortrayal,
    ) -> Result<Self> {
        ensure!(
            !coverages.is_empty() && coverages.len() <= MAX_SOURCES,
            "Continuous selector source-count limit"
        );
        let mosaic = ConservativeCoverage::new(
            coverages
                .iter()
                .map(|coverage| DatumCoverage {
                    coverage,
                    reference: DepthReference(coverage.vertical_datum as u64),
                })
                .collect(),
            policy.target,
            &policy,
        )?;
        let common = *mosaic.numeric_geometry();
        window.validate(&common)?;
        ensure!(
            common.horizontal_crs == 4326,
            "Continuous portrayal requires EPSG:4326"
        );
        ensure!(
            window
                .width
                .checked_mul(window.height)
                .is_some_and(|n| n <= 65536),
            "Continuous tile exceeds 65536 common cells"
        );
        let cw = canonical_window(&common, window);
        let extent = crate::grid_extent(&common)?;
        let bound = |v: crate::GridBoundary| -> Result<f64> { Ok(v.rounded) };
        let bounds = GeoBounds::new(
            bound(extent[0])?,
            bound(extent[2])?,
            bound(extent[1])?,
            bound(extent[3])?,
        );
        let grid = RasterGrid {
            bounds,
            width: u32::try_from(common.width)?,
            height: u32::try_from(common.height)?,
            column: u32::try_from(cw.column)?,
            row: u32::try_from(cw.row)?,
        };
        let mut sources = Vec::new();
        let mut nodes = 0usize;
        for (instance, coverage) in coverages.iter().enumerate() {
            let g = *coverage.geometry();
            let rx = common.width / g.width;
            let ry = common.height / g.height;
            let x0 = (cw.column / rx).saturating_sub(1);
            let y0 = (cw.row / ry).saturating_sub(1);
            let x1 = ((cw.column + cw.width - 1) / rx + 2).min(g.width);
            let y1 = ((cw.row + cw.height - 1) / ry + 2).min(g.height);
            let sw = original_window(
                &g,
                GridWindow {
                    column: x0,
                    row: y0,
                    width: x1 - x0,
                    height: y1 - y0,
                },
            );
            nodes = nodes
                .checked_add(
                    sw.width
                        .checked_mul(sw.height)
                        .context("Candidate size overflow")?,
                )
                .context("Candidate count overflow")?;
            ensure!(
                nodes <= MAX_CANDIDATE_NODES,
                "Continuous candidate-node budget exceeded"
            );
            let tile = coverage.read_window(sw)?;
            ensure!(
                tile.window == sw && tile.samples.len() == sw.width * sw.height,
                "Invalid original source window"
            );
            let mut selected = Vec::with_capacity(tile.samples.len());
            let mut colours = Vec::with_capacity(tile.samples.len());
            for (index, sample) in tile.samples.iter().enumerate() {
                let column = sw.column + index % sw.width;
                let row = sw.row + index / sw.width;
                let (x, y) = g.position(column, row).context("Invalid source node")?;
                let mut one = ShoalestDepth::new(policy.target, x, y)?;
                one.consider(
                    DepthCandidate {
                        source: DepthSource {
                            instance,
                            column,
                            row,
                        },
                        reference: DepthReference(coverage.vertical_datum as u64),
                        x,
                        y,
                        raw_depth: sample.value.map(f64::from),
                        uncertainty: sample.uncertainty.map(f64::from),
                    },
                    &policy,
                )?;
                let value = one.finish()?;
                colours.push(portrayal.rgba_value(value.map(|s| s.adjusted_depth))?);
                selected.push(value);
            }
            let count = selected.len();
            sources.push(CapturedSource {
                geometry: g,
                domain: coverage.domain.clone(),
                tile,
                selected,
                colours,
                ranks: vec![u32::MAX; count],
            });
        }
        let mut order = Vec::with_capacity(nodes);
        for (source, s) in sources.iter().enumerate() {
            for (index, value) in s.selected.iter().enumerate() {
                if let Some(value) = value {
                    order.push((source, index, *value));
                }
            }
        }
        order.sort_by(|a, b| compare(&a.2, &b.2));
        for (rank, (source, index, _)) in order.into_iter().enumerate() {
            sources[source].ranks[index] = u32::try_from(rank)?;
        }
        let x = |column: usize| bounds.min_x + bounds.width() * column as f64 / common.width as f64;
        let y = |row: usize| bounds.max_y - bounds.height() * row as f64 / common.height as f64;
        Ok(Self {
            grid,
            size: [cw.width as u32, cw.height as u32],
            bounds: GeoBounds::new(
                x(cw.column),
                y(cw.row + cw.height),
                x(cw.column + cw.width),
                y(cw.row),
            ),
            sources,
            policy,
        })
    }
    pub fn query(&self, x: f64, y: f64) -> Result<Option<SelectedDepth>> {
        ensure!(
            x.is_finite()
                && y.is_finite()
                && x >= self.bounds.min_x
                && x <= self.bounds.max_x
                && y >= self.bounds.min_y
                && y <= self.bounds.max_y,
            "Continuous query outside tile"
        );
        let mut choice = ShoalestDepth::new(self.policy.target, x, y)?;
        for source in &self.sources {
            if !source.domain.contains(x, y) {
                continue;
            }
            let g = source.geometry;
            for column in super::composition::cells_at_axis(g.origin_x, g.spacing_x, g.width, x)?
                .into_iter()
                .flatten()
            {
                for row in super::composition::cells_at_axis(g.origin_y, g.spacing_y, g.height, y)?
                    .into_iter()
                    .flatten()
                {
                    let index = relative_index(source.tile.window, column, row)?;
                    if let Some(selected) = source.selected[index] {
                        choice.consider(selected.candidate, &self.policy)?;
                    }
                }
            }
        }
        choice.finish()
    }
    pub fn atlas(&self, max_dimension: u32) -> Result<ContinuousSelectorAtlas> {
        ensure!(max_dimension > 0, "Empty GPU texture limit");
        let mut words = vec![0u32; HEADER + self.sources.len() * SOURCE_HEADER];
        words[..10].copy_from_slice(&[
            SELECTOR_MAGIC,
            1,
            self.sources.len() as u32,
            self.grid.width,
            self.grid.height,
            self.grid.column,
            self.grid.row,
            self.size[0],
            self.size[1],
            HEADER as u32,
        ]);
        let mut max_error = 0f64;
        let mut max_coordinate = 0f64;
        let mut vertices = 0usize;
        let mut nodes = 0usize;
        let mut encode = |point: [f64; 2]| -> Result<[u32; 2]> {
            let p = [
                (point[0] - self.grid.bounds.min_x) / self.grid.bounds.width()
                    * self.grid.width as f64
                    - self.grid.column as f64,
                (self.grid.bounds.max_y - point[1]) / self.grid.bounds.height()
                    * self.grid.height as f64
                    - self.grid.row as f64,
            ];
            ensure!(
                p.iter().all(|v| v.is_finite()),
                "Nonfinite domain source coordinates"
            );
            let q = p.map(|v| v as f32);
            ensure!(
                q.iter().all(|v| v.is_finite()),
                "Domain coordinate cast overflow"
            );
            for i in 0..2 {
                max_error = max_error.max((f64::from(q[i]) - p[i]).abs());
                max_coordinate = max_coordinate.max(p[i].abs());
            }
            Ok(q.map(f32::to_bits))
        };
        for (instance, s) in self.sources.iter().enumerate() {
            let cw = canonical_window(&s.geometry, s.tile.window);
            let points: Vec<[f64; 2]> = match &s.domain {
                InstanceDomain::FullGrid => Vec::new(),
                InstanceDomain::Rectangle(b) => {
                    vec![[b[0], b[2]], [b[1], b[2]], [b[1], b[3]], [b[0], b[3]]]
                }
                InstanceDomain::Polygon(p) => p.points()[..p.points().len() - 1].to_vec(),
            };
            vertices = vertices
                .checked_add(points.len())
                .context("Domain vertex count overflow")?;
            ensure!(
                vertices <= MAX_DOMAIN_VERTICES,
                "Aggregate continuous domain vertex budget exceeded"
            );
            let point_offset = u32::try_from(words.len())?;
            for point in &points {
                words.extend(encode(*point)?);
            }
            let data_offset = u32::try_from(words.len())?;
            nodes += s.selected.len();
            for row in cw.row..cw.row + cw.height {
                for column in cw.column..cw.column + cw.width {
                    let original_column = if s.geometry.spacing_x > 0. {
                        column
                    } else {
                        s.geometry.width - 1 - column
                    };
                    let original_row = if s.geometry.spacing_y < 0. {
                        row
                    } else {
                        s.geometry.height - 1 - row
                    };
                    let index = relative_index(s.tile.window, original_column, original_row)?;
                    let mut rgba = s.colours[index];
                    for channel in 0..3 {
                        rgba[channel] = ((rgba[channel] as u16 * rgba[3] as u16 + 127) / 255) as u8;
                    }
                    words.push(s.ranks[index]);
                    words.push(u32::from_le_bytes(rgba));
                }
            }
            ensure!(
                words.len() <= MAX_ATLAS_WORDS,
                "Continuous selector atlas budget exceeded"
            );
            let h = HEADER + instance * SOURCE_HEADER;
            words[h..h + 13].copy_from_slice(&[
                self.grid.width / s.geometry.width as u32,
                self.grid.height / s.geometry.height as u32,
                s.geometry.width as u32,
                s.geometry.height as u32,
                cw.column as u32,
                cw.row as u32,
                cw.width as u32,
                cw.height as u32,
                data_offset,
                point_offset,
                points.len() as u32,
                u32::from(s.geometry.spacing_x < 0.) | u32::from(s.geometry.spacing_y > 0.) << 1,
                instance as u32,
            ]);
        }
        drop(encode);
        let outward = |value: f64| {
            let rounded = value as f32;
            if f64::from(rounded) < value {
                f32::from_bits(rounded.to_bits() + 1)
            } else {
                rounded
            }
        };
        words[10] = outward(max_error).to_bits();
        words[11] = words.len() as u32;
        words[12] = outward(max_coordinate).to_bits();
        let width = max_dimension.min(1024);
        let height = u32::try_from(words.len().div_ceil(width as usize))?;
        ensure!(
            height <= max_dimension,
            "Continuous selector exceeds GPU texture dimension; use smaller common-grid tiles"
        );
        words.resize(width as usize * height as usize, 0);
        let bytes = words.into_iter().flat_map(u32::to_le_bytes).collect();
        Ok(ContinuousSelectorAtlas {
            width,
            height,
            bytes,
            max_coordinate_rounding: max_error,
            max_domain_coordinate: max_coordinate,
            candidate_nodes: nodes,
            domain_vertices: vertices,
        })
    }
}

impl ContinuousSelectorAtlas {
    /// Pre-draw qualification requires independent projection/interpolant evidence.
    /// Do not invent that bound from source centres or use an unchecked zero.
    /// The caller must validate this across the actual visible mesh/viewport.
    pub fn qualify_screen_error(
        &self,
        minimum_source_units_per_pixel: f64,
        coordinate_error_source_units: f64,
        projection_error_pixels: f64,
        maximum_local_coordinate: f64,
    ) -> Result<()> {
        ensure!(
            [
                minimum_source_units_per_pixel,
                coordinate_error_source_units,
                projection_error_pixels,
                maximum_local_coordinate
            ]
            .iter()
            .all(|v| v.is_finite())
                && minimum_source_units_per_pixel > 0.
                && coordinate_error_source_units >= 0.
                && projection_error_pixels >= 0.
                && maximum_local_coordinate >= 0.,
            "Invalid selector screen-error evidence"
        );
        let epsilon = 2. * self.max_coordinate_rounding
            + coordinate_error_source_units
            + 16.
                * f64::from(f32::EPSILON)
                * self
                    .max_domain_coordinate
                    .max(maximum_local_coordinate)
                    .max(1.);
        ensure!(
            epsilon < 0.25,
            "Selector uncertainty spans multiple original-cell boundaries"
        );
        ensure!(
            projection_error_pixels <= 1. / 64.
                && epsilon / minimum_source_units_per_pixel <= 1. / 64.,
            "Continuous selector exceeds 1/32-pixel total geometry/material error budget"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_kernel::{coverage_domain::SimpleRing, CoverageSample};
    fn fixture() -> ContinuousDepthTile {
        let geometry = GridGeometry {
            origin_x: 0.5,
            origin_y: 0.5,
            spacing_x: 1.,
            spacing_y: 1.,
            width: 2,
            height: 2,
            horizontal_crs: 4326,
        };
        let policy = ConstantDatumAdjustments::new(
            DepthReference(10),
            vec![(
                DepthReference(23),
                DepthAdjustment {
                    correction_metres: -4.,
                    provenance: 99,
                },
            )],
        )
        .unwrap();
        // Domain cuts a cell before its centroid. Its shallower original candidate
        // must remain selectable there even though no centroid is within it.
        let narrow = InstanceDomain::Polygon(
            SimpleRing::new(vec![[0., 0.], [0.25, 0.], [0.25, 2.], [0., 2.], [0., 0.]]).unwrap(),
        );
        let window = GridWindow {
            column: 0,
            row: 0,
            width: 2,
            height: 2,
        };
        let mut sources = Vec::new();
        for (instance, (domain, reference, depths)) in [
            (narrow, DepthReference(23), [7., 7., 7., 7.]),
            (
                InstanceDomain::FullGrid,
                DepthReference(10),
                [10., 1., 10., 10.],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut selected = Vec::new();
            for (index, depth) in depths.iter().enumerate() {
                let column = index % 2;
                let row = index / 2;
                let (x, y) = geometry.position(column, row).unwrap();
                let mut selector = ShoalestDepth::new(policy.target, x, y).unwrap();
                selector
                    .consider(
                        DepthCandidate {
                            source: DepthSource {
                                instance,
                                column,
                                row,
                            },
                            reference,
                            x,
                            y,
                            raw_depth: Some(*depth),
                            uncertainty: Some(0.),
                        },
                        &policy,
                    )
                    .unwrap();
                selected.push(selector.finish().unwrap());
            }
            let mut ranks = Vec::new();
            for value in &selected {
                ranks.push(if value.unwrap().adjusted_depth == 1. {
                    0
                } else if instance == 0 {
                    1
                } else {
                    2
                });
            }
            sources.push(CapturedSource {
                geometry,
                domain,
                tile: CoverageTile {
                    window,
                    samples: depths
                        .map(|value| CoverageSample {
                            value: Some(value as f32),
                            uncertainty: Some(0.),
                        })
                        .to_vec(),
                },
                selected,
                colours: vec![
                    if instance == 0 {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    };
                    4
                ],
                ranks,
            });
        }
        ContinuousDepthTile {
            grid: RasterGrid {
                bounds: GeoBounds::new(0., 0., 2., 2.),
                width: 2,
                height: 2,
                column: 0,
                row: 0,
            },
            size: [2, 2],
            bounds: GeoBounds::new(0., 0., 2., 2.),
            sources,
            policy,
        }
    }
    #[test]
    fn narrow_domain_retains_pre_centroid_winner_and_original_provenance() {
        let packet = fixture();
        let a = packet.query(0.1, 0.5).unwrap().unwrap();
        assert_eq!(a.candidate.source.instance, 0);
        assert_eq!(a.candidate.raw_depth, Some(7.));
        assert_eq!(a.adjusted_depth, 3.);
        assert_eq!(a.adjustment.provenance, 99);
        assert_eq!(
            packet
                .query(0.5, 0.5)
                .unwrap()
                .unwrap()
                .candidate
                .source
                .instance,
            1
        );
        assert_eq!(
            packet
                .query(0.25, 0.5)
                .unwrap()
                .unwrap()
                .candidate
                .source
                .instance,
            0
        );
        assert_eq!(
            packet
                .query(f64::from_bits(0.25f64.to_bits() + 1), 0.5)
                .unwrap()
                .unwrap()
                .candidate
                .source
                .instance,
            1
        );
    }
    #[test]
    fn original_closed_cell_boundary_selects_incident_minimum() {
        let packet = fixture();
        let selected = packet.query(1., 0.5).unwrap().unwrap();
        assert_eq!(
            selected.candidate.source,
            DepthSource {
                instance: 1,
                column: 1,
                row: 0
            }
        );
        assert_eq!(selected.adjusted_depth, 1.);
        assert!(packet.query(-0.001, 0.5).is_err());
    }
    #[test]
    fn packet_preserves_all_sources_and_qualified_material_bounds() {
        let packet = fixture();
        let atlas = packet.atlas(64).unwrap();
        assert_eq!(atlas.candidate_nodes, 8);
        assert_eq!(atlas.domain_vertices, 4);
        let words: Vec<_> = atlas
            .bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        assert_eq!(words[0], SELECTOR_MAGIC);
        assert_eq!(words[2], 2);
        assert_eq!(&words[HEADER..HEADER + 4], &[1, 1, 2, 2]);
        assert!(atlas.qualify_screen_error(1., 0., 0., 2.).is_ok());
        assert!(atlas.qualify_screen_error(1., 0.5, 0., 2.).is_err());
        assert!(atlas.qualify_screen_error(1., 0., 0.1, 2.).is_err());
        assert!(packet.atlas(1).is_err());
    }
    #[test]
    fn missing_adjustment_and_identity_override_are_explicit_errors() {
        let provider = ConstantDatumAdjustments::new(DepthReference(10), Vec::new()).unwrap();
        assert!(provider
            .adjustment(DepthReference(23), DepthReference(10), 0., 0.)
            .unwrap()
            .is_none());
        assert!(ConstantDatumAdjustments::new(
            DepthReference(10),
            vec![(
                DepthReference(10),
                DepthAdjustment {
                    correction_metres: 0.,
                    provenance: 0
                }
            )]
        )
        .is_err());
    }
}
