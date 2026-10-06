//! S-101 DataCoverage topology and regional scales. Coordinates remain WGS84
//! longitude/latitude; projection and polygon set operations belong to the kernel.
use anyhow::{ensure, Context, Result};
use ferrite_kernel::scale_policy::CoverageScaleRange;
use ferrite_s100_core::{
    CompositeCurveRecord, CurveRecord, OrientedCurve, PointRecord, RecordId, S101Cell, SegmentType,
    SurfaceRecord,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct GeographicSurface {
    pub exterior: Vec<[f64; 2]>,
    pub holes: Vec<Vec<[f64; 2]>>,
}
#[derive(Debug, Clone)]
pub struct DataCoverage {
    pub feature_key: i64,
    pub scales: CoverageScaleRange,
    pub drawing_index: Option<u32>,
    pub surfaces: Vec<GeographicSurface>,
}
struct Geometry<'a> {
    points: &'a HashMap<i64, PointRecord>,
    curves: &'a HashMap<i64, CurveRecord>,
    composites: &'a HashMap<i64, CompositeCurveRecord>,
}
fn same_point(a: [f64; 2], b: [f64; 2]) -> bool {
    let dx = (a[0] - b[0] + 180.).rem_euclid(360.) - 180.;
    let tolerance = 64. * f64::EPSILON * 180.;
    dx.abs() <= tolerance && (a[1] - b[1]).abs() <= tolerance
}
fn coordinate(p: ferrite_s100_core::Coordinate) -> Result<[f64; 2]> {
    ensure!(
        p.x.is_finite()
            && p.y.is_finite()
            && (-180. ..=180.).contains(&p.x)
            && (-90. ..=90.).contains(&p.y),
        "Invalid coverage geographic coordinate"
    );
    Ok([p.x, p.y])
}
impl Geometry<'_> {
    fn leaf(&self, curve: &CurveRecord) -> Result<Vec<[f64; 2]>> {
        ensure!(
            curve
                .segments
                .iter()
                .all(|s| matches!(s.segment_type, SegmentType::Line)),
            "Unsupported S-101 coverage interpolation"
        );
        let mut result = Vec::new();
        for position in curve.positions_iter() {
            let p = coordinate(*position)?;
            if result.last().is_none_or(|last| !same_point(*last, p)) {
                result.push(p);
            }
        }
        ensure!(
            result.len() >= 2,
            "Coverage curve has fewer than two positions"
        );
        // S-101 4.8.1: level 3a requires both endpoint references. S-100
        // 10a-7.2.4.1: PTAS points coincide with the curve's control endpoints;
        // they do not replace or extend missing coordinate-list endpoints.
        for (id, position) in [
            (curve.start_point, result[0]),
            (curve.end_point, *result.last().unwrap()),
        ] {
            let id = id.context("Coverage curve missing endpoint reference")?;
            ensure!(id.rcnm == 110, "Coverage curve endpoint is not a point");
            let point = self
                .points
                .get(&id.key())
                .context("Missing coverage endpoint point")?;
            ensure!(
                same_point(coordinate(point.position)?, position),
                "Coverage endpoint disagrees with curve positions"
            );
        }
        Ok(result)
    }
    fn ring(
        &self,
        curves: &[OrientedCurve],
        forward: bool,
        budget: &mut usize,
    ) -> Result<Vec<[f64; 2]>> {
        enum Step {
            Enter(RecordId, bool),
            Leave(i64),
        }
        let mut stack = Vec::new();
        if forward {
            for child in curves.iter().rev() {
                stack.push(Step::Enter(child.curve_id, child.orientation));
            }
        } else {
            for child in curves {
                stack.push(Step::Enter(child.curve_id, !child.orientation));
            }
        }
        let mut active = HashSet::new();
        let mut result = Vec::new();
        while let Some(step) = stack.pop() {
            let (id, forward) = match step {
                Step::Leave(key) => {
                    active.remove(&key);
                    continue;
                }
                Step::Enter(id, forward) => (id, forward),
            };
            *budget = budget
                .checked_add(1)
                .context("Coverage traversal overflow")?;
            ensure!(*budget <= 1_000_000, "Coverage traversal limit exceeded");
            let key = id.key();
            ensure!(active.insert(key), "Cyclic coverage curve at {key}");
            stack.push(Step::Leave(key));
            match id.rcnm {
                120 => {
                    let mut points =
                        self.leaf(self.curves.get(&key).context("Missing coverage curve")?)?;
                    if !forward {
                        points.reverse();
                    }
                    if let Some(last) = result.last() {
                        ensure!(same_point(*last, points[0]), "Disconnected coverage ring");
                        points.remove(0);
                    }
                    *budget = budget
                        .checked_add(points.len())
                        .context("Coverage point count overflow")?;
                    ensure!(*budget <= 1_000_000, "Coverage point limit exceeded");
                    result.extend(points);
                }
                125 => {
                    let children = &self
                        .composites
                        .get(&key)
                        .context("Missing coverage composite curve")?
                        .curves;
                    ensure!(!children.is_empty(), "Empty coverage composite curve");
                    if forward {
                        for child in children.iter().rev() {
                            stack.push(Step::Enter(child.curve_id, child.orientation));
                        }
                    } else {
                        for child in children {
                            stack.push(Step::Enter(child.curve_id, !child.orientation));
                        }
                    }
                }
                _ => anyhow::bail!("Invalid coverage ring record type {}", id.rcnm),
            }
        }
        ensure!(
            result.len() >= 4 && same_point(result[0], *result.last().unwrap()),
            "Open or collapsed coverage ring"
        );
        let first = result[0];
        *result.last_mut().unwrap() = first;
        Ok(result)
    }
    fn surface(
        &self,
        surface: &SurfaceRecord,
        forward: bool,
        budget: &mut usize,
    ) -> Result<GeographicSurface> {
        let exterior = self.ring(&surface.exterior_ring, forward, budget)?;
        let holes = surface
            .interior_rings
            .iter()
            .map(|ring| self.ring(ring, forward, budget))
            .collect::<Result<_>>()?;
        Ok(GeographicSurface { exterior, holes })
    }
}
/// Validate the complete inventory before returning any candidate coverage.
/// Masks and per-edge line suppression do not remove DataCoverage fill edges.
pub fn extract_current_coverages(cell: &S101Cell) -> Result<Vec<DataCoverage>> {
    let edition = cell
        .dsid
        .product_edition
        .parse::<ferrite_kernel::SpecificationVersion>()?;
    ensure!(edition.edition==2,"DataCoverage optimum scale model requires S-101 Edition 2; legacy Edition {} needs its own loading policy",edition.edition);
    let geometry = Geometry {
        points: &cell.points,
        curves: &cell.curves,
        composites: &cell.composite_curves,
    };
    let mut budget = 0;
    let mut result = Vec::new();
    let mut keys = cell.features.keys().copied().collect::<Vec<_>>();
    keys.sort_unstable();
    for key in keys {
        let feature = &cell.features[&key];
        let Some(scales) = crate::coverage_scale::feature_scale(feature)? else {
            continue;
        };
        let mut drawing = feature
            .attributes
            .iter()
            .filter(|a| a.code.as_deref() == Some("drawingIndex"));
        let drawing_index = if let Some(a) = drawing.next() {
            ensure!(
                a.paix == 0 && drawing.next().is_none(),
                "Invalid DataCoverage drawingIndex multiplicity"
            );
            Some(
                a.atvl
                    .parse::<u32>()
                    .context("Invalid DataCoverage drawingIndex")?,
            )
        } else {
            None
        };
        let mut surfaces = Vec::new();
        let mut ids = HashSet::new();
        for association in &feature.spatial_associations {
            ensure!(
                association.spatial_id.rcnm == 130,
                "DataCoverage references non-surface geometry"
            );
            ensure!(
                matches!(association.ornt, 1 | 2 | -1),
                "Invalid coverage surface orientation"
            );
            ensure!(
                ids.insert(association.spatial_id),
                "Repeated coverage surface association"
            );
            let surface = cell
                .surfaces
                .get(&association.spatial_id.key())
                .context("Missing coverage surface")?;
            surfaces.push(
                geometry
                    .surface(surface, association.ornt != 2, &mut budget)
                    .with_context(|| format!("DataCoverage {key}"))?,
            );
        }
        ensure!(!surfaces.is_empty(), "DataCoverage has no surface geometry");
        result.push(DataCoverage {
            feature_key: key,
            scales,
            drawing_index,
            surfaces,
        });
    }
    ensure!(!result.is_empty(), "S-101 dataset has no DataCoverage");
    result.sort_unstable_by_key(|c| c.feature_key);
    if let Some(first) = result.first() {
        ensure!(
            result
                .iter()
                .all(|c| c.scales.minimum_denominator == first.scales.minimum_denominator),
            "DataCoverage minimum scales differ within one S-101 dataset"
        );
        let mut indexes = result.iter().filter_map(|c| c.drawing_index);
        if let Some(first) = indexes.next() {
            ensure!(
                indexes.all(|index| index == first),
                "DataCoverage drawing indexes differ within one dataset"
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::{Coordinate, CurveSegment};
    struct Fixture {
        points: HashMap<i64, PointRecord>,
        curves: HashMap<i64, CurveRecord>,
        composites: HashMap<i64, CompositeCurveRecord>,
    }
    impl Fixture {
        fn square() -> Self {
            let mut points = HashMap::new();
            for (i, p) in [[0., 0.], [10., 0.], [10., 10.], [0., 10.]]
                .into_iter()
                .enumerate()
            {
                let id = RecordId::new(110, i as u32 + 1);
                points.insert(
                    id.key(),
                    PointRecord {
                        id,
                        position: Coordinate::new(p[0], p[1]),
                        update_instruction: 1,
                    },
                );
            }
            let mut curves = HashMap::new();
            for (index, start, end) in [(1, 1, 2), (2, 3, 2), (3, 3, 4), (4, 1, 4)] {
                let id = RecordId::new(120, index);
                let a = RecordId::new(110, start);
                let b = RecordId::new(110, end);
                curves.insert(
                    id.key(),
                    CurveRecord {
                        id,
                        segments: vec![CurveSegment {
                            segment_type: SegmentType::Line,
                            positions: vec![points[&a.key()].position, points[&b.key()].position],
                        }],
                        start_point: Some(a),
                        end_point: Some(b),
                        update_instruction: 1,
                    },
                );
            }
            let a = RecordId::new(125, 100);
            let b = RecordId::new(125, 101);
            let composites = HashMap::from([
                (
                    a.key(),
                    CompositeCurveRecord {
                        id: a,
                        curves: vec![
                            OrientedCurve {
                                curve_id: RecordId::new(120, 1),
                                orientation: true,
                            },
                            OrientedCurve {
                                curve_id: RecordId::new(120, 2),
                                orientation: false,
                            },
                        ],
                        update_instruction: 1,
                    },
                ),
                (
                    b.key(),
                    CompositeCurveRecord {
                        id: b,
                        curves: vec![
                            OrientedCurve {
                                curve_id: a,
                                orientation: true,
                            },
                            OrientedCurve {
                                curve_id: RecordId::new(120, 3),
                                orientation: true,
                            },
                            OrientedCurve {
                                curve_id: RecordId::new(120, 4),
                                orientation: false,
                            },
                        ],
                        update_instruction: 1,
                    },
                ),
            ]);
            Self {
                points,
                curves,
                composites,
            }
        }
        fn geometry(&self) -> Geometry<'_> {
            Geometry {
                points: &self.points,
                curves: &self.curves,
                composites: &self.composites,
            }
        }
        fn roots() -> Vec<OrientedCurve> {
            vec![OrientedCurve {
                curve_id: RecordId::new(125, 101),
                orientation: true,
            }]
        }
    }
    #[test]
    fn nested_composites_reverse_order_and_direction_without_losing_vertices() {
        let f = Fixture::square();
        let mut budget = 0;
        let forward = f
            .geometry()
            .ring(&Fixture::roots(), true, &mut budget)
            .unwrap();
        let reverse = f
            .geometry()
            .ring(&Fixture::roots(), false, &mut budget)
            .unwrap();
        assert_eq!(
            forward,
            vec![[0., 0.], [10., 0.], [10., 10.], [0., 10.], [0., 0.]]
        );
        assert_eq!(reverse, forward.into_iter().rev().collect::<Vec<_>>());
    }
    #[test]
    fn missing_cyclic_disconnected_and_mismatched_topology_is_not_partially_returned() {
        for variant in 0..4 {
            let mut f = Fixture::square();
            match variant {
                0 => {
                    f.curves.remove(&RecordId::new(120, 3).key());
                }
                1 => {
                    let a = RecordId::new(125, 100);
                    f.composites.get_mut(&a.key()).unwrap().curves = vec![OrientedCurve {
                        curve_id: a,
                        orientation: true,
                    }];
                }
                2 => {
                    f.composites
                        .get_mut(&RecordId::new(125, 100).key())
                        .unwrap()
                        .curves[1]
                        .orientation = true;
                }
                _ => {
                    f.points
                        .get_mut(&RecordId::new(110, 1).key())
                        .unwrap()
                        .position = Coordinate::new(1., 0.);
                }
            }
            assert!(
                f.geometry().ring(&Fixture::roots(), true, &mut 0).is_err(),
                "variant {variant}"
            );
        }
    }
    #[test]
    fn hole_rings_and_antimeridian_endpoint_identity_are_preserved() {
        let f = Fixture::square();
        let surface = SurfaceRecord {
            id: RecordId::new(130, 1),
            exterior_ring: Fixture::roots(),
            interior_rings: vec![Fixture::roots()],
            update_instruction: 1,
        };
        let result = f.geometry().surface(&surface, true, &mut 0).unwrap();
        assert_eq!(result.holes.len(), 1);
        assert_eq!(result.holes[0], result.exterior);
        // Containment and self intersection are validated after projection by
        // the kernel; topology resolution never flattens holes into exteriors.
        assert!(same_point([180., 40.], [-180., 40.]));
        assert!(!same_point([180., 40.], [-180., 40.0001]));
    }
}
