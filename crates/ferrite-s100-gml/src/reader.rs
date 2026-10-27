//! S-100 Part 10b GML reader (S-100 GML profile 5.0, GML 3.2).
use crate::model::*;
use anyhow::{bail, ensure, Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_kernel::geodesy::{GeodesicRadiusArc, GeographicPosition};
use roxmltree::{Node, NodeType};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

pub const GML: &str = "http://www.opengis.net/gml/3.2";
pub const XLINK: &str = "http://www.w3.org/1999/xlink";
pub const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
pub const S100_GML: &str = "http://www.iho.int/s100gml/5.0";

/// Bounds on untrusted input.
#[derive(Debug, Clone, Copy)]
pub struct ReadLimits {
    pub bytes: usize,
    pub nodes: u32,
    /// Total positions after arc densification.
    pub positions: usize,
}
impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            bytes: 256 * 1024 * 1024,
            nodes: 50_000_000,
            positions: 20_000_000,
        }
    }
}

/// Maximum angular step for sampled arcs and circles.
const ARC_STEP_DEG: f64 = 2.0;
/// Nesting bound for complex attributes and xlinked curve/ring composition
/// (guards against reference cycles and hostile nesting).
const MAX_DEPTH: u32 = 64;

/// Reads a dataset of one of the registered GML `products` (identified by
/// its productIdentifier, else its namespace).
pub fn read_file(
    path: &Path,
    products: &[&GmlIdentity],
    fc: Option<&FeatureCatalogue>,
    limits: ReadLimits,
) -> Result<Dataset> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("Cannot open {}", path.display()))?;
    ensure!(
        meta.len() as usize <= limits.bytes,
        "{} exceeds the {} byte dataset limit",
        path.display(),
        limits.bytes
    );
    let bytes = std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let mut dataset =
        read_bytes(&bytes, products, fc, limits).with_context(|| path.display().to_string())?;
    dataset.source = Some(path.to_path_buf());
    Ok(dataset)
}

/// [`read_file`] of a dataset's bytes.
pub fn read_bytes(
    bytes: &[u8],
    products: &[&GmlIdentity],
    fc: Option<&FeatureCatalogue>,
    limits: ReadLimits,
) -> Result<Dataset> {
    ensure!(
        bytes.len() <= limits.bytes,
        "Dataset exceeds the byte limit"
    );
    let text = std::str::from_utf8(bytes).context("Dataset is not UTF-8")?;
    let text = text.trim_start_matches('\u{feff}');
    let doc = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: limits.nodes,
        },
    )
    .context("Not well-formed XML")?;
    let root = doc.root_element();
    ensure!(
        root.tag_name().name() == "Dataset",
        "Root element is {}, not Dataset",
        root.tag_name().name()
    );
    let namespace = root.tag_name().namespace().unwrap_or("").to_string();
    let mut reader = Reader {
        ids: HashMap::new(),
        issues: Vec::new(),
        positions: 0,
        depth: 0,
        limits,
    };
    for node in doc.descendants().filter(|n| n.is_element()) {
        if let Some(id) = node.attribute((GML, "id")) {
            if reader.ids.insert(id, node).is_some() {
                reader.issue(Severity::Error, "GML", Some(id), "Duplicate gml:id");
            }
        }
    }

    let mut identification = Identification::default();
    let mut envelope = None;
    let mut member_nodes = Vec::new();
    let mut seen_members = false;
    let mut late_identification = false;
    for child in root.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "DatasetIdentificationInformation" => {
                late_identification |= seen_members;
                for f in child.children().filter(|n| n.is_element()) {
                    identification.fields.push((
                        f.tag_name().name().to_string(),
                        f.text().unwrap_or("").trim().to_string(),
                    ));
                }
            }
            "boundedBy" => envelope = reader.envelope(child),
            "members" => {
                seen_members = true;
                member_nodes.extend(child.children().filter(|n| n.is_element()));
            }
            "member" | "imember" => {
                seen_members = true;
                member_nodes.extend(child.children().filter(|n| n.is_element()));
            }
            // Dataset-level geometry containers are referenced by xlink.
            _ => {}
        }
    }
    let product_id = identification.product_identifier().unwrap_or_default();
    let product = products
        .iter()
        .find(|p| p.names(product_id))
        .or_else(|| {
            products
                .iter()
                .find(|p| namespace.contains(&format!("S{}", p.code)))
        })
        .with_context(|| format!("Unsupported product '{product_id}' (namespace {namespace})"))?;
    if late_identification {
        let check = product.late_identification_check;
        reader.issue(
            Severity::Warning,
            check,
            None,
            "DatasetIdentificationInformation follows the members",
        );
    }

    let mut objects = Vec::with_capacity(member_nodes.len());
    for node in member_nodes {
        objects.push(reader.object(node, fc)?);
    }
    let mut index = HashMap::new();
    for (i, o) in objects.iter().enumerate() {
        if o.id.is_empty() {
            reader.issue(
                Severity::Error,
                "GML",
                Some(&o.code),
                "Object without gml:id",
            );
        } else {
            index.insert(o.id.clone(), i);
        }
    }
    for o in &mut objects {
        for r in &mut o.references {
            let local = r.href.strip_prefix('#').unwrap_or(&r.href);
            r.target = (r.href.starts_with('#') || !r.href.contains('#'))
                .then(|| index.get(local).copied())
                .flatten();
            if r.target.is_none() {
                reader.issues.push(Issue {
                    severity: Severity::Error,
                    check: "GML".into(),
                    object: Some(o.id.clone()),
                    message: format!("Unresolved reference {} = '{}'", r.role, r.href),
                });
            }
        }
    }
    Ok(Dataset {
        source: None,
        sha256: Sha256::digest(bytes).into(),
        product: product.id,
        namespace,
        gml_id: root.attribute((GML, "id")).unwrap_or_default().to_string(),
        identification,
        envelope,
        objects,
        index,
        issues: reader.issues,
        imported: false,
    })
}

struct Reader<'a, 'input> {
    ids: HashMap<&'a str, Node<'a, 'input>>,
    issues: Vec<Issue>,
    positions: usize,
    /// Current curve/ring composition depth.
    depth: u32,
    limits: ReadLimits,
}

fn elements<'a, 'i>(node: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    node.children().filter(|n| n.is_element())
}
fn child<'a, 'i>(node: Node<'a, 'i>, local: &str) -> Option<Node<'a, 'i>> {
    elements(node).find(|n| n.tag_name().name() == local)
}
fn is_nil(node: Node) -> Option<Option<String>> {
    let nil = node
        .attribute((XSI, "nil"))
        .is_some_and(|v| matches!(v.trim(), "true" | "1"));
    let reason = node.attribute("nilReason").map(str::to_owned);
    (nil || reason.is_some()).then_some(reason)
}

impl<'a, 'i> Reader<'a, 'i> {
    fn issue(&mut self, severity: Severity, check: &str, object: Option<&str>, message: &str) {
        self.issues.push(Issue {
            severity,
            check: check.into(),
            object: object.map(str::to_owned),
            message: message.into(),
        });
    }

    fn envelope(&mut self, bounded_by: Node) -> Option<[f64; 4]> {
        let env = child(bounded_by, "Envelope")?;
        let lower = self
            .pos_list(child(env, "lowerCorner")?.text()?, env)
            .ok()?;
        let upper = self
            .pos_list(child(env, "upperCorner")?.text()?, env)
            .ok()?;
        let (l, u) = (lower.first()?, upper.first()?);
        Some([
            l[0].min(u[0]),
            l[1].min(u[1]),
            l[0].max(u[0]),
            l[1].max(u[1]),
        ])
    }

    fn object(&mut self, node: Node<'a, 'i>, fc: Option<&FeatureCatalogue>) -> Result<Object> {
        let code = node.tag_name().name().to_string();
        let id = node.attribute((GML, "id")).unwrap_or_default().to_string();
        let mut attributes = Vec::new();
        let mut geometry = Vec::new();
        let mut references = Vec::new();
        for c in elements(node) {
            let name = c.tag_name().name();
            if c.tag_name().namespace() == Some(GML) {
                // gml:boundedBy, gml:description, gml:name
                continue;
            }
            if name == "geometry" {
                for g in elements(c) {
                    match self.geometry_property(g) {
                        Ok(Some(geom)) => geometry.push(geom),
                        Ok(None) => {}
                        Err(e) => self.issues.push(Issue {
                            severity: Severity::Error,
                            check: "GML".into(),
                            object: Some(id.clone()),
                            message: format!("Geometry: {e:#}"),
                        }),
                    }
                }
                continue;
            }
            if let Some(href) = c.attribute((XLINK, "href")) {
                references.push(Reference {
                    role: name.to_string(),
                    href: href.trim().to_string(),
                    target: None,
                });
                continue;
            }
            attributes.push(self.attribute(c, 0));
        }
        let kind = match fc {
            Some(fc) if fc.feature_types.contains_key(&code) => ObjectKind::Feature,
            Some(fc) if fc.information_types.contains_key(&code) => ObjectKind::Information,
            _ if !geometry.is_empty() => ObjectKind::Feature,
            _ => ObjectKind::Information,
        };
        Ok(Object {
            id,
            code,
            kind,
            attributes,
            geometry,
            references,
        })
    }

    fn attribute(&mut self, node: Node, depth: u32) -> Attribute {
        let code = node.tag_name().name().to_string();
        if depth > MAX_DEPTH {
            self.issue(
                Severity::Error,
                "GML",
                None,
                &format!("{code}: attribute nesting too deep"),
            );
            return Attribute {
                code,
                value: Value::Nil { reason: None },
            };
        }
        if let Some(reason) = is_nil(node) {
            return Attribute {
                code,
                value: Value::Nil { reason },
            };
        }
        let kids: Vec<_> = elements(node).collect();
        let value = if kids.is_empty() {
            let text: String = node
                .children()
                .filter(|n| n.node_type() == NodeType::Text)
                .filter_map(|n| n.text())
                .collect();
            Value::Simple {
                text: text.trim().to_string(),
                code: node.attribute("code").map(|c| c.trim().to_string()),
            }
        } else {
            Value::Complex(
                kids.into_iter()
                    .map(|k| self.attribute(k, depth + 1))
                    .collect(),
            )
        };
        Attribute { code, value }
    }

    fn resolve(&self, node: Node<'a, 'i>) -> Result<Node<'a, 'i>> {
        match node.attribute((XLINK, "href")) {
            Some(href) => {
                let id = href.trim().strip_prefix('#').unwrap_or(href.trim());
                self.ids
                    .get(id)
                    .copied()
                    .with_context(|| format!("Unresolved geometry reference '{href}'"))
            }
            None => elements(node)
                .next()
                .with_context(|| format!("Empty {}", node.tag_name().name())),
        }
    }

    /// One of pointProperty, multiPointProperty, curveProperty, surfaceProperty.
    fn geometry_property(&mut self, prop: Node<'a, 'i>) -> Result<Option<Geometry>> {
        if is_nil(prop).is_some() {
            return Ok(None);
        }
        let target = self.resolve(prop)?;
        Ok(Some(match prop.tag_name().name() {
            "pointProperty" => Geometry::Point(self.point(target)?),
            "multiPointProperty" => Geometry::MultiPoint(self.multi_point(target)?),
            "curveProperty" => Geometry::Curve(self.curve(target)?),
            "surfaceProperty" => {
                let (exterior, interiors) = self.surface(target)?;
                Geometry::Surface {
                    exterior,
                    interiors,
                }
            }
            other => bail!("Unsupported geometry property {other}"),
        }))
    }

    fn check_crs(&self, node: Node) -> Result<()> {
        // srsName may be inherited; only EPSG:4326 is permitted for these products.
        let mut n = Some(node);
        while let Some(e) = n {
            if let Some(srs) = e.attribute("srsName") {
                let srs = srs.trim();
                ensure!(
                    srs.ends_with("4326") || srs.ends_with("EPSG::4326"),
                    "Unsupported CRS {srs}"
                );
                return Ok(());
            }
            n = e.parent_element();
        }
        Ok(())
    }

    fn charge(&mut self, n: usize) -> Result<()> {
        self.positions += n;
        ensure!(
            self.positions <= self.limits.positions,
            "Too many positions"
        );
        Ok(())
    }

    /// Parses "lat lon lat lon ..." (EPSG:4326 axis order) to [lon, lat].
    fn pos_list(&mut self, text: &str, context: Node) -> Result<Vec<Coord>> {
        self.check_crs(context)?;
        let dim = context
            .ancestors()
            .filter(|n| n.is_element())
            .find_map(|n| n.attribute("srsDimension"))
            .map(|d| d.trim().parse::<usize>())
            .transpose()
            .context("Bad srsDimension")?
            .unwrap_or(2);
        ensure!(dim == 2 || dim == 3, "Unsupported srsDimension {dim}");
        let values = text
            .split_ascii_whitespace()
            .map(|v| {
                v.parse::<f64>()
                    .with_context(|| format!("Bad coordinate '{v}'"))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !values.is_empty() && values.len() % dim == 0,
            "Coordinate count {} is not a multiple of {dim}",
            values.len()
        );
        self.charge(values.len() / dim)?;
        values
            .chunks(dim)
            .map(|c| {
                let (lat, lon) = (c[0], c[1]);
                ensure!(
                    lat.is_finite() && lon.is_finite() && lat.abs() <= 90. && lon.abs() <= 180.,
                    "Coordinate out of range: {lat} {lon}"
                );
                Ok([lon, lat])
            })
            .collect()
    }

    fn positions(&mut self, node: Node<'a, 'i>) -> Result<Vec<Coord>> {
        if let Some(list) = child(node, "posList") {
            return self.pos_list(list.text().unwrap_or(""), list);
        }
        let mut out = Vec::new();
        for c in elements(node) {
            match c.tag_name().name() {
                "pos" => out.extend(self.pos_list(c.text().unwrap_or(""), c)?),
                "pointProperty" | "pointRep" => out.push(self.point(self.resolve(c)?)?),
                _ => {}
            }
        }
        Ok(out)
    }

    fn point(&mut self, node: Node<'a, 'i>) -> Result<Coord> {
        ensure!(
            node.tag_name().name() == "Point",
            "Expected Point, found {}",
            node.tag_name().name()
        );
        let pos = child(node, "pos").context("Point without gml:pos")?;
        let p = self.pos_list(pos.text().unwrap_or(""), pos)?;
        ensure!(p.len() == 1, "Point with {} positions", p.len());
        Ok(p[0])
    }

    fn multi_point(&mut self, node: Node<'a, 'i>) -> Result<Vec<Coord>> {
        ensure!(
            node.tag_name().name() == "MultiPoint",
            "Expected MultiPoint"
        );
        let mut out = Vec::new();
        for c in elements(node) {
            match c.tag_name().name() {
                "pointMember" => out.push(self.point(self.resolve(c)?)?),
                "pointMembers" => {
                    for p in elements(c) {
                        out.push(self.point(p)?);
                    }
                }
                _ => {}
            }
        }
        ensure!(!out.is_empty(), "Empty MultiPoint");
        Ok(out)
    }

    fn curve(&mut self, node: Node<'a, 'i>) -> Result<Vec<Coord>> {
        ensure!(
            self.depth < MAX_DEPTH,
            "Curve nesting too deep (reference cycle?)"
        );
        self.depth += 1;
        let result = self.curve_inner(node);
        self.depth -= 1;
        result
    }

    fn curve_inner(&mut self, node: Node<'a, 'i>) -> Result<Vec<Coord>> {
        let name = node.tag_name().name();
        let points = match name {
            "Curve" => {
                let segments = child(node, "segments").context("Curve without segments")?;
                let mut out: Vec<Coord> = Vec::new();
                for s in elements(segments) {
                    let part = self.segment(s)?;
                    append(&mut out, part);
                }
                out
            }
            "LineString" => self.positions(node)?,
            "OrientableCurve" => {
                let base = child(node, "baseCurve").context("OrientableCurve without baseCurve")?;
                let mut c = self.curve(self.resolve(base)?)?;
                if node.attribute("orientation").map(str::trim) == Some("-") {
                    c.reverse();
                }
                c
            }
            "CompositeCurve" => {
                let mut out = Vec::new();
                for m in elements(node).filter(|c| c.tag_name().name() == "curveMember") {
                    let part = self.curve(self.resolve(m)?)?;
                    append(&mut out, part);
                }
                out
            }
            other => bail!("Unsupported curve {other}"),
        };
        ensure!(points.len() >= 2, "{name} with fewer than two positions");
        Ok(points)
    }

    fn segment(&mut self, s: Node<'a, 'i>) -> Result<Vec<Coord>> {
        let name = s.tag_name().name();
        match name {
            "LineStringSegment" | "GeodesicString" | "Geodesic" => self.positions(s),
            "CircleByCenterPoint"
            | "ArcByCenterPoint"
            | "S100_CircleByCenterPoint"
            | "S100_ArcByCenterPoint" => {
                let centre = self.positions(s)?;
                ensure!(centre.len() == 1, "{name} needs one centre position");
                let radius = child(s, "radius").context("Arc without radius")?;
                let mut metres: f64 = radius
                    .text()
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .context("Bad radius")?;
                metres *= match radius.attribute("uom").map(str::trim).unwrap_or("m") {
                    "m" | "metre" | "meter" | "metres" => 1.,
                    "km" => 1000.,
                    "NM" | "nmi" | "M" | "[nmi_i]" | "nautical mile" => 1852.,
                    other => bail!("Unsupported radius unit '{other}'"),
                };
                ensure!(metres.is_finite() && metres > 0., "Invalid radius");
                let angle = |n: &str| -> Result<Option<f64>> {
                    child(s, n)
                        .map(|a| {
                            a.text()
                                .unwrap_or("")
                                .trim()
                                .parse::<f64>()
                                .context("Bad angle")
                        })
                        .transpose()
                };
                // S-100 encodes bearings clockwise from true north with a
                // signed angularDistance; GML 3.2 uses mathematical angles
                // (counter-clockwise from east) with startAngle/endAngle.
                let full = name.contains("Circle");
                let (start, sweep) = match angle("angularDistance")? {
                    Some(d) => (
                        angle("startAngle")?.unwrap_or(0.),
                        if full { 360f64.copysign(d) } else { d },
                    ),
                    None if full => (angle("startAngle")?.unwrap_or(0.), 360.),
                    None => {
                        let a = angle("startAngle")?.context("Arc without startAngle")?;
                        let b = angle("endAngle")?.context("Arc without angular distance")?;
                        (90. - a, -(b - a))
                    }
                };
                let centre = GeographicPosition::new(centre[0][1], centre[0][0])?;
                let arc = GeodesicRadiusArc::new(centre, metres, start, sweep)?;
                let samples = arc.sample(ARC_STEP_DEG, 100_000)?;
                self.charge(samples.len())?;
                Ok(samples
                    .into_iter()
                    .map(|p| {
                        [
                            p.longitude_near(centre.longitude())
                                .unwrap_or(p.longitude()),
                            p.latitude(),
                        ]
                    })
                    .collect())
            }
            "Arc" | "ArcString" | "Circle" => {
                let p = self.positions(s)?;
                ensure!(p.len() == 3, "{name} needs three positions");
                let gp = |c: Coord| GeographicPosition::new(c[1], c[0]);
                let arc = ferrite_kernel::geodesy::arc_through_three_points(
                    gp(p[0])?,
                    gp(p[1])?,
                    gp(p[2])?,
                )?;
                let sweep = if name == "Circle" {
                    360f64.copysign(arc.sweep_deg())
                } else {
                    arc.sweep_deg()
                };
                let r = GeodesicRadiusArc::new(
                    arc.center(),
                    arc.radius_m(),
                    arc.start_azimuth_deg(),
                    sweep,
                )?;
                let samples = r.sample(ARC_STEP_DEG, 100_000)?;
                self.charge(samples.len())?;
                Ok(samples
                    .into_iter()
                    .map(|q| {
                        [
                            q.longitude_near(p[0][0]).unwrap_or(q.longitude()),
                            q.latitude(),
                        ]
                    })
                    .collect())
            }
            other => bail!("Unsupported curve segment {other}"),
        }
    }

    fn ring(&mut self, node: Node<'a, 'i>) -> Result<Vec<Coord>> {
        let mut ring = match node.tag_name().name() {
            "LinearRing" => self.positions(node)?,
            "Ring" => {
                let mut out = Vec::new();
                for m in elements(node).filter(|c| c.tag_name().name() == "curveMember") {
                    let part = self.curve(self.resolve(m)?)?;
                    append(&mut out, part);
                }
                out
            }
            other => bail!("Unsupported ring {other}"),
        };
        ensure!(ring.len() >= 3, "Ring with fewer than three positions");
        if ring.first() != ring.last() {
            let first = ring[0];
            let last = *ring.last().unwrap();
            ensure!(
                (first[0] - last[0]).abs() < 1e-7 && (first[1] - last[1]).abs() < 1e-7,
                "Ring is not closed"
            );
            *ring.last_mut().unwrap() = first;
        }
        ensure!(ring.len() >= 4, "Degenerate ring");
        Ok(ring)
    }

    fn surface(&mut self, node: Node<'a, 'i>) -> Result<(Vec<Coord>, Vec<Vec<Coord>>)> {
        let polygon = match node.tag_name().name() {
            "Surface" => {
                let patches = child(node, "patches").context("Surface without patches")?;
                let mut polys = elements(patches);
                let first = polys.next().context("Surface without patches")?;
                ensure!(
                    polys.next().is_none(),
                    "Multi-patch surfaces are not supported"
                );
                ensure!(
                    matches!(first.tag_name().name(), "PolygonPatch" | "Polygon"),
                    "Unsupported patch {}",
                    first.tag_name().name()
                );
                first
            }
            "Polygon" => node,
            other => bail!("Unsupported surface {other}"),
        };
        let exterior = child(polygon, "exterior").context("Surface without exterior")?;
        let exterior = self.ring(elements(exterior).next().context("Empty exterior")?)?;
        let mut interiors = Vec::new();
        for i in elements(polygon).filter(|c| c.tag_name().name() == "interior") {
            interiors.push(self.ring(elements(i).next().context("Empty interior")?)?);
        }
        Ok((exterior, interiors))
    }
}

fn append(out: &mut Vec<Coord>, part: Vec<Coord>) {
    let joined = match (out.last(), part.first()) {
        (Some(l), Some(f)) => (l[0] - f[0]).abs() <= 1e-9 && (l[1] - f[1]).abs() <= 1e-9,
        _ => false,
    };
    out.extend(part.into_iter().skip(joined as usize));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Workspace-relative path, or None when the (git-ignored) data is absent.
    pub(crate) fn workspace_file(relative: &str) -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative);
        if path.exists() {
            Some(path)
        } else {
            eprintln!("skipping: {relative} not present");
            None
        }
    }
    pub(crate) const S124_DIR: &str =
        "TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT/S-124/DATASET_FILES";
    pub(crate) const S124_FC: &str = "Catalogues/FC/S-124/124_FC_2.0.0.xml";
    pub(crate) const S129_FC: &str = "Catalogues/FC/S-129/129_Feature_Catalogue_2.0.0.XML";
    pub(crate) const S129_130: &str = "TestData/IHO-S129-Test-Datasets/12900MCTDS130TS.gml";
    pub(crate) const S129_200: &str = "TestData/IHO-S129-Test-Datasets/12900MCTDS200TS.gml";

    pub(crate) fn fc(relative: &str) -> Option<FeatureCatalogue> {
        Some(FeatureCatalogue::load(workspace_file(relative)?).unwrap())
    }
    /// Identities of the S-124 and S-129 datasets the tests read (the
    /// product crates register the same ones).
    pub(crate) static S124: GmlIdentity = GmlIdentity {
        id: ProductId("S-124"),
        identifiers: &["S-124", "S124"],
        code: "124",
        late_identification_check: "GML",
    };
    pub(crate) static S129: GmlIdentity = GmlIdentity {
        id: ProductId("S-129"),
        identifiers: &["S-129", "S129"],
        code: "129",
        late_identification_check: "S-158-129/15",
    };
    pub(crate) static KNOWN: [&GmlIdentity; 2] = [&S124, &S129];

    pub(crate) fn read(relative: &str, fc: &FeatureCatalogue) -> Option<Dataset> {
        Some(
            read_file(
                &workspace_file(relative)?,
                &KNOWN,
                Some(fc),
                ReadLimits::default(),
            )
            .unwrap(),
        )
    }
    fn s124(n: u32, fc: &FeatureCatalogue) -> Option<Dataset> {
        let name = match n {
            36 => "124GB00S1Y2026N36.GML",
            43..=46 => return read(&format!("{S124_DIR}/124GB00S2Y2026N{n}.GML"), fc),
            _ => unreachable!(),
        };
        read(&format!("{S124_DIR}/{name}"), fc)
    }
    fn close(a: Coord, b: Coord) -> bool {
        (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9
    }
    /// Distance in metres on a sphere (good to ~0.5% for the radius checks).
    fn haversine(a: Coord, b: Coord) -> f64 {
        let (p1, p2) = (a[1].to_radians(), b[1].to_radians());
        let dp = p2 - p1;
        let dl = (b[0] - a[0]).to_radians();
        let h = (dp / 2.).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.).sin().powi(2);
        2. * 6_371_008.8 * h.sqrt().asin()
    }

    #[test]
    fn s124_point_dataset_with_header_reference() {
        let Some(fc) = fc(S124_FC) else { return };
        let Some(ds) = s124(43, &fc) else { return };
        assert_eq!(ds.product, S124.id);
        assert_eq!(ds.gml_id, "DS_124GB00S2Y2026N43");
        assert_eq!(
            ds.identification.file_identifier(),
            Some("124GB00S2Y2026N43.GML")
        );
        assert_eq!(ds.identification.product_edition(), Some("2.0.0"));
        assert!(ds.issues.is_empty(), "{:?}", ds.issues);
        assert_eq!(ds.objects.len(), 2);
        let pre = ds.object("PRE").unwrap();
        assert_eq!(
            (pre.code.as_str(), pre.kind),
            ("NavwarnPreamble", ObjectKind::Information)
        );
        assert_eq!(
            pre.value(&["messageSeriesIdentifier", "warningNumber"]),
            Some("43")
        );
        assert_eq!(
            pre.value(&["messageSeriesIdentifier", "warningType"]),
            Some("2")
        );
        assert_eq!(
            pre.value(&["generalArea", "locationName", "text"]),
            Some("WIGHT")
        );
        assert!(pre.geometry.is_empty());
        let part = ds.object("P1").unwrap();
        assert_eq!(
            (part.code.as_str(), part.kind),
            ("NavwarnPart", ObjectKind::Feature)
        );
        // gml:pos is "lat lon" in EPSG:4326; the model stores [lon, lat].
        assert_eq!(part.geometry, vec![Geometry::Point([-1.091, 50.7573333])]);
        assert_eq!(part.references.len(), 1);
        assert_eq!(part.references[0].role, "header");
        assert_eq!(part.references[0].target, Some(ds.index["PRE"]));
        assert_eq!(
            part.references_with_role("header").collect::<Vec<_>>(),
            vec![0]
        );
        // The UKHO capital-N "Name" is read verbatim; validation reports it.
        assert_eq!(
            part.value(&["featureName", "Name"]),
            Some("OUTER SPIT BUOY")
        );
        assert_eq!(
            part.value(&["warningInformation", "navwarnTypeDetails"]),
            Some("51")
        );
    }

    #[test]
    fn s124_circle_by_centre_point_is_closed_geodesic_ring() {
        let Some(fc) = fc(S124_FC) else { return };
        for (n, id, radius) in [(36, "P1", 18_520.), (44, "P1_A1", 22_224.)] {
            let Some(ds) = s124(n, &fc) else { return };
            assert!(ds.issues.is_empty(), "{:?}", ds.issues);
            let o = ds.object(id).unwrap();
            assert_eq!(o.kind, ObjectKind::Feature);
            let [Geometry::Surface {
                exterior,
                interiors,
            }] = o.geometry.as_slice()
            else {
                panic!("{:?}", o.geometry)
            };
            assert!(interiors.is_empty());
            assert!(exterior.len() >= 100, "densified: {}", exterior.len());
            assert_eq!(exterior.first(), exterior.last());
            let centre = if n == 36 {
                [-1.85, 50.0833333]
            } else {
                [-0.95, 50.6683333]
            };
            for p in exterior {
                let d = haversine(centre, *p);
                assert!((d / radius - 1.).abs() < 0.005, "{d} vs {radius}");
            }
            // Starts due north of the centre (startAngle 0) and runs clockwise.
            assert!((exterior[0][0] - centre[0]).abs() < 1e-9 && exterior[0][1] > centre[1]);
            assert!(exterior[1][0] > centre[0]);
        }
    }

    #[test]
    fn s124_area_affected_is_cross_referenced() {
        let Some(fc) = fc(S124_FC) else { return };
        let Some(ds) = s124(44, &fc) else { return };
        assert_eq!(ds.objects.len(), 3);
        let part = ds.object("P1").unwrap();
        let roles: Vec<_> = part.references.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(roles, ["header", "affects"]);
        assert!(part.references.iter().all(|r| r.target.is_some()));
        let area = ds.object("P1_A1").unwrap();
        assert_eq!(area.code, "NavwarnAreaAffected");
        assert_eq!(
            area.references_with_role("impacts").collect::<Vec<_>>(),
            vec![ds.index["P1"]]
        );
        assert_eq!(part.geometry, vec![Geometry::Point([-0.95, 50.6683333])]);
        let bounds = ds.geometry_bounds().unwrap();
        assert!(
            bounds[0] < -0.95
                && bounds[2] > -0.95
                && bounds[1] < 50.6683333
                && bounds[3] > 50.6683333
        );
    }

    #[test]
    fn s124_all_files_read_cleanly() {
        let Some(fc) = fc(S124_FC) else { return };
        let mut counts = Vec::new();
        for n in [36, 43, 44, 45, 46] {
            let Some(ds) = s124(n, &fc) else { return };
            assert!(ds.issues.is_empty(), "{n}: {:?}", ds.issues);
            assert_eq!(ds.objects_of("NavwarnPreamble").count(), 1);
            counts.push(ds.objects.len());
            for o in &ds.objects {
                let expected = if fc.feature_types.contains_key(&o.code) {
                    ObjectKind::Feature
                } else {
                    assert!(fc.information_types.contains_key(&o.code), "{}", o.code);
                    ObjectKind::Information
                };
                assert_eq!(o.kind, expected);
                for c in o.geometry.iter().flat_map(|g| g.coords()) {
                    // English Channel: lon is the small negative, lat ~ 49-51.
                    assert!(
                        (-3. ..0.).contains(&c[0]) && (49. ..51.).contains(&c[1]),
                        "{c:?}"
                    );
                }
            }
        }
        assert_eq!(counts, [2, 2, 3, 2, 2]);
    }

    fn ring_area(ring: &[Coord]) -> f64 {
        ring.windows(2)
            .map(|w| w[0][0] * w[1][1] - w[1][0] * w[0][1])
            .sum::<f64>()
            / 2.
    }

    #[test]
    fn s129_surfaces_points_and_plan() {
        let Some(fc) = fc(S129_FC) else { return };
        for (file, reversed) in [(S129_130, false), (S129_200, true)] {
            let Some(ds) = read(file, &fc) else { return };
            assert_eq!(ds.product, S129.id);
            assert!(ds.issues.is_empty(), "{file}: {:?}", ds.issues);
            assert_eq!(ds.objects.len(), 304);
            let count = |c: &str| ds.objects_of(c).count();
            assert_eq!(count("UnderKeelClearancePlan"), 1);
            assert_eq!(count("UnderKeelClearancePlanArea"), 1);
            assert_eq!(count("UnderKeelClearanceNonNavigableArea"), 87);
            assert_eq!(count("UnderKeelClearanceAlmostNonNavigableArea"), 200);
            assert_eq!(count("UnderKeelClearanceControlPoint"), 15);
            // FC-driven kinds: the plan is a feature type without geometry.
            assert!(ds.objects.iter().all(|o| o.kind == ObjectKind::Feature));
            let plan = ds.object("TEST_PLAN_TORRES_STRAIT").unwrap();
            assert!(plan.geometry.is_empty() && plan.references.is_empty());
            assert_eq!(plan.value(&["underKeelClearancePurpose"]), Some("2"));
            assert_eq!(plan.value(&["maximumDraught"]), Some("12.2"));
            let (mut surfaces, mut interiors) = (0, 0);
            for o in &ds.objects {
                for g in &o.geometry {
                    if let Geometry::Surface {
                        exterior,
                        interiors: holes,
                    } = g
                    {
                        surfaces += 1;
                        interiors += holes.len();
                        for ring in std::iter::once(exterior).chain(holes) {
                            assert!(ring.len() >= 4);
                            assert_eq!(ring.first(), ring.last());
                        }
                    }
                }
            }
            assert_eq!((surfaces, interiors), (288, 22));
            let area = &ds.object("TEST_PLAN_AREA_TORRES_STRAIT").unwrap().geometry[0];
            let Geometry::Surface { exterior, .. } = area else {
                panic!()
            };
            assert!(close(exterior[0], [142.368421052281, -10.4623380129907]));
            assert_eq!(exterior.len(), 67);
            // The two editions encode the plan area with opposite orientation.
            assert_eq!(ring_area(exterior) > 0., reversed, "{file}");
            let cp = ds.object("CP_01").unwrap();
            assert_eq!(cp.primitive(), "Point");
            assert_eq!(cp.value(&["featureName", "name"]), Some("CP01"));
            let b = ds.geometry_bounds().unwrap();
            assert!(
                b[0] > 141.8 && b[2] < 142.5 && b[1] > -10.7 && b[3] < -10.4,
                "{b:?}"
            );
            let env = ds.envelope.unwrap();
            assert!(env[0] < env[2] && env[1] < env[3] && env[0] > 141. && env[1] < -10.);
        }
    }

    #[test]
    fn s129_fixed_time_range_forms() {
        let Some(fc) = fc(S129_FC) else { return };
        let Some(a) = read(S129_130, &fc) else { return };
        let Some(b) = read(S129_200, &fc) else { return };
        let cp = |ds: &Dataset| {
            ds.object("CP_01")
                .unwrap()
                .attribute("fixedTimeRange")
                .cloned()
        };
        // 130TS has an empty <fixedTimeRange/>; 200TS populates it.
        assert_eq!(
            cp(&a).unwrap().value,
            Value::Simple {
                text: String::new(),
                code: None
            }
        );
        let full = cp(&b).unwrap();
        assert_eq!(full.children().len(), 2);
        assert_eq!(full.children()[0].text(), Some("2024-04-17T21:41:00Z"));
    }

    fn doc(members: &str, extra: &str) -> String {
        format!(
            r#"<Dataset xmlns="http://www.iho.int/S129/2.0" xmlns:S100="http://www.iho.int/s100gml/5.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" gml:id="DS">
<S100:DatasetIdentificationInformation><S100:productIdentifier>S-129</S100:productIdentifier></S100:DatasetIdentificationInformation>
{extra}<members>{members}</members></Dataset>"#
        )
    }

    #[test]
    fn curve_reference_cycles_are_bounded() {
        let xml = doc(
            r##"<UnderKeelClearancePlanArea gml:id="A"><geometry><S100:curveProperty xlink:href="#C1"/></geometry></UnderKeelClearancePlanArea>"##,
            r##"<gml:CompositeCurve gml:id="C1"><gml:curveMember xlink:href="#C2"/></gml:CompositeCurve>
<gml:CompositeCurve gml:id="C2"><gml:curveMember xlink:href="#C1"/></gml:CompositeCurve>"##,
        );
        let ds = read_bytes(xml.as_bytes(), &KNOWN, None, ReadLimits::default()).unwrap();
        assert!(ds.objects[0].geometry.is_empty());
        assert!(
            ds.issues.iter().any(|i| i.message.contains("too deep")),
            "{:?}",
            ds.issues
        );
    }

    #[test]
    fn nil_unresolved_and_orientable_curves() {
        let xml = doc(
            r##"<UnderKeelClearanceControlPoint gml:id="P"><featureName xsi:nil="1" nilReason="unknown"/><theCollection xlink:href="#missing"/>
<geometry><S100:curveProperty><gml:OrientableCurve orientation="-"><gml:baseCurve><gml:LineString><gml:posList>1 2 3 4</gml:posList></gml:LineString></gml:baseCurve></gml:OrientableCurve></S100:curveProperty></geometry></UnderKeelClearanceControlPoint>"##,
            "",
        );
        let ds = read_bytes(xml.as_bytes(), &KNOWN, None, ReadLimits::default()).unwrap();
        let o = &ds.objects[0];
        assert_eq!(
            o.attributes[0].value,
            Value::Nil {
                reason: Some("unknown".into())
            }
        );
        assert_eq!(o.geometry, vec![Geometry::Curve(vec![[4., 3.], [2., 1.]])]);
        assert_eq!(o.references[0].target, None);
        assert_eq!(ds.issues.len(), 1);
        assert!(ds.issues[0]
            .message
            .contains("Unresolved reference theCollection"));
    }

    #[test]
    fn late_identification_is_product_specific() {
        let xml = r#"<Dataset xmlns="http://www.iho.int/S129/2.0" xmlns:S100="http://www.iho.int/s100gml/5.0"><members/><S100:DatasetIdentificationInformation><S100:productIdentifier>S-129</S100:productIdentifier></S100:DatasetIdentificationInformation></Dataset>"#;
        let ds = read_bytes(xml.as_bytes(), &KNOWN, None, ReadLimits::default()).unwrap();
        assert_eq!(ds.issues.len(), 1);
        assert_eq!(ds.issues[0].check, "S-158-129/15");
    }

    #[test]
    fn unclosed_ring_and_bad_crs_are_reported() {
        let xml = doc(
            r##"<UnderKeelClearancePlanArea gml:id="A"><geometry><S100:surfaceProperty><gml:Polygon><gml:exterior><gml:LinearRing><gml:posList>0 0 0 1 1 1 1 0</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></S100:surfaceProperty></geometry></UnderKeelClearancePlanArea>
<UnderKeelClearancePlanArea gml:id="B"><geometry><S100:pointProperty><S100:Point srsName="EPSG:3857"><gml:pos>1 2</gml:pos></S100:Point></S100:pointProperty></geometry></UnderKeelClearancePlanArea>"##,
            "",
        );
        let ds = read_bytes(xml.as_bytes(), &KNOWN, None, ReadLimits::default()).unwrap();
        let messages: Vec<_> = ds.issues.iter().map(|i| i.message.as_str()).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[0].contains("not closed"));
        assert!(messages[1].contains("Unsupported CRS"));
    }

    #[test]
    fn append_joins_segments_without_duplicates() {
        let mut out = vec![];
        append(&mut out, vec![[0., 0.], [1., 0.]]);
        append(&mut out, vec![[1., 0.], [1., 1.]]);
        append(&mut out, vec![[2., 2.]]);
        assert_eq!(out, vec![[0., 0.], [1., 0.], [1., 1.], [2., 2.]]);
    }
}
