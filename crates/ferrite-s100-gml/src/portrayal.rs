//! S-100 Part 9 (XSLT) portrayal of GML datasets.
//!
//! The pipeline is pure (no GPU, no application state):
//!
//! 1. [`input_xml`] builds the Part 9 Appendix 9-A portrayal input
//!    (`Dataset/{InformationTypes, Points, MultiPoints, Curves,
//!    CompositeCurves, Surfaces, Features}`) from a [`Dataset`], adding the
//!    virtual input features a product's portrayal catalogue expects
//!    ([`InputOptions::virtual_features`], for example the S-129
//!    `PlannedRoute` and information boxes).
//! 2. [`XsltPortrayal::transform`] runs the catalogue's top level XSLT rule
//!    with its context parameters (catalogue defaults, overridable).
//! 3. [`display_list_instructions`] converts the resulting Part 9 display list
//!    into Part 9a drawing-instruction strings, one string per feature, which
//!    the host parses with `ferrite_lua::PortrayalResult::parse`.
//! 4. [`synthetic_cell`] provides the geometry of every input feature as an
//!    `S101Cell` so the host's `convert_lua_results_for_cell` can draw them;
//!    the host feature id is `"<code>|<key>"` ([`FeatureInstructions::host_feature_id`]).
//!
//! Interpretation decisions (documented where they are applied):
//! * Colour transparency follows S-100 Part 9 clause 9-12.2.2.3 literally:
//!   0 is opaque and 1 is fully transparent (the XSD default is 0). The S-129
//!   2.0.0 InformationBox rule passes transparency 1 for its text foreground
//!   and background; read normatively the text is invisible. This is kept by
//!   default and reported as a conformance note;
//!   [`ConvertOptions::opaque_invisible_text`] renders such text opaque.
//! * A feature attribute `scaleMinimum` (S-129 clause on dataset loading: it
//!   controls "whether the data should be visible at a particular display
//!   scale") becomes `ScaleMinimum` when the instruction has none.
//! * An instruction assigned to a viewing group the catalogue does not
//!   declare can never be enabled by the user, so by Part 9 clause 9-11.2.2
//!   ("Instruction is disabled if any Viewing Group is disabled") it is never
//!   shown; it is dropped with a note (the host rejects undeclared groups).
//!   [`ConvertOptions::implicit_viewing_groups`] treats listed groups as
//!   declared (an explicit errata correction, off by default).
//! * Symbol references (PointInstruction, SymbolFill, LineSymbol) are
//!   written as `"<product>/<id>"` ([`namespaced_symbol`]) because the host
//!   namespaces overlay symbols; line style and area fill catalogue ids stay
//!   plain (the host rewrites the symbols inside them).
//! * Display plane names are matched case-insensitively to the catalogue's
//!   declared display planes and emitted with the declared spelling.
use crate::model::*;
use anyhow::{bail, Context, Result};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_core::{
    CurveRecord, CurveSegment, DatasetCodeMappings, DatasetIdentification, FeatureRecord,
    MultiPointRecord, OrientedCurve, PointRecord, RecordId, S101Cell, SegmentType,
    SpatialAssociation, SpatialPrimitiveType, SurfaceRecord, FRID,
};
use ferrite_xslt::{Builder, Document, Limits, NodeKind, NodeRef, Output, QName, Stylesheet};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Namespace prefix of the S-100 presentation (display list) schemas.
pub const S100_PRESENTATION: &str = "http://www.iho.int/S100Presentation";

// Record names of the synthetic cell (ISO 8211 RCNM values used by S-101).
const RCNM_POINT: u8 = 110;
const RCNM_MULTIPOINT: u8 = 115;
const RCNM_CURVE: u8 = 120;
const RCNM_SURFACE: u8 = 130;

// ─────────────────────────────────────────────────────────────────────────
// Conformance notes
// ─────────────────────────────────────────────────────────────────────────

/// A deviation from S-100 Part 9 found in the input, the catalogue or its
/// output, and how it was handled. Identical messages are merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The first feature (input `@id`) the message was recorded for.
    pub feature: Option<String>,
    pub message: String,
    pub count: usize,
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.feature {
            Some(id) => write!(f, "{id}: {}", self.message)?,
            None => f.write_str(&self.message)?,
        }
        if self.count > 1 {
            write!(f, " (x{})", self.count)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Notes {
    list: Vec<Note>,
    index: HashMap<String, usize>,
}
impl Notes {
    fn add(&mut self, feature: Option<&str>, message: impl Into<String>) {
        let message = message.into();
        if let Some(i) = self.index.get(&message) {
            self.list[*i].count += 1;
            return;
        }
        self.index.insert(message.clone(), self.list.len());
        self.list.push(Note {
            feature: feature.map(str::to_owned),
            message,
            count: 1,
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Portrayal input (Part 9 Appendix 9-A)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct InputOptions {
    /// Include curve and ring coordinates (Appendix 9-A-5 allows omitting
    /// them; point and multipoint coordinates are always included).
    pub curve_coordinates: bool,
    /// The product's virtual input features (S-129: `PlannedRoute` and the
    /// information boxes); None adds none.
    pub virtual_features: Option<VirtualFeatures>,
}
impl Default for InputOptions {
    fn default() -> Self {
        Self {
            curve_coordinates: true,
            virtual_features: None,
        }
    }
}

/// Adds a product's virtual input features: the dataset, its input features
/// and spatials so far, and a sink for conformance notes (feature `@id`,
/// message). The returned features are appended in order.
pub type VirtualFeatures = fn(
    &Dataset,
    &[InputFeature],
    &[InputSpatial],
    &mut dyn FnMut(Option<&str>, String),
) -> Vec<VirtualFeature>;

/// Geometry of a virtual input feature.
#[derive(Debug, Clone, PartialEq)]
pub enum VirtualGeometry {
    /// An existing spatial of the input (index into its spatials).
    Spatial(usize),
    /// A new curve through these positions.
    Curve(Vec<Coord>),
}

/// A feature of the portrayal input that is not in the dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualFeature {
    /// The input `@id` (conventionally `~<code>[.<dataset feature id>]`).
    pub id: String,
    pub code: String,
    /// Part 9 primitive of the feature.
    pub primitive: &'static str,
    pub geometry: VirtualGeometry,
    pub scale_minimum: Option<u32>,
    /// Simple child elements (thematic attributes) written into the input.
    pub children: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpatialKind {
    Point,
    MultiPoint,
    Curve,
    /// Rings are indices of `Curve` spatials; the first is the exterior.
    Surface {
        rings: Vec<usize>,
    },
}

/// A spatial object of the portrayal input.
#[derive(Debug, Clone, PartialEq)]
pub struct InputSpatial {
    /// The input XML `@id`.
    pub id: String,
    pub kind: SpatialKind,
    /// Positions (empty for surfaces).
    pub coords: Vec<Coord>,
}
impl InputSpatial {
    /// Record id in [`synthetic_cell`].
    pub fn record_id(&self, index: usize) -> RecordId {
        let rcnm = match self.kind {
            SpatialKind::Point => RCNM_POINT,
            SpatialKind::MultiPoint => RCNM_MULTIPOINT,
            SpatialKind::Curve => RCNM_CURVE,
            SpatialKind::Surface { .. } => RCNM_SURFACE,
        };
        RecordId::new(rcnm, index as u32 + 1)
    }
}

/// A feature of the portrayal input (dataset feature or virtual feature).
#[derive(Debug, Clone, PartialEq)]
pub struct InputFeature {
    /// The input XML `@id`; the dataset gml:id for dataset features. Display
    /// list `featureReference` values refer to it.
    pub id: String,
    pub code: String,
    /// Index into `Dataset::objects`, `None` for virtual features.
    pub object: Option<usize>,
    /// Part 9 primitive: None, Point, MultiPoint, Curve or Surface.
    pub primitive: &'static str,
    /// Indices into [`PortrayalInput::spatials`].
    pub spatials: Vec<usize>,
    /// Feature attribute `scaleMinimum`, if a positive integer.
    pub scale_minimum: Option<u32>,
}

/// Part 9 Appendix 9-A input with the bookkeeping needed to map the display
/// list back to features and geometry.
#[derive(Debug, Clone)]
pub struct PortrayalInput {
    pub document: Rc<Document>,
    pub features: Vec<InputFeature>,
    pub spatials: Vec<InputSpatial>,
    pub product: ProductId,
    pub dataset_name: String,
    pub source: Option<PathBuf>,
    pub notes: Vec<Note>,
    by_id: HashMap<String, usize>,
}

impl PortrayalInput {
    /// Index of the input feature with this `@id`.
    pub fn feature_index(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }
    /// The input as XML text (diagnostics, tests).
    pub fn to_xml(&self) -> String {
        ferrite_xslt::serialize(&NodeRef::root_of(Rc::clone(&self.document)))
    }
}

/// Build the Part 9 Appendix 9-A portrayal input of a dataset. The feature
/// catalogue, if given, provides association codes (Appendix 9-A-7: element
/// name = association code, `@role` = role); without it the role name is used
/// as the element name.
pub fn input_xml(
    dataset: &Dataset,
    fc: Option<&FeatureCatalogue>,
    options: &InputOptions,
) -> PortrayalInput {
    let mut notes = Notes::default();
    let mut spatials: Vec<InputSpatial> = Vec::new();
    let mut features: Vec<InputFeature> = Vec::new();
    let mut counters = [0usize; 4];
    let mut new_spatial =
        |spatials: &mut Vec<InputSpatial>, kind: SpatialKind, coords: Vec<Coord>| {
            let (slot, prefix) = match kind {
                SpatialKind::Point => (0, "P"),
                SpatialKind::MultiPoint => (1, "M"),
                SpatialKind::Curve => (2, "C"),
                SpatialKind::Surface { .. } => (3, "S"),
            };
            counters[slot] += 1;
            spatials.push(InputSpatial {
                id: format!("{prefix}{}", counters[slot]),
                kind,
                coords,
            });
            spatials.len() - 1
        };

    for (index, object) in dataset.features() {
        let mut refs = Vec::new();
        for geometry in &object.geometry {
            let spatial = match geometry {
                Geometry::Point(p) => new_spatial(&mut spatials, SpatialKind::Point, vec![*p]),
                Geometry::MultiPoint(p) => {
                    new_spatial(&mut spatials, SpatialKind::MultiPoint, p.clone())
                }
                Geometry::Curve(c) => new_spatial(&mut spatials, SpatialKind::Curve, c.clone()),
                Geometry::Surface {
                    exterior,
                    interiors,
                } => {
                    let mut rings = vec![new_spatial(
                        &mut spatials,
                        SpatialKind::Curve,
                        exterior.clone(),
                    )];
                    for ring in interiors {
                        rings.push(new_spatial(&mut spatials, SpatialKind::Curve, ring.clone()));
                    }
                    new_spatial(&mut spatials, SpatialKind::Surface { rings }, Vec::new())
                }
            };
            refs.push(spatial);
        }
        let primitive = object.primitive();
        if object.geometry.iter().any(|g| g.primitive() != primitive) {
            notes.add(
                Some(&object.id),
                format!(
                    "{} has geometry of mixed primitives; Part 9-A @primitive is {primitive}",
                    object.code
                ),
            );
        }
        features.push(InputFeature {
            id: object.id.clone(),
            code: object.code.clone(),
            object: Some(index),
            primitive,
            spatials: refs,
            scale_minimum: scale_minimum(object, &mut notes),
        });
    }

    // Product virtual features.
    let mut virtual_children: HashMap<usize, Vec<(&'static str, String)>> = HashMap::new();
    if let Some(add_virtual) = options.virtual_features {
        let extra = add_virtual(dataset, &features, &spatials, &mut |feature, message| {
            notes.add(feature, message)
        });
        for v in extra {
            let spatial = match v.geometry {
                VirtualGeometry::Spatial(index) => index,
                VirtualGeometry::Curve(coords) => {
                    new_spatial(&mut spatials, SpatialKind::Curve, coords)
                }
            };
            if !v.children.is_empty() {
                virtual_children.insert(features.len(), v.children);
            }
            features.push(InputFeature {
                id: v.id,
                code: v.code,
                object: None,
                primitive: v.primitive,
                spatials: vec![spatial],
                scale_minimum: v.scale_minimum,
            });
        }
    }

    // Appendix 9-A-1 structure.
    let mut builder = Builder::new();
    let el = |b: &mut Builder, name: &str| b.start_element(QName::local(name));
    let attr = |b: &mut Builder, name: &str, value: &str| {
        b.attribute(QName::local(name), value.to_owned());
    };
    let text_el = |b: &mut Builder, name: &str, value: &str| {
        b.start_element(QName::local(name));
        b.text(value);
        b.end_element();
    };
    let coord = |b: &mut Builder, name: &str, c: &Coord| {
        b.start_element(QName::local(name));
        text_el(b, "x", &c[0].to_string());
        text_el(b, "y", &c[1].to_string());
        b.end_element();
    };

    el(&mut builder, "Dataset");
    el(&mut builder, "InformationTypes");
    for (_, object) in dataset.information() {
        el(&mut builder, &object.code);
        attr(&mut builder, "id", &object.id);
        write_attributes(&mut builder, &object.attributes);
        write_associations(&mut builder, dataset, object, fc, &mut notes);
        builder.end_element();
    }
    builder.end_element();

    let section =
        |b: &mut Builder, name: &str, element: &str, want: &dyn Fn(&SpatialKind) -> bool| {
            el(b, name);
            for s in spatials.iter().filter(|s| want(&s.kind)) {
                el(b, element);
                attr(b, "id", &s.id);
                match &s.kind {
                    SpatialKind::Point => coord(b, "Coordinate2D", &s.coords[0]),
                    SpatialKind::MultiPoint => {
                        s.coords.iter().for_each(|c| coord(b, "Coordinate2D", c))
                    }
                    SpatialKind::Curve => {
                        el(b, "Segment");
                        attr(b, "interpolation", "Linear");
                        if options.curve_coordinates {
                            s.coords.iter().for_each(|c| coord(b, "ControlPoint", c));
                        }
                        b.end_element();
                    }
                    SpatialKind::Surface { rings } => {
                        for (i, ring) in rings.iter().enumerate() {
                            el(b, if i == 0 { "OuterRing" } else { "InnerRing" });
                            el(b, "Curve");
                            attr(b, "ref", &spatials[*ring].id);
                            attr(b, "orientation", "Forward");
                            b.end_element();
                            b.end_element();
                        }
                    }
                }
                b.end_element();
            }
            b.end_element();
        };
    section(&mut builder, "Points", "Point", &|k| {
        *k == SpatialKind::Point
    });
    section(&mut builder, "MultiPoints", "MultiPoint", &|k| {
        *k == SpatialKind::MultiPoint
    });
    section(&mut builder, "Curves", "Curve", &|k| {
        *k == SpatialKind::Curve
    });
    el(&mut builder, "CompositeCurves");
    builder.end_element();
    section(&mut builder, "Surfaces", "Surface", &|k| {
        matches!(k, SpatialKind::Surface { .. })
    });

    el(&mut builder, "Features");
    for (fi, feature) in features.iter().enumerate() {
        el(&mut builder, &feature.code);
        attr(&mut builder, "id", &feature.id);
        attr(&mut builder, "primitive", feature.primitive);
        for s in &feature.spatials {
            let spatial = &spatials[*s];
            match spatial.kind {
                SpatialKind::Point => el(&mut builder, "Point"),
                // Base schema SpatialRelations names the multipoint relation PointSet.
                SpatialKind::MultiPoint => el(&mut builder, "PointSet"),
                SpatialKind::Curve => el(&mut builder, "Curve"),
                SpatialKind::Surface { .. } => el(&mut builder, "Surface"),
            }
            attr(&mut builder, "ref", &spatial.id);
            if spatial.kind == SpatialKind::Curve {
                attr(&mut builder, "orientation", "Forward");
            }
            builder.end_element();
        }
        if let Some(object) = feature.object.map(|i| &dataset.objects[i]) {
            write_attributes(&mut builder, &object.attributes);
            write_associations(&mut builder, dataset, object, fc, &mut notes);
        }
        if let Some(children) = virtual_children.get(&fi) {
            for (name, value) in children {
                text_el(&mut builder, name, value);
            }
        }
        builder.end_element();
    }
    builder.end_element();
    builder.end_element();

    let by_id = features
        .iter()
        .enumerate()
        .map(|(i, f)| (f.id.clone(), i))
        .collect();
    PortrayalInput {
        document: builder.finish(),
        features,
        spatials,
        product: dataset.product,
        dataset_name: dataset.display_name(),
        source: dataset.source.clone(),
        notes: notes.list,
        by_id,
    }
}

fn scale_minimum(object: &Object, notes: &mut Notes) -> Option<u32> {
    let raw = object.value(&["scaleMinimum"])?.trim();
    match raw.parse::<u32>() {
        Ok(v) if v > 0 => {
            if v < 1000 {
                notes.add(
                    Some(&object.id),
                    format!("scaleMinimum {v} hides the feature at every display scale smaller than 1:{v}"),
                );
            }
            Some(v)
        }
        _ => {
            notes.add(
                Some(&object.id),
                format!("Ignored invalid scaleMinimum '{raw}'"),
            );
            None
        }
    }
}

fn write_attributes(builder: &mut Builder, attributes: &[Attribute]) {
    for a in attributes {
        match &a.value {
            // Unknown values carry no information for the rules.
            Value::Nil { .. } => {}
            Value::Simple { text, code } => {
                builder.start_element(QName::local(&a.code));
                // Appendix 9-A-2: enumerations are encoded by their code.
                builder.text(code.as_deref().unwrap_or(text).trim());
                builder.end_element();
            }
            Value::Complex(children) => {
                builder.start_element(QName::local(&a.code));
                write_attributes(builder, children);
                builder.end_element();
            }
        }
    }
}

fn write_associations(
    builder: &mut Builder,
    dataset: &Dataset,
    object: &Object,
    fc: Option<&FeatureCatalogue>,
    notes: &mut Notes,
) {
    for r in &object.references {
        let Some(target) = r.target.map(|t| &dataset.objects[t]) else {
            notes.add(
                Some(&object.id),
                format!("Unresolved association {} left out of the input", r.role),
            );
            continue;
        };
        let name = fc
            .and_then(|fc| association_code(fc, object, target, &r.role))
            .unwrap_or_else(|| {
                notes.add(
                    Some(&object.id),
                    format!(
                        "No catalogue association for {}.{}; element named by the role",
                        object.code, r.role
                    ),
                );
                r.role.clone()
            });
        builder.start_element(QName::local(&name));
        builder.attribute(QName::local("role"), r.role.clone());
        let kind = match target.kind {
            ObjectKind::Feature => "featureRef",
            ObjectKind::Information => "informationRef",
        };
        builder.attribute(QName::local(kind), target.id.clone());
        builder.end_element();
    }
}

fn association_code(
    fc: &FeatureCatalogue,
    object: &Object,
    target: &Object,
    role: &str,
) -> Option<String> {
    let mut code = Some(object.code.clone());
    let mut seen = HashSet::new();
    while let Some(c) = code.take() {
        if !seen.insert(c.clone()) {
            break;
        }
        let found = match object.kind {
            ObjectKind::Feature => {
                let ft = fc.get_feature_type(&c)?;
                code = ft.super_type.clone();
                match target.kind {
                    ObjectKind::Feature => ft
                        .feature_bindings
                        .iter()
                        .find(|b| b.role.as_deref() == Some(role))
                        .and_then(|b| b.association.clone()),
                    ObjectKind::Information => ft
                        .information_bindings
                        .iter()
                        .find(|b| b.role.as_deref() == Some(role))
                        .and_then(|b| b.association.clone()),
                }
            }
            ObjectKind::Information => {
                let it = fc.get_information_type(&c)?;
                code = it.super_type.clone();
                it.information_bindings
                    .iter()
                    .find(|b| b.role.as_deref() == Some(role))
                    .and_then(|b| b.association.clone())
            }
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────
// Catalogue and transformation
// ─────────────────────────────────────────────────────────────────────────

/// What the conversion needs from `portrayal_catalogue.xml`.
#[derive(Debug, Clone, Default)]
pub struct CatalogueInfo {
    /// Context parameter ids and default values (Part 9 clause 9-13.3.22).
    pub context: Vec<(String, String)>,
    /// Declared display plane ids.
    pub display_planes: Vec<String>,
    /// Declared viewing group ids.
    pub viewing_groups: HashSet<String>,
    /// Top level rule file name.
    pub main_rule: Option<String>,
}

impl CatalogueInfo {
    pub fn parse(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse_with_options(
            xml.trim_start_matches('\u{feff}'),
            roxmltree::ParsingOptions {
                allow_dtd: false,
                nodes_limit: 1_000_000,
            },
        )
        .context("Portrayal catalogue is not well-formed XML")?;
        let named = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().name() == name;
        let mut info = CatalogueInfo::default();
        let root = doc.root_element();
        for section in root.children().filter(|n| n.is_element()) {
            match section.tag_name().name() {
                "context" => {
                    for p in section
                        .children()
                        .filter(|n| named(n, "parameter") || named(n, "contextParameter"))
                    {
                        let Some(id) = p.attribute("id") else {
                            continue;
                        };
                        let default = p
                            .children()
                            .find(|n| named(n, "default"))
                            .and_then(|n| n.text())
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        info.context.push((id.to_owned(), default));
                    }
                }
                "displayPlanes" => {
                    for p in section.children().filter(|n| named(n, "displayPlane")) {
                        if let Some(id) = p.attribute("id") {
                            info.display_planes.push(id.to_owned());
                        }
                    }
                }
                "viewingGroups" => {
                    for g in section.children().filter(|n| named(n, "viewingGroup")) {
                        if let Some(id) = g.attribute("id") {
                            info.viewing_groups.insert(id.trim().to_owned());
                        }
                    }
                }
                "rules" => {
                    for r in section.children().filter(|n| named(n, "ruleFile")) {
                        let child = |name: &str| {
                            r.children()
                                .find(|n| named(n, name))
                                .and_then(|n| n.text())
                                .map(|t| t.trim().to_owned())
                        };
                        if child("ruleType").as_deref() == Some("TopLevelTemplate")
                            && child("fileFormat").is_none_or(|f| f.eq_ignore_ascii_case("XSLT"))
                        {
                            info.main_rule = child("fileName");
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(info)
    }

    /// The declared display plane matching `value` case-insensitively.
    fn display_plane(&self, value: &str) -> Option<&str> {
        self.display_planes
            .iter()
            .find(|p| p.eq_ignore_ascii_case(value))
            .map(String::as_str)
    }
}

/// A loaded XSLT portrayal catalogue (Part 9).
pub struct XsltPortrayal {
    pub info: CatalogueInfo,
    pub directory: PathBuf,
    sheet: Stylesheet,
}

impl XsltPortrayal {
    /// Load `portrayal_catalogue.xml` and its top level rule from `Rules/`.
    pub fn load(pc_dir: &Path) -> Result<Self> {
        Self::load_with_limits(pc_dir, Limits::default())
    }
    pub fn load_with_limits(pc_dir: &Path, limits: Limits) -> Result<Self> {
        Self::load_from(&ferrite_xslt::NativeModules, pc_dir, limits)
    }

    /// Load from a module source instead of the file system (the same files,
    /// read through `source`).
    pub fn load_from(
        source: &dyn ferrite_xslt::ModuleSource,
        pc_dir: &Path,
        limits: Limits,
    ) -> Result<Self> {
        let pc = pc_dir.join("portrayal_catalogue.xml");
        let info = match source.read(&pc) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                    .with_context(|| pc.display().to_string())?;
                CatalogueInfo::parse(&text).with_context(|| pc.display().to_string())?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CatalogueInfo::default(),
            Err(e) => return Err(e).with_context(|| pc.display().to_string()),
        };
        let rules = pc_dir.join("Rules");
        let main = info.main_rule.clone().unwrap_or_else(|| "main.xsl".into());
        let path = rules.join(&main);
        if !source.is_file(&path) {
            bail!("Top level portrayal rule {} not found", path.display());
        }
        let sheet = Stylesheet::load_from(source, &path, &rules, limits)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(Self {
            info,
            directory: pc_dir.to_path_buf(),
            sheet,
        })
    }

    /// Context parameters: catalogue defaults overridden by `overrides`.
    pub fn parameters(&self, overrides: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut params: Vec<(String, String)> = self.info.context.clone();
        for (k, v) in overrides {
            match params.iter_mut().find(|(id, _)| id == k) {
                Some(p) => p.1 = (*v).to_owned(),
                None => params.push(((*k).to_owned(), (*v).to_owned())),
            }
        }
        params
    }

    /// Run the rules on the input with the context parameters.
    pub fn transform(&self, input: &PortrayalInput, overrides: &[(&str, &str)]) -> Result<Output> {
        let params = self.parameters(overrides);
        let params: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.sheet
            .transform(Rc::clone(&input.document), &params)
            .map_err(|e| anyhow::anyhow!("XSLT portrayal failed: {e}"))
    }

    /// Transform and convert to Part 9a instruction strings.
    pub fn portray(
        &self,
        input: &PortrayalInput,
        overrides: &[(&str, &str)],
        options: &ConvertOptions,
    ) -> Result<Portrayal> {
        let output = self.transform(input, overrides)?;
        let mut portrayal =
            display_list_instructions(&output.root, input, Some(&self.info), options);
        portrayal.messages = output.messages;
        Ok(portrayal)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Display list → Part 9a
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// Apply a feature's `scaleMinimum` attribute as `ScaleMinimum` when the
    /// instruction has none.
    pub apply_feature_scale_minimum: bool,
    /// Render text whose foreground is fully transparent (transparency 1) as
    /// opaque, treating the value as opacity. Off by default: Part 9 defines
    /// 1 as fully transparent.
    pub opaque_invisible_text: bool,
    /// Viewing groups the catalogue's rules use but do not declare, treated
    /// as declared (host errata correction). Empty by default: an instruction
    /// in an undeclared group can never be enabled, so by Part 9 clause
    /// 9-11.2.2 it is not drawn.
    pub implicit_viewing_groups: HashSet<String>,
}
impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            apply_feature_scale_minimum: true,
            opaque_invisible_text: false,
            implicit_viewing_groups: HashSet::new(),
        }
    }
}

/// Part 9a drawing instructions of one input feature.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureInstructions {
    /// Index into [`PortrayalInput::features`] (also the `key_of` argument of
    /// [`synthetic_cell`]).
    pub feature: usize,
    /// The input `@id` (display list `featureReference`).
    pub feature_id: String,
    pub code: String,
    /// Semicolon separated Part 9a commands.
    pub instructions: String,
}
impl FeatureInstructions {
    /// The id the host expects: `"<code>|<key>"` with the key given to
    /// [`synthetic_cell`] for this feature.
    pub fn host_feature_id(&self, key: i64) -> String {
        format!("{}|{key}", self.code)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Portrayal {
    /// Features in order of first appearance in the display list.
    pub features: Vec<FeatureInstructions>,
    /// Conformance notes from input building and conversion.
    pub notes: Vec<Note>,
    /// `xsl:message` output.
    pub messages: Vec<String>,
}

/// Convert a Part 9 display list (result tree root or `displayList` element,
/// namespaced or not) to Part 9a strings per feature.
pub fn display_list_instructions(
    root: &NodeRef,
    input: &PortrayalInput,
    catalogue: Option<&CatalogueInfo>,
    options: &ConvertOptions,
) -> Portrayal {
    let mut notes = Notes::default();
    for n in &input.notes {
        for _ in 0..n.count {
            notes.add(n.feature.as_deref(), n.message.clone());
        }
    }
    let list = if root.kind() == &NodeKind::Element {
        Some(root.clone())
    } else {
        elements(root).next()
    };
    let mut converter = Converter {
        input,
        catalogue,
        options,
        notes,
        order: Vec::new(),
        states: HashMap::new(),
    };
    match list {
        None => converter
            .notes
            .add(None, "Empty portrayal output: no displayList"),
        Some(list) => {
            if local(&list) != "displayList" {
                converter.notes.add(
                    None,
                    format!("Output root is {}, not displayList", local(&list)),
                );
            } else if let Some(ns) = list.name().and_then(|n| n.namespace.clone()) {
                if !ns.starts_with(S100_PRESENTATION) {
                    converter
                        .notes
                        .add(None, format!("displayList in unexpected namespace {ns}"));
                }
            }
            for child in list.children() {
                match child.kind() {
                    NodeKind::Element => converter.instruction(&child),
                    NodeKind::Text if !child.node().value.trim().is_empty() => converter.notes.add(
                        None,
                        "Text content in displayList (unmatched features handled by built-in XSLT rules) ignored",
                    ),
                    _ => {}
                }
            }
        }
    }
    let Converter {
        order,
        states,
        notes,
        ..
    } = converter;
    let mut states = states;
    let features = order
        .into_iter()
        .map(|fi| {
            let state = states.remove(&fi).expect("state for ordered feature");
            let f = &input.features[fi];
            FeatureInstructions {
                feature: fi,
                feature_id: f.id.clone(),
                code: f.code.clone(),
                instructions: state.commands.join(";"),
            }
        })
        .collect();
    Portrayal {
        features,
        notes: notes.list,
        messages: Vec::new(),
    }
}

fn local(node: &NodeRef) -> &str {
    node.name().map(|n| &*n.local).unwrap_or("")
}
fn elements(node: &NodeRef) -> impl Iterator<Item = NodeRef> + '_ {
    node.children().filter(|c| c.kind() == &NodeKind::Element)
}
fn child(node: &NodeRef, name: &str) -> Option<NodeRef> {
    elements(node).find(|c| local(c) == name)
}
fn children_named<'a>(node: &'a NodeRef, name: &'a str) -> impl Iterator<Item = NodeRef> + 'a {
    elements(node).filter(move |c| local(c) == name)
}
fn text(node: &NodeRef) -> String {
    node.string_value().trim().to_owned()
}
/// A property encoded as attribute (schema) or child element (PC deviation).
/// `Some("")` when present but empty.
fn property(node: &NodeRef, name: &str) -> Option<String> {
    node.attribute(name)
        .map(|v| v.trim().to_owned())
        .or_else(|| child(node, name).map(|c| text(&c)))
}
fn number(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}
fn fmt(value: f64) -> String {
    let v = if value == 0.0 { 0.0 } else { value };
    format!("{v}")
}
/// Part 9a escapes for free text parameters.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&a"),
            ';' => out.push_str("&s"),
            ':' => out.push_str("&c"),
            ',' => out.push_str("&m"),
            _ => out.push(c),
        }
    }
    out
}
/// `"<product>/<id>"` for a symbol reference, without double-prefixing.
pub fn namespaced_symbol(product: ProductId, reference: &str) -> String {
    let prefix = product.as_str();
    match reference.strip_prefix(prefix) {
        Some(rest) if rest.starts_with('/') => reference.to_owned(),
        _ => format!("{prefix}/{reference}"),
    }
}

fn crs_name(value: &str) -> Option<&'static str> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "geographiccrs" => "GeographicCRS",
        "portrayalcrs" => "PortrayalCRS",
        "localcrs" => "LocalCRS",
        "linecrs" => "LineCRS",
        _ => return None,
    })
}
fn enum_value(value: &str, allowed: &[&'static str]) -> Option<&'static str> {
    allowed
        .iter()
        .copied()
        .find(|a| a.eq_ignore_ascii_case(value.trim()))
}

/// Part 9a state per feature string (state persists between commands of one
/// feature, clause 9a-11.2.2).
#[derive(Clone, Default)]
struct Tracker {
    values: HashMap<&'static str, String>,
}
impl Tracker {
    fn initial(cmd: &str) -> &'static str {
        match cmd {
            "DrawingPriority" | "TextVerticalOffset" => "0",
            "LocalOffset" => "0,0",
            "LinePlacement" => "Relative,0.5",
            "AreaPlacement" => "VisibleParts",
            "AreaCRS" => "GlobalGeometry",
            "Rotation" => "PortrayalCRS,0",
            "ScaleFactor" => "1",
            "FontSize" => "10",
            "FontProportion" => "Proportional",
            "FontWeight" => "Medium",
            "FontSlant" => "Upright",
            "FontSerifs" | "FontUnderline" | "FontStrikethrough" | "FontUpperline" | "Hover" => {
                "false"
            }
            "TextAlignHorizontal" => "Start",
            "TextAlignVertical" => "Bottom",
            _ => "",
        }
    }
    fn current(&self, cmd: &'static str) -> &str {
        self.values
            .get(cmd)
            .map(String::as_str)
            .unwrap_or_else(|| Self::initial(cmd))
    }
    /// Emit `cmd:value` when it changes the state.
    fn set(&mut self, out: &mut Vec<String>, cmd: &'static str, value: String) {
        if self.current(cmd) != value {
            out.push(if value.is_empty() {
                cmd.to_owned()
            } else {
                format!("{cmd}:{value}")
            });
            self.values.insert(cmd, value);
        }
    }
    /// Optional state that cannot be cleared in Part 9a: `reset` is the
    /// equivalent of "not given" (ScaleMinimum max integer, ScaleMaximum 0).
    fn set_optional(
        &mut self,
        out: &mut Vec<String>,
        cmd: &'static str,
        value: Option<u32>,
        reset: &str,
    ) {
        let wanted = value.map(|v| v.to_string()).unwrap_or_default();
        if self.current(cmd) != wanted {
            out.push(format!(
                "{cmd}:{}",
                if wanted.is_empty() { reset } else { &wanted }
            ));
            self.values.insert(cmd, wanted);
        }
    }
}

#[derive(Default)]
struct FeatureState {
    tracker: Tracker,
    commands: Vec<String>,
    line_styles: usize,
}

struct Converter<'a> {
    input: &'a PortrayalInput,
    catalogue: Option<&'a CatalogueInfo>,
    options: &'a ConvertOptions,
    notes: Notes,
    order: Vec<usize>,
    states: HashMap<usize, FeatureState>,
}

/// A non-fatal reason to drop one instruction.
type Drop = String;

const COMMON: &[&str] = &[
    "featureReference",
    "spatialReference",
    "viewingGroup",
    "displayPlane",
    "drawingPriority",
    "scaleMinimum",
    "scaleMaximum",
    "id",
    "parentId",
    "hover",
    "timeValid",
    "alertReference",
];

impl Converter<'_> {
    fn instruction(&mut self, node: &NodeRef) {
        let kind = local(node).to_owned();
        let reference = child(node, "featureReference")
            .map(|n| text(&n))
            .or_else(|| node.attribute("featureReference"))
            .unwrap_or_default();
        let Some(feature) = self.input.feature_index(&reference) else {
            self.notes.add(
                None,
                format!("{kind} with unknown featureReference '{reference}' ignored"),
            );
            return;
        };
        let fid = self.input.features[feature].id.clone();
        let mut state_slot = self.states.remove(&feature).unwrap_or_default();
        let mut tracker = state_slot.tracker.clone();
        let mut out = Vec::new();
        let mut line_styles = state_slot.line_styles;
        let result = self.convert(
            node,
            &kind,
            feature,
            &mut tracker,
            &mut out,
            &mut line_styles,
        );
        match result {
            Ok(()) => {
                state_slot.tracker = tracker;
                state_slot.commands.extend(out);
                state_slot.line_styles = line_styles;
                if !self.order.contains(&feature) {
                    self.order.push(feature);
                }
            }
            Err(reason) => self
                .notes
                .add(Some(&fid), format!("{kind} dropped: {reason}")),
        }
        if self.order.contains(&feature) || !state_slot.commands.is_empty() {
            self.states.insert(feature, state_slot);
        }
    }

    fn note(&mut self, feature: usize, message: impl Into<String>) {
        let id = self.input.features[feature].id.clone();
        self.notes.add(Some(&id), message);
    }

    fn unknown_children(&mut self, feature: usize, node: &NodeRef, known: &[&str]) {
        for c in elements(node) {
            let name = local(&c);
            if !known.contains(&name) {
                let message = format!("Non-schema element {name} in {} ignored", local(node));
                self.note(feature, message);
            }
        }
    }

    fn convert(
        &mut self,
        node: &NodeRef,
        kind: &str,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
        line_styles: &mut usize,
    ) -> Result<(), Drop> {
        self.visibility(node, feature, tracker, out)?;
        match kind {
            "nullInstruction" => {
                self.unknown_children(feature, node, COMMON);
                out.push("NullInstruction".into());
            }
            "pointInstruction" => {
                let mut known = COMMON.to_vec();
                known.push("symbol");
                self.unknown_children(feature, node, &known);
                let symbol = child(node, "symbol").ok_or("no symbol")?;
                let reference = self.symbol_state(&symbol, feature, tracker, out, true)?;
                out.push(format!("PointInstruction:{reference}"));
            }
            "lineInstruction" => {
                let mut known = COMMON.to_vec();
                known.extend(["lineStyle", "lineStyleReference", "compositeLineStyle"]);
                self.unknown_children(feature, node, &known);
                let styles = self.line_styles(node, feature, out, line_styles)?;
                if styles.is_empty() {
                    return Err("no line style".into());
                }
                let unsuppressed =
                    property(node, "suppression").is_some_and(|s| s == "false" || s == "0");
                let cmd = if unsuppressed {
                    "LineInstructionUnsuppressed"
                } else {
                    "LineInstruction"
                };
                out.push(format!("{cmd}:{}", styles.join(",")));
            }
            "areaInstruction" => {
                let mut known = COMMON.to_vec();
                known.extend([
                    "colorFill",
                    "areaFillReference",
                    "pixmapFill",
                    "symbolFill",
                    "hatchFill",
                ]);
                self.unknown_children(feature, node, &known);
                self.area_fill(node, feature, tracker, out, line_styles)?;
            }
            "textInstruction" => {
                let mut known = COMMON.to_vec();
                known.extend(["textPoint", "textLine"]);
                self.unknown_children(feature, node, &known);
                self.text(node, feature, tracker, out)?;
            }
            "augmentedPoint" | "augmentedRay" | "augmentedPath" | "augmentedArea" => {
                self.augmented(node, kind, feature, tracker, out, line_styles)?;
            }
            "coverageInstruction" => {
                return Err("coverage instructions are not supported for GML datasets".into())
            }
            other => return Err(format!("unknown instruction element {other}")),
        }
        Ok(())
    }

    /// DrawingInstruction attributes (Part 9 clause 9-11.2.2).
    fn visibility(
        &mut self,
        node: &NodeRef,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
    ) -> Result<(), Drop> {
        let mut groups = Vec::new();
        for g in children_named(node, "viewingGroup") {
            let value = text(&g);
            if value.is_empty() {
                self.note(feature, "Empty viewingGroup ignored");
            } else {
                groups.push(value);
            }
        }
        if groups.is_empty() {
            self.note(
                feature,
                "Instruction without viewingGroup (Part 9 requires 1..*)",
            );
        }
        if let Some(catalogue) = self.catalogue {
            if let Some(g) = groups.iter().find(|g| {
                !catalogue.viewing_groups.contains(*g)
                    && !self.options.implicit_viewing_groups.contains(*g)
            }) {
                return Err(format!(
                    "viewing group {g} is not declared by the catalogue"
                ));
            }
            if let Some(g) = groups
                .iter()
                .find(|g| !catalogue.viewing_groups.contains(*g))
            {
                self.note(
                    feature,
                    format!("Undeclared viewing group {g} treated as declared (ConvertOptions::implicit_viewing_groups)"),
                );
            }
        }
        if groups.iter().any(|g| g.contains([';', ':', ','])) {
            return Err("viewing group with reserved characters".into());
        }
        tracker.set(out, "ViewingGroup", groups.join(","));

        match child(node, "displayPlane")
            .map(|n| text(&n))
            .filter(|p| !p.is_empty())
        {
            Some(plane) => {
                let resolved = match self.catalogue.and_then(|c| c.display_plane(&plane)) {
                    Some(declared) => {
                        if declared != plane {
                            self.note(
                                feature,
                                format!("displayPlane {plane} matched to declared {declared}"),
                            );
                        }
                        declared.to_owned()
                    }
                    None => {
                        let canonical = enum_value(&plane, &["UnderRadar", "OverRadar"]);
                        if self.catalogue.is_some() {
                            self.note(
                                feature,
                                format!("displayPlane {plane} is not declared by the catalogue"),
                            );
                        }
                        canonical.map(str::to_owned).unwrap_or(plane)
                    }
                };
                tracker.set(out, "DisplayPlane", escape(&resolved));
            }
            None => {
                self.note(feature, "Instruction without displayPlane");
                if !tracker.current("DisplayPlane").is_empty() {
                    self.note(
                        feature,
                        "Part 9a cannot clear DisplayPlane; previous plane kept",
                    );
                }
            }
        }

        let priority = match child(node, "drawingPriority").map(|n| text(&n)) {
            Some(p) => match number(&p) {
                Some(v) if v.fract() == 0.0 && v.abs() <= i32::MAX as f64 => v as i32,
                _ => return Err(format!("invalid drawingPriority '{p}'")),
            },
            None => {
                self.note(feature, "Instruction without drawingPriority; 0 used");
                0
            }
        };
        tracker.set(out, "DrawingPriority", priority.to_string());

        let scale = |this: &mut Self, name: &str| -> Option<u32> {
            let raw = child(node, name).map(|n| text(&n))?;
            match raw.parse::<u32>() {
                Ok(v) if v > 0 => Some(v),
                _ => {
                    if !raw.is_empty() {
                        this.note(feature, format!("Invalid {name} '{raw}' ignored"));
                    }
                    None
                }
            }
        };
        let mut minimum = scale(self, "scaleMinimum");
        if minimum.is_none() && self.options.apply_feature_scale_minimum {
            minimum = self.input.features[feature].scale_minimum;
        }
        let maximum = scale(self, "scaleMaximum");
        tracker.set_optional(out, "ScaleMinimum", minimum, &u32::MAX.to_string());
        tracker.set_optional(out, "ScaleMaximum", maximum, "0");

        let id = property(node, "id").unwrap_or_default();
        tracker.set(out, "Id", escape(&id));
        let parent = property(node, "parentId").unwrap_or_default();
        tracker.set(out, "Parent", escape(&parent));
        let hover = property(node, "hover").is_some_and(|h| h == "true" || h == "1");
        tracker.set(out, "Hover", hover.to_string());

        if child(node, "timeValid").is_some() {
            self.note(feature, "timeValid is not converted");
        }
        if let Some(alert) = child(node, "alertReference") {
            let reference = alert.attribute("reference").unwrap_or_else(|| text(&alert));
            if !reference.is_empty() {
                out.push(format!("AlertReference:{}", escape(&reference)));
            }
        }
        for s in children_named(node, "spatialReference") {
            let reference = s.attribute("reference").unwrap_or_else(|| text(&s));
            let forward = s
                .attribute("forward")
                .is_none_or(|f| f != "false" && f != "0");
            let spatial = self
                .input
                .spatials
                .iter()
                .enumerate()
                .find(|(_, sp)| sp.id == reference);
            match spatial {
                Some((i, sp))
                    if matches!(sp.kind, SpatialKind::Curve | SpatialKind::Surface { .. }) =>
                {
                    let kind = if sp.kind == SpatialKind::Curve {
                        "Curve"
                    } else {
                        "Surface"
                    };
                    let rcid = sp.record_id(i).rcid;
                    out.push(format!("SpatialReference:{kind}|{rcid},{forward}"));
                }
                _ => self.note(
                    feature,
                    format!("spatialReference '{reference}' not supported; feature geometry used"),
                ),
            }
        }
        Ok(())
    }

    fn color(&mut self, feature: usize, node: &NodeRef, what: &str) -> Result<(String, f64), Drop> {
        // Schema: <color transparency="t">TOKEN</color>. PC deviation:
        // <color><token>T</token><transparency|trasparency>t</...></color>.
        let (token, transparency) = match child(node, "token") {
            Some(token) => {
                let t = child(node, "transparency").or_else(|| {
                    let misspelt = child(node, "trasparency");
                    if misspelt.is_some() {
                        self.note(
                            feature,
                            format!("Misspelt <trasparency> in {what} read as transparency"),
                        );
                    }
                    misspelt
                });
                self.note(
                    feature,
                    format!("{what} encoded with token/transparency child elements"),
                );
                (text(&token), t.map(|t| text(&t)))
            }
            None => (
                text(node),
                node.attribute("transparency").map(|t| t.trim().to_owned()),
            ),
        };
        if token.is_empty() || token.contains([';', ':', ',']) {
            return Err(format!("{what} without a valid colour token"));
        }
        let transparency = match transparency.filter(|t| !t.is_empty()) {
            None => 0.0,
            Some(t) => match number(&t) {
                Some(v) if (0.0..=1.0).contains(&v) => v,
                Some(v) => {
                    self.note(
                        feature,
                        format!("{what} transparency {t} clamped to [0, 1]"),
                    );
                    v.clamp(0.0, 1.0)
                }
                None => {
                    self.note(
                        feature,
                        format!("{what} transparency '{t}' invalid; 0 used"),
                    );
                    0.0
                }
            },
        };
        Ok((token, transparency))
    }

    /// Symbol transform state and colour overrides; returns the reference.
    fn symbol_state(
        &mut self,
        symbol: &NodeRef,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
        placement: bool,
    ) -> Result<String, Drop> {
        self.unknown_children(
            feature,
            symbol,
            &[
                "offset",
                "overrideAll",
                "override",
                "linePlacement",
                "areaPlacement",
                "rotation",
                "rotationCRS",
                "scaleFactor",
            ],
        );
        let reference = symbol
            .attribute("reference")
            .map(|r| r.trim().to_owned())
            .unwrap_or_default();
        if reference.is_empty() || reference.contains([';', ':', ',', '&']) {
            return Err(format!("invalid symbol reference '{reference}'"));
        }
        let rotation = self.number_property(feature, symbol, "rotation", 0.0);
        let crs = match property(symbol, "rotationCRS").filter(|c| !c.is_empty()) {
            None => {
                if property(symbol, "rotationCRS").is_some() {
                    self.note(feature, "Empty rotationCRS; PortrayalCRS used");
                }
                "PortrayalCRS"
            }
            Some(c) => match crs_name(&c) {
                Some(n) => n,
                None => {
                    self.note(
                        feature,
                        format!("Unknown rotationCRS '{c}'; PortrayalCRS used"),
                    );
                    "PortrayalCRS"
                }
            },
        };
        tracker.set(out, "Rotation", format!("{crs},{}", fmt(rotation)));
        let scale = self.number_property(feature, symbol, "scaleFactor", 1.0);
        tracker.set(out, "ScaleFactor", fmt(scale));
        let (x, y) = child(symbol, "offset")
            .map(|o| self.vector(feature, &o))
            .unwrap_or((0.0, 0.0));
        tracker.set(out, "LocalOffset", format!("{},{}", fmt(x), fmt(y)));
        if placement {
            let line = child(symbol, "linePlacement")
                .map(|p| self.line_placement(feature, &p))
                .unwrap_or_else(|| "Relative,0.5".into());
            tracker.set(out, "LinePlacement", line);
            let area = child(symbol, "areaPlacement")
                .map(|p| self.area_placement(feature, &p))
                .unwrap_or("VisibleParts");
            tracker.set(out, "AreaPlacement", area.into());
        }
        self.overrides(symbol, feature, out)?;
        Ok(self.symbol_id(&reference))
    }

    /// Host overlay symbols are namespaced by product: `"<product>/<id>"`
    /// (idempotent; '/' is not a Part 9a special character).
    fn symbol_id(&self, reference: &str) -> String {
        namespaced_symbol(self.input.product, reference)
    }

    fn overrides(
        &mut self,
        node: &NodeRef,
        feature: usize,
        out: &mut Vec<String>,
    ) -> Result<(), Drop> {
        if let Some(all) = child(node, "overrideAll") {
            let (token, t) = self.color(feature, &all, "overrideAll")?;
            out.push(format!("OverrideAll:{token},{}", fmt(t)));
        }
        for o in children_named(node, "override") {
            // Part 9 9-12.3.1.2: the association end (override) is the colour
            // replaced, the role color is the replacement.
            let (Some(from), Some(to)) = (child(&o, "override"), child(&o, "color")) else {
                self.note(feature, "Incomplete override colour ignored");
                continue;
            };
            let (a, at) = self.color(feature, &from, "override")?;
            let (b, bt) = self.color(feature, &to, "override color")?;
            out.push(format!("OverrideColor:{a},{},{b},{}", fmt(at), fmt(bt)));
        }
        Ok(())
    }

    fn number_property(&mut self, feature: usize, node: &NodeRef, name: &str, default: f64) -> f64 {
        match property(node, name) {
            None => default,
            Some(v) if v.is_empty() => {
                self.note(
                    feature,
                    format!("Empty {name} in {}; default {default} used", local(node)),
                );
                default
            }
            Some(v) => number(&v).unwrap_or_else(|| {
                self.note(
                    feature,
                    format!("Invalid {name} '{v}'; default {default} used"),
                );
                default
            }),
        }
    }

    fn vector(&mut self, feature: usize, node: &NodeRef) -> (f64, f64) {
        // PC deviation: direction/vector/x.
        let node = child(node, "vector").unwrap_or_else(|| node.clone());
        (
            self.number_property(feature, &node, "x", 0.0),
            self.number_property(feature, &node, "y", 0.0),
        )
    }

    fn line_placement(&mut self, feature: usize, node: &NodeRef) -> String {
        let mode = property(node, "placementMode")
            .and_then(|m| enum_value(&m, &["Relative", "Absolute"]))
            .unwrap_or("Relative");
        let offset = self.number_property(feature, node, "offset", 0.5);
        let visible = property(node, "visibleParts").is_some_and(|v| v == "true" || v == "1");
        if visible {
            format!("{mode},{},,true", fmt(offset))
        } else {
            format!("{mode},{}", fmt(offset))
        }
    }

    fn area_placement(&mut self, feature: usize, node: &NodeRef) -> &'static str {
        match property(node, "placementMode").filter(|m| !m.is_empty()) {
            None => "VisibleParts",
            Some(m) => enum_value(&m, &["VisibleParts", "Geographic"]).unwrap_or_else(|| {
                self.note(
                    feature,
                    format!("Unknown areaPlacement mode '{m}'; VisibleParts used"),
                );
                "VisibleParts"
            }),
        }
    }

    /// Line styles of a LineStyleGroup; inline styles are defined with
    /// Dash/LineSymbol/LineStyle commands. Returns the names to reference.
    fn line_styles(
        &mut self,
        node: &NodeRef,
        feature: usize,
        out: &mut Vec<String>,
        counter: &mut usize,
    ) -> Result<Vec<String>, Drop> {
        let mut names = Vec::new();
        for c in elements(node) {
            match local(&c) {
                "lineStyleReference" => {
                    let r = c.attribute("reference").unwrap_or_else(|| text(&c));
                    if r.is_empty() || r.contains([';', ':', ',', '&']) {
                        return Err(format!("invalid lineStyleReference '{r}'"));
                    }
                    names.push(r.trim().to_owned());
                }
                "lineStyle" => names.push(self.line_style(&c, feature, out, counter)?),
                "compositeLineStyle" => names.extend(self.line_styles(&c, feature, out, counter)?),
                _ => {}
            }
        }
        Ok(names)
    }

    fn line_style(
        &mut self,
        node: &NodeRef,
        feature: usize,
        out: &mut Vec<String>,
        counter: &mut usize,
    ) -> Result<String, Drop> {
        self.unknown_children(
            feature,
            node,
            &[
                "intervalLength",
                "pen",
                "dash",
                "symbol",
                "capStyle",
                "joinStyle",
                "offset",
            ],
        );
        let pen = child(node, "pen").ok_or("lineStyle without pen")?;
        let width = pen
            .attribute("width")
            .or_else(|| child(&pen, "width").map(|w| text(&w)))
            .and_then(|w| number(&w))
            .filter(|w| *w >= 0.0)
            .ok_or("lineStyle pen without a valid width")?;
        let color = child(&pen, "color").ok_or("lineStyle pen without color")?;
        let (token, transparency) = self.color(feature, &color, "pen color")?;
        let mut dashes = Vec::new();
        for d in children_named(node, "dash") {
            let start = self.number_property(feature, &d, "start", 0.0);
            let length = self.number_property(feature, &d, "length", 0.0);
            dashes.push((start, length));
        }
        let mut interval = match property(node, "intervalLength") {
            Some(v) if !v.is_empty() => Some(
                number(&v)
                    .filter(|v| *v >= 0.0)
                    .ok_or_else(|| format!("invalid intervalLength '{v}'"))?,
            ),
            Some(_) => {
                self.note(feature, "Empty intervalLength (solid line) ignored");
                None
            }
            None => None,
        };
        if interval.is_none() && !dashes.is_empty() {
            let end = dashes.iter().map(|(s, l)| s + l).fold(0.0, f64::max);
            self.note(
                feature,
                "Dashed lineStyle without intervalLength; dash extent used",
            );
            interval = Some(end);
        }
        let mut symbols = Vec::new();
        for s in children_named(node, "symbol") {
            let reference = s.attribute("reference").unwrap_or_default();
            if reference.is_empty() || reference.contains([';', ':', ',', '&']) {
                return Err(format!("invalid line symbol reference '{reference}'"));
            }
            let position = self.number_property(feature, &s, "position", 0.0);
            let rotation = self.number_property(feature, &s, "rotation", 0.0);
            let scale = self.number_property(feature, &s, "scaleFactor", 1.0);
            let crs = property(&s, "crsType")
                .and_then(|c| crs_name(&c))
                .filter(|c| *c != "GeographicCRS")
                .unwrap_or("LocalCRS");
            let reference = self.symbol_id(&reference);
            symbols.push(format!(
                "LineSymbol:{reference},{},{},{crs},{}",
                fmt(position),
                fmt(rotation),
                fmt(scale)
            ));
        }
        let cap = match property(node, "capStyle").filter(|c| !c.is_empty()) {
            None => "Butt",
            Some(c) => enum_value(&c, &["Butt", "Square", "Round"])
                .ok_or(format!("invalid capStyle '{c}'"))?,
        };
        let join = match property(node, "joinStyle").filter(|c| !c.is_empty()) {
            None => "Miter",
            Some(c) => enum_value(&c, &["Miter", "Bevel", "Round"])
                .ok_or(format!("invalid joinStyle '{c}'"))?,
        };
        let offset = self.number_property(feature, node, "offset", 0.0);
        *counter += 1;
        let name = format!("_xslt{counter}");
        for (start, length) in dashes {
            out.push(format!("Dash:{},{}", fmt(start), fmt(length)));
        }
        out.extend(symbols);
        out.push(format!(
            "LineStyle:{name},{},{},{token},{},{cap},{join},{}",
            interval.map(fmt).unwrap_or_default(),
            fmt(width),
            fmt(transparency),
            fmt(offset)
        ));
        Ok(name)
    }

    fn area_fill(
        &mut self,
        node: &NodeRef,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
        counter: &mut usize,
    ) -> Result<(), Drop> {
        let fill = elements(node)
            .find(|c| {
                matches!(
                    local(c),
                    "colorFill" | "areaFillReference" | "pixmapFill" | "symbolFill" | "hatchFill"
                )
            })
            .ok_or("no area fill")?;
        let area_crs = |this: &mut Self, tracker: &mut Tracker, out: &mut Vec<String>| {
            let crs = match property(&fill, "areaCRS").filter(|c| !c.is_empty()) {
                None => "GlobalGeometry",
                Some(c) => enum_value(&c, &["Global", "LocalGeometry", "GlobalGeometry"])
                    .unwrap_or_else(|| {
                        this.note(
                            feature,
                            format!("Unknown areaCRS '{c}'; GlobalGeometry used"),
                        );
                        "GlobalGeometry"
                    }),
            };
            tracker.set(out, "AreaCRS", crs.into());
        };
        match local(&fill) {
            "colorFill" => {
                let color = child(&fill, "color").ok_or("colorFill without color")?;
                let (token, t) = self.color(feature, &color, "colorFill color")?;
                out.push(format!("ColorFill:{token},{}", fmt(t)));
            }
            "areaFillReference" => {
                let r = fill.attribute("reference").unwrap_or_else(|| text(&fill));
                if r.is_empty() || r.contains([';', ':', ',', '&']) {
                    return Err(format!("invalid areaFillReference '{r}'"));
                }
                out.push(format!("AreaFillReference:{r}"));
            }
            "pixmapFill" => {
                let pixmap = child(&fill, "pixmap").ok_or("pixmapFill without pixmap")?;
                let r = pixmap.attribute("reference").unwrap_or_default();
                if r.is_empty() || r.contains([';', ':', ',', '&']) {
                    return Err(format!("invalid pixmap reference '{r}'"));
                }
                area_crs(self, tracker, out);
                self.overrides(&pixmap, feature, out)?;
                out.push(format!("PixmapFill:{r}"));
            }
            "symbolFill" => {
                self.unknown_children(
                    feature,
                    &fill,
                    &["areaCRS", "symbol", "v1", "v2", "clipSymbols"],
                );
                area_crs(self, tracker, out);
                let symbol = child(&fill, "symbol").ok_or("symbolFill without symbol")?;
                let reference = self.symbol_state(&symbol, feature, tracker, out, false)?;
                let v1 = child(&fill, "v1")
                    .map(|v| self.vector(feature, &v))
                    .ok_or("symbolFill without v1")?;
                let v2 = child(&fill, "v2")
                    .map(|v| self.vector(feature, &v))
                    .ok_or("symbolFill without v2")?;
                let clip = match property(&fill, "clipSymbols") {
                    Some(c) if c.is_empty() => {
                        self.note(feature, "Empty clipSymbols; default true used");
                        true
                    }
                    Some(c) => c != "false" && c != "0",
                    None => true,
                };
                out.push(format!(
                    "SymbolFill:{reference},{},{},{},{},{clip}",
                    fmt(v1.0),
                    fmt(v1.1),
                    fmt(v2.0),
                    fmt(v2.1)
                ));
            }
            "hatchFill" => {
                area_crs(self, tracker, out);
                let hatches: Vec<NodeRef> = children_named(&fill, "hatch").collect();
                if hatches.is_empty() {
                    return Err("hatchFill without hatch".into());
                }
                for hatch in hatches {
                    let direction = child(&hatch, "direction")
                        .map(|d| self.vector(feature, &d))
                        .ok_or("hatch without direction")?;
                    let distance = self.number_property(feature, &hatch, "distance", 0.0);
                    if distance <= 0.0 {
                        return Err("hatch distance must be positive".into());
                    }
                    let styles = self.line_styles(&hatch, feature, out, counter)?;
                    if styles.is_empty() || styles.len() > 2 {
                        return Err("hatch requires one or two line styles".into());
                    }
                    out.push(format!(
                        "HatchFill:{},{},{},{}",
                        fmt(direction.0),
                        fmt(direction.1),
                        fmt(distance),
                        styles.join(",")
                    ));
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn text(
        &mut self,
        node: &NodeRef,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
    ) -> Result<(), Drop> {
        let (placement, kind) = match (child(node, "textPoint"), child(node, "textLine")) {
            (Some(p), _) => (p, "textPoint"),
            (None, Some(l)) => (l, "textLine"),
            _ => return Err("no textPoint or textLine".into()),
        };
        let command = self.text_command(&placement, kind, feature, tracker, out)?;
        out.push(command);
        Ok(())
    }

    /// Text state commands; returns the TextInstruction command.
    fn text_command(
        &mut self,
        placement: &NodeRef,
        kind: &str,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
    ) -> Result<String, Drop> {
        let mut known = vec!["element", "horizontalAlignment", "verticalAlignment"];
        if kind == "textPoint" {
            known.extend(["offset", "rotation", "areaPlacement", "linePlacement"]);
        } else {
            known.extend(["startOffset", "endOffset", "placementMode"]);
        }
        self.unknown_children(feature, placement, &known);
        let h = match property(placement, "horizontalAlignment").filter(|v| !v.is_empty()) {
            None => "Start",
            Some(v) => enum_value(&v, &["Start", "Center", "End"]).unwrap_or_else(|| {
                self.note(
                    feature,
                    format!("Unknown horizontalAlignment '{v}'; Start used"),
                );
                "Start"
            }),
        };
        let v = match property(placement, "verticalAlignment").filter(|v| !v.is_empty()) {
            None => "Bottom",
            Some(v) => enum_value(&v, &["Top", "Center", "Bottom"]).unwrap_or_else(|| {
                self.note(
                    feature,
                    format!("Unknown verticalAlignment '{v}'; Bottom used"),
                );
                "Bottom"
            }),
        };
        let elements_list: Vec<NodeRef> = children_named(placement, "element").collect();
        let first = elements_list.first().ok_or("text without element")?.clone();
        let mut content = String::new();
        for e in &elements_list {
            content.push_str(
                &child(e, "text")
                    .map(|t| t.string_value())
                    .unwrap_or_default(),
            );
        }
        let content = content.trim().to_owned();
        if content.is_empty() {
            return Err("empty text".into());
        }
        if elements_list.len() > 1 {
            self.note(
                feature,
                "Text with several elements drawn with the first element's style",
            );
        }

        if kind == "textPoint" {
            let (x, y) = child(placement, "offset")
                .map(|o| self.vector(feature, &o))
                .unwrap_or((0.0, 0.0));
            tracker.set(out, "LocalOffset", format!("{},{}", fmt(x), fmt(y)));
            let rotation = self.number_property(feature, placement, "rotation", 0.0);
            // TextPoint rotation is relative to the portrayal CRS (9-12.6.3.5).
            tracker.set(out, "Rotation", format!("PortrayalCRS,{}", fmt(rotation)));
            let area = child(placement, "areaPlacement")
                .map(|p| self.area_placement(feature, &p))
                .unwrap_or("VisibleParts");
            tracker.set(out, "AreaPlacement", area.into());
            let line = child(placement, "linePlacement")
                .map(|p| self.line_placement(feature, &p))
                .unwrap_or_else(|| "Relative,0.5".into());
            tracker.set(out, "LinePlacement", line);
        } else {
            let mode = property(placement, "placementMode")
                .and_then(|m| enum_value(&m, &["Relative", "Absolute"]))
                .unwrap_or("Relative");
            let start = self.number_property(feature, placement, "startOffset", 0.5);
            let end = property(placement, "endOffset").and_then(|e| number(&e));
            let value = match end {
                Some(end) => format!("{mode},{},{}", fmt(start), fmt(end)),
                None => format!("{mode},{}", fmt(start)),
            };
            tracker.set(out, "LinePlacement", value);
            tracker.set(out, "LocalOffset", "0,0".into());
            tracker.set(out, "Rotation", "PortrayalCRS,0".into());
        }
        tracker.set(out, "TextAlignHorizontal", h.into());
        tracker.set(out, "TextAlignVertical", v.into());
        tracker.set(out, "ScaleFactor", "1".into());

        // Text element style (9-12.6.3.8).
        let e = &first;
        self.unknown_children(
            feature,
            e,
            &[
                "text",
                "bodySize",
                "flags",
                "fontCharacteristics",
                "fontReference",
                "foreground",
                "background",
                "verticalOffset",
            ],
        );
        let size = self.number_property(feature, e, "bodySize", 10.0);
        tracker.set(out, "FontSize", fmt(size));
        let vertical = self.number_property(feature, e, "verticalOffset", 0.0);
        tracker.set(out, "TextVerticalOffset", fmt(vertical));
        let flags: Vec<String> = child(e, "flags")
            .map(|f| {
                children_named(&f, "flag")
                    .map(|x| text(&x).to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        tracker.set(
            out,
            "FontUnderline",
            flags.iter().any(|f| f == "underline").to_string(),
        );
        tracker.set(
            out,
            "FontStrikethrough",
            flags.iter().any(|f| f == "strikethrough").to_string(),
        );
        tracker.set(
            out,
            "FontUpperline",
            flags.iter().any(|f| f == "upperline").to_string(),
        );
        let (mut weight, mut slant, mut serifs, mut proportion, mut font_ref) =
            ("Medium", "Upright", false, "Proportional", String::new());
        if let Some(fc) = child(e, "fontCharacteristics") {
            self.unknown_children(feature, &fc, &["serifs", "weight", "slant", "proportion"]);
            if let Some(w) = property(&fc, "weight").filter(|w| !w.is_empty()) {
                weight = enum_value(&w, &["Light", "Medium", "Bold"]).unwrap_or_else(|| {
                    self.note(feature, format!("Unknown font weight '{w}'"));
                    "Medium"
                });
            }
            if let Some(s) = property(&fc, "slant").filter(|s| !s.is_empty()) {
                slant = enum_value(&s, &["Upright", "Italics"]).unwrap_or_else(|| {
                    self.note(feature, format!("Unknown font slant '{s}'"));
                    "Upright"
                });
            }
            if let Some(p) = property(&fc, "proportion").filter(|p| !p.is_empty()) {
                proportion = enum_value(&p, &["MonoSpaced", "Proportional"]).unwrap_or_else(|| {
                    self.note(feature, format!("Unknown font proportion '{p}'"));
                    "Proportional"
                });
            }
            serifs = property(&fc, "serifs").is_some_and(|s| s == "true" || s == "1");
        } else if let Some(fr) = child(e, "fontReference") {
            font_ref = fr.attribute("reference").unwrap_or_else(|| text(&fr));
        }
        tracker.set(out, "FontWeight", weight.into());
        tracker.set(out, "FontSlant", slant.into());
        tracker.set(out, "FontSerifs", serifs.to_string());
        tracker.set(out, "FontProportion", proportion.into());
        tracker.set(out, "FontReference", escape(&font_ref));

        let foreground = child(e, "foreground").ok_or("text element without foreground")?;
        let (token, mut t) = self.color(feature, &foreground, "text foreground")?;
        if t >= 1.0 {
            if self.options.opaque_invisible_text {
                self.note(feature, "Fully transparent text foreground drawn opaque (ConvertOptions::opaque_invisible_text)");
                t = 0.0;
            } else {
                self.note(feature, "Text foreground transparency 1 is fully transparent (Part 9 9-12.2.2.3); text is invisible");
            }
        }
        tracker.set(out, "FontColor", format!("{token},{}", fmt(t)));
        match child(e, "background") {
            Some(b) => {
                let (token, mut bt) = self.color(feature, &b, "text background")?;
                if bt >= 1.0 && self.options.opaque_invisible_text && t == 0.0 {
                    bt = 0.0;
                }
                tracker.set(out, "FontBackgroundColor", format!("{token},{}", fmt(bt)));
            }
            None => {
                // No background: fully transparent fill (Part 9a initial state).
                let current = tracker.current("FontBackgroundColor").to_owned();
                if let Some((token, t)) = current.split_once(',') {
                    if t != "1" {
                        tracker.set(out, "FontBackgroundColor", format!("{token},1"));
                    }
                }
            }
        }
        Ok(format!("TextInstruction:{}", escape(&content)))
    }

    fn augmented(
        &mut self,
        node: &NodeRef,
        kind: &str,
        feature: usize,
        tracker: &mut Tracker,
        out: &mut Vec<String>,
        counter: &mut usize,
    ) -> Result<(), Drop> {
        let crs = child(node, "crs")
            .map(|c| text(&c))
            .and_then(|c| crs_name(&c))
            .filter(|c| *c != "LineCRS")
            .ok_or("augmented geometry without a supported crs")?;
        match kind {
            "augmentedPoint" => {
                let position = child(node, "position").ok_or("augmentedPoint without position")?;
                let (x, y) = self.vector(feature, &position);
                out.push(format!("AugmentedPoint:{crs},{},{}", fmt(x), fmt(y)));
                let mut drew = false;
                if let Some(symbol) = child(node, "symbol") {
                    let reference = self.symbol_state(&symbol, feature, tracker, out, true)?;
                    out.push(format!("PointInstruction:{reference}"));
                    drew = true;
                }
                if let Some(p) = child(node, "textPoint") {
                    let command = self.text_command(&p, "textPoint", feature, tracker, out)?;
                    out.push(command);
                    drew = true;
                }
                if !drew {
                    return Err("augmentedPoint without symbol or text".into());
                }
            }
            "augmentedRay" => {
                let direction_crs = child(node, "rotationCRS")
                    .map(|c| text(&c))
                    .and_then(|c| crs_name(&c))
                    .unwrap_or(crs);
                let direction = child(node, "direction")
                    .and_then(|d| number(&text(&d)))
                    .ok_or("augmentedRay without direction")?;
                let length = child(node, "length")
                    .and_then(|d| number(&text(&d)))
                    .ok_or("augmentedRay without length")?;
                let styles = self.line_styles(node, feature, out, counter)?;
                if styles.is_empty() {
                    return Err("augmentedRay without line style".into());
                }
                out.push(format!(
                    "AugmentedRay:{direction_crs},{},{crs},{}",
                    fmt(direction),
                    fmt(length)
                ));
                out.push(format!("LineInstruction:{}", styles.join(",")));
            }
            _ => {
                let path = child(node, "path").ok_or("augmented path without path")?;
                for segment in elements(&path) {
                    let point = |this: &mut Self, name: &str| {
                        child(&segment, name).map(|p| this.vector(feature, &p))
                    };
                    match local(&segment) {
                        "polyline" => {
                            let pts: Vec<String> = children_named(&segment, "point")
                                .map(|p| {
                                    let (x, y) = self.vector(feature, &p);
                                    format!("{},{}", fmt(x), fmt(y))
                                })
                                .collect();
                            if pts.len() < 2 {
                                return Err("polyline with fewer than two points".into());
                            }
                            out.push(format!("Polyline:{}", pts.join(",")));
                        }
                        "arc3Points" => {
                            let (Some(a), Some(b), Some(c)) = (
                                point(self, "startPoint"),
                                point(self, "medianPoint"),
                                point(self, "endPoint"),
                            ) else {
                                return Err("incomplete arc3Points".into());
                            };
                            out.push(format!(
                                "Arc3Points:{},{},{},{},{},{}",
                                fmt(a.0),
                                fmt(a.1),
                                fmt(b.0),
                                fmt(b.1),
                                fmt(c.0),
                                fmt(c.1)
                            ));
                        }
                        "arcByRadius" | "annulus" => {
                            let center = point(self, "center").ok_or("arc without center")?;
                            let (start, sweep) = match child(&segment, "sector") {
                                Some(s) => (
                                    self.number_property(feature, &s, "startAngle", 0.0),
                                    self.number_property(feature, &s, "angularDistance", 360.0),
                                ),
                                None => (0.0, 360.0),
                            };
                            if local(&segment) == "arcByRadius" {
                                let r = self.number_property(feature, &segment, "radius", 0.0);
                                out.push(format!(
                                    "ArcByRadius:{},{},{},{},{}",
                                    fmt(center.0),
                                    fmt(center.1),
                                    fmt(r),
                                    fmt(start),
                                    fmt(sweep)
                                ));
                            } else {
                                let outer =
                                    self.number_property(feature, &segment, "outerRadius", 0.0);
                                let inner =
                                    self.number_property(feature, &segment, "innerRadius", 0.0);
                                out.push(format!(
                                    "Annulus:{},{},{},{},{},{}",
                                    fmt(center.0),
                                    fmt(center.1),
                                    fmt(outer),
                                    fmt(inner),
                                    fmt(start),
                                    fmt(sweep)
                                ));
                            }
                        }
                        other => return Err(format!("unknown path segment {other}")),
                    }
                }
                out.push(format!("AugmentedPath:{crs},{crs},{crs}"));
                let styles = self.line_styles(node, feature, out, counter)?;
                let filled = kind == "augmentedArea"
                    && elements(node).any(|c| {
                        matches!(
                            local(&c),
                            "colorFill"
                                | "areaFillReference"
                                | "pixmapFill"
                                | "symbolFill"
                                | "hatchFill"
                        )
                    });
                if filled {
                    self.area_fill(node, feature, tracker, out, counter)?;
                }
                if !styles.is_empty() {
                    out.push(format!("LineInstruction:{}", styles.join(",")));
                }
                if styles.is_empty() && !filled {
                    return Err(format!("{kind} without line style or fill"));
                }
            }
        }
        if let Some(p) = child(node, "textPoint").filter(|_| kind != "augmentedPoint") {
            let command = self.text_command(&p, "textPoint", feature, tracker, out)?;
            out.push(command);
        }
        out.push("ClearGeometry".into());
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Synthetic cell for the host renderer
// ─────────────────────────────────────────────────────────────────────────

/// The geometry of every input feature as an `S101Cell`. `key_of(i)` gives
/// the feature record key of `input.features[i]`; the host feature id is
/// `"<code>|<key>"`. Spatial record ids are `InputSpatial::record_id`.
pub fn synthetic_cell(input: &PortrayalInput, key_of: impl Fn(usize) -> i64) -> S101Cell {
    let mut cell = S101Cell {
        file_path: input.source.clone().unwrap_or_default(),
        dsid: DatasetIdentification {
            dataset_name: input.dataset_name.clone(),
            dataset_title: input.dataset_name.clone(),
            product_identifier: input.product.as_str().into(),
            ..Default::default()
        },
        code_mappings: DatasetCodeMappings::new(),
        coord_factor: 1.0,
        coord_factor_y: 1.0,
        coord_factor_z: 1.0,
        coord_origin_x: 0.0,
        coord_origin_y: 0.0,
        coord_origin_z: 0.0,
        minimum_display_scale: None,
        maximum_display_scale: None,
        points: HashMap::new(),
        multi_points: HashMap::new(),
        curves: HashMap::new(),
        composite_curves: HashMap::new(),
        surfaces: HashMap::new(),
        features: HashMap::new(),
        information: HashMap::new(),
        spatial_information_associations: HashMap::new(),
    };
    let position = |c: &Coord| ferrite_s100_core::Coordinate::new(c[0], c[1]);
    for (i, s) in input.spatials.iter().enumerate() {
        let id = s.record_id(i);
        match &s.kind {
            SpatialKind::Point => {
                cell.points.insert(
                    id.key(),
                    PointRecord {
                        id,
                        position: position(&s.coords[0]),
                        update_instruction: 1,
                    },
                );
            }
            SpatialKind::MultiPoint => {
                cell.multi_points.insert(
                    id.key(),
                    MultiPointRecord {
                        id,
                        positions: s.coords.iter().map(position).collect(),
                        update_instruction: 1,
                    },
                );
            }
            SpatialKind::Curve => {
                cell.curves.insert(
                    id.key(),
                    CurveRecord {
                        id,
                        segments: vec![CurveSegment {
                            segment_type: SegmentType::Line,
                            positions: s.coords.iter().map(position).collect(),
                        }],
                        start_point: None,
                        end_point: None,
                        update_instruction: 1,
                    },
                );
            }
            SpatialKind::Surface { rings } => {
                let ring = |r: &usize| {
                    vec![OrientedCurve {
                        curve_id: input.spatials[*r].record_id(*r),
                        orientation: true,
                    }]
                };
                cell.surfaces.insert(
                    id.key(),
                    SurfaceRecord {
                        id,
                        exterior_ring: rings.first().map(ring).unwrap_or_default(),
                        interior_rings: rings.iter().skip(1).map(ring).collect(),
                        update_instruction: 1,
                    },
                );
            }
        }
    }
    for (i, f) in input.features.iter().enumerate() {
        let key = key_of(i);
        let primitive_type = match f.primitive {
            "Point" => SpatialPrimitiveType::Point,
            "MultiPoint" => SpatialPrimitiveType::MultiPoint,
            "Curve" => SpatialPrimitiveType::Curve,
            "Surface" => SpatialPrimitiveType::Surface,
            _ => SpatialPrimitiveType::NoGeometry,
        };
        cell.features.insert(
            key,
            FeatureRecord {
                frid: FRID {
                    rcid: key as u32,
                    nftc: 0,
                    rver: 1,
                    ruin: 1,
                },
                foid: None,
                attributes: Vec::new(),
                spatial_associations: f
                    .spatials
                    .iter()
                    .map(|s| SpatialAssociation {
                        spatial_id: input.spatials[*s].record_id(*s),
                        ornt: 1,
                        usag: 1,
                        mask: 0,
                        scale_minimum: None,
                        scale_maximum: None,
                        update_instruction: 1,
                    })
                    .collect(),
                information_associations: Vec::new(),
                feature_associations: Vec::new(),
                masks: Vec::new(),
                feature_code: Some(f.code.clone()),
                primitive_type,
            },
        );
    }
    cell
}
