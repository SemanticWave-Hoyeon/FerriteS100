//! App orchestration for authenticated Part 16 plane rules; readers stay in product crates.
use anyhow::{ensure, Result};
use ferrite_interoperability::{Assignment, AuthenticatedCatalogue, Selection};
use ferrite_kernel::{GridGeometry, GridWindow, NumericCoverageSource};
use ferrite_render::{DisplayPlane, RasterDrawOrder, RasterLayer, RenderContext};

/// Level 2 context for one composition: the mariner's predefined combination
/// and where the replacing products have data coverage.
#[derive(Default)]
pub struct Level2 {
    pub selection: Selection,
    s102: Vec<CoverageOccupancy>,
}
impl Level2 {
    /// `combination` is the identifier the mariner selected; it applies only
    /// while all its products are displayed. Coverage is read only when the
    /// active combination suppresses a layer.
    pub fn new<'a>(
        ic: &AuthenticatedCatalogue,
        combination: Option<&str>,
        displayed: &[&str],
        s102: impl IntoIterator<Item = &'a dyn NumericCoverageSource>,
    ) -> Result<Self> {
        let selection = ic.catalogue.select(combination, displayed)?;
        let suppresses = ic
            .catalogue
            .active_combination(selection)
            .is_some_and(|c| c.suppressed.iter().any(|l| l.product == "S-101"));
        let s102 = if suppresses {
            s102.into_iter()
                .filter_map(|c| CoverageOccupancy::build(c).transpose())
                .collect::<Result<_>>()?
        } else {
            Vec::new()
        };
        Ok(Self { selection, s102 })
    }
    fn covers(&self, product: &str, extent: [f64; 4]) -> bool {
        // Only S-102 is composed with S-101 in this application.
        product == "S-102" && self.s102.iter().any(|c| c.covers(extent))
    }
}

/// Valid-node occupancy of one geographic S-102 grid in blocks of nodes. A
/// block counts only when every node in it holds data, so extents are judged
/// covered conservatively (never across fill values or masked nodes).
struct CoverageOccupancy {
    geometry: GridGeometry,
    /// None for a geographic (EPSG:4326) grid.
    projection: Option<ferrite_kernel::projection::Wgs84ProjectedCrs>,
    columns: usize,
    rows: usize,
    /// Prefix sums of incomplete blocks, (columns + 1) x (rows + 1).
    incomplete: Vec<u32>,
}
const OCCUPANCY_BLOCK: usize = 16;
impl CoverageOccupancy {
    /// Geographic grids, and WGS84 UTM/UPS grids through their projection;
    /// None for any other CRS, where no suppression is inferred.
    fn build(source: &dyn NumericCoverageSource) -> Result<Option<Self>> {
        let geometry = *source.numeric_geometry();
        let projection = match geometry.horizontal_crs {
            4326 => None,
            epsg => match ferrite_kernel::projection::Wgs84ProjectedCrs::from_epsg(epsg) {
                Ok(crs) => Some(crs),
                Err(_) => return Ok(None),
            },
        };
        geometry.validate()?;
        let columns = geometry.width.div_ceil(OCCUPANCY_BLOCK);
        let rows = geometry.height.div_ceil(OCCUPANCY_BLOCK);
        let mut complete = vec![true; columns * rows];
        source.visit_window_values(
            GridWindow {
                column: 0,
                row: 0,
                width: geometry.width,
                height: geometry.height,
            },
            &mut |index, value| {
                if value.is_none() {
                    let (c, r) = (index % geometry.width, index / geometry.width);
                    complete[(r / OCCUPANCY_BLOCK) * columns + c / OCCUPANCY_BLOCK] = false;
                }
                Ok(())
            },
        )?;
        let mut incomplete = vec![0u32; (columns + 1) * (rows + 1)];
        for r in 0..rows {
            for c in 0..columns {
                incomplete[(r + 1) * (columns + 1) + c + 1] = u32::from(!complete[r * columns + c])
                    + incomplete[r * (columns + 1) + c + 1]
                    + incomplete[(r + 1) * (columns + 1) + c]
                    - incomplete[r * (columns + 1) + c];
            }
        }
        Ok(Some(Self {
            geometry,
            projection,
            columns,
            rows,
            incomplete,
        }))
    }
    /// The geographic extent in grid coordinates. A projected extent is the
    /// bounding box of its densely sampled boundary, widened by one node so
    /// curvature between samples cannot shrink it.
    fn native_extent(&self, [west, south, east, north]: [f64; 4]) -> Option<[f64; 4]> {
        let Some(crs) = self.projection else {
            return Some([west, south, east, north]);
        };
        use ferrite_kernel::geodesy::GeographicPosition;
        const STEPS: usize = 16;
        let mut out = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for i in 0..=STEPS {
            let t = i as f64 / STEPS as f64;
            let (lon, lat) = (west + (east - west) * t, south + (north - south) * t);
            for (lon, lat) in [(lon, south), (lon, north), (west, lat), (east, lat)] {
                let p = crs
                    .project(GeographicPosition::new(lat, lon).ok()?)
                    .ok()
                    .filter(|p| p.within_crs_area)?
                    .position;
                out = [
                    out[0].min(p.easting()),
                    out[1].min(p.northing()),
                    out[2].max(p.easting()),
                    out[3].max(p.northing()),
                ];
            }
        }
        let (dx, dy) = (self.geometry.spacing_x.abs(), self.geometry.spacing_y.abs());
        Some([out[0] - dx, out[1] - dy, out[2] + dx, out[3] + dy])
    }
    fn covers(&self, extent: [f64; 4]) -> bool {
        let Some([west, south, east, north]) = self.native_extent(extent) else {
            return false;
        };
        let g = &self.geometry;
        // A node stands for the cell centred on it (GridGeometry::nearest).
        let column = |x: f64| (x - g.origin_x) / g.spacing_x + 0.5;
        let row = |y: f64| (y - g.origin_y) / g.spacing_y + 0.5;
        let (c0, c1) = (column(west), column(east));
        let (r0, r1) = (row(south), row(north));
        let (c0, c1) = (c0.min(c1).floor(), c0.max(c1).floor());
        let (r0, r1) = (r0.min(r1).floor(), r0.max(r1).floor());
        if !(c0 >= 0. && r0 >= 0. && c1 < g.width as f64 && r1 < g.height as f64) {
            return false;
        }
        let (bc0, bc1) = (c0 as usize / OCCUPANCY_BLOCK, c1 as usize / OCCUPANCY_BLOCK);
        let (br0, br1) = (r0 as usize / OCCUPANCY_BLOCK, r1 as usize / OCCUPANCY_BLOCK);
        debug_assert!(bc1 < self.columns && br1 < self.rows);
        let w = self.columns + 1;
        let sum = self.incomplete[(br1 + 1) * w + bc1 + 1] + self.incomplete[br0 * w + bc0]
            - self.incomplete[br0 * w + bc1 + 1]
            - self.incomplete[(br1 + 1) * w + bc0];
        sum == 0
    }
}
pub fn compose_vectors(
    ic: &AuthenticatedCatalogue,
    level2: &Level2,
    cells: &[ferrite_s100_core::S101Cell],
    fc: &ferrite_feature_catalog::FeatureCatalogue,
    portrayal: (&ferrite_portrayal_catalog::PortrayalCatalogue, &str),
    context: &mut RenderContext,
) -> Result<usize> {
    let mut instructions = context.raw_instructions().to_vec();
    let mut changed = 0;
    for (index, cell) in cells.iter().enumerate() {
        let plan = ferrite_s101::plan_interoperability_in(
            &ic.catalogue,
            level2.selection,
            cell,
            fc,
            &instructions,
            u32::try_from(index)?,
            &|product, extent| level2.covers(product, extent),
            Some(portrayal),
        )?;
        changed += plan.len();
        plan.apply(&mut instructions)?;
    }
    context.set_instructions_from_cache(instructions);
    Ok(changed)
}
/// Preserve one ordered instruction stream and original global source cell indexes.
/// Missing catalogue ownership is an error, never a global-FC fallback.
pub fn compose_vectors_for_cells(
    ic: &AuthenticatedCatalogue,
    level2: &Level2,
    cells: &[ferrite_s100_core::S101Cell],
    fcs: &[&ferrite_feature_catalog::BoundFeatureCatalogue],
    pcs: &[&ferrite_portrayal_catalog::PortrayalCatalogue],
    profile: &str,
    context: &mut RenderContext,
) -> Result<usize> {
    ensure!(
        cells.len() == pcs.len(),
        "IC cell portrayal owner count mismatch"
    );
    ensure!(
        cells.len() == fcs.len(),
        "IC cell catalogue owner count mismatch"
    );
    let catalogues: Vec<&ferrite_feature_catalog::FeatureCatalogue> =
        fcs.iter().map(|fc| &***fc).collect();
    let mut instructions = context.raw_instructions().to_vec();
    let plan = ferrite_s101::plan_interoperability_for_cells_in(
        &ic.catalogue,
        level2.selection,
        cells,
        &catalogues,
        &instructions,
        &|product, extent| level2.covers(product, extent),
        &pcs.iter()
            .map(|pc| Some((*pc, profile)))
            .collect::<Vec<_>>(),
    )?;
    let changed = plan.len();
    plan.apply(&mut instructions)?;
    context.set_instructions_from_cache(instructions);
    Ok(changed)
}
pub fn compose_raster(
    mut layer: RasterLayer,
    assignment: Option<&Assignment>,
) -> Result<RasterLayer> {
    if let Some(a) = assignment {
        ensure!(
            a.plane.stage == ferrite_kernel::CompositionStage::Chart && a.priority >= 0,
            "Invalid raster IC assignment"
        );
        // A coverage raster has no symbol or line style to substitute.
        ensure!(
            a.substitution.is_none(),
            "substituteSymbolization cannot apply to a raster coverage"
        );
        layer.draw_order = RasterDrawOrder {
            stage: a.plane.stage,
            display_plane: DisplayPlane::Interoperability(a.plane.order),
            priority: a.priority,
        };
        layer.viewing_groups = vec![a.viewing_group];
    }
    Ok(layer)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projected_grids_judge_coverage_in_their_own_crs() {
        // UTM 30N grid around (50.75N, 1.35W): 2 km x 1 km at 20 m spacing.
        let crs = ferrite_kernel::projection::Wgs84ProjectedCrs::from_epsg(32630).unwrap();
        let centre = crs
            .project(ferrite_kernel::geodesy::GeographicPosition::new(50.75, -1.35).unwrap())
            .unwrap()
            .position;
        let g = Grid(GridGeometry {
            width: 100,
            height: 50,
            origin_x: centre.easting() - 1000.,
            origin_y: centre.northing() - 500.,
            spacing_x: 20.,
            spacing_y: 20.,
            horizontal_crs: 32630,
        });
        let o = CoverageOccupancy::build(&g).unwrap().unwrap();
        // ~140 m square near the centre, clear of the fill node at (40, 5).
        assert!(o.covers([-1.351, 50.7495, -1.349, 50.7505]));
        // Larger than the grid: not covered.
        assert!(!o.covers([-1.4, 50.7, -1.3, 50.8]));
    }
    /// 64 x 32 geographic grid at 0.01° from (129, 35); one fill node at (40, 5).
    struct Grid(GridGeometry);
    impl NumericCoverageSource for Grid {
        fn numeric_geometry(&self) -> &GridGeometry {
            &self.0
        }
        fn visit_window_values(
            &self,
            window: GridWindow,
            visitor: &mut dyn FnMut(usize, Option<f64>) -> Result<()>,
        ) -> Result<()> {
            for i in 0..window.width * window.height {
                let (c, r) = (
                    window.column + i % window.width,
                    window.row + i / window.width,
                );
                visitor(i, ((c, r) != (40, 5)).then_some(10.))?;
            }
            Ok(())
        }
    }
    fn grid(crs: u32) -> Grid {
        Grid(GridGeometry {
            width: 64,
            height: 32,
            origin_x: 129.,
            origin_y: 35.,
            spacing_x: 0.01,
            spacing_y: 0.01,
            horizontal_crs: crs,
        })
    }
    #[test]
    fn occupancy_requires_complete_data_blocks_inside_the_grid() {
        let o = CoverageOccupancy::build(&grid(4326)).unwrap().unwrap();
        // Columns 0..=15, rows 0..=15: one complete block.
        assert!(o.covers([129., 35., 129.15, 35.15]));
        // Block (2, 0) holds the fill node.
        assert!(!o.covers([129.33, 35.01, 129.34, 35.02]));
        assert!(o.covers([129.5, 35.2, 129.6, 35.3]));
        // Beyond the grid, including the half-cell margin.
        assert!(!o.covers([128.9, 35., 129.1, 35.1]));
        assert!(!o.covers([129.5, 35.2, 129.7, 35.4]));
        assert!(CoverageOccupancy::build(&grid(3857)).unwrap().is_none());
    }
}
