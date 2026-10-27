//! Conversion of non-S-100 navigational warning exports (SHOM PING KML 2.2,
//! GPX 1.1 and ESRI shapefile) into an in-memory S-124 [`Dataset`].
//!
//! Imported datasets are marked `imported: true` and are never
//! authoritative: they carry no References, never cancel anything, and the
//! lifecycle logic ([`crate::navwarn`]) replaces them with an authoritative
//! dataset of the same message identity. Every import adds an `IMPORT`
//! warning issue saying so.
//!
//! Mapping:
//! * One NavwarnPreamble (`gml:id` `PRE`). messageSeriesIdentifier is
//!   synthesised from the source title or file name when it reads as
//!   `<series> <number>/<yy>` or `<series>-<number>-<yy>` (for example
//!   `AVINAV CHERBOURG 72/26` gives series `AVINAV CHERBOURG`, number 72,
//!   year 2026); unknown mandatory values are encoded as nil (`unknown`),
//!   never guessed. agencyResponsibleForProduction comes from the GPX
//!   author when present. navwarnTypeGeneral is taken from a source line
//!   equal to one of its codelist labels (GPX metadata description).
//!   publicationTime is the source's own time (GPX metadata time, KML
//!   TimeStamp) or the import time.
//! * One NavwarnPart per geometry (`P1`, `P2`, ...), each with a `header`
//!   reference to the preamble. Descriptions become
//!   warningInformation/information text; with a feature catalogue, a
//!   description equal to a navwarnTypeDetails label is encoded as that code.
//!   Free text is tagged with language `und` (ISO 639-2 undetermined).
//! * Coordinates are EPSG:4326 (KML, GPX); shapefiles in EPSG:3857 (Web
//!   Mercator, read from the .prj) are unprojected; EPSG:4326 shapefiles are
//!   read as is. Closed GPX tracks and routes (at least four points, first
//!   equals last) become surfaces.
use crate::navwarn::NAVWARN_TYPE_GENERAL;
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_kernel::geodesy::Mercator;
use ferrite_s100_gml::*;
use roxmltree::Node;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Bound on any single imported source file.
pub const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;
/// Bound on imported positions.
pub const MAX_IMPORT_POSITIONS: usize = 5_000_000;
const S124_NAMESPACE: &str = "http://www.iho.int/S124/gml/2.0";
const LANGUAGE: &str = "und";

/// Source formats understood by [`import_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Kml,
    Gpx,
    /// The bytes are the `.shp`; companions are read from the same folder.
    Shapefile,
    /// A zip archive holding a shapefile.
    Zip,
}

impl ImportFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Kml => "KML",
            Self::Gpx => "GPX",
            Self::Shapefile => "ESRI shapefile",
            Self::Zip => "zipped ESRI shapefile",
        }
    }
    /// Detect from the file extension, then from the content.
    pub fn detect(path: &Path, bytes: &[u8]) -> Option<Self> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        match ext.as_deref() {
            Some("kml") => return Some(Self::Kml),
            Some("gpx") => return Some(Self::Gpx),
            Some("shp") => return Some(Self::Shapefile),
            Some("zip") => return Some(Self::Zip),
            _ => {}
        }
        if bytes.starts_with(b"PK\x03\x04") {
            return Some(Self::Zip);
        }
        if bytes.len() >= 100 && bytes[0..4] == [0, 0, 0x27, 0x0a] {
            return Some(Self::Shapefile);
        }
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        if head.contains("<kml") {
            Some(Self::Kml)
        } else if head.contains("<gpx") {
            Some(Self::Gpx)
        } else {
            None
        }
    }
}

/// Import a KML, GPX, shapefile (`.shp` with `.shx`/`.dbf`/`.prj` beside it)
/// or zipped shapefile, timestamped now.
pub fn import_path(path: &Path) -> Result<Dataset> {
    import_path_with(path, None, Utc::now())
}

/// [`import_path`] with an optional S-124 feature catalogue (to encode
/// navwarnTypeDetails codes) and an explicit import time.
pub fn import_path_with(
    path: &Path,
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    let bytes = read_bounded(path)?;
    let format = ImportFormat::detect(path, &bytes)
        .with_context(|| format!("{}: not a KML, GPX, shapefile or zip file", path.display()))?;
    let mut ds = match format {
        ImportFormat::Shapefile => {
            let companions = ShapefileCompanions::beside(path)?;
            import_shapefile(&bytes, &companions, source_name(path), fc, now)
        }
        _ => import_bytes(&bytes, format, source_name(path), fc, now),
    }
    .with_context(|| path.display().to_string())?;
    ds.source = Some(path.to_path_buf());
    Ok(ds)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("Cannot open {}", path.display()))?;
    ensure!(
        meta.len() as usize <= MAX_IMPORT_BYTES,
        "{} exceeds the {} byte import limit",
        path.display(),
        MAX_IMPORT_BYTES
    );
    std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))
}

fn source_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("import")
}

/// Import from memory. `name` is the source file name (used for the title
/// and series identifier when the content has none). Shapefiles need their
/// companions; use [`import_shapefile`] or a zip.
pub fn import_bytes(
    bytes: &[u8],
    format: ImportFormat,
    name: &str,
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    ensure!(
        bytes.len() <= MAX_IMPORT_BYTES,
        "Import exceeds the byte limit"
    );
    match format {
        ImportFormat::Kml => import_kml(bytes, name, fc, now),
        ImportFormat::Gpx => import_gpx(bytes, name, fc, now),
        ImportFormat::Shapefile => bail!("A shapefile import needs its .dbf/.prj companions"),
        ImportFormat::Zip => import_zip(bytes, name, fc, now),
    }
}

// ---------------------------------------------------------------------------
// Dataset builder
// ---------------------------------------------------------------------------

/// Parsed series identifier of a source title, e.g. `AVINAV CHERBOURG 72/26`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSeries {
    pub series: String,
    pub number: u32,
    pub year: i32,
}

/// Read `<series> <number>/<yy|yyyy>` or `<series>-<number>-<yy|yyyy>`
/// (file names), optionally followed by ` #k/n` part counters.
pub fn parse_source_series(text: &str) -> Option<SourceSeries> {
    let text = text.trim();
    let text = text.split(" #").next()?.trim();
    let year_of = |y: &str| -> Option<i32> {
        if !y.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        match y.len() {
            2 => Some(2000 + y.parse::<i32>().ok()?),
            4 => y.parse().ok(),
            _ => None,
        }
    };
    let number_of = |n: &str| -> Option<u32> {
        let n = n.trim_start_matches(['N', 'n', '°', 'º', 'o', '.']).trim();
        (!n.is_empty() && n.len() <= 6 && n.chars().all(|c| c.is_ascii_digit()))
            .then(|| n.parse().ok())?
    };
    // "<series> <number>/<year>"
    if let Some((head, year)) = text.rsplit_once('/') {
        if let (Some(year), Some((series, number))) =
            (year_of(year.trim()), head.trim().rsplit_once(' '))
        {
            if let Some(number) = number_of(number) {
                let series = series.trim();
                if !series.is_empty() {
                    return Some(SourceSeries {
                        series: series.to_string(),
                        number,
                        year,
                    });
                }
            }
        }
    }
    // "<series>-<number>-<year>"
    let mut it = text.rsplitn(3, '-');
    let (year, number, series) = (it.next()?, it.next()?, it.next()?);
    let (year, number) = (year_of(year.trim())?, number_of(number.trim())?);
    let series = series.trim();
    (!series.is_empty()).then(|| SourceSeries {
        series: series.to_string(),
        number,
        year,
    })
}

fn simple(code: &str, text: impl Into<String>) -> Attribute {
    Attribute {
        code: code.into(),
        value: Value::Simple {
            text: text.into(),
            code: None,
        },
    }
}
fn enumerated(code: &str, value: u32, label: &str) -> Attribute {
    Attribute {
        code: code.into(),
        value: Value::Simple {
            text: label.into(),
            code: Some(value.to_string()),
        },
    }
}
fn complex(code: &str, children: Vec<Attribute>) -> Attribute {
    Attribute {
        code: code.into(),
        value: Value::Complex(children),
    }
}
fn unknown(code: &str) -> Attribute {
    Attribute {
        code: code.into(),
        value: Value::Nil {
            reason: Some("unknown".into()),
        },
    }
}
fn text_attr(code: &str, text: &str) -> Attribute {
    complex(
        code,
        vec![simple("language", LANGUAGE), simple("text", text)],
    )
}

/// One NavwarnPart to be built.
#[derive(Debug, Clone, Default)]
struct PartSpec {
    geometry: Option<Geometry>,
    feature_name: Option<String>,
    information: Vec<String>,
}

#[derive(Debug, Default)]
struct DatasetSpec {
    title: Option<String>,
    series: Option<SourceSeries>,
    agency: Option<String>,
    type_general: Option<(u32, String)>,
    publication_time: Option<DateTime<Utc>>,
    parts: Vec<PartSpec>,
    notes: Vec<String>,
}

/// Collapse white space, drop markup tags and bound the length.
fn clean_text(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(4000).collect()
}

fn type_general_of(line: &str, fc: Option<&FeatureCatalogue>) -> Option<(u32, String)> {
    let l = line.trim();
    if let Some(attr) = fc.and_then(|fc| fc.simple_attributes.get("navwarnTypeGeneral")) {
        if let Some(v) = attr
            .listed_values
            .iter()
            .find(|v| v.label.eq_ignore_ascii_case(l))
        {
            return Some((v.code, v.label.clone()));
        }
    }
    NAVWARN_TYPE_GENERAL
        .iter()
        .position(|label| label.eq_ignore_ascii_case(l))
        .map(|i| (i as u32 + 1, NAVWARN_TYPE_GENERAL[i].to_string()))
}

fn type_details_of(text: &str, fc: Option<&FeatureCatalogue>) -> Option<(u32, String)> {
    let attr = fc?.simple_attributes.get("navwarnTypeDetails")?;
    let t = text.trim();
    attr.listed_values
        .iter()
        .find(|v| v.label.eq_ignore_ascii_case(t))
        .map(|v| (v.code, v.label.clone()))
}

fn build(
    spec: DatasetSpec,
    format: ImportFormat,
    name: &str,
    sha: [u8; 32],
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    ensure!(
        !spec.parts.is_empty(),
        "No geometry found in the {} source",
        format.label()
    );
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let series = spec
        .series
        .clone()
        .or_else(|| spec.title.as_deref().and_then(parse_source_series))
        .or_else(|| parse_source_series(stem));
    let publication = spec.publication_time.unwrap_or(now);

    let mut msi = Vec::new();
    msi.push(match &spec.agency {
        Some(a) => simple("agencyResponsibleForProduction", a.as_str()),
        None => unknown("agencyResponsibleForProduction"),
    });
    match &series {
        Some(s) => {
            msi.push(simple("nameOfSeries", s.series.as_str()));
            msi.push(simple("warningNumber", s.number.to_string()));
            msi.push(unknown("warningType"));
            msi.push(simple("year", s.year.to_string()));
        }
        None => {
            msi.push(simple(
                "nameOfSeries",
                format!("Imported {}", format.label()),
            ));
            msi.push(unknown("warningNumber"));
            msi.push(unknown("warningType"));
            msi.push(unknown("year"));
        }
    }
    let mut pre = vec![
        unknown("generalArea"),
        complex("messageSeriesIdentifier", msi),
    ];
    if let Some(t) = &spec.title {
        pre.push(text_attr("navwarnTitle", t));
    }
    pre.push(unknown("intService"));
    pre.push(match &spec.type_general {
        Some((code, label)) => enumerated("navwarnTypeGeneral", *code, label),
        None => unknown("navwarnTypeGeneral"),
    });
    pre.push(simple(
        "publicationTime",
        publication.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    ));

    let mut objects = vec![Object {
        id: "PRE".into(),
        code: "NavwarnPreamble".into(),
        kind: ObjectKind::Information,
        attributes: pre,
        geometry: Vec::new(),
        references: Vec::new(),
    }];
    for (k, p) in spec.parts.iter().enumerate() {
        let mut attrs = Vec::new();
        let mut info = Vec::new();
        for text in &p.information {
            match type_details_of(text, fc) {
                Some((code, label)) => info.push(enumerated("navwarnTypeDetails", code, &label)),
                None => info.push(text_attr("information", text)),
            }
        }
        // FC order: information before navwarnTypeDetails.
        info.sort_by_key(|a| a.code != "information");
        attrs.push(if info.is_empty() {
            unknown("warningInformation")
        } else {
            complex("warningInformation", info)
        });
        if let Some(n) = &p.feature_name {
            attrs.push(complex(
                "featureName",
                vec![simple("language", LANGUAGE), simple("name", n.as_str())],
            ));
        }
        objects.push(Object {
            id: format!("P{}", k + 1),
            code: "NavwarnPart".into(),
            kind: ObjectKind::Feature,
            attributes: attrs,
            geometry: p.geometry.iter().cloned().collect(),
            references: vec![Reference {
                role: "header".into(),
                href: "#PRE".into(),
                target: Some(0),
            }],
        });
    }
    let index: HashMap<String, usize> = objects
        .iter()
        .enumerate()
        .map(|(i, o)| (o.id.clone(), i))
        .collect();
    let title = spec
        .title
        .clone()
        .or_else(|| {
            series
                .as_ref()
                .map(|s| format!("{} {}/{:02}", s.series, s.number, s.year % 100))
        })
        .unwrap_or_else(|| stem.to_string());
    let identification = Identification {
        fields: vec![
            ("encodingSpecification".into(), "S-100 Part 10b".into()),
            ("encodingSpecificationEdition".into(), "1.0".into()),
            ("productIdentifier".into(), "S-124".into()),
            ("productEdition".into(), "2.0.0".into()),
            ("applicationProfile".into(), "1".into()),
            ("datasetFileIdentifier".into(), name.to_string()),
            ("datasetTitle".into(), title),
            (
                "datasetReferenceDate".into(),
                publication.format("%Y-%m-%d").to_string(),
            ),
            ("datasetLanguage".into(), LANGUAGE.into()),
            ("datasetTopicCategory".into(), "oceans".into()),
            ("datasetPurpose".into(), "base".into()),
            ("updateNumber".into(), "0".into()),
        ],
    };
    let mut issues = vec![Issue {
        severity: Severity::Warning,
        check: "IMPORT".into(),
        object: None,
        message: format!(
            "Imported from {} ({name}); not an S-100 dataset, not authoritative, not to be used for navigation",
            format.label()
        ),
    }];
    for n in spec.notes {
        issues.push(Issue {
            severity: Severity::Warning,
            check: "IMPORT".into(),
            object: None,
            message: n,
        });
    }
    let hex: String = sha[..8].iter().map(|b| format!("{b:02x}")).collect();
    let mut ds = Dataset {
        source: None,
        sha256: sha,
        product: crate::PRODUCT,
        namespace: S124_NAMESPACE.into(),
        gml_id: format!("IMPORT_{hex}"),
        identification,
        envelope: None,
        objects,
        index,
        issues,
        imported: true,
    };
    ds.envelope = ds.geometry_bounds();
    Ok(ds)
}

fn check_coord(lon: f64, lat: f64) -> Result<Coord> {
    ensure!(
        lon.is_finite()
            && lat.is_finite()
            && (-180.0..=180.0).contains(&lon)
            && (-90.0..=90.0).contains(&lat),
        "Coordinate out of range: {lon}, {lat}"
    );
    Ok([lon, lat])
}

fn close_ring(mut r: Vec<Coord>) -> Vec<Coord> {
    if r.first() != r.last() {
        if let Some(f) = r.first().copied() {
            r.push(f);
        }
    }
    r
}

/// Points to a Point, closed point sequences (>= 4, first = last) to a
/// Surface, others to a Curve.
fn line_or_area(points: Vec<Coord>) -> Option<Geometry> {
    match points.len() {
        0 => None,
        1 => Some(Geometry::Point(points[0])),
        n if n >= 4 && points[0] == points[n - 1] => Some(Geometry::Surface {
            exterior: points,
            interiors: Vec::new(),
        }),
        _ => Some(Geometry::Curve(points)),
    }
}

fn parse_xml(bytes: &[u8]) -> Result<roxmltree::Document<'_>> {
    let text = std::str::from_utf8(bytes).context("Not UTF-8")?;
    let text = text.trim_start_matches('\u{feff}');
    roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 5_000_000,
        },
    )
    .context("Not well-formed XML")
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}
fn child_text(n: Node, name: &str) -> Option<String> {
    let t: String = child(n, name)?
        .descendants()
        .filter(|d| d.is_text())
        .filter_map(|d| d.text())
        .collect();
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

// ---------------------------------------------------------------------------
// KML
// ---------------------------------------------------------------------------

fn kml_coordinates(text: &str, budget: &mut usize) -> Result<Vec<Coord>> {
    let mut out = Vec::new();
    for tuple in text.split_whitespace() {
        let mut it = tuple.split(',');
        let lon: f64 = it
            .next()
            .unwrap_or("")
            .trim()
            .parse()
            .context("Bad KML longitude")?;
        let lat: f64 = it
            .next()
            .context("KML tuple without latitude")?
            .trim()
            .parse()
            .context("Bad KML latitude")?;
        *budget = budget.checked_sub(1).context("Too many positions")?;
        out.push(check_coord(lon, lat)?);
    }
    Ok(out)
}

fn kml_geometries(node: Node, out: &mut Vec<Geometry>, budget: &mut usize) -> Result<()> {
    for g in node.children().filter(|c| c.is_element()) {
        let coords_of = |n: Node, budget: &mut usize| -> Result<Vec<Coord>> {
            let t = n
                .descendants()
                .find(|d| d.is_element() && d.tag_name().name() == "coordinates")
                .and_then(|c| c.text())
                .unwrap_or("");
            kml_coordinates(t, budget)
        };
        match g.tag_name().name() {
            "Point" => {
                if let Some(p) = coords_of(g, budget)?.first() {
                    out.push(Geometry::Point(*p));
                }
            }
            "LineString" => {
                let c = coords_of(g, budget)?;
                match c.len() {
                    0 => {}
                    1 => out.push(Geometry::Point(c[0])),
                    _ => out.push(Geometry::Curve(c)),
                }
            }
            "LinearRing" => {
                let c = close_ring(coords_of(g, budget)?);
                if c.len() >= 4 {
                    out.push(Geometry::Surface {
                        exterior: c,
                        interiors: Vec::new(),
                    });
                }
            }
            "Polygon" => {
                let exterior = match child(g, "outerBoundaryIs") {
                    Some(o) => close_ring(coords_of(o, budget)?),
                    None => continue,
                };
                ensure!(
                    exterior.len() >= 4,
                    "KML polygon ring with fewer than 3 positions"
                );
                let mut interiors = Vec::new();
                for inner in g
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "innerBoundaryIs")
                {
                    let r = close_ring(coords_of(inner, budget)?);
                    if r.len() >= 4 {
                        interiors.push(r);
                    }
                }
                out.push(Geometry::Surface {
                    exterior,
                    interiors,
                });
            }
            "MultiGeometry" => kml_geometries(g, out, budget)?,
            _ => {}
        }
    }
    Ok(())
}

fn import_kml(
    bytes: &[u8],
    name: &str,
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    let doc = parse_xml(bytes)?;
    let root = doc.root_element();
    ensure!(
        root.tag_name().name() == "kml",
        "Root element is {}, not kml",
        root.tag_name().name()
    );
    let mut spec = DatasetSpec::default();
    let document = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Document");
    spec.title = document
        .and_then(|d| child_text(d, "name"))
        .map(|t| clean_text(&t));
    if let Some(d) = document.and_then(|d| child_text(d, "description")) {
        for line in d.lines() {
            if let Some(t) = type_general_of(line, fc) {
                spec.type_general.get_or_insert(t);
            }
        }
    }
    let mut budget = MAX_IMPORT_POSITIONS;
    let placemarks: Vec<Node> = root
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Placemark")
        .collect();
    for pm in &placemarks {
        let mut geoms = Vec::new();
        kml_geometries(*pm, &mut geoms, &mut budget)?;
        let pname = child_text(*pm, "name").map(|t| clean_text(&t));
        let desc = child_text(*pm, "description").map(|t| clean_text(&t));
        let time = pm
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "when")
            .and_then(|n| n.text())
            .and_then(crate::navwarn::parse_date_time);
        if let Some(t) = time {
            spec.publication_time = Some(spec.publication_time.map_or(t, |p| p.max(t)));
        }
        // A single named placemark titles the warning when the document has
        // no name of its own.
        if spec.title.is_none() && placemarks.len() == 1 {
            spec.title = pname.clone();
        }
        for g in geoms {
            spec.parts.push(PartSpec {
                geometry: Some(g),
                feature_name: pname.clone().filter(|n| Some(n) != spec.title.as_ref()),
                information: desc.iter().cloned().collect(),
            });
        }
    }
    let folder = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Folder")
        .and_then(|f| child_text(f, "name"));
    if let Some(f) = folder {
        spec.notes
            .push(format!("Source folder identifier: {}", clean_text(&f)));
    }
    build(
        spec,
        ImportFormat::Kml,
        name,
        Sha256::digest(bytes).into(),
        fc,
        now,
    )
}

// ---------------------------------------------------------------------------
// GPX
// ---------------------------------------------------------------------------

fn gpx_point(n: Node) -> Result<Coord> {
    let lat: f64 = n
        .attribute("lat")
        .context("GPX point without lat")?
        .trim()
        .parse()
        .context("Bad GPX lat")?;
    let lon: f64 = n
        .attribute("lon")
        .context("GPX point without lon")?
        .trim()
        .parse()
        .context("Bad GPX lon")?;
    check_coord(lon, lat)
}

fn import_gpx(
    bytes: &[u8],
    name: &str,
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    let doc = parse_xml(bytes)?;
    let root = doc.root_element();
    ensure!(
        root.tag_name().name() == "gpx",
        "Root element is {}, not gpx",
        root.tag_name().name()
    );
    let mut spec = DatasetSpec::default();
    let mut budget = MAX_IMPORT_POSITIONS;
    let meta_name = child(root, "metadata")
        .and_then(|m| child_text(m, "name"))
        .map(|t| clean_text(&t));
    if let Some(m) = child(root, "metadata") {
        spec.title = meta_name.clone();
        spec.agency = child(m, "author")
            .and_then(|a| child_text(a, "name"))
            .map(|t| clean_text(&t));
        spec.publication_time =
            child_text(m, "time").and_then(|t| crate::navwarn::parse_date_time(&t));
        if let Some(desc) = child_text(m, "desc") {
            // PING: a disclaimer paragraph, then the warning category.
            let mut rest = Vec::new();
            for line in desc.lines().map(str::trim).filter(|l| !l.is_empty()) {
                match type_general_of(line, fc) {
                    Some(t) if spec.type_general.is_none() => spec.type_general = Some(t),
                    _ => rest.push(line),
                }
            }
            if !rest.is_empty() {
                spec.notes.push(format!(
                    "Source description: {}",
                    clean_text(&rest.join(" "))
                ));
            }
        }
    }
    // A sub-element name that only repeats the warning title plus a part
    // counter ("... #1/1") is not a feature name.
    let feature_name = |n: Node| -> Option<String> {
        let t = child_text(n, "name").map(|t| clean_text(&t))?;
        match &meta_name {
            Some(m) if t.starts_with(m.as_str()) => None,
            _ => Some(t),
        }
    };
    let information = |n: Node| -> Vec<String> {
        child_text(n, "desc")
            .or_else(|| child_text(n, "cmt"))
            .map(|t| clean_text(&t))
            .filter(|t| !t.is_empty())
            .into_iter()
            .collect()
    };
    for el in root.children().filter(|c| c.is_element()) {
        match el.tag_name().name() {
            "wpt" => {
                budget = budget.checked_sub(1).context("Too many positions")?;
                spec.parts.push(PartSpec {
                    geometry: Some(Geometry::Point(gpx_point(el)?)),
                    feature_name: feature_name(el),
                    information: information(el),
                });
            }
            "rte" | "trk" => {
                let segments: Vec<Node> = if el.tag_name().name() == "rte" {
                    vec![el]
                } else {
                    el.children()
                        .filter(|c| c.is_element() && c.tag_name().name() == "trkseg")
                        .collect()
                };
                for seg in segments {
                    let mut pts = Vec::new();
                    for p in seg.children().filter(|c| {
                        c.is_element() && matches!(c.tag_name().name(), "rtept" | "trkpt")
                    }) {
                        budget = budget.checked_sub(1).context("Too many positions")?;
                        pts.push(gpx_point(p)?);
                    }
                    if let Some(g) = line_or_area(pts) {
                        spec.parts.push(PartSpec {
                            geometry: Some(g),
                            feature_name: feature_name(el),
                            information: information(el),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    build(
        spec,
        ImportFormat::Gpx,
        name,
        Sha256::digest(bytes).into(),
        fc,
        now,
    )
}

// ---------------------------------------------------------------------------
// Shapefile
// ---------------------------------------------------------------------------

/// The optional companion files of a `.shp`.
#[derive(Debug, Clone, Default)]
pub struct ShapefileCompanions {
    pub dbf: Option<Vec<u8>>,
    pub prj: Option<String>,
    pub cpg: Option<String>,
}

impl ShapefileCompanions {
    /// Read `.dbf`, `.prj` and `.cpg` next to a `.shp` (any extension case).
    pub fn beside(shp: &Path) -> Result<Self> {
        let find = |ext: &str| -> Option<PathBuf> {
            [ext.to_string(), ext.to_ascii_uppercase()]
                .iter()
                .map(|e| shp.with_extension(e))
                .find(|p| p.is_file())
        };
        Ok(Self {
            dbf: find("dbf").map(|p| read_bounded(&p)).transpose()?,
            prj: find("prj")
                .map(|p| read_bounded(&p).map(|b| String::from_utf8_lossy(&b).into_owned()))
                .transpose()?,
            cpg: find("cpg")
                .map(|p| read_bounded(&p).map(|b| String::from_utf8_lossy(&b).trim().to_string()))
                .transpose()?,
        })
    }
}

/// Coordinate reference system of a shapefile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeCrs {
    Wgs84,
    WebMercator,
}

/// Recognise EPSG:3857 and EPSG:4326 from a `.prj` (WKT1 / ESRI WKT).
pub fn crs_from_prj(prj: &str) -> Result<ShapeCrs> {
    let p = prj.trim();
    let upper = p.to_ascii_uppercase();
    let compact: String = upper.chars().filter(|c| !c.is_whitespace()).collect();
    if upper.starts_with("PROJCS") || upper.starts_with("PROJCRS") {
        // The outermost AUTHORITY is the last one in WKT1.
        let last_authority = compact.rfind("AUTHORITY[").map(|i| &compact[i..]);
        if last_authority.is_some_and(|a| a.starts_with("AUTHORITY[\"EPSG\",\"3857\"]"))
            || [
                "PSEUDO-MERCATOR",
                "PSEUDO_MERCATOR",
                "MERCATOR_AUXILIARY_SPHERE",
                "POPULAR VISUALISATION",
                "WEB_MERCATOR",
                "WEB MERCATOR",
            ]
            .iter()
            .any(|k| upper.contains(k))
        {
            return Ok(ShapeCrs::WebMercator);
        }
        bail!("Unsupported shapefile projection (only EPSG:3857 and EPSG:4326)");
    }
    if (upper.starts_with("GEOGCS") || upper.starts_with("GEOGCRS"))
        && (upper.contains("WGS") && upper.contains("84"))
    {
        return Ok(ShapeCrs::Wgs84);
    }
    bail!("Unsupported shapefile coordinate reference system")
}

fn le_i32(b: &[u8], at: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(
        b.get(at..at + 4)
            .context("Truncated shapefile")?
            .try_into()?,
    ))
}
fn be_i32(b: &[u8], at: usize) -> Result<i32> {
    Ok(i32::from_be_bytes(
        b.get(at..at + 4)
            .context("Truncated shapefile")?
            .try_into()?,
    ))
}
fn le_f64(b: &[u8], at: usize) -> Result<f64> {
    Ok(f64::from_le_bytes(
        b.get(at..at + 8)
            .context("Truncated shapefile")?
            .try_into()?,
    ))
}

type Ring = Vec<[f64; 2]>;

/// A shape record in source coordinates.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Null,
    Points(Vec<[f64; 2]>, bool),
    /// Parts of a polyline (`false`) or rings of a polygon (`true`).
    Parts(Vec<Vec<[f64; 2]>>, bool),
}

fn read_shp(b: &[u8], budget: &mut usize) -> Result<Vec<Shape>> {
    ensure!(
        b.len() >= 100 && be_i32(b, 0)? == 9994,
        "Not an ESRI shapefile"
    );
    let file_len = (be_i32(b, 24)? as i64 * 2).clamp(100, b.len() as i64) as usize;
    let mut shapes = Vec::new();
    let mut at = 100;
    while at + 8 <= file_len {
        let len = be_i32(b, at + 4)?;
        ensure!(len >= 2, "Bad shapefile record length");
        let content = b
            .get(at + 8..at + 8 + len as usize * 2)
            .context("Truncated shapefile record")?;
        at += 8 + len as usize * 2;
        let kind = le_i32(content, 0)?;
        let shape = match kind {
            0 => Shape::Null,
            1 | 11 | 21 => {
                *budget = budget.checked_sub(1).context("Too many positions")?;
                Shape::Points(vec![[le_f64(content, 4)?, le_f64(content, 12)?]], false)
            }
            8 | 18 | 28 => {
                let n = le_i32(content, 36)?;
                ensure!(n >= 0, "Bad point count");
                let n = n as usize;
                *budget = budget.checked_sub(n).context("Too many positions")?;
                let mut pts = Vec::with_capacity(n);
                for i in 0..n {
                    pts.push([le_f64(content, 40 + 16 * i)?, le_f64(content, 48 + 16 * i)?]);
                }
                Shape::Points(pts, true)
            }
            3 | 13 | 23 | 5 | 15 | 25 => {
                let (np, n) = (le_i32(content, 36)?, le_i32(content, 40)?);
                ensure!(np >= 0 && n >= 0, "Bad part or point count");
                let (np, n) = (np as usize, n as usize);
                *budget = budget.checked_sub(n).context("Too many positions")?;
                ensure!(
                    content.len() >= 44 + 4 * np + 16 * n,
                    "Truncated shapefile record"
                );
                let mut starts = Vec::with_capacity(np);
                for i in 0..np {
                    let s = le_i32(content, 44 + 4 * i)?;
                    ensure!(s >= 0 && (s as usize) <= n, "Bad part start");
                    starts.push(s as usize);
                }
                let base = 44 + 4 * np;
                let mut parts = Vec::with_capacity(np);
                for (i, &s) in starts.iter().enumerate() {
                    let e = starts.get(i + 1).copied().unwrap_or(n);
                    ensure!(e >= s, "Bad part order");
                    let mut pts = Vec::with_capacity(e - s);
                    for j in s..e {
                        pts.push([
                            le_f64(content, base + 16 * j)?,
                            le_f64(content, base + 16 * j + 8)?,
                        ]);
                    }
                    parts.push(pts);
                }
                Shape::Parts(parts, matches!(kind, 5 | 15 | 25))
            }
            31 => bail!("MultiPatch shapes are not supported"),
            k => bail!("Unknown shape type {k}"),
        };
        shapes.push(shape);
    }
    Ok(shapes)
}

/// dBASE records as (field name, value) lists; empty when there are no fields.
fn read_dbf(b: &[u8], utf8: bool) -> Result<Vec<Vec<(String, String)>>> {
    ensure!(b.len() >= 32, "Truncated dBASE file");
    let count = u32::from_le_bytes(b[4..8].try_into()?) as usize;
    let header_len = u16::from_le_bytes(b[8..10].try_into()?) as usize;
    let record_len = u16::from_le_bytes(b[10..12].try_into()?) as usize;
    ensure!(
        header_len >= 33 && header_len <= b.len(),
        "Bad dBASE header"
    );
    let mut fields = Vec::new();
    let mut at = 32;
    let mut offset = 1; // deletion flag
    while at + 32 <= header_len && b[at] != 0x0d {
        let name: String = b[at..at + 11]
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as char)
            .collect();
        let len = b[at + 16] as usize;
        fields.push((name.trim().to_string(), offset, len));
        offset += len;
        at += 32;
    }
    ensure!(record_len >= offset, "dBASE record shorter than its fields");
    let decode = |s: &[u8]| -> String {
        if utf8 {
            String::from_utf8_lossy(s).trim().to_string()
        } else {
            s.iter()
                .map(|c| *c as char)
                .collect::<String>()
                .trim()
                .to_string() // ISO 8859-1
        }
    };
    let mut records = Vec::new();
    for r in 0..count {
        let start = header_len + r * record_len;
        let Some(rec) = b.get(start..start + record_len) else {
            break;
        };
        if rec[0] == b'*' {
            records.push(Vec::new()); // deleted: keep alignment with the shapes
            continue;
        }
        records.push(
            fields
                .iter()
                .map(|(n, o, l)| (n.clone(), decode(&rec[*o..*o + *l])))
                .filter(|(_, v)| !v.is_empty())
                .collect(),
        );
    }
    Ok(records)
}

fn to_wgs84(p: [f64; 2], crs: ShapeCrs) -> Result<Coord> {
    match crs {
        ShapeCrs::Wgs84 => check_coord(p[0], p[1]),
        ShapeCrs::WebMercator => {
            let g = Mercator::Web.unproject(p)?;
            check_coord(g.longitude(), g.latitude())
        }
    }
}

fn shape_geometries(shape: &Shape, crs: ShapeCrs) -> Result<Vec<Geometry>> {
    let conv = |pts: &[[f64; 2]]| -> Result<Vec<Coord>> {
        pts.iter().map(|p| to_wgs84(*p, crs)).collect()
    };
    Ok(match shape {
        Shape::Null => Vec::new(),
        Shape::Points(p, false) => conv(p)?.into_iter().map(Geometry::Point).collect(),
        Shape::Points(p, true) => match p.len() {
            0 => Vec::new(),
            1 => vec![Geometry::Point(to_wgs84(p[0], crs)?)],
            _ => vec![Geometry::MultiPoint(conv(p)?)],
        },
        Shape::Parts(parts, false) => {
            let mut out = Vec::new();
            for part in parts {
                let c = conv(part)?;
                match c.len() {
                    0 => {}
                    1 => out.push(Geometry::Point(c[0])),
                    _ => out.push(Geometry::Curve(c)),
                }
            }
            out
        }
        Shape::Parts(rings, true) => {
            // Outer rings are clockwise, holes counter-clockwise (in the
            // source plane); a hole belongs to the outer ring containing it.
            let signed_area = |r: &[[f64; 2]]| -> f64 {
                r.windows(2)
                    .map(|w| w[0][0] * w[1][1] - w[1][0] * w[0][1])
                    .sum::<f64>()
                    / 2.0
            };
            let mut outers: Vec<(Ring, Vec<Ring>)> = Vec::new();
            let mut holes = Vec::new();
            for r in rings.iter().filter(|r| r.len() >= 3) {
                let mut r = r.clone();
                if r.first() != r.last() {
                    r.push(r[0]);
                }
                if signed_area(&r) <= 0.0 {
                    outers.push((r, Vec::new()));
                } else {
                    holes.push(r);
                }
            }
            if outers.is_empty() {
                // Wrongly oriented data: treat every ring as an outer ring.
                outers = holes.drain(..).map(|h| (h, Vec::new())).collect();
            }
            for h in holes {
                let q = h[0];
                let owner = outers
                    .iter()
                    .position(|(o, _)| ferrite_kernel::inside_ring(o.iter().copied(), q))
                    .unwrap_or(0);
                outers[owner].1.push(h);
            }
            let mut out = Vec::new();
            for (o, hs) in outers {
                out.push(Geometry::Surface {
                    exterior: conv(&o)?,
                    interiors: hs.iter().map(|h| conv(h)).collect::<Result<_>>()?,
                });
            }
            out
        }
    })
}

fn is_name_field(f: &str) -> bool {
    matches!(
        f.to_ascii_lowercase().as_str(),
        "name" | "nom" | "nom_obj" | "objnam" | "label"
    )
}
fn is_title_field(f: &str) -> bool {
    matches!(f.to_ascii_lowercase().as_str(), "title" | "titre")
}
fn is_text_field(f: &str) -> bool {
    matches!(
        f.to_ascii_lowercase().as_str(),
        "desc"
            | "descr"
            | "description"
            | "info"
            | "information"
            | "text"
            | "texte"
            | "comment"
            | "cmt"
    )
}

/// Import a shapefile from its `.shp` bytes and companions.
pub fn import_shapefile(
    shp: &[u8],
    companions: &ShapefileCompanions,
    name: &str,
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    ensure!(
        shp.len() <= MAX_IMPORT_BYTES,
        "Import exceeds the byte limit"
    );
    let mut budget = MAX_IMPORT_POSITIONS;
    let shapes = read_shp(shp, &mut budget)?;
    let mut spec = DatasetSpec::default();
    let crs = match &companions.prj {
        Some(prj) => crs_from_prj(prj)?,
        None => {
            let all_degrees = shapes.iter().all(|s| match s {
                Shape::Null => true,
                Shape::Points(p, _) => p.iter().all(|c| c[0].abs() <= 180.0 && c[1].abs() <= 90.0),
                Shape::Parts(ps, _) => ps
                    .iter()
                    .flatten()
                    .all(|c| c[0].abs() <= 180.0 && c[1].abs() <= 90.0),
            });
            ensure!(
                all_degrees,
                "Shapefile without .prj and coordinates outside degree range"
            );
            spec.notes
                .push("Shapefile without .prj: coordinates assumed to be WGS 84 degrees".into());
            ShapeCrs::Wgs84
        }
    };
    let utf8 = companions
        .cpg
        .as_deref()
        .is_none_or(|c| c.to_ascii_uppercase().replace('-', "").contains("UTF8"));
    let records = match &companions.dbf {
        Some(d) => read_dbf(d, utf8)?,
        None => Vec::new(),
    };
    for (i, shape) in shapes.iter().enumerate() {
        let attrs = records.get(i).cloned().unwrap_or_default();
        let feature_name = attrs
            .iter()
            .find(|(f, _)| is_name_field(f))
            .map(|(_, v)| clean_text(v));
        if let Some((_, t)) = attrs.iter().find(|(f, _)| is_title_field(f)) {
            spec.title.get_or_insert_with(|| clean_text(t));
        }
        let mut information: Vec<String> = attrs
            .iter()
            .filter(|(f, _)| is_text_field(f))
            .map(|(_, v)| clean_text(v))
            .collect();
        if information.is_empty() {
            let other: Vec<String> = attrs
                .iter()
                .filter(|(f, _)| !is_name_field(f) && !is_title_field(f))
                .map(|(f, v)| format!("{f}: {}", clean_text(v)))
                .collect();
            if !other.is_empty() {
                information.push(other.join("; "));
            }
        }
        for g in shape_geometries(shape, crs)? {
            spec.parts.push(PartSpec {
                geometry: Some(g),
                feature_name: feature_name.clone(),
                information: information.clone(),
            });
        }
    }
    if crs == ShapeCrs::WebMercator {
        spec.notes.push(
            "Shapefile coordinates converted from EPSG:3857 (WGS 84 / Pseudo-Mercator)".into(),
        );
    }
    let mut hasher = Sha256::new();
    hasher.update(shp);
    if let Some(d) = &companions.dbf {
        hasher.update(d);
    }
    if let Some(p) = &companions.prj {
        hasher.update(p.as_bytes());
    }
    build(
        spec,
        ImportFormat::Shapefile,
        name,
        hasher.finalize().into(),
        fc,
        now,
    )
}

// ---------------------------------------------------------------------------
// Zip (stored and deflate entries)
// ---------------------------------------------------------------------------

fn le_u16(b: &[u8], at: usize) -> Result<usize> {
    Ok(u16::from_le_bytes(b.get(at..at + 2).context("Truncated zip")?.try_into()?) as usize)
}
fn le_u32(b: &[u8], at: usize) -> Result<usize> {
    Ok(u32::from_le_bytes(b.get(at..at + 4).context("Truncated zip")?.try_into()?) as usize)
}

/// Entries of a zip archive by lower-case file name (no directories).
fn zip_entries(b: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    // End of central directory: search the last 64 KiB + 22 bytes.
    let from = b.len().saturating_sub(65_557);
    let eocd = (from..b.len().saturating_sub(21))
        .rev()
        .find(|&i| b[i..i + 4] == *b"PK\x05\x06")
        .context("Zip end of central directory not found")?;
    let count = le_u16(b, eocd + 10)?;
    let mut at = le_u32(b, eocd + 16)?;
    ensure!(count <= 4096, "Too many zip entries");
    let mut out = HashMap::new();
    let mut total = 0usize;
    for _ in 0..count {
        ensure!(
            b.get(at..at + 4) == Some(b"PK\x01\x02"),
            "Bad zip central directory"
        );
        let method = le_u16(b, at + 10)?;
        let crc = le_u32(b, at + 16)? as u32;
        let csize = le_u32(b, at + 20)?;
        let usize_ = le_u32(b, at + 24)?;
        let (nlen, xlen, clen) = (
            le_u16(b, at + 28)?,
            le_u16(b, at + 30)?,
            le_u16(b, at + 32)?,
        );
        let local = le_u32(b, at + 42)?;
        let name =
            String::from_utf8_lossy(b.get(at + 46..at + 46 + nlen).context("Truncated zip")?)
                .into_owned();
        at += 46 + nlen + xlen + clen;
        if name.ends_with('/') {
            continue;
        }
        ensure!(
            b.get(local..local + 4) == Some(b"PK\x03\x04"),
            "Bad zip local header"
        );
        let data_at = local + 30 + le_u16(b, local + 26)? + le_u16(b, local + 28)?;
        let data = b
            .get(data_at..data_at + csize)
            .context("Truncated zip entry")?;
        total = total.saturating_add(usize_);
        ensure!(
            usize_ <= MAX_IMPORT_BYTES && total <= MAX_IMPORT_BYTES,
            "Zip content exceeds the import limit"
        );
        let bytes = match method {
            0 => data.to_vec(),
            8 => {
                use std::io::Read;
                // Read one byte past the declared size to detect a lying header.
                let mut out = Vec::with_capacity(usize_);
                flate2::read::DeflateDecoder::new(data)
                    .take(usize_ as u64 + 1)
                    .read_to_end(&mut out)
                    .with_context(|| format!("Zip entry '{name}' is not valid deflate data"))?;
                out
            }
            m => bail!("Zip entry '{name}' uses unsupported compression method {m}"),
        };
        ensure!(
            bytes.len() == usize_ && crc32(&bytes) == crc,
            "Zip entry '{name}' is corrupt"
        );
        let base = name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&name)
            .to_ascii_lowercase();
        out.insert(base, bytes);
    }
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn import_zip(
    bytes: &[u8],
    name: &str,
    fc: Option<&FeatureCatalogue>,
    now: DateTime<Utc>,
) -> Result<Dataset> {
    let entries = zip_entries(bytes)?;
    let shp_name = {
        let mut shps: Vec<&String> = entries.keys().filter(|k| k.ends_with(".shp")).collect();
        shps.sort();
        match shps.as_slice() {
            [one] => (*one).clone(),
            [] => {
                if let Some(k) = entries
                    .keys()
                    .find(|k| k.ends_with(".kml") || k.ends_with(".gpx"))
                {
                    let fmt = if k.ends_with(".kml") {
                        ImportFormat::Kml
                    } else {
                        ImportFormat::Gpx
                    };
                    return import_bytes(&entries[k], fmt, k, fc, now);
                }
                bail!("No shapefile in the zip archive")
            }
            _ => bail!("The zip archive holds several shapefiles; import them separately"),
        }
    };
    let stem = shp_name.trim_end_matches(".shp");
    let get = |ext: &str| entries.get(&format!("{stem}.{ext}"));
    let companions = ShapefileCompanions {
        dbf: get("dbf").cloned(),
        prj: get("prj").map(|b| String::from_utf8_lossy(b).into_owned()),
        cpg: get("cpg").map(|b| String::from_utf8_lossy(b).trim().to_string()),
    };
    let mut ds = import_shapefile(&entries[&shp_name], &companions, name, fc, now)?;
    ds.sha256 = Sha256::digest(bytes).into();
    Ok(ds)
}
