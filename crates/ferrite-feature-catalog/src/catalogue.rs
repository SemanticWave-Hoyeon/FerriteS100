//! Feature Catalogue container and XML parsing

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::{
    Result,
    AttributeValueType, Multiplicity, ListedValue, SpatialPrimitive,
    SimpleAttribute, ComplexAttribute, AttributeBinding,
    FeatureType, InformationType,
};

/// Feature Catalogue
#[derive(Debug, Clone)]
pub struct FeatureCatalogue {
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
    /// Load Feature Catalogue from XML file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        tracing::info!("Loading Feature Catalogue: {}", path.display());

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut xml_reader = Reader::from_reader(reader);
        xml_reader.config_mut().trim_text(true);

        let mut catalogue = FeatureCatalogue {
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

        let mut buf = Vec::new();

        loop {
            match xml_reader.read_event_into(&mut buf)? {
                Event::Start(ref e) => {
                    let local_name = get_local_name(e);
                    match local_name.as_str() {
                        "S100_FC_SimpleAttribute" => {
                            let attr = parse_simple_attribute(&mut xml_reader)?;
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
                        "name" if catalogue.name.is_empty() => {
                            catalogue.name = read_text_content(&mut xml_reader)?;
                        }
                        "scope" if catalogue.scope.is_empty() => {
                            catalogue.scope = read_text_content(&mut xml_reader)?;
                        }
                        "versionNumber" if catalogue.version.is_empty() => {
                            catalogue.version = read_text_content(&mut xml_reader)?;
                        }
                        "versionDate" if catalogue.version_date.is_empty() => {
                            catalogue.version_date = read_text_content(&mut xml_reader)?;
                        }
                        "productId" if catalogue.product_id.is_empty() => {
                            catalogue.product_id = read_text_content(&mut xml_reader)?;
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        tracing::info!(
            "FC loaded: {} feature types, {} simple attrs, {} complex attrs, {} info types",
            catalogue.feature_types.len(),
            catalogue.simple_attributes.len(),
            catalogue.complex_attributes.len(),
            catalogue.information_types.len()
        );

        Ok(catalogue)
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

/// Read text content of current element
fn read_text_content<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<String> {
    let mut buf = Vec::new();
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Text(e) => {
                text = e.unescape()?.to_string();
            }
            Event::End(_) => break,
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(text)
}

/// Parse simple attribute element
fn parse_simple_attribute<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<SimpleAttribute> {
    let mut attr = SimpleAttribute {
        code: String::new(),
        name: String::new(),
        definition: None,
        value_type: AttributeValueType::Text,
        uom: None,
        listed_values: Vec::new(),
        quantitative_range: None,
    };

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    // Elements with text content - read_text_content consumes the End event
                    "code" => attr.code = read_text_content(reader)?,
                    "name" => attr.name = read_text_content(reader)?,
                    "definition" => attr.definition = Some(read_text_content(reader)?),
                    "valueType" => {
                        let vt_str = read_text_content(reader)?;
                        attr.value_type = parse_value_type(&vt_str);
                    }
                    "uom" => attr.uom = Some(read_text_content(reader)?),
                    // Sub-element - parse_listed_value consumes the entire element including End
                    "listedValue" => {
                        let lv = parse_listed_value(reader)?;
                        attr.listed_values.push(lv);
                    }
                    // Container element - track depth
                    "listedValues" => depth += 1,
                    // Other elements - track depth
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

    Ok(attr)
}

/// Parse complex attribute element
fn parse_complex_attribute<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<ComplexAttribute> {
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
                    "code" => attr.code = read_text_content(reader)?,
                    "name" => attr.name = read_text_content(reader)?,
                    "definition" => attr.definition = Some(read_text_content(reader)?),
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

/// Parse feature type element
fn parse_feature_type<R: std::io::BufRead>(start: &BytesStart, reader: &mut Reader<R>) -> Result<FeatureType> {
    let mut ft = FeatureType {
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
                    "code" => ft.code = read_text_content(reader)?,
                    "name" => ft.name = read_text_content(reader)?,
                    "definition" => ft.definition = Some(read_text_content(reader)?),
                    "superType" => ft.super_type = Some(read_text_content(reader)?),
                    // Sub-parser consumes entire element including End
                    "attributeBinding" => {
                        let binding = parse_attribute_binding(e, reader)?;
                        ft.attribute_bindings.push(binding);
                    }
                    "permittedPrimitives" | "geometry" => {
                        let prim_str = read_text_content(reader)?;
                        if let Some(prim) = parse_spatial_primitive(&prim_str) {
                            ft.permitted_primitives.push(prim);
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
            Event::Empty(_) => {}
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(ft)
}

/// Parse information type element
fn parse_information_type<R: std::io::BufRead>(start: &BytesStart, reader: &mut Reader<R>) -> Result<InformationType> {
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
                    "code" => it.code = read_text_content(reader)?,
                    "name" => it.name = read_text_content(reader)?,
                    "definition" => it.definition = Some(read_text_content(reader)?),
                    "superType" => it.super_type = Some(read_text_content(reader)?),
                    // Sub-parser consumes entire element including End
                    "attributeBinding" => {
                        let binding = parse_attribute_binding(e, reader)?;
                        it.attribute_bindings.push(binding);
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

    Ok(it)
}

/// Parse attribute binding element
fn parse_attribute_binding<R: std::io::BufRead>(start: &BytesStart, reader: &mut Reader<R>) -> Result<AttributeBinding> {
    let mut binding = AttributeBinding {
        attribute_code: String::new(),
        multiplicity: Multiplicity::default(),
        sequential: false,
        permitted_values: Vec::new(),
    };

    // Read sequential attribute from element
    binding.sequential = get_attr_value(start, "sequential") == Some("true".to_string());

    let mut buf = Vec::new();
    let mut depth = 1;

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref e) => {
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    "attribute" | "attributeCode" => {
                        // Check for ref attribute first
                        if let Some(ref_val) = get_attr_value(e, "ref") {
                            binding.attribute_code = ref_val;
                            // Skip to end of this element
                            skip_element(reader)?;
                        } else {
                            binding.attribute_code = read_text_content(reader)?;
                        }
                    }
                    "lower" => {
                        let val = read_text_content(reader)?;
                        binding.multiplicity.lower = val.parse().unwrap_or(0);
                    }
                    "upper" => {
                        // Check for infinite attribute
                        if get_attr_value(e, "infinite") == Some("true".to_string()) {
                            binding.multiplicity.upper = None;
                        } else {
                            let val = read_text_content(reader)?;
                            if val == "*" || val.is_empty() {
                                binding.multiplicity.upper = None;
                            } else {
                                binding.multiplicity.upper = val.parse().ok();
                            }
                        }
                    }
                    _ => depth += 1,
                }
            }
            Event::Empty(ref e) => {
                // Handle self-closing elements like <S100FC:attribute ref="..." />
                let local_name = get_local_name(e);
                match local_name.as_str() {
                    "attribute" | "attributeCode" => {
                        if let Some(ref_val) = get_attr_value(e, "ref") {
                            binding.attribute_code = ref_val;
                        }
                    }
                    // Handle self-closing upper tag: <S100Base:upper xsi:nil="true" infinite="true" />
                    "upper" => {
                        if get_attr_value(e, "infinite") == Some("true".to_string())
                            || get_attr_value(e, "nil") == Some("true".to_string())
                        {
                            binding.multiplicity.upper = None; // unbounded
                        }
                    }
                    _ => {}
                }
            }
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
                    "label" => lv.label = read_text_content(reader)?,
                    "code" => {
                        let val = read_text_content(reader)?;
                        lv.code = val.parse().unwrap_or(0);
                    }
                    "definition" => lv.definition = Some(read_text_content(reader)?),
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

/// Parse attribute value type string
fn parse_value_type(s: &str) -> AttributeValueType {
    match s.to_lowercase().as_str() {
        "boolean" => AttributeValueType::Boolean,
        "enumeration" => AttributeValueType::Enumeration,
        "integer" => AttributeValueType::Integer,
        "real" => AttributeValueType::Real,
        "text" => AttributeValueType::Text,
        "date" => AttributeValueType::Date,
        "time" => AttributeValueType::Time,
        "datetime" => AttributeValueType::DateTime,
        "truncateddate" => AttributeValueType::TruncatedDate,
        "uri" => AttributeValueType::Uri,
        "url" => AttributeValueType::Url,
        "urn" => AttributeValueType::Urn,
        "s100_codelist" => AttributeValueType::S100CodeList,
        _ => AttributeValueType::Text,
    }
}

/// Parse spatial primitive string
fn parse_spatial_primitive(s: &str) -> Option<SpatialPrimitive> {
    match s.to_lowercase().as_str() {
        "point" => Some(SpatialPrimitive::Point),
        "curve" => Some(SpatialPrimitive::Curve),
        "surface" => Some(SpatialPrimitive::Surface),
        "coverage" => Some(SpatialPrimitive::Coverage),
        "nogeometry" | "none" => Some(SpatialPrimitive::NoGeometry),
        _ => None,
    }
}
