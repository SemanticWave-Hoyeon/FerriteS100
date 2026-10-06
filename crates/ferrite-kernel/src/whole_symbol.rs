//! Whole painted-symbol containment in a caller-supplied planar domain.
//!
//! The adapter supplies the complete painted fill/stroke support and complete
//! authored area in the SAME coordinates and units. SVG viewport, symbol-box,
//! raster centres, viewport/horizon clipping and dataset coverage cannot replace
//! these inputs. Original coordinates are retained: there is no polygon union,
//! difference, integer-grid overlay or positive-area threshold in this module.
//!
//! Contact with the area boundary is accepted (closed-set containment). SVG
//! stroke/curve flattening, AA/filter footprints and complete geodesic projection
//! remain adapter contracts; this API alone does not prove portrayal conformity.
use anyhow::{ensure, Result};
use geo::{Area, CoordsIter, MapCoords, MultiPolygon, Polygon, Relate, Validation};
use std::sync::Arc;

/// Logical input complexity bounds checked BEFORE geometry validation/copying.
/// These are not exact bounds for allocator rounding or geo topology scratch.
#[derive(Debug, Clone, Copy)]
pub struct ShapeLimits {
    pub max_coordinates: usize,
    pub max_components: usize,
    pub max_rings: usize,
}
fn admit(polygons: &[Polygon<f64>], limits: ShapeLimits) -> Result<usize> {
    ensure!(!polygons.is_empty(), "Empty whole symbol shape");
    ensure!(
        polygons.len() <= limits.max_components,
        "Whole symbol component budget exceeded"
    );
    let mut coordinates = 0usize;
    let mut rings = 0usize;
    for p in polygons {
        coordinates = coordinates
            .checked_add(p.coords_iter().count())
            .ok_or_else(|| anyhow::anyhow!("Whole symbol coordinate count overflow"))?;
        rings = rings
            .checked_add(p.interiors().len())
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("Whole symbol ring count overflow"))?;
        ensure!(
            coordinates <= limits.max_coordinates,
            "Whole symbol coordinate budget exceeded"
        );
        ensure!(
            rings <= limits.max_rings,
            "Whole symbol ring budget exceeded"
        );
    }
    for p in polygons {
        validate_polygon(p)?;
    }
    Ok(coordinates)
}
fn validate_polygon(p: &Polygon<f64>) -> Result<()> {
    ensure!(
        p.coords_iter().all(|c| c.x.is_finite() && c.y.is_finite()),
        "Non-finite whole symbol geometry"
    );
    p.check_validation()
        .map_err(|e| anyhow::anyhow!("Invalid whole symbol polygon: {e:?}"))?;
    ensure!(
        p.unsigned_area().is_finite() && p.unsigned_area() > 0.,
        "Degenerate whole symbol polygon"
    );
    Ok(())
}

/// Validated raw authored area. Multipolygon components must be disjoint and
/// topologically valid; no normalization silently changes submitted boundaries.
#[derive(Debug, Clone)]
pub struct WholeSymbolArea {
    geometry: Arc<MultiPolygon<f64>>,
    coordinates: usize,
}
impl WholeSymbolArea {
    /// Raw-ring adapter, retaining original coordinates without union/overlay.
    pub fn from_rings(
        exterior: &[[f64; 2]],
        holes: &[Vec<[f64; 2]>],
        limits: ShapeLimits,
    ) -> Result<Self> {
        let rings = holes
            .len()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Whole symbol ring count overflow"))?;
        ensure!(
            rings <= limits.max_rings && limits.max_components >= 1,
            "Whole symbol ring/component budget exceeded"
        );
        let count = std::iter::once(exterior)
            .chain(holes.iter().map(|h| h.as_slice()))
            .try_fold(0usize, |n, r| {
                let closing = usize::from(!r.is_empty() && r.first() != r.last());
                n.checked_add(r.len())
                    .and_then(|n| n.checked_add(closing))
                    .ok_or_else(|| anyhow::anyhow!("Whole symbol coordinate count overflow"))
            })?;
        ensure!(
            count <= limits.max_coordinates,
            "Whole symbol coordinate budget exceeded"
        );
        let line = |r: &[[f64; 2]]| {
            geo::LineString::from(r.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>())
        };
        Self::from_polygons(
            vec![Polygon::new(
                line(exterior),
                holes.iter().map(|h| line(h)).collect(),
            )],
            limits,
        )
    }
    pub fn from_polygons(polygons: Vec<Polygon<f64>>, limits: ShapeLimits) -> Result<Self> {
        let coordinates = admit(&polygons, limits)?;
        let geometry = MultiPolygon(polygons);
        geometry
            .check_validation()
            .map_err(|e| anyhow::anyhow!("Invalid whole symbol area: {e:?}"))?;
        Ok(Self {
            geometry: Arc::new(geometry),
            coordinates,
        })
    }
}

/// Uncomposited painted support relative to the physical pivot. Components can
/// overlap: (union of components) is covered iff EACH component is covered.
/// Thus individual SVG shapes require no lossy union before containment.
#[derive(Debug, Clone)]
pub struct PaintedSymbolSupport {
    components: Arc<[Polygon<f64>]>,
    coordinates: usize,
}
impl PaintedSymbolSupport {
    /// Filled triangle adapter for a tessellated graphic. Degenerate triangles
    /// are errors here; a renderer may discard exact zero-area output earlier.
    pub fn from_triangles(triangles: &[[[f64; 2]; 3]], limits: ShapeLimits) -> Result<Self> {
        let coordinates = triangles
            .len()
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Whole symbol triangle count overflow"))?;
        ensure!(
            coordinates <= limits.max_coordinates
                && triangles.len() <= limits.max_components
                && triangles.len() <= limits.max_rings,
            "Whole symbol triangle budget exceeded"
        );
        let mut polygons = Vec::new();
        polygons
            .try_reserve_exact(triangles.len())
            .map_err(|e| anyhow::anyhow!("Whole symbol allocation failed: {e}"))?;
        for t in triangles {
            polygons.push(Polygon::new(
                geo::LineString::from(vec![
                    (t[0][0], t[0][1]),
                    (t[1][0], t[1][1]),
                    (t[2][0], t[2][1]),
                    (t[0][0], t[0][1]),
                ]),
                vec![],
            ));
        }
        Self::from_polygons(polygons, limits)
    }
    pub fn from_polygons(polygons: Vec<Polygon<f64>>, limits: ShapeLimits) -> Result<Self> {
        let coordinates = admit(&polygons, limits)?;
        Ok(Self {
            components: polygons.into(),
            coordinates,
        })
    }
}

/// The renderer supplies signed lattice identities in original painter order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolSite {
    pub source_ordinal: usize,
    pub lattice_index: [i64; 2],
    pub origin: [f64; 2],
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolDecision {
    pub site: SymbolSite,
    pub completely_contained: bool,
}
#[derive(Debug, Clone, Copy)]
pub struct WholeSymbolLimits {
    pub max_sites: usize,
    /// Area-coordinate x support-coordinate x sites metric for relation inputs.
    /// Sorting, translation and self-validation are separately non-zero costs.
    pub max_cross_coordinate_pairs: usize,
    /// Support-coordinate squared x sites, conservatively includes validation.
    pub max_support_coordinate_pairs: usize,
    /// Requested Vec<SymbolDecision> payload; excludes allocator overhead.
    pub max_decision_bytes: usize,
    /// Maximum origin-addition error per axis in caller coordinate units.
    pub max_translation_error: f64,
}

fn translate(p: &Polygon<f64>, origin: [f64; 2], max_error: f64) -> Result<Polygon<f64>> {
    // Knuth TwoSum captures a lost small origin as well as cancellation. The
    // operation is IEEE-754 nearest addition; no fast-math reassociation is used.
    let error = |a: f64, b: f64, sum: f64| {
        let virtual_b = sum - a;
        (a - (sum - virtual_b)) + (b - virtual_b)
    };
    ensure!(
        p.coords_iter().all(|c| {
            let x = c.x + origin[0];
            let y = c.y + origin[1];
            x.is_finite()
                && y.is_finite()
                && error(c.x, origin[0], x).abs() <= max_error
                && error(c.y, origin[1], y).abs() <= max_error
        }),
        "Symbol translation precision limit exceeded"
    );
    let moved = p.map_coords(|c| geo::Coord {
        x: c.x + origin[0],
        y: c.y + origin[1],
    });
    // Rounding can collapse edges or make rings invalid despite small error.
    validate_polygon(&moved)?;
    Ok(moved)
}

/// Decide EVERY submitted site, retaining rejected identities and painter order.
/// Invalid input/budget failure returns Err for the entire batch, never a prefix.
/// One translated component is alive at a time. Accepted motifs must be rendered
/// separately; masking a precomposited periodic cell cannot implement this rule.
pub fn select_whole_symbols(
    area: &WholeSymbolArea,
    support: &PaintedSymbolSupport,
    sites: &[SymbolSite],
    limits: WholeSymbolLimits,
) -> Result<Vec<SymbolDecision>> {
    ensure!(
        limits.max_translation_error.is_finite() && limits.max_translation_error >= 0.,
        "Invalid symbol translation error limit"
    );
    ensure!(
        sites.len() <= limits.max_sites,
        "Whole symbol site budget exceeded"
    );
    let cross = area
        .coordinates
        .checked_mul(support.coordinates)
        .and_then(|n| n.checked_mul(sites.len()))
        .ok_or_else(|| anyhow::anyhow!("Whole symbol cross count overflow"))?;
    let own = support
        .coordinates
        .checked_mul(support.coordinates)
        .and_then(|n| n.checked_mul(sites.len()))
        .ok_or_else(|| anyhow::anyhow!("Whole symbol support count overflow"))?;
    let bytes = sites
        .len()
        .checked_mul(std::mem::size_of::<SymbolDecision>())
        .ok_or_else(|| anyhow::anyhow!("Whole symbol decision byte overflow"))?;
    ensure!(
        cross <= limits.max_cross_coordinate_pairs,
        "Whole symbol cross budget exceeded"
    );
    ensure!(
        own <= limits.max_support_coordinate_pairs,
        "Whole symbol support budget exceeded"
    );
    ensure!(
        bytes <= limits.max_decision_bytes,
        "Whole symbol decision budget exceeded"
    );
    ensure!(
        sites.iter().all(|s| s.origin.iter().all(|c| c.is_finite())),
        "Non-finite symbol origin"
    );
    let mut decisions = Vec::with_capacity(sites.len());
    for &site in sites {
        let mut completely_contained = true;
        for p in support.components.iter() {
            let translated = translate(p, site.origin, limits.max_translation_error)?;
            // Do not short-circuit component validation on a rejected site.
            completely_contained &= area.geometry.relate(&translated).is_covers();
        }
        decisions.push(SymbolDecision {
            site,
            completely_contained,
        });
    }
    Ok(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shape_limits() -> ShapeLimits {
        ShapeLimits {
            max_coordinates: 1000,
            max_components: 100,
            max_rings: 100,
        }
    }
    fn limits() -> WholeSymbolLimits {
        WholeSymbolLimits {
            max_sites: 100,
            max_cross_coordinate_pairs: 100_000,
            max_support_coordinate_pairs: 100_000,
            max_decision_bytes: 10_000,
            max_translation_error: 1e-9,
        }
    }
    fn polygon(points: &[[f64; 2]], holes: &[Vec<[f64; 2]>]) -> Polygon<f64> {
        let line = |points: &[[f64; 2]]| {
            geo::LineString::from(points.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>())
        };
        Polygon::new(line(points), holes.iter().map(|p| line(p)).collect())
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon<f64> {
        polygon(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]], &[])
    }
    fn area(p: Polygon<f64>) -> WholeSymbolArea {
        WholeSymbolArea::from_polygons(vec![p], shape_limits()).unwrap()
    }
    fn support(p: Vec<Polygon<f64>>) -> PaintedSymbolSupport {
        PaintedSymbolSupport::from_polygons(p, shape_limits()).unwrap()
    }
    fn site(origin: [f64; 2]) -> SymbolSite {
        SymbolSite {
            source_ordinal: 42,
            lattice_index: [-3, 7],
            origin,
        }
    }
    fn contains(a: &WholeSymbolArea, p: Vec<Polygon<f64>>, origin: [f64; 2]) -> bool {
        select_whole_symbols(a, &support(p), &[site(origin)], limits()).unwrap()[0]
            .completely_contained
    }
    fn ring() -> Polygon<f64> {
        polygon(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![[4., 4.], [6., 4.], [6., 6.], [4., 6.], [4., 4.]]],
        )
    }
    #[test]
    fn exterior_crossing_and_boundary_contact_preserve_every_site() {
        let sites = [
            site([0., 0.]),
            site([9., 9.]),
            site([9.01, 9.]),
            site([-0.01, 0.]),
        ];
        let d = select_whole_symbols(
            &area(rect(0., 0., 10., 10.)),
            &support(vec![rect(0., 0., 1., 1.)]),
            &sites,
            limits(),
        )
        .unwrap();
        assert_eq!(
            d.iter().map(|x| x.completely_contained).collect::<Vec<_>>(),
            [true, true, false, false]
        );
        assert_eq!(d.iter().map(|x| x.site).collect::<Vec<_>>(), sites);
    }
    #[test]
    fn enclosed_hole_rejects_even_with_all_motif_corners_inside() {
        let a = area(ring());
        assert!(!contains(&a, vec![rect(2., 2., 8., 8.)], [0., 0.]));
        assert!(contains(&a, vec![rect(0., 0., 2., 2.)], [0., 0.]));
    }
    #[test]
    fn concave_area_rejects_bridge_despite_inside_corners() {
        let a = area(polygon(
            &[
                [0., 0.],
                [6., 0.],
                [6., 6.],
                [4., 6.],
                [4., 2.],
                [2., 2.],
                [2., 6.],
                [0., 6.],
                [0., 0.],
            ],
            &[],
        ));
        assert!(!contains(&a, vec![rect(1., 3., 5., 4.)], [0., 0.]));
    }
    #[test]
    fn disconnected_motif_accepts_when_bbox_crosses_hole() {
        assert!(contains(
            &area(ring()),
            vec![rect(2., 4., 3., 6.), rect(7., 4., 8., 6.)],
            [0., 0.]
        ));
    }
    #[test]
    fn transparent_support_hole_can_enclose_area_hole() {
        assert!(contains(&area(ring()), vec![ring()], [0., 0.]));
        assert!(!contains(
            &area(ring()),
            vec![rect(0., 0., 10., 10.)],
            [0., 0.]
        ));
    }
    #[test]
    fn raw_coordinates_preserved_and_tiny_protrusion_not_swallowed() {
        let p = rect(0.1, 0.1, 1. + 1e-10, 0.9);
        let s = support(vec![p.clone()]);
        let original = p
            .coords_iter()
            .map(|c| [c.x.to_bits(), c.y.to_bits()])
            .collect::<Vec<_>>();
        let stored = s.components[0]
            .coords_iter()
            .map(|c| [c.x.to_bits(), c.y.to_bits()])
            .collect::<Vec<_>>();
        assert_eq!(original, stored);
        assert!(
            !select_whole_symbols(&area(rect(0., 0., 1., 1.)), &s, &[site([0., 0.])], limits())
                .unwrap()[0]
                .completely_contained
        );
    }
    #[test]
    fn tiny_raw_hole_is_not_normalized_away() {
        let a = area(polygon(
            &[[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]],
            &[vec![
                [4., 4.],
                [4. + 1e-10, 4.],
                [4. + 1e-10, 6.],
                [4., 6.],
                [4., 4.],
            ]],
        ));
        assert!(!contains(&a, vec![rect(2., 2., 8., 8.)], [0., 0.]));
    }
    #[test]
    fn overlapping_support_components_need_no_union() {
        assert!(contains(
            &area(rect(0., 0., 2., 2.)),
            vec![rect(0., 0., 1.5, 1.5), rect(0.5, 0.5, 2., 2.)],
            [0., 0.]
        ));
        assert!(!contains(
            &area(rect(0., 0., 2., 2.)),
            vec![rect(0., 0., 1.5, 1.5), rect(0.5, 0.5, 2.01, 2.)],
            [0., 0.]
        ));
    }
    #[test]
    fn overlapping_sites_remain_independent_in_original_order() {
        let sites = [
            SymbolSite {
                lattice_index: [-1, 0],
                ..site([0., 0.])
            },
            SymbolSite {
                lattice_index: [0, 0],
                ..site([0.5, 0.])
            },
            SymbolSite {
                lattice_index: [1, 0],
                ..site([1., 0.])
            },
        ];
        let d = select_whole_symbols(
            &area(rect(0., 0., 1.5, 1.)),
            &support(vec![rect(0., 0., 1., 1.)]),
            &sites,
            limits(),
        )
        .unwrap();
        assert_eq!(
            d.iter().map(|x| x.completely_contained).collect::<Vec<_>>(),
            [true, true, false]
        );
        assert_eq!(d.iter().map(|x| x.site).collect::<Vec<_>>(), sites);
    }
    #[test]
    fn small_origin_absorbed_by_large_coordinate_is_rejected() {
        let p = rect(1e16, 0., 1e16 + 8., 8.);
        let s = support(vec![p.clone()]);
        let l = WholeSymbolLimits {
            max_translation_error: 0.,
            ..limits()
        };
        assert!(select_whole_symbols(&area(p), &s, &[site([1., 0.])], l).is_err());
        assert!(
            select_whole_symbols(&area(rect(0., 0., 8., 8.)), &s, &[site([-1e16, 0.])], l).unwrap()
                [0]
            .completely_contained
        );
    }
    #[test]
    fn invalid_shapes_and_admission_budgets_rejected() {
        assert!(WholeSymbolArea::from_polygons(vec![], shape_limits()).is_err());
        assert!(PaintedSymbolSupport::from_polygons(vec![], shape_limits()).is_err());
        assert!(WholeSymbolArea::from_polygons(
            vec![rect(0., 0., 2., 2.), rect(1., 1., 3., 3.)],
            shape_limits()
        )
        .is_err());
        assert!(PaintedSymbolSupport::from_polygons(
            vec![rect(0., 0., 1., 1.)],
            ShapeLimits {
                max_coordinates: 4,
                ..shape_limits()
            }
        )
        .is_err());
        assert!(PaintedSymbolSupport::from_polygons(
            vec![rect(0., 0., 1., 1.)],
            ShapeLimits {
                max_components: 0,
                ..shape_limits()
            }
        )
        .is_err());
        assert!(PaintedSymbolSupport::from_polygons(
            vec![ring()],
            ShapeLimits {
                max_rings: 1,
                ..shape_limits()
            }
        )
        .is_err());
        assert!(
            WholeSymbolArea::from_polygons(vec![rect(0., 0., f64::NAN, 1.)], shape_limits())
                .is_err()
        );
    }
    #[test]
    fn budgets_and_invalid_late_site_fail_whole_batch() {
        let a = area(rect(0., 0., 10., 10.));
        let s = support(vec![rect(0., 0., 1., 1.)]);
        let sites = [site([1., 1.]), site([2., 2.])];
        let l = limits();
        for bad in [
            WholeSymbolLimits { max_sites: 1, ..l },
            WholeSymbolLimits {
                max_cross_coordinate_pairs: 49,
                ..l
            },
            WholeSymbolLimits {
                max_support_coordinate_pairs: 49,
                ..l
            },
            WholeSymbolLimits {
                max_decision_bytes: 1,
                ..l
            },
            WholeSymbolLimits {
                max_translation_error: -1.,
                ..l
            },
        ] {
            assert!(select_whole_symbols(&a, &s, &sites, bad).is_err());
        }
        assert!(select_whole_symbols(&a, &s, &[sites[0], site([f64::NAN, 0.])], l).is_err());
        assert!(select_whole_symbols(&a, &s, &[sites[0], site([1e20, 0.])], l).is_err());
    }
}
