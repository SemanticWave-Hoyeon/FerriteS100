//! Feature Catalogue container and XML parsing

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::{
    Association, AttributeBinding, AttributeConstraints, AttributeValueType, AttributeVisibility,
    ComplexAttribute, FeatureBinding, FeatureCatalogueExtras, FeatureType, InfoToInfoBinding,
    InformationBinding, InformationType, ListedValue, Multiplicity, NumericRange,
    QuantitativeRange, Result, Role, RoleType, SimpleAttribute, SpatialPrimitive, UnitOfMeasure,
};

/// Feature Catalogue
#[derive(Debug, Clone)]
pub struct FeatureCatalogue {
    pub source_path: std::path::PathBuf,
    pub name: String,
    pub scope: String,
    pub version: String,
    pub version_date: String,
    pub product_id: String,
    pub simple_attributes: HashMap<String, SimpleAttribute>,
    pub complex_attributes: HashMap<String, ComplexAttribute>,
    pub feature_types: HashMap<String, FeatureType>,
    pub information_types: HashMap<String, InformationType>,
}

impl FeatureCatalogue {
    /// Resolve a binding on a feature, information type or complex attribute.
    /// Inherited bindings are searched without following catalogue cycles indefinitely.
    pub fn attribute_visibility(
        &self,
        owner: &str,
        attribute: &str,
    ) -> Option<AttributeVisibility> {
        if let Some(complex) = self.complex_attributes.get(owner) {
            return complex
                .sub_attributes
                .iter()
                .find(|b| b.attribute_code == attribute)
                .map(|b| b.visibility);
        }
        let mut current = Some(owner);
        let mut seen = std::collections::HashSet::new();
        while let Some(code) = current {
            if !seen.insert(code) {
                return None;
            }
            let (bindings, parent) = if let Some(feature) = self.feature_types.get(code) {
                (&feature.attribute_bindings, feature.super_type.as_deref())
            } else {
                let info = self.information_types.get(code)?;
                (&info.attribute_bindings, info.super_type.as_deref())
            };
            if let Some(binding) = bindings.iter().find(|b| b.attribute_code == attribute) {
                return Some(binding.visibility);
            }
            current = parent;
        }
        None
    }

    /// Maximum allowed XML file size (50 MB)
    /// Security: Prevents resource exhaustion from oversized files
    const MAX_XML_SIZE: u64 = 50 * 1024 * 1024;

    fn read_bounded(path: &Path) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(Self::MAX_XML_SIZE + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > Self::MAX_XML_SIZE {
            return Err(crate::FCError::InvalidValue(
                "FC XML byte limit exceeded (50 MiB)".into(),
            ));
        }
        Ok(bytes)
    }

    /// Load Feature Catalogue from XML file
    ///
    /// Security: File size is limited to prevent resource exhaustion attacks
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::load_with_extras(path)?.0)
    }

    /// Load a Feature Catalogue together with its roles, associations, units
    /// of measure and attribute constraints.
    pub fn load_with_extras<P: AsRef<Path>>(path: P) -> Result<(Self, FeatureCatalogueExtras)> {
        let path = path.as_ref();
        tracing::info!("Loading Feature Catalogue: {}", path.display());
        let bytes = Self::read_bounded(path)?;
        Self::parse_bytes(path, &bytes)
    }

    /// Parse an in-memory Feature Catalogue XML document. `path` is only recorded
    /// as [`FeatureCatalogue::source_path`].
    pub fn from_bytes_with_extras(
        path: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<(Self, FeatureCatalogueExtras)> {
        if bytes.len() as u64 > Self::MAX_XML_SIZE {
            return Err(crate::FCError::InvalidValue(
                "FC XML byte limit exceeded (50 MiB)".into(),
            ));
        }
        Self::parse_bytes(path.as_ref(), bytes)
    }

    /// Retain a digest of the same bounded input buffer consumed by the parser.
    pub fn load_bound<P: AsRef<Path>>(path: P) -> Result<BoundFeatureCatalogue> {
        use sha2::Digest;
        let path = path.as_ref();
        let bytes = Self::read_bounded(path)?;
        let digest = sha2::Sha256::digest(&bytes).into();
        let (catalogue, extras) = Self::parse_bytes(path, &bytes)?;
        Ok(BoundFeatureCatalogue {
            catalogue,
            extras,
            digest,
        })
    }

    fn parse_bytes(path: &Path, bytes: &[u8]) -> Result<(Self, FeatureCatalogueExtras)> {
        let reader = BufReader::new(bytes);
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut catalogue = FeatureCatalogue {
            source_path: path.to_path_buf(),
            name: String::new(),
            scope: String::new(),
            version: String::new(),
            version_date: String::new(),
            product_id: String::new(),
            simple_attributes: HashMap::new(),
            complex_attributes: HashMap::new(),
            feature_types: HashMap::new(),
            information_types: HashMap::new(),
        };
        let mut extras = FeatureCatalogueExtras::default();

        let mut buf = Vec::new();
        let mut catalogue_root_seen = false;
        // Number of currently open elements; 1 while directly inside the root.
        let mut depth = 0usize;

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    if !catalogue_root_seen {
                        if local_name != "S100_FC_FeatureCatalogue" {
                            return Err(crate::FCError::InvalidValue(
                                "Not an S-100 Feature Catalogue XML document".into(),
                            ));
                        }
                        catalogue_root_seen = true;
                        depth = 1;
                        buf.clear();
                        continue;
                    }
                    let top = depth == 1;
                    match local_name.as_str() {
                        "S100_FC_SimpleAttribute" => {
                            let (attr, uom, constraints) = parse_simple_attribute(&mut xml_reader)?;
                            if let Some(uom) = uom {
                                extras.units_of_measure.insert(attr.code.clone(), uom);
                            }
                            if let Some(constraints) = constraints {
                                extras
                                    .attribute_constraints
                                    .insert(attr.code.clone(), constraints);
                            }
                            catalogue.simple_attributes.insert(attr.code.clone(), attr);
                        }
                        "S100_FC_ComplexAttribute" => {
                            let attr = parse_complex_attribute(&mut xml_reader)?;
                            catalogue.complex_attributes.insert(attr.code.clone(), attr);
                        }
                        "S100_FC_FeatureType" => {
                            let ft = parse_feature_type(e, &mut xml_reader)?;
                            catalogue.feature_types.insert(ft.code.clone(), ft);
                        }
                        "S100_FC_InformationType" => {
                            let it = parse_information_type(e, &mut xml_reader)?;
                            catalogue.information_types.insert(it.code.clone(), it);
                        }
                        "S100_FC_Role" => {
                            let role = parse_role(&mut xml_reader)?;
                            extras.roles.insert(role.code.clone(), role);
                        }
                        "S100_FC_FeatureAssociation" => {
                            let assoc = parse_association(e, &mut xml_reader)?;
                            extras
                                .feature_associations
                                .insert(assoc.code.clone(), assoc);
                        }
                        "S100_FC_InformationAssociation" => {
                            let assoc = parse_association(e, &mut xml_reader)?;
                            extras
                                .information_associations
                                .insert(assoc.code.clone(), assoc);
                        }
                        // Catalogue metadata: only direct children of the root, so
                        // e.g. producer/CI_Organisation names are never taken.
                        "name" if top && catalogue.name.is_empty() => {
                            catalogue.name = read_text_content(&mut xml_reader)?;
                        }
                        "scope" if top && catalogue.scope.is_empty() => {
                            catalogue.scope = read_text_content(&mut xml_reader)?;
                        }
                        "versionNumber" if top && catalogue.version.is_empty() => {
                            catalogue.version = read_text_content(&mut xml_reader)?;
                        }
                        "versionDate" if top && catalogue.version_date.is_empty() => {
                            catalogue.version_date = read_text_content(&mut xml_reader)?;
                        }
                        "productId" if top && catalogue.product_id.is_empty() => {
                            catalogue.product_id = read_text_content(&mut xml_reader)?;
                        }
                        _ => depth += 1,
                    }
                }
                Event::End(_) => depth = depth.saturating_sub(1),
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        if !catalogue_root_seen {
            return Err(crate::FCError::InvalidValue(
                "Missing S-100 Feature Catalogue XML root".into(),
            ));
        }

        tracing::info!(
            "FC loaded: {} feature types, {} simple attrs, {} complex attrs, {} info types, {} roles, {} feature assocs, {} info assocs",
            catalogue.feature_types.len(),
            catalogue.simple_attributes.len(),
            catalogue.complex_attributes.len(),
            catalogue.information_types.len(),
            extras.roles.len(),
            extras.feature_associations.len(),
            extras.information_associations.len(),
        );

        Ok((catalogue, extras))
    }

    /// Get simple attribute by code
    pub fn get_simple_attribute(&self, code: &str) -> Option<&SimpleAttribute> {
        self.simple_attributes.get(code)
    }

    /// Get complex attribute by code
    pub fn get_complex_attribute(&self, code: &str) -> Option<&ComplexAttribute> {
        self.complex_attributes.get(code)
    }

    /// Get feature type by code
    pub fn get_feature_type(&self, code: &str) -> Option<&FeatureType> {
        self.feature_types.get(code)
    }

    /// Get information type by code
    pub fn get_information_type(&self, code: &str) -> Option<&InformationType> {
        self.information_types.get(code)
    }

    /// Check if attribute is simple or complex
    pub fn is_simple_attribute(&self, code: &str) -> bool {
        self.simple_attributes.contains_key(code)
    }

    /// Get all feature type codes
    pub fn feature_type_codes(&self) -> Vec<String> {
        self.feature_types.keys().cloned().collect()
    }
}

/// Get local name without namespace prefix
fn get_local_name(e: &BytesStart) -> String {
    let name = e.local_name();
    String::from_utf8_lossy(name.as_ref()).to_string()
}

/// Get attribute value from element
fn get_attr_value(e: &BytesStart, name: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        let local_name = attr.key.local_name();
        let attr_name = String::from_utf8_lossy(local_name.as_ref());
        if attr_name == name {
            return Some(String::from_utf8_lossy(&attr.value).to_string());
        }
    }
    None
}

fn attr_is_true(e: &BytesStart, name: &str) -> bool {
    get_attr_value(e, name).is_some_and(|v| v.trim() == "true")
}

/// Read the text content of the current element up to its matching end tag.
/// Text split by comments, CDATA sections or nested elements is concatenated.
fn read_text_content<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<String> {
    // Whitespace next to an entity or CDATA boundary is significant, so read untrimmed
    // and trim only the ends of the whole value (as trim_text did for a single event).
    let (start, end) = (
        reader.config().trim_text_start,
        reader.config().trim_text_end,
    );
    reader.config_mut().trim_text(false);
    let text = read_text_untrimmed(reader);
    reader.config_mut().trim_text_start = start;
    reader.config_mut().trim_text_end = end;
    Ok(text?
        .trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
        .to_string())
}

fn read_text_untrimmed<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<String> {
    let mut buf = Vec::new();
    let mut text = String::new();
    let mut depth = 1usize;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Text(e) => text.push_str(&e.unescape()?),
            Event::CData(e) => text.push_str(&String::from_utf8_lossy(&e)),
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(text)
}

/// Value of a reference element (`<x ref="code"/>` or `<x ref="code"></x>`), falling back
/// to its text content. Consumes the element when `is_start`.
fn ref_or_text<R: std::io::BufRead>(
    e: &BytesStart,
    is_start: bool,
    reader: &mut Reader<R>,
) -> Result<String> {
    if let Some(r) = get_attr_value(e, "ref") {
        if is_start {
            skip_element(reader)?;
        }
        Ok(r)
    } else if is_start {
        read_text_content(reader)
    } else {
        Ok(String::new())
    }
}

/// Handle `lower` / `upper` (Start or Empty). Returns true when the element was a bound.
fn apply_multiplicity_bound<R: std::io::BufRead>(
    e: &BytesStart,
    is_start: bool,
    reader: &mut Reader<R>,
    multiplicity: &mut Multiplicity,
) -> Result<bool> {
    match get_local_name(e).as_str() {
        "lower" => {
            if is_start {
                multiplicity.lower = read_text_content(reader)?.trim().parse().unwrap_or(0);
            }
            Ok(true)
        }
        "upper" => {
            let unbounded = attr_is_true(e, "infinite");
            if is_start {
                let val = read_text_content(reader)?;
                let val = val.trim();
                multiplicity.upper = if unbounded || val == "*" || val.is_empty() {
                    None
                } else {
                    val.parse().ok()
                };
            } else if unbounded || attr_is_true(e, "nil") {
                multiplicity.upper = None; // unbounded
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Parse simple attribute element
fn parse_simple_attribute<R: std::io::BufRead>(
    reader: &mut Reader<R>,
) -> Result<(
    SimpleAttribute,
    Option<UnitOfMeasure>,
    Option<AttributeConstraints>,
)> {
    let mut attr = SimpleAttribute {
        code: String::new(),
        name: String::new(),
        definition: None,
        value_type: AttributeValueType::Text,
        uom: None,
        listed_values: Vec::new(),
        quantitative_range: None,
    };
    let mut unit = None;
    let mut constraints = None;

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    // Elements with text content - read_text_content consumes the End event
                    "code" if depth == 1 => attr.code = read_text_content(reader)?,
                    "name" if depth == 1 => attr.name = read_text_content(reader)?,
                    "definition" if depth == 1 => {
                        attr.definition = Some(read_text_content(reader)?)
                    }
                    "valueType" if depth == 1 => {
                        let vt_str = read_text_content(reader)?;
                        attr.value_type = parse_value_type(&vt_str);
                    }
                    "uom" if depth == 1 => {
                        let u = parse_uom(reader)?;
                        attr.uom = u.symbol.clone().or_else(|| u.name.clone());
                        unit = Some(u);
                    }
                    "constraints" if depth == 1 => {
                        let c = parse_constraints(reader)?;
                        if let Some(range) = &c.range {
                            attr.quantitative_range = Some(QuantitativeRange {
                                minimum: range.lower_bound,
                                maximum: range.upper_bound,
                            });
                        }
                        constraints = Some(c);
                    }
                    // Sub-element - parse_listed_value consumes the entire element including End
                    "listedValue" => {
                        let lv = parse_listed_value(reader)?;
                        attr.listed_values.push(lv);
                    }
                    // Containers (listedValues, definitionReference, ...) - track depth
                    _ => depth += 1,
                }
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Empty(_) => {} // Self-closing tags don't affect depth
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok((attr, unit, constraints))
}

/// Parse `<uom>`: structured `S100_UnitOfMeasure` (name/symbol/definition) or legacy text.
fn parse_uom<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<UnitOfMeasure> {
    let mut uom = UnitOfMeasure::default();
    let mut plain = String::new();
    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => match get_local_name(e).as_str() {
                "name" => uom.name = Some(read_text_content(reader)?),
                "symbol" => uom.symbol = Some(read_text_content(reader)?),
                "definition" => uom.definition = Some(read_text_content(reader)?),
                _ => depth += 1,
            },
            Event::Text(e) if depth == 1 => plain.push_str(&e.unescape()?),
            Event::CData(e) if depth == 1 => plain.push_str(&String::from_utf8_lossy(&e)),
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let plain = plain.trim();
    if uom.symbol.is_none() && !plain.is_empty() {
        uom.symbol = Some(plain.to_string());
    }
    Ok(uom)
}

/// Parse `<constraints>` (`S100_CD_AttributeConstraints`).
fn parse_constraints<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<AttributeConstraints> {
    let mut c = AttributeConstraints::default();
    let mut buf = Vec::new();
    let mut depth = 1;
    let parse_f64 = |s: String| s.trim().parse::<f64>().ok();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => match get_local_name(e).as_str() {
                "stringLength" => c.string_length = read_text_content(reader)?.trim().parse().ok(),
                "textPattern" => c.text_pattern = Some(read_text_content(reader)?),
                "precision" => c.precision = read_text_content(reader)?.trim().parse().ok(),
                "lowerBound" => {
                    c.range
                        .get_or_insert_with(NumericRange::default)
                        .lower_bound = parse_f64(read_text_content(reader)?)
                }
                "upperBound" => {
                    c.range
                        .get_or_insert_with(NumericRange::default)
                        .upper_bound = parse_f64(read_text_content(reader)?)
                }
                "closure" => {
                    c.range.get_or_insert_with(NumericRange::default).closure =
                        Some(read_text_content(reader)?.trim().to_string())
                }
                "range" => {
                    c.range.get_or_insert_with(NumericRange::default);
                    depth += 1
                }
                _ => depth += 1,
            },
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(c)
}

/// Parse complex attribute element
fn parse_complex_attribute<R: std::io::BufRead>(
    reader: &mut Reader<R>,
) -> Result<ComplexAttribute> {
    let mut attr = ComplexAttribute {
        code: String::new(),
        name: String::new(),
        definition: None,
        sub_attributes: Vec::new(),
    };

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    "code" if depth == 1 => attr.code = read_text_content(reader)?,
                    "name" if depth == 1 => attr.name = read_text_content(reader)?,
                    "definition" if depth == 1 => {
                        attr.definition = Some(read_text_content(reader)?)
                    }
                    // Sub-parser consumes entire element including End
                    "subAttributeBinding" => {
                        let binding = parse_attribute_binding(e, reader)?;
                        attr.sub_attributes.push(binding);
                    }
                    _ => depth += 1,
                }
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Empty(_) => {}
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(attr)
}

/// Feature/information binding as written in the XML, before typing.
struct TypeBinding {
    target: String,
    association: Option<String>,
    role: Option<String>,
    role_type: RoleType,
    multiplicity: Multiplicity,
}

/// Parse `featureBinding` / `informationBinding`. `target` is the local name of the
/// referenced type element (`featureType` or `informationType`).
fn parse_type_binding<R: std::io::BufRead>(
    start: &BytesStart,
    reader: &mut Reader<R>,
    target: &str,
) -> Result<TypeBinding> {
    let role_type = match get_attr_value(start, "roleType") {
        None => RoleType::default(),
        Some(v) => RoleType::parse(&v).unwrap_or_else(|| {
            tracing::warn!("Unknown roleType {v:?}; treating as association");
            RoleType::default()
        }),
    };
    let mut binding = TypeBinding {
        target: String::new(),
        association: None,
        role: None,
        role_type,
        multiplicity: Multiplicity::default(),
    };

    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        let (e, is_start) = match reader.read_event_into(&mut buf)? {
            Event::Start(e) => (e.into_owned(), true),
            Event::Empty(e) => (e.into_owned(), false),
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                buf.clear();
                continue;
            }
            Event::Eof => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        buf.clear();
        if apply_multiplicity_bound(&e, is_start, reader, &mut binding.multiplicity)? {
            continue;
        }
        let local_name = get_local_name(&e);
        match local_name.as_str() {
            "association" => binding.association = Some(ref_or_text(&e, is_start, reader)?),
            "role" => binding.role = Some(ref_or_text(&e, is_start, reader)?),
            name if name == target => binding.target = ref_or_text(&e, is_start, reader)?,
            _ if is_start => depth += 1,
            _ => {}
        }
    }
    Ok(binding)
}

/// Parse feature type element
fn parse_feature_type<R: std::io::BufRead>(
    start: &BytesStart,
    reader: &mut Reader<R>,
) -> Result<FeatureType> {
    let mut ft = FeatureType {
        feature_use_type: None,
        code: String::new(),
        name: String::new(),
        definition: None,
        is_abstract: false,
        super_type: None,
        attribute_bindings: Vec::new(),
        information_bindings: Vec::new(),
        feature_bindings: Vec::new(),
        permitted_primitives: Vec::new(),
    };

    // Read isAbstract attribute from element
    ft.is_abstract = get_attr_value(start, "isAbstract") == Some("true".to_string());

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    "code" if depth == 1 => ft.code = read_text_content(reader)?,
                    "name" if depth == 1 => ft.name = read_text_content(reader)?,
                    "definition" if depth == 1 => ft.definition = Some(read_text_content(reader)?),
                    "superType" if depth == 1 => {
                        ft.super_type = Some(ref_or_text(e, true, reader)?)
                    }
                    "featureUseType" if depth == 1 => {
                        if ft.feature_use_type.is_some() {
                            return Err(crate::FCError::InvalidValue(
                                "Duplicate featureUseType".into(),
                            ));
                        }
                        ft.feature_use_type =
                            Some(crate::FeatureUseType::parse(&read_text_content(reader)?)?);
                    }
                    // Sub-parsers consume entire element including End
                    "attributeBinding" => {
                        let binding = parse_attribute_binding(e, reader)?;
                        ft.attribute_bindings.push(binding);
                    }
                    "featureBinding" => {
                        let b = parse_type_binding(e, reader, "featureType")?;
                        ft.feature_bindings.push(FeatureBinding {
                            feature_type_code: b.target,
                            association: b.association,
                            role: b.role,
                            role_type: b.role_type,
                            multiplicity: b.multiplicity,
                        });
                    }
                    "informationBinding" => {
                        let b = parse_type_binding(e, reader, "informationType")?;
                        ft.information_bindings.push(InformationBinding {
                            information_type_code: b.target,
                            association: b.association,
                            role: b.role,
                            role_type: b.role_type,
                            multiplicity: b.multiplicity,
                        });
                    }
                    "permittedPrimitives" | "geometry" => {
                        let prim_str = read_text_content(reader)?;
                        match parse_spatial_primitive(&prim_str) {
                            Some(prim) => ft.permitted_primitives.push(prim),
                            None => tracing::warn!(
                                "Unknown permitted primitive {prim_str:?} on {}",
                                ft.code
                            ),
                        }
                    }
                    _ => depth += 1,
                }
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Empty(ref e) if depth == 1 && get_local_name(e) == "featureUseType" => {
                return Err(crate::FCError::InvalidValue("Empty featureUseType".into()));
            }
            Event::Empty(ref e) if depth == 1 && get_local_name(e) == "superType" => {
                if let Some(r) = get_attr_value(e, "ref") {
                    ft.super_type = Some(r);
                }
            }
            Event::Empty(_) => {}
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(ft)
}

/// Parse information type element
fn parse_information_type<R: std::io::BufRead>(
    start: &BytesStart,
    reader: &mut Reader<R>,
) -> Result<InformationType> {
    let mut it = InformationType {
        code: String::new(),
        name: String::new(),
        definition: None,
        is_abstract: false,
        super_type: None,
        attribute_bindings: Vec::new(),
        information_bindings: Vec::new(),
    };

    // Read isAbstract attribute from element
    it.is_abstract = get_attr_value(start, "isAbstract") == Some("true".to_string());

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    "code" if depth == 1 => it.code = read_text_content(reader)?,
                    "name" if depth == 1 => it.name = read_text_content(reader)?,
                    "definition" if depth == 1 => it.definition = Some(read_text_content(reader)?),
                    "superType" if depth == 1 => {
                        it.super_type = Some(ref_or_text(e, true, reader)?)
                    }
                    // Sub-parsers consume entire element including End
                    "attributeBinding" => {
                        let binding = parse_attribute_binding(e, reader)?;
                        it.attribute_bindings.push(binding);
                    }
                    "informationBinding" => {
                        let b = parse_type_binding(e, reader, "informationType")?;
                        it.information_bindings.push(InfoToInfoBinding {
                            information_type_code: b.target,
                            association: b.association,
                            role: b.role,
                            role_type: b.role_type,
                            multiplicity: b.multiplicity,
                        });
                    }
                    _ => depth += 1,
                }
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Empty(ref e) if depth == 1 && get_local_name(e) == "superType" => {
                if let Some(r) = get_attr_value(e, "ref") {
                    it.super_type = Some(r);
                }
            }
            Event::Empty(_) => {}
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(it)
}

/// Parse `S100_FC_Role`
fn parse_role<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<Role> {
    let mut role = Role::default();
    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => match get_local_name(e).as_str() {
                "code" if depth == 1 => role.code = read_text_content(reader)?,
                "name" if depth == 1 => role.name = read_text_content(reader)?,
                "definition" if depth == 1 => role.definition = Some(read_text_content(reader)?),
                _ => depth += 1,
            },
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(role)
}

/// Parse `S100_FC_FeatureAssociation` / `S100_FC_InformationAssociation`
fn parse_association<R: std::io::BufRead>(
    start: &BytesStart,
    reader: &mut Reader<R>,
) -> Result<Association> {
    let mut assoc = Association {
        is_abstract: attr_is_true(start, "isAbstract"),
        ..Association::default()
    };
    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        let (e, is_start) = match reader.read_event_into(&mut buf)? {
            Event::Start(e) => (e.into_owned(), true),
            Event::Empty(e) => (e.into_owned(), false),
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                buf.clear();
                continue;
            }
            Event::Eof => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        buf.clear();
        let top = depth == 1;
        match get_local_name(&e).as_str() {
            "code" if top && is_start => assoc.code = read_text_content(reader)?,
            "name" if top && is_start => assoc.name = read_text_content(reader)?,
            "definition" if top && is_start => assoc.definition = Some(read_text_content(reader)?),
            "superType" if top => assoc.super_type = Some(ref_or_text(&e, is_start, reader)?),
            "role" if top => assoc.roles.push(ref_or_text(&e, is_start, reader)?),
            "attributeBinding" if is_start => {
                assoc
                    .attribute_bindings
                    .push(parse_attribute_binding(&e, reader)?);
            }
            _ if is_start => depth += 1,
            _ => {}
        }
    }
    Ok(assoc)
}

/// Parse `<permittedValues><value>n</value>...</permittedValues>`.
fn parse_permitted_values<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<Vec<u32>> {
    let mut values = Vec::new();
    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) if get_local_name(e) == "value" => {
                let text = read_text_content(reader)?;
                match text.trim().parse() {
                    Ok(v) => values.push(v),
                    Err(_) => tracing::warn!("Non-numeric permitted value {text:?}"),
                }
            }
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(values)
}

/// Parse attribute binding element
fn parse_attribute_binding<R: std::io::BufRead>(
    start: &BytesStart,
    reader: &mut Reader<R>,
) -> Result<AttributeBinding> {
    let mut binding = AttributeBinding {
        attribute_code: String::new(),
        visibility: AttributeVisibility::Public,
        multiplicity: Multiplicity::default(),
        sequential: false,
        permitted_values: Vec::new(),
    };

    // Read sequential attribute from element
    binding.sequential = get_attr_value(start, "sequential") == Some("true".to_string());

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        let (e, is_start) = match reader.read_event_into(&mut buf)? {
            Event::Start(e) => (e.into_owned(), true),
            Event::Empty(e) => (e.into_owned(), false),
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                buf.clear();
                continue;
            }
            Event::Eof => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        buf.clear();
        if apply_multiplicity_bound(&e, is_start, reader, &mut binding.multiplicity)? {
            continue;
        }
        let local_name = get_local_name(&e);
        match local_name.as_str() {
            "attribute" | "attributeCode" => {
                let code = ref_or_text(&e, is_start, reader)?;
                if is_start || !code.is_empty() {
                    binding.attribute_code = code;
                }
            }
            "attributeVisibility" if is_start => {
                binding.visibility = match read_text_content(reader)?.trim() {
                    "publicVisibility" => AttributeVisibility::Public,
                    "protectedVisibility" => AttributeVisibility::Protected,
                    "privateVisibility" => AttributeVisibility::Private,
                    value => {
                        return Err(crate::FCError::InvalidValue(format!(
                            "Unknown attributeVisibility: {value}"
                        )))
                    }
                };
            }
            "permittedValues" if is_start => {
                binding.permitted_values = parse_permitted_values(reader)?;
            }
            _ if is_start => depth += 1,
            _ => {}
        }
    }

    Ok(binding)
}

/// Skip an element and its content
fn skip_element<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<()> {
    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Parse listed value element
fn parse_listed_value<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<ListedValue> {
    let mut lv = ListedValue {
        label: String::new(),
        code: 0,
        definition: None,
    };

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    "label" if depth == 1 => lv.label = read_text_content(reader)?,
                    "code" if depth == 1 => {
                        let val = read_text_content(reader)?;
                        lv.code = val.trim().parse().unwrap_or(0);
                    }
                    "definition" if depth == 1 => lv.definition = Some(read_text_content(reader)?),
                    _ => depth += 1,
                }
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Empty(_) => {}
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(lv)
}

/// Parse attribute value type string (`S100_CD_AttributeValueType`, S-100 4.x/5.x).
fn parse_value_type(s: &str) -> AttributeValueType {
    match s.trim().to_lowercase().as_str() {
        "boolean" => AttributeValueType::Boolean,
        "enumeration" => AttributeValueType::Enumeration,
        "integer" => AttributeValueType::Integer,
        "real" => AttributeValueType::Real,
        "text" => AttributeValueType::Text,
        "date" => AttributeValueType::Date,
        "time" => AttributeValueType::Time,
        "datetime" => AttributeValueType::DateTime,
        "truncateddate" => AttributeValueType::TruncatedDate,
        "s100_truncateddate" => AttributeValueType::S100TruncatedDate,
        "uri" => AttributeValueType::Uri,
        "url" => AttributeValueType::Url,
        "urn" => AttributeValueType::Urn,
        "s100_codelist" => AttributeValueType::S100CodeList,
        other => {
            tracing::warn!("Unknown attribute valueType {other:?}; treating as text");
            AttributeValueType::Text
        }
    }
}

/// Parse spatial primitive string (`S100_FC_SpatialPrimitiveType`)
fn parse_spatial_primitive(s: &str) -> Option<SpatialPrimitive> {
    match s.trim().to_lowercase().as_str() {
        "point" => Some(SpatialPrimitive::Point),
        "pointset" => Some(SpatialPrimitive::PointSet),
        "curve" => Some(SpatialPrimitive::Curve),
        "surface" => Some(SpatialPrimitive::Surface),
        "coverage" => Some(SpatialPrimitive::Coverage),
        "arcbycenterpoint" => Some(SpatialPrimitive::ArcByCenterPoint),
        "circlebycenterpoint" => Some(SpatialPrimitive::CircleByCenterPoint),
        "nogeometry" | "none" => Some(SpatialPrimitive::NoGeometry),
        _ => None,
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;
    #[test]
    fn binding_visibility_consumes_xml_and_rejects_unknown_values() {
        for (value, expected) in [
            ("publicVisibility", AttributeVisibility::Public),
            ("protectedVisibility", AttributeVisibility::Protected),
            ("privateVisibility", AttributeVisibility::Private),
        ] {
            let xml = format!("<attributeBinding><multiplicity><lower>0</lower><upper>1</upper></multiplicity><attributeVisibility>{value}</attributeVisibility><attribute ref=\"depth\"/></attributeBinding>");
            let mut reader = Reader::from_str(&xml);
            let Event::Start(start) = reader.read_event().unwrap() else {
                panic!()
            };
            let binding = parse_attribute_binding(&start, &mut reader).unwrap();
            assert_eq!(binding.visibility, expected);
            assert_eq!(binding.attribute_code, "depth");
            assert_eq!(binding.multiplicity.upper, Some(1));
        }
        let xml = "<attributeBinding><attributeVisibility>unknown</attributeVisibility></attributeBinding>";
        let mut reader = Reader::from_str(xml);
        let Event::Start(start) = reader.read_event().unwrap() else {
            panic!()
        };
        assert!(parse_attribute_binding(&start, &mut reader).is_err());
    }
}

#[cfg(test)]
mod feature_use_tests {
    use super::*;
    fn parse(content: &str) -> Result<FeatureType> {
        let xml = format!("<S100_FC_FeatureType><code>Test</code>{content}</S100_FC_FeatureType>");
        let mut reader = Reader::from_str(&xml);
        let Event::Start(start) = reader.read_event().unwrap() else {
            panic!()
        };
        parse_feature_type(&start, &mut reader)
    }
    #[test]
    fn feature_use_categories_are_preserved_not_guessed() {
        for value in ["geographic", "meta", "cartographic", "theme"] {
            let ft = parse(&format!("<featureUseType>{value}</featureUseType>")).unwrap();
            assert_eq!(ft.code, "Test");
            assert_eq!(ft.feature_use_type.unwrap().as_str(), value);
        }
        assert_eq!(parse("").unwrap().feature_use_type, None);
        assert_eq!(
            parse(
                "<definitionReference><featureUseType>meta</featureUseType></definitionReference>"
            )
            .unwrap()
            .feature_use_type,
            None
        );
    }
    #[test]
    fn invalid_empty_and_duplicate_classifications_are_rejected() {
        for content in [
            "<featureUseType>unknown</featureUseType>",
            "<featureUseType>Geographic</featureUseType>",
            "<featureUseType/>",
            "<featureUseType></featureUseType>",
            "<featureUseType>meta</featureUseType><featureUseType>geographic</featureUseType>",
        ] {
            assert!(parse(content).is_err(), "{content}");
        }
    }
}

/// Parsed catalogue and exact source identity, replaced as one value.
#[derive(Debug, Clone)]
pub struct BoundFeatureCatalogue {
    catalogue: FeatureCatalogue,
    extras: FeatureCatalogueExtras,
    digest: [u8; 32],
}
impl std::ops::Deref for BoundFeatureCatalogue {
    type Target = FeatureCatalogue;
    fn deref(&self) -> &Self::Target {
        &self.catalogue
    }
}
impl BoundFeatureCatalogue {
    pub fn source_digest(&self) -> &[u8; 32] {
        &self.digest
    }
    /// Roles, associations, units of measure and constraints of the same source.
    pub fn extras(&self) -> &FeatureCatalogueExtras {
        &self.extras
    }
}

#[cfg(test)]
mod source_identity_tests {
    use super::*;
    #[test]
    fn bound_metadata_and_digest_survive_original_edit_and_deletion() {
        use sha2::Digest;
        let path = std::env::temp_dir().join(format!(
            "ferrite-fc-source-{}-{}.xml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let xml = |name: &str| {
            format!("<S100_FC_FeatureCatalogue><name>{name}</name><versionNumber>2.0</versionNumber><productId>S-101</productId></S100_FC_FeatureCatalogue>")
        };
        let a = xml("A");
        let b = xml("B");
        std::fs::write(&path, &a).unwrap();
        let first = FeatureCatalogue::load_bound(&path).unwrap();
        assert_eq!(FeatureCatalogue::load(&path).unwrap().name, first.name);
        assert_eq!(first.name, "A");
        assert_eq!(
            first.source_digest().as_slice(),
            sha2::Sha256::digest(a.as_bytes()).as_slice()
        );
        std::fs::write(&path, &b).unwrap();
        let second = FeatureCatalogue::load_bound(&path).unwrap();
        assert_eq!(second.name, "B");
        assert_ne!(first.source_digest(), second.source_digest());
        std::fs::remove_file(path).unwrap();
        assert_eq!(first.name, "A");
        assert_eq!(second.name, "B");
    }
}
