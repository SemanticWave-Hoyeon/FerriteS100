//! S-101 line portrayal geometry: curves and oriented surface boundaries.
use ferrite_render::{ScaleRange, WorldPoint};
use ferrite_s100_core::{
    CompositeCurveRecord, CurveRecord, FeatureRecord, RecordId, S101Cell, SurfaceRecord,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct SpatialLine {
    pub spatial_id: RecordId,
    pub points: Vec<WorldPoint>,
    pub scale_range: ScaleRange,
}
/// Intersect denominator ranges: SCAMIN is the upper denominator bound.
pub fn intersect_scale_ranges(a: ScaleRange, b: ScaleRange) -> ScaleRange {
    let minimum = match (a.scale_minimum, b.scale_minimum) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let maximum = match (a.scale_maximum, b.scale_maximum) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    ScaleRange {
        scale_minimum: minimum,
        scale_maximum: maximum,
    }
}
struct Geometry<'a> {
    curves: &'a HashMap<i64, CurveRecord>,
    composites: &'a HashMap<i64, CompositeCurveRecord>,
    surfaces: &'a HashMap<i64, SurfaceRecord>,
}
/// Explicit references take precedence over feature associations. Masks apply at
/// every component level; area fills use their full surface through a separate path.
pub fn resolve_feature_line_geometry(
    cell: &S101Cell,
    feature: &FeatureRecord,
    references: &[(String, bool)],
) -> Result<Vec<SpatialLine>, String> {
    resolve(
        Geometry {
            curves: &cell.curves,
            composites: &cell.composite_curves,
            surfaces: &cell.surfaces,
        },
        feature,
        references,
    )
}
fn resolve(
    geometry: Geometry<'_>,
    feature: &FeatureRecord,
    references: &[(String, bool)],
) -> Result<Vec<SpatialLine>, String> {
    // Both truncated dataset-limit edges and explicitly suppressed edges are
    // excluded from normal line styling. MUIN=2 is retained for the update engine.
    let masks: HashSet<_> = feature
        .masks
        .iter()
        .filter(|m| m.update_instruction == 1 && matches!(m.mask_type, 1 | 2))
        .map(|m| m.spatial_id.key())
        .collect();
    let mut roots = Vec::new();
    if references.is_empty() {
        for sa in &feature.spatial_associations {
            if matches!(sa.spatial_id.rcnm, 120 | 125 | 130) {
                roots.push((
                    sa.spatial_id,
                    sa.ornt != 2,
                    ScaleRange {
                        scale_minimum: sa.scale_minimum,
                        scale_maximum: sa.scale_maximum,
                    },
                    sa.mask == 2,
                ));
            }
        }
    } else {
        for (reference, forward) in references {
            let (kind, value) = reference
                .split_once('|')
                .ok_or_else(|| format!("Invalid line spatial reference: {reference}"))?;
            if let Some(surface_text) = value
                .strip_prefix("exterior_")
                .filter(|_| kind == "CompositeCurve")
            {
                let key: i64 = surface_text
                    .parse()
                    .map_err(|_| "Invalid exterior ring ID")?;
                let surface = geometry
                    .surfaces
                    .get(&key)
                    .ok_or("Missing exterior surface")?;
                let association = feature
                    .spatial_associations
                    .iter()
                    .find(|sa| sa.spatial_id.key() == key);
                let range = association
                    .map(|sa| ScaleRange {
                        scale_minimum: sa.scale_minimum,
                        scale_maximum: sa.scale_maximum,
                    })
                    .unwrap_or_default();
                let members: Vec<_> = if *forward {
                    surface.exterior_ring.iter().collect()
                } else {
                    surface.exterior_ring.iter().rev().collect()
                };
                for child in members {
                    roots.push((
                        child.curve_id,
                        *forward == child.orientation,
                        range,
                        masks.contains(&key),
                    ));
                }
                continue;
            }
            if let Some(value) = value
                .strip_prefix("interior_")
                .filter(|_| kind == "CompositeCurve")
            {
                let (surface_text, index) =
                    value.rsplit_once('_').ok_or("Invalid interior ring ID")?;
                let key: i64 = surface_text
                    .parse()
                    .map_err(|_| "Invalid interior surface ID")?;
                let index: usize = index.parse().map_err(|_| "Invalid interior ring index")?;
                let surface = geometry
                    .surfaces
                    .get(&key)
                    .ok_or("Missing interior surface")?;
                let ring = surface
                    .interior_rings
                    .get(index)
                    .ok_or("Interior ring outside surface")?;
                let association = feature
                    .spatial_associations
                    .iter()
                    .find(|sa| sa.spatial_id.key() == key);
                let range = association
                    .map(|sa| ScaleRange {
                        scale_minimum: sa.scale_minimum,
                        scale_maximum: sa.scale_maximum,
                    })
                    .unwrap_or_default();
                let members: Vec<_> = if *forward {
                    ring.iter().collect()
                } else {
                    ring.iter().rev().collect()
                };
                for child in members {
                    roots.push((
                        child.curve_id,
                        *forward == child.orientation,
                        range,
                        masks.contains(&key),
                    ));
                }
                continue;
            }
            let rcnm = match kind {
                "Curve" => 120,
                "CompositeCurve" => 125,
                "Surface" => 130,
                _ => return Err(format!("Unsupported line geometry type: {kind}")),
            };
            let key: i64 = value
                .parse()
                .map_err(|_| format!("Invalid spatial identifier: {reference}"))?;
            if key < 0 {
                return Err("Negative spatial identifier".into());
            }
            let id = if key <= u32::MAX as i64 {
                RecordId::new(rcnm, key as u32)
            } else {
                if key >> 32 != rcnm as i64 {
                    return Err("Spatial reference type and identifier disagree".into());
                }
                RecordId::new(rcnm, key as u32)
            };
            let association = feature
                .spatial_associations
                .iter()
                .find(|sa| sa.spatial_id.key() == id.key());
            let scale_range = association
                .map(|sa| ScaleRange {
                    scale_minimum: sa.scale_minimum,
                    scale_maximum: sa.scale_maximum,
                })
                .unwrap_or_default();
            roots.push((
                id,
                *forward,
                scale_range,
                association.is_some_and(|sa| sa.mask == 2),
            ));
        }
    }
    enum Step {
        Enter(RecordId, bool, ScaleRange, bool),
        Leave(i64),
    }
    let mut stack: Vec<_> = roots
        .into_iter()
        .rev()
        .map(|(id, f, s, m)| Step::Enter(id, f, s, m))
        .collect();
    let mut active = HashSet::new();
    let mut result = Vec::new();
    let mut visits = 0usize;
    while let Some(step) = stack.pop() {
        let (id, forward, range, masked) = match step {
            Step::Leave(key) => {
                active.remove(&key);
                continue;
            }
            Step::Enter(id, f, s, m) => (id, f, s, m),
        };
        visits += 1;
        if visits > 1_000_000 {
            return Err("Line geometry traversal limit exceeded".into());
        }
        let key = id.key();
        if masked || masks.contains(&key) {
            continue;
        }
        if !active.insert(key) {
            return Err(format!("Cyclic line geometry at {key}"));
        }
        stack.push(Step::Leave(key));
        if let Some(curve) = geometry.curves.get(&key) {
            let mut points: Vec<_> = curve
                .positions_iter()
                .map(|p| WorldPoint::new(p.x, p.y))
                .collect();
            if !forward {
                points.reverse();
            }
            if points.len() >= 2 {
                result.push(SpatialLine {
                    spatial_id: id,
                    points,
                    scale_range: range,
                });
            }
        } else {
            let children = if let Some(composite) = geometry.composites.get(&key) {
                composite.curves.iter().collect::<Vec<_>>()
            } else if let Some(surface) = geometry.surfaces.get(&key) {
                surface
                    .exterior_ring
                    .iter()
                    .chain(surface.interior_rings.iter().flatten())
                    .collect::<Vec<_>>()
            } else {
                return Err(format!("Missing line geometry {key}"));
            };
            // A reversed composite reverses both order and each child direction.
            if forward {
                for child in children.into_iter().rev() {
                    stack.push(Step::Enter(child.curve_id, child.orientation, range, false));
                }
            } else {
                for child in children {
                    stack.push(Step::Enter(
                        child.curve_id,
                        !child.orientation,
                        range,
                        false,
                    ));
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::{
        Coordinate, CurveSegment, MaskRecord, OrientedCurve, SegmentType, SpatialAssociation,
        SpatialPrimitiveType, FRID,
    };
    fn feature(id: RecordId) -> FeatureRecord {
        FeatureRecord {
            frid: FRID {
                rcid: 1,
                nftc: 1,
                rver: 1,
                ruin: 1,
            },
            foid: None,
            attributes: vec![],
            spatial_associations: vec![SpatialAssociation {
                spatial_id: id,
                ornt: 1,
                usag: 0,
                mask: 0,
                scale_minimum: Some(10000),
                scale_maximum: Some(1000),
                update_instruction: 1,
            }],
            information_associations: vec![],
            feature_associations: vec![],
            masks: vec![],
            feature_code: Some("River".into()),
            primitive_type: SpatialPrimitiveType::Surface,
        }
    }
    fn curve(rcid: u32) -> CurveRecord {
        CurveRecord {
            id: RecordId::new(120, rcid),
            segments: vec![CurveSegment {
                segment_type: SegmentType::Line,
                positions: vec![
                    Coordinate::new(rcid as f64, 0.),
                    Coordinate::new(rcid as f64, 1.),
                ],
            }],
            start_point: None,
            end_point: None,
            update_instruction: 1,
        }
    }
    #[test]
    fn surface_outer_inner_and_nested_composite_masks_preserve_orientation() {
        let curves = HashMap::from([
            (curve(1).id.key(), curve(1)),
            (curve(2).id.key(), curve(2)),
            (curve(3).id.key(), curve(3)),
        ]);
        let composite = CompositeCurveRecord {
            id: RecordId::new(125, 1),
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
        };
        let surface = SurfaceRecord {
            id: RecordId::new(130, 1),
            exterior_ring: vec![OrientedCurve {
                curve_id: composite.id,
                orientation: false,
            }],
            interior_rings: vec![vec![OrientedCurve {
                curve_id: RecordId::new(120, 3),
                orientation: true,
            }]],
            update_instruction: 1,
        };
        let composites = HashMap::from([(composite.id.key(), composite)]);
        let surfaces = HashMap::from([(surface.id.key(), surface)]);
        let g = || Geometry {
            curves: &curves,
            composites: &composites,
            surfaces: &surfaces,
        };
        let mut f = feature(RecordId::new(130, 1));
        let result = resolve(g(), &f, &[]).unwrap();
        assert_eq!(
            result.iter().map(|l| l.spatial_id.rcid).collect::<Vec<_>>(),
            vec![2, 1, 3]
        );
        assert_eq!(result[0].points[0].y, 0.);
        assert_eq!(result[1].points[0].y, 1.);
        assert!(result[0].scale_range.is_visible_at(5000));
        assert!(!result[0].scale_range.is_visible_at(20000));
        let virtual_exterior = resolve(
            g(),
            &f,
            &[(
                format!("CompositeCurve|exterior_{}", RecordId::new(130, 1).key()),
                true,
            )],
        )
        .unwrap();
        assert_eq!(
            virtual_exterior
                .iter()
                .map(|l| l.spatial_id.rcid)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        let virtual_interior = resolve(
            g(),
            &f,
            &[(
                format!("CompositeCurve|interior_{}_0", RecordId::new(130, 1).key()),
                false,
            )],
        )
        .unwrap();
        assert_eq!(virtual_interior[0].points[0].y, 1.);
        f.masks.push(MaskRecord {
            spatial_id: RecordId::new(120, 2),
            mask_type: 2,
            update_instruction: 1,
        });
        assert_eq!(resolve(g(), &f, &[]).unwrap().len(), 2);
        f.masks.push(MaskRecord {
            spatial_id: RecordId::new(125, 1),
            mask_type: 1,
            update_instruction: 1,
        });
        assert_eq!(resolve(g(), &f, &[]).unwrap().len(), 1);
        assert_eq!(
            resolve(
                g(),
                &f,
                &[(format!("Curve|{}", RecordId::new(120, 3).key()), false)]
            )
            .unwrap()[0]
                .points[0]
                .y,
            1.
        );
    }
    #[test]
    fn cyclic_or_missing_geometry_fails_without_partial_output() {
        let curves = HashMap::new();
        let surfaces = HashMap::new();
        let id = RecordId::new(125, 1);
        let composites = HashMap::from([(
            id.key(),
            CompositeCurveRecord {
                id,
                curves: vec![OrientedCurve {
                    curve_id: id,
                    orientation: true,
                }],
                update_instruction: 1,
            },
        )]);
        let g = || Geometry {
            curves: &curves,
            composites: &composites,
            surfaces: &surfaces,
        };
        assert!(resolve(g(), &feature(id), &[]).is_err());
        assert!(resolve(g(), &feature(RecordId::new(120, 9)), &[]).is_err());
    }
    #[test]
    fn scale_intersection_keeps_only_shared_denominators() {
        let r = intersect_scale_ranges(
            ScaleRange {
                scale_minimum: Some(20000),
                scale_maximum: Some(2000),
            },
            ScaleRange {
                scale_minimum: Some(10000),
                scale_maximum: Some(1000),
            },
        );
        assert!(r.is_visible_at(5000));
        assert!(!r.is_visible_at(15000));
        assert!(!r.is_visible_at(1500));
    }
}
