//! Bounded namespace-aware route subset. This is not a full XSD/product validator.
use crate::route::{Route, Waypoint};
use quick_xml::{events::Event, Reader};
use roxmltree::{Document, Node};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: u32 = 100_000;
const MAX_DEPTH: usize = 128;
const MAX_WAYPOINTS: usize = 16_384;
const GML: &str = "http://www.opengis.net/gml/3.2";
const XLINK: &str = "http://www.w3.org/1999/xlink";
const NS1: &str = "http://www.iho.int/S421/gml/cs0/1.0";
const NS2: &str = "http://www.iec.ch/S421/2.0";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Published1,
    Candidate2,
}
impl Profile {
    fn namespace(self) -> &'static str {
        match self {
            Self::Published1 => NS1,
            Self::Candidate2 => NS2,
        }
    }
    fn property(self, n: Node<'_, '_>, name: &str) -> bool {
        n.is_element()
            && n.tag_name().name() == name
            && n.tag_name().namespace()
                == match self {
                    Self::Published1 => None,
                    Self::Candidate2 => Some(NS2),
                }
    }
    fn s100(self) -> &'static str {
        match self {
            Self::Published1 => "http://www.iho.int/s100gml/1.0",
            Self::Candidate2 => "http://www.iho.int/s100gml/5.0",
        }
    }
}
/// Declared FC value; Orthodrome does not by itself select a numerical solver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LegGeometry {
    Loxodrome,
    Orthodrome,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedLeg {
    pub gml_id: String,
    /// Supported producer-example incoming-leg convention, checked against route order.
    pub from_waypoint_id: u32,
    pub to_waypoint_id: u32,
    pub geometry: LegGeometry,
    pub original_geometry_text: String,
}
// A supplied curve is never discarded in favour of waypoint-derived control points.
// Narrow explicit-CRS, explicit-interpolation, two-control subset only.
fn validate_leg_curve(
    leg: Node<'_, '_>,
    profile: Profile,
    geometry: LegGeometry,
    start: [f64; 2],
    end: [f64; 2],
) -> Result<(), String> {
    let Some(container) = property(leg, profile, "geometry")? else {
        return Ok(());
    };
    let mut properties = container.children().filter(|n| n.is_element());
    let prop = properties.next().ok_or("Missing leg curveProperty")?;
    if properties.next().is_some() || !prop.has_tag_name((profile.s100(), "curveProperty")) {
        return Err("Unsupported leg geometry property".into());
    }
    let mut curves = prop.children().filter(|n| n.is_element());
    let curve = curves.next().ok_or("Missing explicit leg Curve")?;
    if curves.next().is_some() || !curve.has_tag_name((profile.s100(), "Curve")) {
        return Err("Referenced/multiple leg curves unsupported".into());
    }
    let crs = curve
        .ancestors()
        .find_map(|n| n.attribute("srsName"))
        .ok_or("Missing leg Curve CRS; no Envelope inheritance")?;
    if !matches!(
        crs,
        "EPSG:4326" | "urn:ogc:def:crs:EPSG::4326" | "http://www.opengis.net/def/crs/EPSG/0/4326"
    ) {
        return Err("Unsupported leg Curve CRS".into());
    }
    let mut groups = curve.children().filter(|n| n.is_element());
    let segments = groups.next().ok_or("Missing leg segments")?;
    if groups.next().is_some() || !segments.has_tag_name((GML, "segments")) {
        return Err("Unsupported leg Curve members".into());
    }
    let mut members = segments.children().filter(|n| n.is_element());
    let segment = members.next().ok_or("Missing leg segment")?;
    if members.next().is_some() {
        return Err("Multiple control segments not yet implemented".into());
    }
    // Standard GML GeodesicString has geodesic interpolation. Do not invent
    // a loxodromic GML LineStringSegment (whose interpolation is linear).
    if geometry != LegGeometry::Orthodrome
        || !segment.has_tag_name((GML, "GeodesicString"))
        || segment
            .attribute("interpolation")
            .is_some_and(|value| value != "geodesic")
    {
        return Err(
            "Explicit leg control segment unsupported or conflicts with declaration".into(),
        );
    }
    let mut controls = segment.children().filter(|n| n.is_element());
    let list = controls.next().ok_or("Missing leg posList")?;
    if controls.next().is_some() || !list.has_tag_name((GML, "posList")) {
        return Err("Unsupported leg controls".into());
    }
    for node in list.ancestors() {
        if node.attribute("srsDimension").is_some_and(|v| v != "2")
            || node
                .attribute("axisLabels")
                .is_some_and(|v| !matches!(v, "Lat Long" | "Lat Lon" | "Latitude Longitude"))
            || node.attribute("srsName").is_some_and(|v| {
                !matches!(
                    v,
                    "EPSG:4326"
                        | "urn:ogc:def:crs:EPSG::4326"
                        | "http://www.opengis.net/def/crs/EPSG/0/4326"
                )
            })
        {
            return Err("Leg control CRS/axes mismatch".into());
        }
    }
    let mut tokens = text(list)?.split_whitespace();
    for expected in [start[1], start[0], end[1], end[0]] {
        let actual: f64 = tokens
            .next()
            .ok_or("Leg control count mismatch")?
            .parse()
            .map_err(|_| "Invalid leg coordinate")?;
        if !actual.is_finite() || actual.to_bits() != expected.to_bits() {
            return Err("Leg controls differ from ordered original waypoint endpoints".into());
        }
    }
    if tokens.next().is_some() {
        return Err("Intermediate leg control points unsupported".into());
    }
    Ok(())
}
fn leg_geometry(node: Node<'_, '_>, profile: Profile) -> Result<LegGeometry, String> {
    let lexical = text(node)?;
    let geometry = match profile {
        Profile::Published1 => match lexical.parse::<u8>() {
            Ok(1) => LegGeometry::Loxodrome,
            Ok(2) => LegGeometry::Orthodrome,
            _ => return Err("Unsupported published leg geometry code".into()),
        },
        Profile::Candidate2 => match lexical {
            "loxodrome" => LegGeometry::Loxodrome,
            "orthodrome" => LegGeometry::Orthodrome,
            _ => return Err("Unsupported CDV leg geometry label".into()),
        },
    };
    if let Some(code) = node.attribute("code") {
        let expected = match geometry {
            LegGeometry::Loxodrome => 1,
            LegGeometry::Orthodrome => 2,
        };
        if code.parse::<u8>().ok() != Some(expected) {
            return Err("Leg geometry label/code mismatch".into());
        }
    }
    Ok(geometry)
}
#[derive(Debug, Clone)]
pub struct ImportedRoute {
    pub route_id: String,
    pub edition: u32,
    pub gml_id: String,
    pub route: Route,
    /// Only explicitly declared, reference-validated incoming legs; absence is not a default.
    pub legs: Vec<ImportedLeg>,
}
impl ImportedRoute {
    /// Current host renderer emits projected straight segments. Declared curves must
    /// reach a curve-capable evaluator rather than silently losing their FC meaning.
    pub fn require_straight_host_illustration_supported(&self) -> Result<(), String> {
        if !self.legs.is_empty() {
            return Err("Declared S421 leg curves require semantic path evaluation; straight host illustration unsupported".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
pub struct ImportedDataset {
    pub profile: Profile,
    /// Original lexical metadata/unsupported schedules/legs remain available, never rewritten.
    pub original_xml: Arc<str>,
    pub routes: Vec<ImportedRoute>,
    /// Producer-example bare local href syntax is accepted only in Published1.
    /// It is not evidence that arbitrary URI references are permitted by GML.
    pub compatibility_warnings: Vec<String>,
}
pub(crate) fn preflight(xml: &str) -> Result<(), String> {
    if xml.len() > MAX_XML_BYTES {
        return Err("S421 XML receiver byte limit exceeded".into());
    }
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut nodes = 0u32;
    loop {
        let event = reader.read_event().map_err(|e| e.to_string())?;
        if matches!(event, Event::DocType(_)) {
            return Err("DTD forbidden in S421 XML".into());
        }
        if matches!(
            event,
            Event::Start(_)
                | Event::Empty(_)
                | Event::Text(_)
                | Event::CData(_)
                | Event::Comment(_)
                | Event::PI(_)
        ) {
            nodes = nodes.checked_add(1).ok_or("XML node count overflow")?;
            if nodes > MAX_NODES {
                return Err("S421 XML receiver node limit exceeded".into());
            }
        }
        match event {
            Event::Start(_) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err("S421 XML receiver depth limit exceeded".into());
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or("Unbalanced XML")?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 {
        return Err("Unbalanced XML".into());
    }
    Ok(())
}
pub(crate) fn read_xml_bounded(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take((MAX_XML_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_XML_BYTES {
        return Err("Catalogue XML receiver byte limit exceeded".into());
    }
    let xml = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    preflight(&xml)?;
    // Generic catalogue/resource XML safety only. Route semantics belong solely
    // to import_dataset/export_published; FC/PC are not S421 Dataset roots.
    Document::parse_with_options(
        &xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_NODES,
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(xml)
}
fn property<'a, 'i>(
    n: Node<'a, 'i>,
    p: Profile,
    name: &str,
) -> Result<Option<Node<'a, 'i>>, String> {
    let mut found = n.children().filter(|n| p.property(*n, name));
    let first = found.next();
    if found.next().is_some() {
        return Err(format!("Duplicate {name}"));
    }
    Ok(first)
}
fn required<'a, 'i>(n: Node<'a, 'i>, p: Profile, name: &str) -> Result<Node<'a, 'i>, String> {
    property(n, p, name)?.ok_or_else(|| format!("Missing {name}"))
}
fn text<'a, 'i>(n: Node<'a, 'i>) -> Result<&'a str, String> {
    if n.children().any(|n| n.is_element()) {
        return Err("Scalar property contains element".into());
    }
    if n.children().filter(|child| child.is_text()).count() != 1 {
        return Err("Split/missing scalar text unsupported; no trailing text discarded".into());
    }
    let s = n.text().unwrap_or("").trim();
    if s.is_empty() || s.len() > 65536 {
        return Err("Empty/oversized scalar".into());
    }
    Ok(s)
}
// Published RadiusType is xs:decimal with fractionDigits=2. Reject unsupported
// lexical forms before binary64 decoding, rather than silently rounding them.
pub(crate) fn published_radius(s: &str) -> Result<f64, String> {
    let body = s
        .strip_prefix('+')
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    let mut parts = body.split('.');
    let integer = parts.next().unwrap_or("");
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || (integer.is_empty() && fraction.is_empty())
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.trim_end_matches('0').len() > 2
    {
        return Err("Radius outside published decimal subset".into());
    }
    let value: f64 = s.parse().map_err(|_| "Invalid radius")?;
    if !value.is_finite() || !(0.0..=5.0).contains(&value) {
        return Err("Radius outside published range 0..5".into());
    }
    Ok(value)
}
fn check_status(node: Node<'_, '_>, profile: Profile) -> Result<(), String> {
    let value = text(node)?;
    match profile {
        Profile::Published1 => {
            let code: u8 = value.parse().map_err(|_| "Invalid published status")?;
            if !(1..=11).contains(&code) {
                return Err("Invalid published status".into());
            }
        }
        Profile::Candidate2 => {
            let labels = [
                "Initial",
                "Planned",
                "Recommended",
                "Checked",
                "Used for Monitoring",
                "Terminated",
                "Errors",
                "Incomplete",
                "Route issues",
            ];
            let index = labels
                .iter()
                .position(|label| *label == value)
                .ok_or("Unsupported CDV status")?;
            if let Some(code) = node.attribute("code") {
                if code
                    .parse::<usize>()
                    .map_err(|_| "Invalid CDV status code")?
                    != index + 1
                {
                    return Err("CDV status label/code mismatch".into());
                }
            }
        }
    }
    Ok(())
}
fn positive(n: Node<'_, '_>) -> Result<u32, String> {
    let n: u32 = text(n)?.parse().map_err(|_| "Invalid positive integer")?;
    if n == 0 {
        return Err("Zero positive integer".into());
    }
    Ok(n)
}
fn resolve<'a, 'i>(
    property: Node<'a, 'i>,
    ids: &HashMap<&str, Node<'a, 'i>>,
    p: Profile,
    kind: &str,
) -> Result<Node<'a, 'i>, String> {
    let href = property
        .attribute((XLINK, "href"))
        .ok_or("Missing xlink href")?;
    // Published fixtures also use bare local ID tokens. Never resolve URLs or paths.
    let id = href
        .strip_prefix('#')
        .or_else(|| {
            (p == Profile::Published1 && !href.contains(['/', ':', '#', '?']) && !href.is_empty())
                .then_some(href)
        })
        .ok_or("External/invalid route reference unsupported")?;
    let target = *ids
        .get(id)
        .ok_or_else(|| format!("Unresolved reference {href}"))?;
    if target.tag_name().namespace() != Some(p.namespace()) || target.tag_name().name() != kind {
        return Err(format!("Reference {href} is not {kind}"));
    }
    Ok(target)
}
fn position<'a, 'i>(
    n: Node<'a, 'i>,
    p: Profile,
    ids: &HashMap<&'a str, Node<'a, 'i>>,
) -> Result<(f64, f64), String> {
    let geometry = required(n, p, "geometry")?;
    let mut props = geometry
        .children()
        .filter(|n| n.has_tag_name((p.s100(), "pointProperty")));
    let prop = props.next().ok_or("Missing S100 pointProperty")?;
    if props.next().is_some() {
        return Err("Multiple waypoint geometries".into());
    }
    let point = if let Some(href) = prop.attribute((XLINK, "href")) {
        let id = href
            .strip_prefix('#')
            .ok_or("External point reference unsupported")?;
        *ids.get(id).ok_or("Unresolved point reference")?
    } else {
        let mut points = prop.children().filter(|n| n.is_element());
        let point = points.next().ok_or("Missing Point")?;
        if points.next().is_some() {
            return Err("Multiple pointProperty objects".into());
        }
        point
    };
    if !point.has_tag_name((p.s100(), "Point")) {
        return Err("Waypoint geometry is not S100 Point".into());
    }
    let crs = point
        .ancestors()
        .find_map(|n| n.attribute("srsName"))
        .ok_or("Missing point CRS; Envelope CRS is not inherited")?;
    if !matches!(
        crs,
        "EPSG:4326" | "urn:ogc:def:crs:EPSG::4326" | "http://www.opengis.net/def/crs/EPSG/0/4326"
    ) {
        return Err(format!("Unsupported point CRS {crs}"));
    }
    if point
        .ancestors()
        .filter_map(|n| n.attribute("srsDimension"))
        .any(|s| s != "2")
    {
        return Err("Non-2D waypoint geometry".into());
    }
    if point
        .ancestors()
        .filter_map(|n| n.attribute("axisLabels"))
        .any(|s| !matches!(s, "Lat Long" | "Lat Lon" | "Latitude Longitude"))
    {
        return Err("Axis labels contradict supported EPSG4326 latitude-longitude order".into());
    }
    let mut pos = point.children().filter(|n| n.has_tag_name((GML, "pos")));
    let pos = pos.next().ok_or("Missing gml:pos")?;
    if point
        .children()
        .filter(|n| n.has_tag_name((GML, "pos")))
        .count()
        != 1
    {
        return Err("Duplicate position".into());
    }
    if pos.attribute("srsDimension").is_some_and(|v| v != "2")
        || pos
            .attribute("axisLabels")
            .is_some_and(|v| !matches!(v, "Lat Long" | "Lat Lon" | "Latitude Longitude"))
        || pos.attribute("srsName").is_some_and(|v| {
            !matches!(
                v,
                "EPSG:4326"
                    | "urn:ogc:def:crs:EPSG::4326"
                    | "http://www.opengis.net/def/crs/EPSG/0/4326"
            )
        })
    {
        return Err("Position metadata contradicts supported Point CRS/axes".into());
    }
    let mut tokens = text(pos)?.split_whitespace();
    let lat: f64 = tokens
        .next()
        .ok_or("Missing latitude")?
        .parse()
        .map_err(|_| "Invalid latitude")?;
    let lon: f64 = tokens
        .next()
        .ok_or("Missing longitude")?
        .parse()
        .map_err(|_| "Invalid longitude")?;
    if tokens.next().is_some()
        || !lat.is_finite()
        || !lon.is_finite()
        || !(-90. ..=90.).contains(&lat)
        || !(-180. ..=180.).contains(&lon)
    {
        return Err("Invalid EPSG4326 position".into());
    }
    Ok((lon, lat))
}
fn supported_ncname(id: &str) -> bool {
    // Deliberately bounded ASCII NCName subset. Unicode NCNames are an
    // unsupported capability, not a declaration that such XML is invalid.
    let mut bytes = id.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}
pub fn import_dataset(xml: &str) -> Result<ImportedDataset, String> {
    preflight(xml)?;
    let document = Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_NODES,
        },
    )
    .map_err(|e| e.to_string())?;
    let root = document.root_element();
    if root.tag_name().name() != "Dataset" {
        return Err("Expected S421 Dataset".into());
    }
    let profile = match root.tag_name().namespace() {
        Some(NS1) => Profile::Published1,
        Some(NS2) => Profile::Candidate2,
        _ => return Err("Unsupported S421 namespace".into()),
    };
    let mut ids = HashMap::new();
    for n in root.descendants().filter(|n| n.is_element()) {
        if let Some(id) = n.attribute((GML, "id")) {
            if id.is_empty()
                || id.len() > 256
                || !supported_ncname(id)
                || ids.insert(id, n).is_some()
            {
                return Err("Invalid/duplicate gml:id".into());
            }
        }
    }
    let mut features = Vec::new();
    for container in root.children().filter(|n| match profile {
        Profile::Published1 => profile.property(*n, "member") || profile.property(*n, "imember"),
        Profile::Candidate2 => profile.property(*n, "members"),
    }) {
        for n in container.children().filter(|n| n.is_element()) {
            if n.tag_name().namespace() != Some(profile.namespace()) {
                return Err("Unexpected feature namespace".into());
            }
            features.push(n);
        }
    }
    let feature_ids: HashSet<_> = features.iter().map(|n| n.id()).collect();
    let mut routes = Vec::new();
    let mut claimed_legs = HashSet::new();
    for route_node in features
        .iter()
        .copied()
        .filter(|n| n.tag_name().name() == "Route")
    {
        let version = text(required(route_node, profile, "routeFormatVersion")?)?;
        if version
            != match profile {
                Profile::Published1 => "1.0",
                Profile::Candidate2 => "2.0",
            }
        {
            return Err("Route version contradicts namespace".into());
        }
        let route_id = text(required(route_node, profile, "routeID")?)?.to_owned();
        let edition = positive(required(route_node, profile, "routeEditionNo")?)?;
        let info = resolve(
            required(route_node, profile, "routeInfo")?,
            &ids,
            profile,
            "RouteInfo",
        )?;
        if !feature_ids.contains(&info.id()) {
            return Err("RouteInfo not a dataset member".into());
        }
        let name = text(required(info, profile, "routeInfoName")?)?;
        check_status(required(info, profile, "routeInfoStatus")?, profile)?;
        let mut route = Route::with_name(0, name);
        let mut legs = Vec::new();
        let gml_id = route_node
            .attribute((GML, "id"))
            .ok_or("Route has no gml:id")?
            .to_owned();
        if let Some(prop) = property(route_node, profile, "routeWaypoints")? {
            let group = resolve(prop, &ids, profile, "RouteWaypoints")?;
            if !feature_ids.contains(&group.id()) {
                return Err("RouteWaypoints not a dataset member".into());
            }
            if resolve(
                required(group, profile, "routeWaypointsCollection")?,
                &ids,
                profile,
                "Route",
            )? != route_node
            {
                return Err("Waypoints belong to another route".into());
            }
            let references: Vec<_> = group
                .children()
                .filter(|n| profile.property(*n, "routeWaypoint"))
                .collect();
            if references.len() < 2 || references.len() > MAX_WAYPOINTS {
                return Err("Waypoint collection outside supported cardinality".into());
            }
            let mut waypoint_ids = HashSet::new();
            let mut targets = HashSet::new();
            for reference in references {
                let wp = resolve(reference, &ids, profile, "RouteWaypoint")?;
                if !feature_ids.contains(&wp.id()) || !targets.insert(wp.id()) {
                    return Err("Waypoint not a unique dataset member".into());
                }
                if resolve(
                    required(wp, profile, "routeWaypointCollection")?,
                    &ids,
                    profile,
                    "RouteWaypoints",
                )? != group
                {
                    return Err("Waypoint belongs to another collection".into());
                }
                let id = positive(required(wp, profile, "routeWaypointID")?)?;
                if !waypoint_ids.insert(id) {
                    return Err("Duplicate waypoint ID".into());
                }
                let (lon, lat) = position(wp, profile, &ids)?;
                let radius_node = property(wp, profile, "routeWaypointTurnRadius")?;
                let radius = match (profile, radius_node) {
                    (Profile::Published1, Some(node)) => Some(published_radius(text(node)?)?),
                    (Profile::Published1, None) => {
                        return Err("Missing published turn radius".into())
                    }
                    (Profile::Candidate2, Some(node)) => {
                        let value: f64 = text(node)?.parse().map_err(|_| "Invalid CDV radius")?;
                        if !value.is_finite() || value < 0.0 {
                            return Err("Unsupported CDV radius".into());
                        }
                        Some(value)
                    }
                    (Profile::Candidate2, None) => None,
                };
                let mut waypoint = Waypoint::new(id, lon, lat);
                waypoint.turn_radius = radius;
                if let Some(name) = property(wp, profile, "routeWaypointName")? {
                    waypoint.name = Some(text(name)?.to_owned());
                }
                if let Some(reference) = property(wp, profile, "routeWaypointLeg")? {
                    let leg = resolve(reference, &ids, profile, "RouteWaypointLeg")?;
                    if !feature_ids.contains(&leg.id()) || !claimed_legs.insert(leg.id()) {
                        return Err("Leg not a unique dataset member".into());
                    }
                    if resolve(
                        required(leg, profile, "routeWaypointLegCollection")?,
                        &ids,
                        profile,
                        "RouteWaypoint",
                    )? != wp
                    {
                        return Err("Leg belongs to another waypoint".into());
                    }
                    let previous = route
                        .waypoints
                        .last()
                        .ok_or("First-waypoint leg outside supported incoming-leg convention")?;
                    let geometry_node = required(leg, profile, "routeWaypointLegGeometryType")?;
                    let geometry = leg_geometry(geometry_node, profile)?;
                    waypoint.incoming_geometry = Some(geometry);
                    validate_leg_curve(
                        leg,
                        profile,
                        geometry,
                        [previous.lon, previous.lat],
                        [lon, lat],
                    )?;
                    legs.push(ImportedLeg {
                        gml_id: leg
                            .attribute((GML, "id"))
                            .ok_or("Leg has no gml:id")?
                            .to_owned(),
                        from_waypoint_id: previous.id,
                        to_waypoint_id: id,
                        geometry,
                        original_geometry_text: text(geometry_node)?.to_owned(),
                    });
                }
                route.add_waypoint(waypoint);
            }
        }
        let next = route
            .waypoints
            .iter()
            .map(|wp| wp.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("Waypoint counter exhausted")?;
        route.set_next_id(next);
        routes.push(ImportedRoute {
            route_id,
            edition,
            gml_id,
            route,
            legs,
        });
    }
    if routes.is_empty() {
        return Err("No S421 Route member".into());
    }
    Ok(ImportedDataset {
        profile,
        original_xml: Arc::from(xml),
        routes,
        compatibility_warnings: if profile == Profile::Published1
            && root.descendants().any(|node| {
                node.attribute((XLINK, "href"))
                    .is_some_and(|href| !href.starts_with('#') && !href.is_empty())
            }) {
            vec!["Published producer example contains bare local xlink href; accepted local-ID compatibility subset, not general URI resolution".into()]
        } else {
            Vec::new()
        },
    })
}
pub fn import_route(xml: &str) -> Result<Route, String> {
    let mut d = import_dataset(xml)?;
    if d.routes.len() != 1 {
        return Err("Single-route UI cannot import multiple route members".into());
    }
    Ok(d.routes.remove(0).route)
}
/// Explicit identity/edition/status; no fabricated timestamps or rounded coordinates.
pub struct PublishedExport<'a> {
    pub route_id: &'a str,
    pub edition: u32,
    pub status: u8,
}
pub fn export_published(route: &Route, metadata: PublishedExport<'_>) -> Result<String, String> {
    if route.waypoints.len() < 2
        || route.waypoints.len() > MAX_WAYPOINTS
        || metadata.edition == 0
        || !(1..=11).contains(&metadata.status)
    {
        return Err("Invalid published export metadata/cardinality".into());
    }
    if route.waypoints[0].incoming_geometry.is_some() {
        return Err("First waypoint cannot declare an incoming leg".into());
    }
    for pair in route.waypoints.windows(2) {
        match pair[1].incoming_geometry {
            Some(LegGeometry::Loxodrome) => {
                return Err(
                    "Loxodrome export unsupported; refusing to erase route geometry".into(),
                );
            }
            Some(LegGeometry::Orthodrome) => {
                crate::navigation::evaluate_leg(
                    [pair[0].lon, pair[0].lat],
                    [pair[1].lon, pair[1].lat],
                    LegGeometry::Orthodrome,
                )?;
            }
            None => {}
        }
    }
    let name = route.name.as_deref().ok_or("Route name required")?;
    for value in [name, metadata.route_id] {
        if value.len() < 3
            || value.len() > 4096
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
            || !value.as_bytes()[0].is_ascii_alphanumeric()
            || !value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        {
            return Err("Name/identity outside supported published export text subset".into());
        }
    }
    let mut ids = HashSet::new();
    for wp in &route.waypoints {
        if wp.id == 0
            || !ids.insert(wp.id)
            || !wp.lat.is_finite()
            || !wp.lon.is_finite()
            || !(-90. ..=90.).contains(&wp.lat)
            || !(-180. ..=180.).contains(&wp.lon)
        {
            return Err("Invalid export waypoint".into());
        }
        let radius = wp
            .turn_radius
            .ok_or("Explicit turn radius required; not inferred")?;
        published_radius(&radius.to_string())?;
    }
    let mut xml=format!("<?xml version=\"1.0\"?><S421:Dataset xmlns:S421=\"{NS1}\" xmlns:S100=\"http://www.iho.int/s100gml/1.0\" xmlns:gml=\"{GML}\" xmlns:xlink=\"{XLINK}\" gml:id=\"DATASET.RTE\"><member><S421:Route gml:id=\"RTE\"><routeFormatVersion>1.0</routeFormatVersion><routeID>{}</routeID><routeEditionNo>{}</routeEditionNo><routeInfo xlink:href=\"#RTE.INFO\"/><routeWaypoints xlink:href=\"#RTE.WPTS\"/></S421:Route></member><imember><S421:RouteInfo gml:id=\"RTE.INFO\"><routeInfoName>{}</routeInfoName><routeInfoStatus>{}</routeInfoStatus></S421:RouteInfo></imember><member><S421:RouteWaypoints gml:id=\"RTE.WPTS\"><routeWaypointsCollection xlink:href=\"#RTE\"/>",escape(metadata.route_id),metadata.edition,escape(name),metadata.status);
    for wp in &route.waypoints {
        xml.push_str(&format!("<routeWaypoint xlink:href=\"#WPT.{}\"/>", wp.id));
    }
    xml.push_str("</S421:RouteWaypoints></member>");
    for wp in &route.waypoints {
        xml.push_str(&format!("<member><S421:RouteWaypoint gml:id=\"WPT.{}\"><geometry><S100:pointProperty><S100:Point gml:id=\"POINT.{}\" srsName=\"EPSG:4326\" srsDimension=\"2\"><gml:pos>{} {}</gml:pos></S100:Point></S100:pointProperty></geometry><routeWaypointID>{}</routeWaypointID>",wp.id,wp.id,wp.lat,wp.lon,wp.id));
        if let Some(name) = &wp.name {
            xml.push_str(&format!(
                "<routeWaypointName>{}</routeWaypointName>",
                escape(name)
            ));
        }
        xml.push_str(&format!("<routeWaypointTurnRadius>{}</routeWaypointTurnRadius><routeWaypointCollection xlink:href=\"#RTE.WPTS\"/>",wp.turn_radius.unwrap()));
        if wp.incoming_geometry.is_some() {
            xml.push_str(&format!(
                "<routeWaypointLeg xlink:href=\"#LEG.{}\"/>",
                wp.id
            ));
        }
        xml.push_str("</S421:RouteWaypoint></member>");
    }
    for pair in route.waypoints.windows(2) {
        let (start, end) = (&pair[0], &pair[1]);
        if end.incoming_geometry == Some(LegGeometry::Orthodrome) {
            // Published S-421 GM_Curve -> S100:curveProperty. EPSG:4326 axis
            // order is latitude, longitude; shortest-decimal f64 formatting
            // round-trips the exact waypoint values without resampling.
            xml.push_str(&format!("<member><S421:RouteWaypointLeg gml:id=\"LEG.{}\"><geometry><S100:curveProperty><S100:Curve gml:id=\"CURVE.{}\" srsName=\"EPSG:4326\" srsDimension=\"2\"><gml:segments><gml:GeodesicString interpolation=\"geodesic\"><gml:posList>{} {} {} {}</gml:posList></gml:GeodesicString></gml:segments></S100:Curve></S100:curveProperty></geometry><routeWaypointLegGeometryType>2</routeWaypointLegGeometryType><routeWaypointLegCollection xlink:href=\"#WPT.{}\"/></S421:RouteWaypointLeg></member>",end.id,end.id,start.lat,start.lon,end.lat,end.lon,end.id));
        }
    }
    xml.push_str("</S421:Dataset>");
    preflight(&xml)?;
    // Validate the exact output with the same namespace/reference/coordinate reader;
    // independent offline XSD validation remains a separate qualification gate.
    import_dataset(&xml)?;
    Ok(xml)
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
/// Legacy controller adapter: choose explicit local route identity; missing radius still rejects.
pub fn export_route(route: &Route) -> Result<String, String> {
    export_published(
        route,
        PublishedExport {
            route_id: &format!("FERRITE.ROUTE.{}", route.id),
            edition: 1,
            status: 1,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplied_curve_controls_and_interpolation_are_not_silently_replaced() {
        let xml = with_leg("2", 9);
        let curve = "<geometry><S100:curveProperty><S100:Curve srsName=\"EPSG:4326\"><gml:segments><gml:GeodesicString interpolation=\"geodesic\"><gml:posList>59.23456789012345 25.123456789012345 -89.99999999999999 -179.99999999999997</gml:posList></gml:GeodesicString></gml:segments></S100:Curve></S100:curveProperty></geometry>";
        let xml = xml.replace(
            "<routeWaypointLegGeometryType>",
            &format!("{curve}<routeWaypointLegGeometryType>"),
        );
        assert!(import_dataset(&xml).is_ok());
        assert!(import_dataset(
            &xml.replace("interpolation=\"geodesic\"", "interpolation=\"linear\"")
        )
        .is_err());
        assert!(import_dataset(&xml.replace(
            "59.23456789012345 25.123456789012345",
            "59.23456789012345 25.0"
        ))
        .is_err());
        assert!(import_dataset(
            &xml.replace(" srsName=\"EPSG:4326\"><gml:segments>", "><gml:segments>")
        )
        .is_err());
    }
    #[test]
    fn leg_fc_profiles_are_distinct_and_code_consistent() {
        for (profile, xml, expected) in [
            (Profile::Published1, "<v>1</v>", LegGeometry::Loxodrome),
            (Profile::Published1, "<v>2</v>", LegGeometry::Orthodrome),
            (
                Profile::Candidate2,
                "<v code=\"1\">loxodrome</v>",
                LegGeometry::Loxodrome,
            ),
            (
                Profile::Candidate2,
                "<v code=\"2\">orthodrome</v>",
                LegGeometry::Orthodrome,
            ),
        ] {
            let document = Document::parse(xml).unwrap();
            assert_eq!(
                leg_geometry(document.root_element(), profile).unwrap(),
                expected
            );
        }
        for (profile, xml) in [
            (Profile::Published1, "<v>loxodrome</v>"),
            (Profile::Candidate2, "<v>1</v>"),
            (Profile::Candidate2, "<v code=\"2\">loxodrome</v>"),
            (Profile::Published1, "<v>3</v>"),
        ] {
            let document = Document::parse(xml).unwrap();
            assert!(leg_geometry(document.root_element(), profile).is_err());
        }
    }
    fn with_leg(value: &str, target: u32) -> String {
        let xml = exported().replace(
            "<routeWaypointID>9</routeWaypointID>",
            "<routeWaypointID>9</routeWaypointID><routeWaypointLeg xlink:href=\"#LEG.1\"/>",
        );
        xml.replace("</S421:Dataset>", &format!(
            "<member><S421:RouteWaypointLeg gml:id=\"LEG.1\"><routeWaypointLegGeometryType>{value}</routeWaypointLegGeometryType><routeWaypointLegCollection xlink:href=\"#WPT.{target}\"/></S421:RouteWaypointLeg></member></S421:Dataset>"))
    }
    #[test]
    fn declared_leg_preserves_source_and_exact_ordered_endpoints() {
        let xml = with_leg("2", 9);
        let dataset = import_dataset(&xml).unwrap();
        assert_eq!(&*dataset.original_xml, xml);
        let route = &dataset.routes[0];
        assert_eq!(route.legs.len(), 1);
        assert_eq!(route.legs[0].from_waypoint_id, 2);
        assert_eq!(route.legs[0].to_waypoint_id, 9);
        assert_eq!(route.legs[0].geometry, LegGeometry::Orthodrome);
        assert_eq!(
            route.route.waypoints[1].incoming_geometry,
            Some(LegGeometry::Orthodrome)
        );
        assert_eq!(route.legs[0].original_geometry_text, "2");
        assert!(route
            .require_straight_host_illustration_supported()
            .is_err());
        assert!(import_dataset(&exported()).unwrap().routes[0]
            .require_straight_host_illustration_supported()
            .is_ok());
    }
    #[test]
    fn wrong_reverse_reference_duplicate_geometry_and_external_leg_reject() {
        assert!(import_dataset(&with_leg("1", 2))
            .unwrap_err()
            .contains("another waypoint"));
        let duplicate = with_leg("1", 9).replace(
            "</routeWaypointLegGeometryType>",
            "</routeWaypointLegGeometryType><routeWaypointLegGeometryType>2</routeWaypointLegGeometryType>");
        assert!(import_dataset(&duplicate)
            .unwrap_err()
            .contains("Duplicate"));
        let external = with_leg("1", 9).replace("#LEG.1", "https://example.invalid/leg");
        assert!(import_dataset(&external).unwrap_err().contains("External"));
    }
    #[test]
    fn missing_or_first_waypoint_leg_is_not_assigned_an_implicit_curve() {
        assert!(import_dataset(&with_leg("", 9)).is_err());
        let first = with_leg("1", 2)
            .replace(
                "<routeWaypointID>9</routeWaypointID><routeWaypointLeg xlink:href=\"#LEG.1\"/>",
                "<routeWaypointID>9</routeWaypointID>",
            )
            .replace(
                "<routeWaypointID>2</routeWaypointID>",
                "<routeWaypointID>2</routeWaypointID><routeWaypointLeg xlink:href=\"#LEG.1\"/>",
            );
        assert!(import_dataset(&first)
            .unwrap_err()
            .contains("First-waypoint"));
    }
    const MIN1: &str = include_str!("../tests/fixtures/v1-GMIN.gml");
    const BASIC1: &str = include_str!("../tests/fixtures/v1-GBASIC.gml");
    const FULL1: &str = include_str!("../tests/fixtures/v1-GFULL.gml");
    const MIN2: &str = include_str!("../tests/fixtures/v2-GMIN.gml");
    const BASIC2: &str = include_str!("../tests/fixtures/v2-GBASIC.gml");
    const FULL2: &str = include_str!("../tests/fixtures/v2-GFULL.gml");
    #[test]
    fn official_published_minimum_preserves_original_and_latitude_first() {
        let d = import_dataset(MIN1).unwrap();
        assert_eq!(d.profile, Profile::Published1);
        assert_eq!(&*d.original_xml, MIN1);
        assert_eq!(d.routes[0].route.waypoints.len(), 2);
        let w = &d.routes[0].route.waypoints[0];
        assert_eq!(w.lat.to_bits(), 59.892863_f64.to_bits());
        assert_eq!(w.lon.to_bits(), 25.822235_f64.to_bits());
    }
    #[test]
    fn official_published_basic_and_full_have_dangling_waypoint_references() {
        for xml in [BASIC1, FULL1] {
            assert!(import_dataset(xml)
                .unwrap_err()
                .contains("Unresolved reference"));
        }
    }
    #[test]
    fn official_cdv_minimum_is_metadata_only_and_basic_has_ordered_waypoints() {
        let minimum = import_dataset(MIN2).unwrap();
        assert_eq!(minimum.profile, Profile::Candidate2);
        assert!(minimum.routes[0].route.waypoints.is_empty());
        let basic = import_dataset(BASIC2).unwrap();
        assert_eq!(basic.routes[0].route.waypoints.len(), 10);
        assert_eq!(
            basic.routes[0]
                .route
                .waypoints
                .iter()
                .map(|w| w.id)
                .collect::<Vec<_>>(),
            (1..=10).collect::<Vec<_>>()
        );
        assert_eq!(&*basic.original_xml, BASIC2);
    }
    #[test]
    fn official_cdv_full_has_no_inheritable_point_crs() {
        assert!(import_dataset(FULL2)
            .unwrap_err()
            .contains("Missing point CRS"));
    }
    fn exported() -> String {
        let mut route = Route::with_name(7, "Route Alpha");
        route.add_waypoint(
            Waypoint::new(2, 25.123456789012345, 59.23456789012345).with_turn_radius(0.25),
        );
        route.add_waypoint(
            Waypoint::new(9, -179.99999999999997, -89.99999999999999).with_turn_radius(5.0),
        );
        export_published(
            &route,
            PublishedExport {
                route_id: "RTE.Alpha",
                edition: 3,
                status: 11,
            },
        )
        .unwrap()
    }
    #[test]
    fn declared_geodesic_export_roundtrips_model_endpoints_and_metrics() {
        let mut route = import_dataset(&exported()).unwrap().routes.remove(0).route;
        route.waypoints[1].incoming_geometry = Some(LegGeometry::Orthodrome);
        let xml = export_published(
            &route,
            PublishedExport {
                route_id: "RTE.Alpha",
                edition: 3,
                status: 11,
            },
        )
        .unwrap();
        let reopened = import_dataset(&xml).unwrap().routes.remove(0);
        assert_eq!(reopened.legs.len(), 1);
        assert_eq!(reopened.legs[0].geometry, LegGeometry::Orthodrome);
        assert_eq!(reopened.legs[0].from_waypoint_id, 2);
        assert_eq!(reopened.legs[0].to_waypoint_id, 9);
        for (before, after) in route.waypoints.iter().zip(&reopened.route.waypoints) {
            assert_eq!(before.lon.to_bits(), after.lon.to_bits());
            assert_eq!(before.lat.to_bits(), after.lat.to_bits());
            assert_eq!(before.incoming_geometry, after.incoming_geometry);
        }
        assert_eq!(route.leg_metrics(), reopened.route.leg_metrics());
        assert!(xml.contains("<gml:GeodesicString interpolation=\"geodesic\">"));
    }
    #[test]
    fn unsupported_declared_export_never_erases_or_guesses_geometry() {
        let mut route = import_dataset(&exported()).unwrap().routes.remove(0).route;
        route.waypoints[1].incoming_geometry = Some(LegGeometry::Loxodrome);
        let export = |r: &Route| {
            export_published(
                r,
                PublishedExport {
                    route_id: "RTE.Alpha",
                    edition: 3,
                    status: 11,
                },
            )
        };
        assert!(export(&route).unwrap_err().contains("refusing to erase"));
        route.waypoints[1].incoming_geometry = None;
        route.waypoints[0].incoming_geometry = Some(LegGeometry::Orthodrome);
        assert!(export(&route).unwrap_err().contains("First waypoint"));
        route.waypoints[0].incoming_geometry = None;
        route.waypoints[1].incoming_geometry = Some(LegGeometry::Orthodrome);
        route.waypoints[0].lon = -30.;
        route.waypoints[1].lon = 150.;
        assert!(export(&route).unwrap_err().contains("Ambiguous"));
    }
    #[test]
    fn published_export_roundtrip_identity_references_order_and_unrounded_coordinates() {
        let xml = exported();
        let d = import_dataset(&xml).unwrap();
        assert_eq!(d.profile, Profile::Published1);
        assert_eq!(d.routes[0].route_id, "RTE.Alpha");
        assert_eq!(d.routes[0].edition, 3);
        let w = &d.routes[0].route.waypoints;
        assert_eq!(w.iter().map(|w| w.id).collect::<Vec<_>>(), vec![2, 9]);
        assert_eq!(w[0].lon.to_bits(), 25.123456789012345_f64.to_bits());
        assert_eq!(w[0].lat.to_bits(), 59.23456789012345_f64.to_bits());
        assert_eq!(w[1].lon.to_bits(), (-179.99999999999997_f64).to_bits());
        assert_eq!(w[1].lat.to_bits(), (-89.99999999999999_f64).to_bits());
    }
    #[test]
    fn dtd_deep_broad_and_oversized_documents_reject_before_dom() {
        assert!(preflight("<!DOCTYPE Dataset [<!ENTITY x 'x'>]><Dataset/>").is_err());
        let deep = format!(
            "{}{}",
            "<x>".repeat(MAX_DEPTH + 1),
            "</x>".repeat(MAX_DEPTH + 1)
        );
        assert!(preflight(&deep).unwrap_err().contains("depth"));
        let broad = format!("<x>{}</x>", "<x/>".repeat(MAX_NODES as usize));
        assert!(preflight(&broad).unwrap_err().contains("node"));
        assert!(preflight(&" ".repeat(MAX_XML_BYTES + 1))
            .unwrap_err()
            .contains("byte"));
    }
    #[test]
    fn malformed_coordinates_namespace_duplicate_ids_and_external_refs_reject() {
        let xml = exported();
        for changed in [
            xml.replace(NS1, "http://untrusted.invalid/S421"),
            xml.replace("gml:id=\"POINT.9\"", "gml:id=\"POINT.2\""),
            xml.replace("#WPT.2", "https://untrusted.invalid/WPT.2"),
            xml.replace("EPSG:4326", "EPSG:3857"),
        ] {
            assert!(import_dataset(&changed).is_err(), "{changed}");
        }
        let d = Document::parse(&xml).unwrap();
        let pos = d
            .descendants()
            .find(|n| n.has_tag_name((GML, "pos")))
            .unwrap()
            .text()
            .unwrap();
        for replacement in ["NaN 25", "91 25", "59 181", "59 25 0"] {
            assert!(import_dataset(&xml.replace(pos, replacement)).is_err());
        }
    }
    #[test]
    fn reference_order_and_backlink_identity_are_not_guessed() {
        let xml = exported();
        let swapped = xml
            .replace("#WPT.2", "#TEMP")
            .replace("#WPT.9", "#WPT.2")
            .replace("#TEMP", "#WPT.9");
        assert_eq!(
            import_route(&swapped)
                .unwrap()
                .waypoints
                .iter()
                .map(|w| w.id)
                .collect::<Vec<_>>(),
            vec![9, 2]
        );
        assert!(import_dataset(&xml.replace("#RTE.WPTS", "#WPT.2")).is_err());
        assert!(import_dataset(&xml.replace("#WPT.9", "#WPT.2")).is_err());
    }
    #[test]
    fn position_own_metadata_split_text_and_invalid_ncname_reject() {
        let xml = exported();
        for changed in [
            xml.replace("<gml:pos>", "<gml:pos srsDimension=\"3\">"),
            xml.replace("<gml:pos>", "<gml:pos axisLabels=\"Long Lat\">"),
            xml.replace("gml:id=\"POINT.2\"", "gml:id=\"1invalid\""),
        ] {
            assert!(import_dataset(&changed).is_err());
        }
        let doc = Document::parse(&xml).unwrap();
        let value = doc
            .descendants()
            .find(|n| n.has_tag_name((GML, "pos")))
            .unwrap()
            .text()
            .unwrap();
        assert!(
            import_dataset(&xml.replace(value, &format!("{value}<!-- split --> 999"))).is_err()
        );
        let bare = xml.replace("#RTE.INFO", "RTE.INFO");
        assert_eq!(
            import_dataset(&bare).unwrap().compatibility_warnings.len(),
            1
        );
    }
    #[test]
    fn decimal_radius_has_no_epsilon_or_silent_rounding() {
        for valid in ["0", "+.25", "5.0000", "-0.00"] {
            assert!(published_radius(valid).is_ok());
        }
        for invalid in [
            "5.0000000000000001",
            "0.001",
            "5.01",
            "-0.01",
            "1e0",
            "NaN",
            ".",
            "",
        ] {
            assert!(published_radius(invalid).is_err(), "{invalid}");
        }
        assert!(import_dataset(&exported().replace(
            "<routeWaypointTurnRadius>0.25",
            "<routeWaypointTurnRadius>0.251"
        ))
        .is_err());
    }
}
