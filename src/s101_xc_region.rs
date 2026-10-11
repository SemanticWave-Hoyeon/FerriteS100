//! Exact representation correspondence, not a general geographic equivalence oracle.
use crate::s101_xc_coverage::{XcDataCoverage, XcScale};
use anyhow::{ensure, Context, Result};
use ferrite_s100_core::{OrientedCurve, S101Cell, SegmentType};
use std::collections::{BTreeMap, BTreeSet};
const GML: &str = "http://www.opengis.net/gml/3.2";
const MAX_VERTICES: usize = 65536;
const MAX_COORD_TEXT: usize = 4 * 1024 * 1024;
type Point = [u64; 2];
type Ring = Vec<Point>;
type ScaleTuple = (Option<u32>, u32, u32);
type RegionalScales = BTreeMap<Polygon, Vec<ScaleTuple>>;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Polygon {
    exterior: Ring,
    holes: Vec<Ring>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionStatus {
    ExactRepresentationAgreement,
    Unverified,
}
fn point(x: f64, y: f64) -> Option<Point> {
    if !x.is_finite()
        || !y.is_finite()
        || !(-180.0..=180.0).contains(&x)
        || !(-90.0..=90.0).contains(&y)
    {
        return None;
    }
    Some([
        if x == 0.0 { 0 } else { x.to_bits() },
        if y == 0.0 { 0 } else { y.to_bits() },
    ])
}
// Adjacent duplicates and one closure duplicate are representation-only. Never
// remove collinear points, quantize, unwrap longitude, or discard interior rings.
fn canonical_ring(points: Vec<Point>) -> Option<Ring> {
    if points.len() > MAX_VERTICES {
        return None;
    }
    let mut points: Vec<_> = points.into_iter().fold(Vec::new(), |mut v, p| {
        if v.last() != Some(&p) {
            v.push(p);
        }
        v
    });
    if points.len() < 4 || points.first() != points.last() {
        return None;
    }
    points.pop();
    let unique: BTreeSet<_> = points.iter().copied().collect();
    if unique.len() != points.len() || points.len() < 3 {
        return None;
    }
    let start = points.iter().enumerate().min_by_key(|(_, p)| **p)?.0;
    let forward: Vec<_> = (0..points.len())
        .map(|i| points[(start + i) % points.len()])
        .collect();
    let backward: Vec<_> = (0..points.len())
        .map(|i| points[(start + points.len() - i) % points.len()])
        .collect();
    Some(forward.min(backward))
}
fn single_element<'a, 'input>(
    n: roxmltree::Node<'a, 'input>,
) -> Option<roxmltree::Node<'a, 'input>> {
    if n.children()
        .any(|c| c.is_text() && c.text().is_some_and(|t| !t.trim().is_empty()))
    {
        return None;
    }
    let mut elements = n.children().filter(|c| c.is_element());
    let one = elements.next()?;
    if elements.next().is_some() {
        None
    } else {
        Some(one)
    }
}
fn xml_polygon(xml: &str, wrapper: roxmltree::Node<'_, '_>) -> Result<Option<Polygon>> {
    const XC: &str = "http://www.iho.int/s100/xc/5.2";
    const GEX: &str = "http://standards.iso.org/iso/19115/-3/gex/1.0";
    if !wrapper.has_tag_name((XC, "boundingPolygon")) {
        return Ok(None);
    }
    let Some(property) = single_element(wrapper) else {
        return Ok(None);
    };
    if !property.has_tag_name((GEX, "polygon")) {
        return Ok(None);
    }
    let Some(p) = single_element(property) else {
        return Ok(None);
    };
    if !p.has_tag_name((GML, "Polygon")) {
        return Ok(None);
    }
    if !matches!(
        p.attribute("srsName"),
        Some("urn:ogc:def:crs:EPSG::4326") | Some("http://www.opengis.net/def/crs/EPSG/0/4326")
    ) {
        return Ok(None);
    }
    // EPSG4326 axis order latitude, longitude. Explicit conflicting attributes
    // are unsupported, never silently reordered.
    for n in p.descendants().filter(|n| n.is_element()) {
        if n.attribute("srsDimension").is_some_and(|v| v != "2")
            || n.attribute("axisLabels").is_some_and(|v| v != "Lat Long")
            || (n != p && n.attribute("srsName").is_some())
        {
            return Ok(None);
        }
        if n.tag_name().namespace() != Some(GML)
            || !matches!(
                n.tag_name().name(),
                "Polygon" | "exterior" | "interior" | "LinearRing" | "posList"
            )
        {
            return Ok(None);
        }
    }
    let mut exterior = None;
    let mut holes = Vec::new();
    let mut total = 0usize;
    for boundary in p.children().filter(|n| n.is_element()) {
        if !boundary.has_tag_name((GML, "exterior")) && !boundary.has_tag_name((GML, "interior")) {
            return Ok(None);
        }
        let Some(linear) = single_element(boundary) else {
            return Ok(None);
        };
        if !linear.has_tag_name((GML, "LinearRing")) {
            return Ok(None);
        }
        let Some(list) = single_element(linear) else {
            return Ok(None);
        };
        if !list.has_tag_name((GML, "posList")) || list.children().any(|n| n.is_element()) {
            return Ok(None);
        }
        let text = xml
            .get(list.range())
            .context("XC posList captured range invalid")?;
        if text.len() > MAX_COORD_TEXT {
            return Ok(None);
        }
        let mut tokens = list
            .children()
            .filter_map(|n| n.text())
            .flat_map(str::split_ascii_whitespace);
        let mut points = Vec::new();
        while let Some(lat) = tokens.next() {
            let Some(lon) = tokens.next() else {
                return Ok(None);
            };
            let (Ok(lat), Ok(lon)) = (lat.parse::<f64>(), lon.parse::<f64>()) else {
                return Ok(None);
            };
            let Some(q) = point(lon, lat) else {
                return Ok(None);
            };
            total += 1;
            if total > MAX_VERTICES {
                return Ok(None);
            }
            points.push(q);
        }
        let Some(ring) = canonical_ring(points) else {
            return Ok(None);
        };
        match boundary.tag_name().name() {
            "exterior" if exterior.is_none() => exterior = Some(ring),
            "interior" => holes.push(ring),
            _ => return Ok(None),
        }
    }
    holes.sort();
    Ok(exterior.map(|exterior| Polygon { exterior, holes }))
}
fn feature_ring(curves: &[OrientedCurve], cell: &S101Cell) -> Option<Ring> {
    let mut points = Vec::new();
    for c in curves {
        // Composite/curved geometry requires its own verified traversal contract.
        let curve = cell.curves.get(&c.curve_id.key())?;
        if curve.segments.len() != 1
            || curve
                .segments
                .iter()
                .any(|s| s.segment_type != SegmentType::Line)
        {
            return None;
        }
        let first = curve.positions_iter().next();
        let last = curve.positions_iter().last();
        for (id, coordinate) in [(curve.start_point, first), (curve.end_point, last)] {
            if let Some(id) = id {
                let endpoint = cell.points.get(&id.key())?;
                let coordinate = coordinate?;
                if point(endpoint.position.x, endpoint.position.y)?
                    != point(coordinate.x, coordinate.y)?
                {
                    return None;
                }
            }
        }
        let count = curve.positions_iter().count();
        if points.len().checked_add(count)? > MAX_VERTICES {
            return None;
        }
        let mut part = curve
            .positions_iter()
            .map(|p| point(p.x, p.y))
            .collect::<Option<Vec<_>>>()?;
        if !c.orientation {
            part.reverse();
        }
        if points.last().is_some() && points.last() != part.first() {
            return None;
        }
        points.extend(part);
    }
    canonical_ring(points)
}
fn scale(row: &XcDataCoverage) -> Option<ScaleTuple> {
    let positive = |v: &XcScale| {
        if let XcScale::Positive { denominator, .. } = v {
            Some(*denominator)
        } else {
            None
        }
    };
    Some((
        Some(positive(&row.minimum)?),
        positive(&row.optimum)?,
        positive(&row.maximum)?,
    ))
}
// Geometry and scale are a multiset, so duplicate boundaries and differing
// attribution remain visible. No source ordinal correspondence is assumed.
fn check_maps(a: &RegionalScales, b: &RegionalScales) -> Result<RegionStatus> {
    if a.keys().ne(b.keys()) || a.iter().any(|(p, v)| b[p].len() != v.len()) {
        return Ok(RegionStatus::Unverified);
    }
    ensure!(
        a == b,
        "XC exact DataCoverage boundary has different regional scale attribution (S1014.5.2)"
    );
    Ok(RegionStatus::ExactRepresentationAgreement)
}
pub(crate) fn compare(
    bytes: &[u8],
    rows: &[XcDataCoverage],
    cell: &S101Cell,
) -> Result<RegionStatus> {
    if rows.len() > 4096
        || cell.coord_factor.to_bits() != (1.0f64 / 10_000_000.0).to_bits()
        || cell.coord_factor_y.to_bits() != (1.0f64 / 10_000_000.0).to_bits()
        || cell.coord_origin_x != 0.0
        || cell.coord_origin_y != 0.0
    {
        return Ok(RegionStatus::Unverified);
    }
    let xml = std::str::from_utf8(bytes)?;
    let doc = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 200_000,
        },
    )?;
    let nodes: Vec<_> = doc
        .descendants()
        .filter(|n| n.is_element())
        .map(|n| {
            let r = n.range();
            ((r.start, r.end), n)
        })
        .collect();
    ensure!(
        nodes.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "XC DOM range index order invalid"
    );
    let mut total = 0usize;
    let mut xc = BTreeMap::new();
    for row in rows {
        let (Some(p), Some(s)) = (
            xml_polygon(
                xml,
                nodes[nodes
                    .binary_search_by_key(
                        &(
                            row.bounding_polygon_range.start,
                            row.bounding_polygon_range.end,
                        ),
                        |(range, _)| *range,
                    )
                    .map_err(|_| {
                        anyhow::anyhow!("XC retained polygon range does not identify node")
                    })?]
                .1,
            )?,
            scale(row),
        ) else {
            return Ok(RegionStatus::Unverified);
        };
        total = total
            .checked_add(p.exterior.len() + p.holes.iter().map(Vec::len).sum::<usize>())
            .context("XC vertex count overflow")?;
        if total > MAX_VERTICES {
            return Ok(RegionStatus::Unverified);
        }
        xc.entry(p).or_insert_with(Vec::new).push(s);
    }
    total = 0;
    let mut actual = BTreeMap::new();
    for (key, s) in ferrite_s101::coverage_scale::cell_scales(cell)? {
        let feature = &cell.features[&key];
        if feature.primitive_type != ferrite_s100_core::SpatialPrimitiveType::Surface
            || feature.spatial_associations.len() != 1
        {
            return Ok(RegionStatus::Unverified);
        }
        let Some(surface) = cell
            .surfaces
            .get(&feature.spatial_associations[0].spatial_id.key())
        else {
            return Ok(RegionStatus::Unverified);
        };
        let Some(exterior) = feature_ring(&surface.exterior_ring, cell) else {
            return Ok(RegionStatus::Unverified);
        };
        let Some(mut holes) = surface
            .interior_rings
            .iter()
            .map(|r| feature_ring(r, cell))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(RegionStatus::Unverified);
        };
        holes.sort();
        total = total
            .checked_add(exterior.len() + holes.iter().map(Vec::len).sum::<usize>())
            .context("Feature vertex count overflow")?;
        if total > MAX_VERTICES {
            return Ok(RegionStatus::Unverified);
        }
        actual
            .entry(Polygon { exterior, holes })
            .or_insert_with(Vec::new)
            .push((
                s.minimum_denominator,
                s.optimum_denominator,
                s.maximum_denominator,
            ));
    }
    for v in xc.values_mut().chain(actual.values_mut()) {
        v.sort();
    }
    check_maps(&xc, &actual)
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_s100_core::{
        Attribute, DatasetCodeMappings, FeatureRecord, S101Cell, SpatialPrimitiveType, FRID,
    };
    use std::{collections::HashMap, path::PathBuf};
    fn feature(key: u32, min: &str, opt: u32, max: u32) -> FeatureRecord {
        FeatureRecord {
            frid: FRID {
                rcid: key,
                nftc: 0,
                rver: 1,
                ruin: 1,
            },
            foid: None,
            attributes: [
                ("minimumDisplayScale", min.to_owned()),
                ("optimumDisplayScale", opt.to_string()),
                ("maximumDisplayScale", max.to_string()),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (code, atvl))| Attribute {
                natc: 0,
                atix: i as u16 + 1,
                paix: 0,
                atvl,
                value: None,
                code: Some(code.into()),
            })
            .collect(),
            spatial_associations: vec![],
            information_associations: vec![],
            feature_associations: vec![],
            masks: vec![],
            feature_code: Some("DataCoverage".into()),
            primitive_type: SpatialPrimitiveType::Surface,
        }
    }
    fn cell(features: Vec<FeatureRecord>) -> S101Cell {
        S101Cell {
            file_path: PathBuf::from("/private-test/101.000"),
            dsid: ferrite_s100_core::DatasetIdentification::default(),
            code_mappings: DatasetCodeMappings::new(),
            coord_factor: 1. / 10_000_000.,
            coord_factor_y: 1. / 10_000_000.,
            coord_factor_z: 0.01,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
            minimum_display_scale: None,
            maximum_display_scale: None,
            points: HashMap::new(),
            multi_points: HashMap::new(),
            curves: HashMap::new(),
            composite_curves: HashMap::new(),
            surfaces: HashMap::new(),
            features: features
                .into_iter()
                .map(|f| (i64::from(f.frid.rcid), f))
                .collect(),
            information: HashMap::new(),
            spatial_information_associations: HashMap::new(),
        }
    }

    fn ring(x: f64) -> Ring {
        canonical_ring(vec![
            point(x, 0.).unwrap(),
            point(x + 1., 0.).unwrap(),
            point(x + 1., 1.).unwrap(),
            point(x, 0.).unwrap(),
        ])
        .unwrap()
    }
    #[test]
    fn rotation_reversal_closure_exact() {
        let a = ring(0.);
        let mut b = a.clone();
        b.rotate_left(1);
        b.reverse();
        b.push(b[0]);
        assert_eq!(canonical_ring(b), Some(a));
    }
    #[test]
    fn no_quantization_or_collinear_removal() {
        assert_ne!(ring(0.), ring(1e-10));
        assert!(canonical_ring(vec![point(0., 0.).unwrap(); 4]).is_none());
    }
    #[test]
    fn regional_swap_with_same_numeric_multiset_rejects() {
        let p = Polygon {
            exterior: ring(0.),
            holes: vec![],
        };
        let q = Polygon {
            exterior: ring(2.),
            holes: vec![],
        };
        let a = BTreeMap::from([
            (p.clone(), vec![(Some(90000), 45000, 12000)]),
            (q.clone(), vec![(Some(90000), 22000, 6000)]),
        ]);
        let b = BTreeMap::from([
            (q, vec![(Some(90000), 45000, 12000)]),
            (p, vec![(Some(90000), 22000, 6000)]),
        ]);
        assert!(check_maps(&a, &b).is_err());
        assert_eq!(
            check_maps(&a, &a).unwrap(),
            RegionStatus::ExactRepresentationAgreement
        );
    }
    #[test]
    fn hole_not_bbox_or_sibling() {
        let a = Polygon {
            exterior: ring(0.),
            holes: vec![ring(0.1)],
        };
        let b = Polygon {
            exterior: ring(0.),
            holes: vec![],
        };
        let scale = vec![(Some(90000), 45000, 12000)];
        assert_eq!(
            check_maps(
                &BTreeMap::from([(a, scale.clone())]),
                &BTreeMap::from([(b, scale)])
            )
            .unwrap(),
            RegionStatus::Unverified
        );
    }
    #[test]
    fn xml_axis_and_exact_parser() {
        let xml = r#"<xc:boundingPolygon xmlns:xc="http://www.iho.int/s100/xc/5.2" xmlns:gex="http://standards.iso.org/iso/19115/-3/gex/1.0" xmlns:gml="http://www.opengis.net/gml/3.2"><gex:polygon><gml:Polygon srsName="urn:ogc:def:crs:EPSG::4326"><gml:exterior><gml:LinearRing><gml:posList>0 2 0 3 1 3 0 2</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gex:polygon></xc:boundingPolygon>"#;
        let doc = roxmltree::Document::parse(xml).unwrap();
        let p = xml_polygon(xml, doc.root_element()).unwrap().unwrap();
        assert_eq!(p.exterior, ring(2.));
        let bad = xml.replace("EPSG::4326", "EPSG::3857");
        let doc = roxmltree::Document::parse(&bad).unwrap();
        assert!(xml_polygon(&bad, doc.root_element()).unwrap().is_none());
    }

    #[test]
    fn actual_cell_surface_to_captured_xml_positive_and_wrong_attribution() {
        use ferrite_s100_core::{
            Coordinate, CurveRecord, CurveSegment, RecordId, SpatialAssociation, SurfaceRecord,
        };
        let mut c = cell(vec![feature(1, "90000", 45000, 12000)]);
        let cid = RecordId::new(120, 1);
        let sid = RecordId::new(130, 1);
        c.features
            .get_mut(&1)
            .unwrap()
            .spatial_associations
            .push(SpatialAssociation {
                spatial_id: sid,
                ornt: 1,
                usag: 1,
                mask: 0,
                scale_minimum: None,
                scale_maximum: None,
                update_instruction: 1,
            });
        c.curves.insert(
            cid.key(),
            CurveRecord {
                id: cid,
                segments: vec![CurveSegment {
                    segment_type: SegmentType::Line,
                    positions: vec![
                        Coordinate::new(2., 0.),
                        Coordinate::new(3., 0.),
                        Coordinate::new(3., 1.),
                        Coordinate::new(2., 0.),
                    ],
                }],
                start_point: None,
                end_point: None,
                update_instruction: 1,
            },
        );
        c.surfaces.insert(
            sid.key(),
            SurfaceRecord {
                id: sid,
                exterior_ring: vec![OrientedCurve {
                    curve_id: cid,
                    orientation: true,
                }],
                interior_rings: vec![],
                update_instruction: 1,
            },
        );
        let xml = r#"<xc:boundingPolygon xmlns:xc="http://www.iho.int/s100/xc/5.2" xmlns:gex="http://standards.iso.org/iso/19115/-3/gex/1.0" xmlns:gml="http://www.opengis.net/gml/3.2"><gex:polygon><gml:Polygon srsName="http://www.opengis.net/def/crs/EPSG/0/4326"><gml:exterior><gml:LinearRing><gml:posList>0 2 0 3 1 3 0 2</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gex:polygon></xc:boundingPolygon>"#;
        let positive = |v: u32| XcScale::Positive {
            lexical: v.to_string(),
            denominator: v,
        };
        let mut row = XcDataCoverage {
            entry_range: 0..xml.len(),
            bounding_polygon_range: 0..xml.len(),
            minimum: positive(90000u32),
            optimum: positive(45000),
            maximum: positive(12000),
        };
        assert_eq!(
            compare(xml.as_bytes(), &[row.clone()], &c).unwrap(),
            RegionStatus::ExactRepresentationAgreement
        );
        row.maximum = positive(6000);
        assert!(compare(xml.as_bytes(), &[row], &c).is_err());
        let point_id = RecordId::new(110, 9);
        c.points.insert(
            point_id.key(),
            ferrite_s100_core::PointRecord {
                id: point_id,
                position: Coordinate::new(99., 0.),
                update_instruction: 1,
            },
        );
        c.curves.get_mut(&cid.key()).unwrap().start_point = Some(point_id);
        assert!(
            feature_ring(&c.surfaces[&sid.key()].exterior_ring, &c).is_none(),
            "V1 would certify omitted separate endpoint"
        );
        c.points.get_mut(&point_id.key()).unwrap().position = Coordinate::new(2., 0.);
        assert!(feature_ring(&c.surfaces[&sid.key()].exterior_ring, &c).is_some());
        c.points.remove(&point_id.key());
        assert!(feature_ring(&c.surfaces[&sid.key()].exterior_ring, &c).is_none());
        c.curves.get_mut(&cid.key()).unwrap().start_point = None;
        c.curves.get_mut(&cid.key()).unwrap().segments[0].segment_type = SegmentType::Arc;
        assert!(feature_ring(&c.surfaces[&sid.key()].exterior_ring, &c).is_none());
    }

    #[test]
    fn nested_poslist_and_wrong_boundary_hierarchy_unverified() {
        let base = r#"<xc:boundingPolygon xmlns:xc="http://www.iho.int/s100/xc/5.2" xmlns:gex="http://standards.iso.org/iso/19115/-3/gex/1.0" xmlns:gml="http://www.opengis.net/gml/3.2"><gex:polygon><gml:Polygon srsName="http://www.opengis.net/def/crs/EPSG/0/4326"><gml:exterior><gml:LinearRing><gml:posList>0 2 0 3 1 3 0 2</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></gex:polygon></xc:boundingPolygon>"#;
        for bad in [
            base.replace("<gml:LinearRing>", "<gml:LinearRing><gml:interior>")
                .replace("</gml:LinearRing>", "</gml:interior></gml:LinearRing>"),
            base.replace("<gml:posList>", "<gml:posList><gml:posList>")
                .replace("</gml:posList>", "</gml:posList></gml:posList>"),
            base.replace("gml:LinearRing", "gml:Polygon"),
        ] {
            let doc = roxmltree::Document::parse(&bad).unwrap();
            assert!(xml_polygon(&bad, doc.root_element()).unwrap().is_none());
        }
    }
}
