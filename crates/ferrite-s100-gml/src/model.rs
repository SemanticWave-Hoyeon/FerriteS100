//! Product-neutral model of an S-100 Part 10b GML dataset.
use std::collections::HashMap;
use std::path::PathBuf;

/// Longitude, latitude in degrees (EPSG:4326 values, easting first).
pub type Coord = [f64; 2];

pub use ferrite_product_api::ProductId;

/// How the datasets of a registered GML product name it. Product crates
/// (`ferrite-s124`, `ferrite-s129`) supply theirs through
/// [`crate::product::GmlRules::identity`]; the reader, the exchange-set
/// validation and the overlay sniffing take the registered identities.
#[derive(Debug)]
pub struct GmlIdentity {
    pub id: ProductId,
    /// productIdentifier spellings of the product (`"S-124"`, `"S124"`).
    pub identifiers: &'static [&'static str],
    /// Product code: dataset file name prefix and the namespace segment
    /// `S<code>` (`"124"`).
    pub code: &'static str,
    /// Check identifier of a DatasetIdentificationInformation that follows
    /// the members.
    pub late_identification_check: &'static str,
}

impl GmlIdentity {
    /// Whether `value` (a productIdentifier or product folder) names this
    /// product.
    pub fn names(&self, value: &str) -> bool {
        self.identifiers.contains(&value.trim())
    }
    /// The product specification identifier (`"S-124"`).
    pub fn identifier(&self) -> &'static str {
        self.id.as_str()
    }
}

/// The product of `products` that `value` (a productIdentifier or product
/// folder) names.
pub fn identify(products: &[&GmlIdentity], value: &str) -> Option<ProductId> {
    products.iter().find(|p| p.names(value)).map(|p| p.id)
}

/// The identity of `product` among `products`.
pub fn identity_of<'a>(
    products: &[&'a GmlIdentity],
    product: ProductId,
) -> Option<&'a GmlIdentity> {
    products.iter().copied().find(|p| p.id == product)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Feature,
    Information,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Simple value: text content and, for enumerations, the `code` attribute.
    Simple {
        text: String,
        code: Option<String>,
    },
    Complex(Vec<Attribute>),
    /// Present but unknown (`xsi:nil` or `nilReason`).
    Nil {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub code: String,
    pub value: Value,
}

impl Attribute {
    /// Enumeration code if present, else the trimmed text.
    pub fn simple_value(&self) -> Option<&str> {
        match &self.value {
            Value::Simple { text, code } => Some(code.as_deref().unwrap_or(text.as_str())),
            _ => None,
        }
    }
    pub fn text(&self) -> Option<&str> {
        match &self.value {
            Value::Simple { text, .. } => Some(text),
            _ => None,
        }
    }
    pub fn children(&self) -> &[Attribute] {
        match &self.value {
            Value::Complex(c) => c,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    /// The role name, which is the element name in Part 10b.
    pub role: String,
    pub href: String,
    /// Index of the referenced object in the dataset, when resolved.
    pub target: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Point(Coord),
    MultiPoint(Vec<Coord>),
    /// A densified curve (arcs and circles sampled geodesically).
    Curve(Vec<Coord>),
    /// Closed rings; the first position is repeated at the end.
    Surface {
        exterior: Vec<Coord>,
        interiors: Vec<Vec<Coord>>,
    },
}

impl Geometry {
    pub fn primitive(&self) -> &'static str {
        match self {
            Self::Point(_) => "Point",
            Self::MultiPoint(_) => "MultiPoint",
            Self::Curve(_) => "Curve",
            Self::Surface { .. } => "Surface",
        }
    }
    pub fn coords(&self) -> Box<dyn Iterator<Item = &Coord> + '_> {
        match self {
            Self::Point(p) => Box::new(std::iter::once(p)),
            Self::MultiPoint(p) | Self::Curve(p) => Box::new(p.iter()),
            Self::Surface {
                exterior,
                interiors,
            } => Box::new(exterior.iter().chain(interiors.iter().flatten())),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub id: String,
    pub code: String,
    pub kind: ObjectKind,
    pub attributes: Vec<Attribute>,
    pub geometry: Vec<Geometry>,
    pub references: Vec<Reference>,
}

impl Object {
    pub fn attribute(&self, code: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.code == code)
    }
    pub fn attributes_named<'a>(&'a self, code: &'a str) -> impl Iterator<Item = &'a Attribute> {
        self.attributes.iter().filter(move |a| a.code == code)
    }
    /// The first simple value at a path of attribute codes.
    pub fn value(&self, path: &[&str]) -> Option<&str> {
        fn find<'a>(attrs: &'a [Attribute], path: &[&str]) -> Option<&'a str> {
            let (head, rest) = path.split_first()?;
            attrs.iter().filter(|a| a.code == *head).find_map(|a| {
                if rest.is_empty() {
                    a.simple_value()
                } else {
                    find(a.children(), rest)
                }
            })
        }
        find(&self.attributes, path)
    }
    pub fn references_with_role<'a>(&'a self, role: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.references
            .iter()
            .filter(move |r| r.role == role)
            .filter_map(|r| r.target)
    }
    /// The Part 9 primitive name of the object's geometry.
    pub fn primitive(&self) -> &'static str {
        match self.geometry.first() {
            None => "None",
            Some(g) => g.primitive(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Identification {
    /// S100:DatasetIdentificationInformation children in document order.
    pub fields: Vec<(String, String)>,
}

impl Identification {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn product_identifier(&self) -> Option<&str> {
        self.get("productIdentifier")
    }
    pub fn product_edition(&self) -> Option<&str> {
        self.get("productEdition")
    }
    pub fn file_identifier(&self) -> Option<&str> {
        self.get("datasetFileIdentifier")
    }
    pub fn title(&self) -> Option<&str> {
        self.get("datasetTitle")
    }
    pub fn reference_date(&self) -> Option<&str> {
        self.get("datasetReferenceDate")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub severity: Severity,
    /// Check identifier, for example "S-158-129/14" or "GML".
    pub check: String,
    pub object: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        match &self.object {
            Some(o) => write!(f, "{level} [{}] {o}: {}", self.check, self.message),
            None => write!(f, "{level} [{}] {}", self.check, self.message),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dataset {
    pub source: Option<PathBuf>,
    pub sha256: [u8; 32],
    pub product: ProductId,
    pub namespace: String,
    pub gml_id: String,
    pub identification: Identification,
    /// West, south, east, north from the dataset envelope, if present.
    pub envelope: Option<[f64; 4]>,
    pub objects: Vec<Object>,
    pub index: HashMap<String, usize>,
    /// Problems found while reading (encoding and geometry level).
    pub issues: Vec<Issue>,
    /// True when converted from a non-S-100 source (never authoritative).
    pub imported: bool,
}

impl Dataset {
    pub fn object(&self, id: &str) -> Option<&Object> {
        self.index.get(id).map(|i| &self.objects[*i])
    }
    pub fn features(&self) -> impl Iterator<Item = (usize, &Object)> {
        self.objects
            .iter()
            .enumerate()
            .filter(|(_, o)| o.kind == ObjectKind::Feature)
    }
    pub fn information(&self) -> impl Iterator<Item = (usize, &Object)> {
        self.objects
            .iter()
            .enumerate()
            .filter(|(_, o)| o.kind == ObjectKind::Information)
    }
    pub fn objects_of<'a>(&'a self, code: &'a str) -> impl Iterator<Item = (usize, &'a Object)> {
        self.objects
            .iter()
            .enumerate()
            .filter(move |(_, o)| o.code == code)
    }
    /// West, south, east, north of all geometry (no antimeridian handling).
    pub fn geometry_bounds(&self) -> Option<[f64; 4]> {
        let mut b: Option<[f64; 4]> = None;
        for c in self
            .objects
            .iter()
            .flat_map(|o| o.geometry.iter())
            .flat_map(|g| g.coords())
        {
            let e = b.get_or_insert([c[0], c[1], c[0], c[1]]);
            e[0] = e[0].min(c[0]);
            e[1] = e[1].min(c[1]);
            e[2] = e[2].max(c[0]);
            e[3] = e[3].max(c[1]);
        }
        b
    }
    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .source
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
        {
            return name;
        }
        self.identification
            .file_identifier()
            .map(str::to_owned)
            .unwrap_or_else(|| self.gml_id.clone())
    }
}
