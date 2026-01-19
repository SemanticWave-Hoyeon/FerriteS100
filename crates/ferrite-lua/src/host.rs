//! Host Functions for Lua
//!
//! These functions are exposed to Lua scripts for accessing feature data,
//! attributes, and emitting drawing instructions.
//!
//! Based on S-100 standard's host_functions.cpp

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use mlua::{Lua, MultiValue, Result as LuaResult, Value};

use crate::{
    AttributeValue, CellData, ComplexAttribute, ContextParameters, FeatureInfo, InformationInfo,
    PortrayalResult, PrimitiveType, SpatialInfo,
};

/// Parse attribute path and navigate to target complex attribute
/// Path format: "sectorCharacteristics:1;lightSector:2;" where numbers are 1-based indices
/// Reference: S-100 standard/GISLibrary/GF_ObjectType.cpp - getComplexAttributeCount
fn parse_path(path: &str) -> Vec<(String, usize)> {
    let mut items = Vec::new();
    if path.is_empty() {
        return items;
    }

    // Add trailing semicolon if not present (like S-100 standard does)
    let path_with_semi = if path.ends_with(';') {
        path.to_string()
    } else {
        format!("{};", path)
    };

    for token in path_with_semi.split(';') {
        if token.is_empty() {
            continue;
        }
        if let Some(colon_idx) = token.find(':') {
            let code = token[..colon_idx].to_string();
            let index_str = &token[colon_idx + 1..];
            if let Ok(index) = index_str.parse::<usize>() {
                // Convert from 1-based (Lua) to 0-based (Rust)
                items.push((code, index.saturating_sub(1)));
            }
        }
    }
    items
}

/// Navigate to a nested complex attribute using path and count instances of attr_code
/// Reference: S-100 standard/GISLibrary/GF_ObjectType.cpp - getComplexAttributeCount
fn navigate_and_count_complex(
    root_attrs: &HashMap<String, Vec<ComplexAttribute>>,
    path: &str,
    attr_code: &str,
) -> usize {
    let path_items = parse_path(path);

    if path_items.is_empty() {
        // Root level - count instances of attr_code at feature level
        root_attrs.get(attr_code).map(|v| v.len()).unwrap_or(0)
    } else {
        // Navigate to the target complex attribute
        let mut current: Option<&ComplexAttribute> = None;

        for (i, (code, index)) in path_items.iter().enumerate() {
            if i == 0 {
                // First level - look in root_attrs
                current = root_attrs.get(code).and_then(|v| v.get(*index));
            } else if let Some(parent) = current {
                // Subsequent levels - look in parent's complex_attrs
                current = parent.complex_attrs.get(code).and_then(|v| v.get(*index));
            } else {
                return 0;
            }
        }

        // Count attr_code in the final complex attribute
        if let Some(target) = current {
            target.get_sub_attribute_count(attr_code)
        } else {
            0
        }
    }
}

/// Navigate to a nested complex attribute using path and get all simple attribute values
/// Reference: S-100 standard/GISLibrary/GF_ObjectType.cpp - getAttributeValues
/// Returns multiple values for multi-valued attributes like 'colour'
fn navigate_and_get_simple_values<'a>(
    root_attrs: &'a HashMap<String, Vec<ComplexAttribute>>,
    path: &str,
    attr_code: &str,
) -> Option<&'a Vec<AttributeValue>> {
    let path_items = parse_path(path);

    if path_items.is_empty() {
        return None; // Cannot get simple attr without a path
    }

    // Navigate to the target complex attribute
    let mut current: Option<&ComplexAttribute> = None;

    for (i, (code, index)) in path_items.iter().enumerate() {
        if i == 0 {
            // First level - look in root_attrs
            current = root_attrs.get(code).and_then(|v| v.get(*index));
        } else if let Some(parent) = current {
            // Subsequent levels - look in parent's complex_attrs
            current = parent.complex_attrs.get(code).and_then(|v| v.get(*index));
        } else {
            return None;
        }
    }

    // Get all simple attribute values from the final complex attribute
    current.and_then(|target| target.get_simple_values(attr_code))
}

/// Feature Catalogue type information for Lua
#[derive(Debug, Clone, Default)]
pub struct TypeCatalogue {
    /// Feature type codes
    pub feature_codes: Vec<String>,
    /// Information type codes
    pub information_codes: Vec<String>,
    /// Simple attribute codes
    pub simple_attribute_codes: Vec<String>,
    /// Complex attribute codes
    pub complex_attribute_codes: Vec<String>,
    /// Role codes
    pub role_codes: Vec<String>,
    /// Information association codes
    pub information_association_codes: Vec<String>,
    /// Feature association codes
    pub feature_association_codes: Vec<String>,
    /// Feature type info (code → {Alias, SuperType, SubTypes, ...})
    pub feature_type_info: HashMap<String, FeatureTypeInfo>,
    /// Simple attribute type info
    pub simple_attribute_info: HashMap<String, SimpleAttributeInfo>,
    /// Complex attribute type info
    pub complex_attribute_info: HashMap<String, ComplexAttributeInfo>,
}

/// Feature type metadata from Feature Catalogue
#[derive(Debug, Clone)]
pub struct FeatureTypeInfo {
    pub code: String,
    pub alias: Option<String>,
    pub definition: Option<String>,
    pub super_type: Option<String>,
    pub sub_types: Vec<String>,
    pub bindings: Vec<AttributeBinding>,
}

/// Attribute binding in feature type
#[derive(Debug, Clone)]
pub struct AttributeBinding {
    pub attribute_code: String,
    pub lower: u32,
    pub upper: Option<u32>,
    pub sequential: bool,
}

/// Simple attribute metadata
#[derive(Debug, Clone)]
pub struct SimpleAttributeInfo {
    pub code: String,
    pub alias: Option<String>,
    pub value_type: String,        // "text", "enumeration", "real", "integer", "boolean"
    pub listed_values: Vec<(i32, String, String)>, // (value, label, definition)
}

/// Complex attribute metadata
#[derive(Debug, Clone)]
pub struct ComplexAttributeInfo {
    pub code: String,
    pub alias: Option<String>,
    pub sub_attributes: Vec<String>,
    /// Sub-attribute bindings with multiplicity info (code → (lower, upper))
    /// upper=None means unbounded
    pub sub_attribute_bindings: HashMap<String, (u32, Option<u32>)>,
}

impl TypeCatalogue {
    /// Create TypeCatalogue from Feature Catalogue
    pub fn from_feature_catalogue(fc: &ferrite_feature_catalog::FeatureCatalogue) -> Self {
        let mut catalogue = TypeCatalogue::default();

        // Extract feature type codes and info
        for (code, ft) in &fc.feature_types {
            catalogue.feature_codes.push(code.clone());

            // Build sub-types by scanning all types for their super_type
            let sub_types: Vec<String> = fc
                .feature_types
                .iter()
                .filter_map(|(c, t)| {
                    if t.super_type.as_ref() == Some(code) {
                        Some(c.clone())
                    } else {
                        None
                    }
                })
                .collect();

            let bindings: Vec<AttributeBinding> = ft
                .attribute_bindings
                .iter()
                .map(|b| AttributeBinding {
                    attribute_code: b.attribute_code.clone(),
                    lower: b.multiplicity.lower,
                    upper: b.multiplicity.upper,
                    sequential: b.sequential,
                })
                .collect();

            catalogue.feature_type_info.insert(
                code.clone(),
                FeatureTypeInfo {
                    code: code.clone(),
                    alias: Some(ft.name.clone()),
                    definition: ft.definition.clone(),
                    super_type: ft.super_type.clone(),
                    sub_types,
                    bindings,
                },
            );
        }

        // Extract information type codes
        for code in fc.information_types.keys() {
            catalogue.information_codes.push(code.clone());
        }

        // Extract simple attribute codes and info
        for (code, sa) in &fc.simple_attributes {
            catalogue.simple_attribute_codes.push(code.clone());

            let value_type = match sa.value_type {
                ferrite_feature_catalog::AttributeValueType::Text => "text",
                ferrite_feature_catalog::AttributeValueType::Enumeration => "enumeration",
                ferrite_feature_catalog::AttributeValueType::Real => "real",
                ferrite_feature_catalog::AttributeValueType::Integer => "integer",
                ferrite_feature_catalog::AttributeValueType::Boolean => "boolean",
                ferrite_feature_catalog::AttributeValueType::Date => "date",
                ferrite_feature_catalog::AttributeValueType::Time => "time",
                ferrite_feature_catalog::AttributeValueType::DateTime => "dateTime",
                _ => "text", // Fallback for any other types
            };

            let listed_values: Vec<(i32, String, String)> = sa
                .listed_values
                .iter()
                .map(|lv| (lv.code as i32, lv.label.clone(), lv.definition.clone().unwrap_or_default()))
                .collect();

            catalogue.simple_attribute_info.insert(
                code.clone(),
                SimpleAttributeInfo {
                    code: code.clone(),
                    alias: Some(sa.name.clone()),
                    value_type: value_type.to_string(),
                    listed_values,
                },
            );
        }

        // Extract complex attribute codes and info
        for (code, ca) in &fc.complex_attributes {
            catalogue.complex_attribute_codes.push(code.clone());

            let sub_attributes: Vec<String> = ca
                .sub_attributes
                .iter()
                .map(|b| b.attribute_code.clone())
                .collect();

            // Build sub-attribute bindings with multiplicity info
            let mut sub_attribute_bindings = HashMap::new();
            for binding in &ca.sub_attributes {
                let lower = binding.multiplicity.lower;
                let upper = binding.multiplicity.upper;
                sub_attribute_bindings.insert(
                    binding.attribute_code.clone(),
                    (lower, upper),
                );
            }

            catalogue.complex_attribute_info.insert(
                code.clone(),
                ComplexAttributeInfo {
                    code: code.clone(),
                    alias: Some(ca.name.clone()),
                    sub_attributes,
                    sub_attribute_bindings,
                },
            );
        }

        // Sort the code lists for consistency
        catalogue.feature_codes.sort();
        catalogue.information_codes.sort();
        catalogue.simple_attribute_codes.sort();
        catalogue.complex_attribute_codes.sort();

        // Debug: verify featureName is in complex attributes
        if catalogue.complex_attribute_codes.contains(&"featureName".to_string()) {
            tracing::debug!("TypeCatalogue: featureName is in complex_attribute_codes");
        } else if catalogue.simple_attribute_codes.contains(&"featureName".to_string()) {
            tracing::warn!("TypeCatalogue: featureName is INCORRECTLY in simple_attribute_codes!");
        } else {
            tracing::warn!("TypeCatalogue: featureName is NOT in any attribute codes!");
        }

        // Debug: verify colour is in simple attributes (used by LightSectored)
        if catalogue.simple_attribute_codes.contains(&"colour".to_string()) {
            tracing::warn!("TypeCatalogue: colour is in simple_attribute_codes (correct)");
        } else if catalogue.complex_attribute_codes.contains(&"colour".to_string()) {
            tracing::error!("TypeCatalogue: colour is INCORRECTLY in complex_attribute_codes!");
        } else {
            tracing::error!("TypeCatalogue: colour is NOT in any attribute codes!");
        }

        // Debug: check lightSector's sub-attributes
        if let Some(light_sector_info) = catalogue.complex_attribute_info.get("lightSector") {
            tracing::warn!(
                "TypeCatalogue: lightSector has sub_attributes: {:?}",
                light_sector_info.sub_attributes
            );
            if light_sector_info.sub_attributes.contains(&"colour".to_string()) {
                tracing::warn!("TypeCatalogue: lightSector contains colour as sub-attribute (correct)");
            } else {
                tracing::error!("TypeCatalogue: lightSector does NOT contain colour as sub-attribute!");
            }
        } else {
            tracing::error!("TypeCatalogue: lightSector not found in complex_attribute_info!");
        }

        // Debug: check sectorLimit is in complex attributes
        if catalogue.complex_attribute_codes.contains(&"sectorLimit".to_string()) {
            tracing::warn!("TypeCatalogue: sectorLimit is in complex_attribute_codes (correct)");
            if let Some(sector_limit_info) = catalogue.complex_attribute_info.get("sectorLimit") {
                tracing::warn!(
                    "TypeCatalogue: sectorLimit has sub_attributes: {:?}",
                    sector_limit_info.sub_attributes
                );
            }
        } else {
            tracing::error!("TypeCatalogue: sectorLimit is NOT in complex_attribute_codes!");
        }

        catalogue
    }
}

/// Host function registry
pub struct HostFunctions {
    /// Feature data cache
    features: Arc<RwLock<HashMap<i64, FeatureInfo>>>,
    /// Information type data cache
    information_types: Arc<RwLock<HashMap<i64, InformationInfo>>>,
    /// Spatial data cache
    spatials: Arc<RwLock<HashMap<i64, SpatialInfo>>>,
    /// Feature associations (feature_id → [(target_id, assoc_code, role_code)])
    feature_associations: Arc<RwLock<HashMap<i64, Vec<(i64, String, String)>>>>,
    /// Information associations (feature_id → [(info_id, assoc_code, role_code)])
    information_associations: Arc<RwLock<HashMap<i64, Vec<(i64, String, String)>>>>,
    /// Spatial to features reverse mapping
    spatial_to_features: Arc<RwLock<HashMap<i64, Vec<i64>>>>,
    /// Collected portrayal results
    results: Arc<RwLock<Vec<PortrayalResult>>>,
    /// Context parameters
    context: Arc<RwLock<ContextParameters>>,
    /// Type catalogue (from Feature Catalogue)
    type_catalogue: Arc<RwLock<TypeCatalogue>>,
}

impl HostFunctions {
    pub fn new() -> Self {
        HostFunctions {
            features: Arc::new(RwLock::new(HashMap::new())),
            information_types: Arc::new(RwLock::new(HashMap::new())),
            spatials: Arc::new(RwLock::new(HashMap::new())),
            feature_associations: Arc::new(RwLock::new(HashMap::new())),
            information_associations: Arc::new(RwLock::new(HashMap::new())),
            spatial_to_features: Arc::new(RwLock::new(HashMap::new())),
            results: Arc::new(RwLock::new(Vec::new())),
            context: Arc::new(RwLock::new(ContextParameters::default())),
            type_catalogue: Arc::new(RwLock::new(TypeCatalogue::default())),
        }
    }

    /// Set feature data
    pub fn set_features(&self, features: HashMap<i64, FeatureInfo>) {
        if let Ok(mut f) = self.features.write() {
            *f = features;
        }
    }

    /// Set information type data
    pub fn set_information_types(&self, info_types: HashMap<i64, InformationInfo>) {
        if let Ok(mut i) = self.information_types.write() {
            *i = info_types;
        }
    }

    /// Set spatial data
    pub fn set_spatials(&self, spatials: HashMap<i64, SpatialInfo>) {
        if let Ok(mut s) = self.spatials.write() {
            *s = spatials;
        }
    }

    /// Set feature associations
    pub fn set_feature_associations(&self, assocs: HashMap<i64, Vec<(i64, String, String)>>) {
        if let Ok(mut a) = self.feature_associations.write() {
            *a = assocs;
        }
    }

    /// Set information associations
    pub fn set_information_associations(&self, assocs: HashMap<i64, Vec<(i64, String, String)>>) {
        if let Ok(mut a) = self.information_associations.write() {
            *a = assocs;
        }
    }

    /// Set spatial to features mapping
    pub fn set_spatial_to_features(&self, mapping: HashMap<i64, Vec<i64>>) {
        if let Ok(mut m) = self.spatial_to_features.write() {
            *m = mapping;
        }
    }

    /// Set context parameters
    pub fn set_context(&self, params: ContextParameters) {
        if let Ok(mut c) = self.context.write() {
            *c = params;
        }
    }

    /// Set type catalogue
    pub fn set_type_catalogue(&self, catalogue: TypeCatalogue) {
        if let Ok(mut c) = self.type_catalogue.write() {
            *c = catalogue;
        }
    }

    /// Initialize from cell data
    pub fn from_cell_data(&self, cell_data: &CellData) {
        // Copy features
        if let Ok(mut f) = self.features.write() {
            *f = cell_data.features.clone();
        }
        // Copy information types
        if let Ok(mut i) = self.information_types.write() {
            *i = cell_data.information_types.clone();
        }
        // Copy spatials
        if let Ok(mut s) = self.spatials.write() {
            *s = cell_data.spatials.clone();
        }
        // Copy feature associations
        if let Ok(mut a) = self.feature_associations.write() {
            *a = cell_data
                .feature_associations
                .iter()
                .map(|(k, v)| {
                    (
                        *k,
                        v.iter()
                            .map(|fa| (fa.target_id, fa.association_code.clone(), fa.role_code.clone()))
                            .collect(),
                    )
                })
                .collect();
        }
        // Copy information associations
        if let Ok(mut a) = self.information_associations.write() {
            *a = cell_data
                .information_associations
                .iter()
                .map(|(k, v)| {
                    (
                        *k,
                        v.iter()
                            .map(|ia| (ia.info_id, ia.association_code.clone(), ia.role_code.clone()))
                            .collect(),
                    )
                })
                .collect();
        }
        // Copy spatial to features
        if let Ok(mut m) = self.spatial_to_features.write() {
            *m = cell_data.spatial_to_features.clone();
        }
    }

    /// Get collected results
    pub fn get_results(&self) -> Vec<PortrayalResult> {
        self.results.read().map(|r| r.clone()).unwrap_or_default()
    }

    /// Clear results
    pub fn clear_results(&self) {
        if let Ok(mut r) = self.results.write() {
            r.clear();
        }
    }

    /// Register all host functions with Lua
    pub fn register(&self, lua: &Lua) -> LuaResult<()> {
        let globals = lua.globals();

        // ========================================
        // Feature Functions
        // ========================================

        // HostGetFeatureIDs - Get all feature IDs
        let features = self.features.clone();
        globals.set(
            "HostGetFeatureIDs",
            lua.create_function(move |lua, ()| {
                let ids: Vec<i64> = features
                    .read()
                    .map(|f| f.keys().cloned().collect())
                    .unwrap_or_default();

                let table = lua.create_table()?;
                for (i, id) in ids.iter().enumerate() {
                    table.set(i + 1, *id)?;
                }
                Ok(table)
            })?,
        )?;

        // HostFeatureGetCode - Get feature type code
        let features = self.features.clone();
        globals.set(
            "HostFeatureGetCode",
            lua.create_function(move |_, feature_id: i64| {
                let code = features
                    .read()
                    .ok()
                    .and_then(|f| f.get(&feature_id).map(|fi| fi.code.clone()))
                    .unwrap_or_default();
                Ok(code)
            })?,
        )?;

        // HostFeatureGetSimpleAttribute - Get simple attribute value from nested complex attributes
        // Parameters: (feature_id, attribute_path, attribute_code)
        // Reference: S-100 standard/GISLibrary/GF_ObjectType.cpp - getSimpleAttributeValues(path, code)
        // Path format: "sectorCharacteristics:1;lightSector:1;sectorLimit:1;sectorLimitOne:1"
        // Returns: table with values (empty table {} if attribute not found)
        let features = self.features.clone();
        globals.set(
            "HostFeatureGetSimpleAttribute",
            lua.create_function(
                move |lua, (feature_id, attr_path, attr_code): (i64, Value, String)| {
                    // Convert attributePath (table or string) to semicolon-separated string
                    let path_str = match &attr_path {
                        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
                        Value::Table(t) => {
                            let mut parts = Vec::new();
                            for i in 1..=t.len().unwrap_or(0) {
                                if let Ok(Value::String(s)) = t.get::<Value>(i) {
                                    if let Ok(s) = s.to_str() {
                                        parts.push(s.to_string());
                                    }
                                }
                            }
                            if parts.is_empty() {
                                String::new()
                            } else {
                                parts.join(";") + ";"
                            }
                        }
                        _ => String::new(),
                    };

                    let table = lua.create_table()?;

                    let features_guard = match features.read() {
                        Ok(f) => f,
                        Err(_) => return Ok(Value::Table(table)),
                    };

                    let feature = match features_guard.get(&feature_id) {
                        Some(f) => f,
                        None => return Ok(Value::Table(table)),
                    };

                    // If path is provided, navigate through nested complex attributes
                    // Reference: S-100 standard/GISLibrary/GF_ObjectType.cpp line 236-296
                    if !path_str.is_empty() {
                        // Use navigate_and_get_simple_values for nested lookup
                        if let Some(values) = navigate_and_get_simple_values(
                            &feature.complex_attributes,
                            &path_str,
                            &attr_code,
                        ) {
                            for (idx, value) in values.iter().enumerate() {
                                match value {
                                    AttributeValue::Text(s) => {
                                        table.set(idx + 1, s.as_str())?;
                                    }
                                    AttributeValue::Integer(i) => {
                                        table.set(idx + 1, *i)?;
                                    }
                                    AttributeValue::Real(r) => {
                                        table.set(idx + 1, format!("{}", r))?;
                                    }
                                    AttributeValue::Boolean(b) => {
                                        table.set(idx + 1, *b)?;
                                    }
                                    AttributeValue::Enumeration(val, _) => {
                                        table.set(idx + 1, *val)?;
                                    }
                                    AttributeValue::List(items) => {
                                        for item in items.iter() {
                                            let len = table.len().unwrap_or(0);
                                            table.set(len + 1, item.as_str())?;
                                        }
                                    }
                                }
                            }
                        }
                        return Ok(Value::Table(table));
                    }

                    // No path - look up in top-level attributes
                    if let Some(attr_val) = feature.attributes.get(&attr_code) {
                        match attr_val {
                            AttributeValue::Text(s) => {
                                table.set(1, s.as_str())?;
                            }
                            AttributeValue::Integer(i) => {
                                table.set(1, *i)?;
                            }
                            AttributeValue::Real(r) => {
                                table.set(1, format!("{}", r))?;
                            }
                            AttributeValue::Boolean(b) => {
                                table.set(1, *b)?;
                            }
                            AttributeValue::Enumeration(val, _label) => {
                                table.set(1, *val)?;
                            }
                            AttributeValue::List(items) => {
                                for (i, item) in items.iter().enumerate() {
                                    table.set(i + 1, item.as_str())?;
                                }
                            }
                        }
                    }

                    Ok(Value::Table(table))
                },
            )?,
        )?;

        // HostFeatureGetComplexAttributeCount - Get count of complex attribute instances
        // Reference: S-100 standard/GISLibrary/host_data.cpp - hd_get_feature_complex_attribute_count
        // Path format: "sectorCharacteristics:1;lightSector:2;" or empty for root level
        let features = self.features.clone();
        globals.set(
            "HostFeatureGetComplexAttributeCount",
            lua.create_function(
                move |_, (feature_id, attr_path, attr_code): (i64, Value, String)| {
                    // attributePath can be a table (array of strings like ["sectorCharacteristics:1"])
                    // or a string. Convert to semicolon-separated path string.
                    let path_str = match &attr_path {
                        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
                        Value::Table(t) => {
                            // Iterate table and join with semicolons
                            let mut parts = Vec::new();
                            for i in 1..=t.len().unwrap_or(0) {
                                if let Ok(Value::String(s)) = t.get::<Value>(i) {
                                    if let Ok(s) = s.to_str() {
                                        parts.push(s.to_string());
                                    }
                                }
                            }
                            if parts.is_empty() {
                                String::new()
                            } else {
                                parts.join(";") + ";"
                            }
                        }
                        _ => String::new(),
                    };

                    let count = features
                        .read()
                        .ok()
                        .and_then(|f| {
                            f.get(&feature_id).map(|fi| {
                                // Navigate to the target complex attribute using path
                                navigate_and_count_complex(&fi.complex_attributes, &path_str, &attr_code)
                            })
                        })
                        .unwrap_or(0);
                    Ok(count as i64)
                },
            )?,
        )?;

        // HostGetSimpleAttribute - Get sub-attribute value from complex attribute
        // Reference: S-100 standard/GISLibrary/host_data.cpp - get_simple_attribute_values
        // Path format: table ["complexAttrCode:index", "subAttr:index"] or string
        let features = self.features.clone();
        globals.set(
            "HostGetSimpleAttribute",
            lua.create_function(
                move |lua, (container_id, attr_path, attr_code): (i64, Value, String)| {
                    // Convert attributePath (table or string) to semicolon-separated string
                    let path_str = match &attr_path {
                        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
                        Value::Table(t) => {
                            let mut parts = Vec::new();
                            for i in 1..=t.len().unwrap_or(0) {
                                if let Ok(Value::String(s)) = t.get::<Value>(i) {
                                    if let Ok(s) = s.to_str() {
                                        parts.push(s.to_string());
                                    }
                                }
                            }
                            if parts.is_empty() {
                                String::new()
                            } else {
                                parts.join(";") + ";"
                            }
                        }
                        _ => String::new(),
                    };

                    let table = lua.create_table()?;

                    // Debug: log ALL calls to HostGetSimpleAttribute for sector attributes
                    if attr_code.contains("sector") || attr_code.contains("Sector") {
                        tracing::error!(
                            "RUST HostGetSimpleAttribute: attr='{}', container={}, path='{}'",
                            attr_code, container_id, path_str
                        );
                    }

                    if let Ok(features_guard) = features.read() {
                        // Debug: check if feature exists
                        if attr_code.contains("sector") || attr_code.contains("Sector") {
                            let feature_exists = features_guard.get(&container_id).is_some();
                            tracing::error!(
                                "RUST HostGetSimpleAttribute: feature {} exists={}",
                                container_id, feature_exists
                            );
                        }
                        if let Some(fi) = features_guard.get(&container_id) {
                            // Navigate to the target complex attribute and get all values
                            // S-101 supports multi-valued attributes (e.g., colour = [1, 3])
                            if let Some(values) = navigate_and_get_simple_values(
                                &fi.complex_attributes,
                                &path_str,
                                &attr_code,
                            ) {
                                // Debug for critical attribute lookups
                                if attr_code == "sectorBearing" {
                                    tracing::warn!(
                                        "HostGetSimpleAttribute: found {} values for sectorBearing",
                                        values.len()
                                    );
                                }
                                // Return all values in the table
                                for (idx, value) in values.iter().enumerate() {
                                    match value {
                                        AttributeValue::Text(s) => {
                                            table.set(idx + 1, s.as_str())?;
                                        }
                                        AttributeValue::Integer(i) => {
                                            table.set(idx + 1, *i)?;
                                        }
                                        AttributeValue::Real(r) => {
                                            table.set(idx + 1, format!("{}", r))?;
                                        }
                                        AttributeValue::Boolean(b) => {
                                            table.set(idx + 1, *b)?;
                                        }
                                        AttributeValue::Enumeration(val, _) => {
                                            table.set(idx + 1, *val)?;
                                        }
                                        AttributeValue::List(items) => {
                                            // For List type, flatten all items into the table
                                            for item in items.iter() {
                                                let len = table.len().unwrap_or(0);
                                                table.set(len + 1, item.as_str())?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Return empty table if not found (Lua expects #values to work)
                    Ok(Value::Table(table))
                },
            )?,
        )?;

        // HostGetComplexAttributeCount - Get count of nested complex attribute instances
        // Reference: S-100 standard/GISLibrary/host_data.cpp - get_complex_attribute_count
        // Used for looking up nested complex attributes (e.g., lightSector within sectorCharacteristics)
        let features = self.features.clone();
        globals.set(
            "HostGetComplexAttributeCount",
            lua.create_function(
                move |_, (container_id, attr_path, attr_code): (i64, Value, String)| {
                    // Convert attributePath (table or string) to semicolon-separated string
                    let path_str = match &attr_path {
                        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
                        Value::Table(t) => {
                            let mut parts = Vec::new();
                            for i in 1..=t.len().unwrap_or(0) {
                                if let Ok(Value::String(s)) = t.get::<Value>(i) {
                                    if let Ok(s) = s.to_str() {
                                        parts.push(s.to_string());
                                    }
                                }
                            }
                            if parts.is_empty() {
                                String::new()
                            } else {
                                parts.join(";") + ";"
                            }
                        }
                        _ => String::new(),
                    };

                    // DEBUG: Log ALL calls to HostGetComplexAttributeCount
                    tracing::debug!(
                        "HostGetComplexAttributeCount CALLED: container_id={}, attr='{}', path='{}'",
                        container_id, attr_code, path_str
                    );

                    let count = features
                        .read()
                        .ok()
                        .and_then(|f| {
                            f.get(&container_id).map(|fi| {
                                let cnt = navigate_and_count_complex(&fi.complex_attributes, &path_str, &attr_code);
                                // Debug for LightSectored nested attrs
                                if attr_code.contains("sector") || attr_code.contains("Sector") || attr_code.contains("light") || attr_code.contains("Light") {
                                    tracing::warn!(
                                        "HostGetComplexAttributeCount: attr={}, path='{}', count={}",
                                        attr_code, path_str, cnt
                                    );
                                }
                                cnt
                            })
                        })
                        .unwrap_or(0);
                    Ok(count as i64)
                },
            )?,
        )?;

        // HostFeatureGetSpatialAssociations - Get spatial associations
        // Must call Lua's CreateSpatialAssociation to create proper objects with metatables
        // IMPORTANT: Sort so that Surface associations come first (higher RCNM = higher priority)
        // This ensures feature.PrimitiveType returns the correct type in Lua
        let features = self.features.clone();
        globals.set(
            "HostFeatureGetSpatialAssociations",
            lua.create_function(move |lua, feature_id: i64| {
                let table = lua.create_table()?;

                if let Ok(features) = features.read() {
                    if let Some(feature) = features.get(&feature_id) {
                        // Filter out invalid spatial refs (RCNM=0 -> None type)
                        // These cause Lua errors because SpatialType["None"] doesn't exist
                        let valid_refs: Vec<_> = feature.spatial_refs.iter()
                            .filter(|r| !matches!(r.spatial_type, PrimitiveType::None))
                            .collect();

                        // Sort by spatial type: Surface(4) > CompositeCurve(3) > Curve(2) > MultiPoint(1) > Point(0)
                        // This ensures GetSpatialAssociation()[1] returns Surface for Surface features
                        let mut sorted_refs = valid_refs;
                        sorted_refs.sort_by(|a, b| {
                            let type_order = |t: &PrimitiveType| match t {
                                PrimitiveType::Surface => 4,
                                PrimitiveType::CompositeCurve => 3,
                                PrimitiveType::Curve => 2,
                                PrimitiveType::MultiPoint => 1,
                                PrimitiveType::Point => 0,
                                _ => -1,
                            };
                            type_order(&b.spatial_type).cmp(&type_order(&a.spatial_type))
                        });

                        for (i, spatial_ref) in sorted_refs.iter().enumerate() {
                            // Get spatial type name
                            let spatial_type_name = match spatial_ref.spatial_type {
                                PrimitiveType::Point => "Point",
                                PrimitiveType::MultiPoint => "MultiPoint",
                                PrimitiveType::Curve => "Curve",
                                PrimitiveType::CompositeCurve => "CompositeCurve",
                                PrimitiveType::Surface => "Surface",
                                _ => "None",
                            };

                            // Get orientation from the spatial association
                            // 1 = Forward, 2 = Reverse
                            let orientation = if spatial_ref.orientation == 2 {
                                "Reverse"
                            } else {
                                "Forward"
                            };

                            // Create spatial ID in "Type|RCID" format like S-100 standard
                            let spatial_id_str = format!("{}|{}", spatial_type_name, spatial_ref.spatial_id);

                            // Call Lua's CreateSpatialAssociation function
                            let create_fn: mlua::Function = lua.globals().get("CreateSpatialAssociation")?;
                            let sa: Value = create_fn.call((
                                spatial_type_name,
                                spatial_id_str,
                                orientation,
                                Value::Nil, // scaleMinimum
                                Value::Nil, // scaleMaximum
                            ))?;

                            table.set(i + 1, sa)?;
                        }
                    }
                }

                Ok(table)
            })?,
        )?;

        // HostFeatureGetAssociatedFeatureIDs - Get associated feature IDs
        let feature_assocs = self.feature_associations.clone();
        globals.set(
            "HostFeatureGetAssociatedFeatureIDs",
            lua.create_function(
                move |lua, (feature_id, assoc_code, role_code): (i64, Value, Value)| {
                    let table = lua.create_table()?;

                    let assoc_filter: Option<String> = match assoc_code {
                        Value::String(s) => Some(s.to_str()?.to_string()),
                        _ => None,
                    };
                    let role_filter: Option<String> = match role_code {
                        Value::String(s) => Some(s.to_str()?.to_string()),
                        _ => None,
                    };

                    if let Ok(assocs) = feature_assocs.read() {
                        if let Some(feature_assocs) = assocs.get(&feature_id) {
                            let mut idx = 1;
                            for (target_id, ac, rc) in feature_assocs {
                                let matches = assoc_filter
                                    .as_ref()
                                    .map_or(true, |f| f == ac)
                                    && role_filter.as_ref().map_or(true, |f| f == rc);

                                if matches {
                                    table.set(idx, *target_id)?;
                                    idx += 1;
                                }
                            }
                        }
                    }

                    Ok(table)
                },
            )?,
        )?;

        // HostFeatureGetAssociatedInformationIDs - Get associated information IDs
        let info_assocs = self.information_associations.clone();
        globals.set(
            "HostFeatureGetAssociatedInformationIDs",
            lua.create_function(
                move |lua, (feature_id, assoc_code, role_code): (i64, Value, Value)| {
                    let table = lua.create_table()?;

                    let assoc_filter: Option<String> = match assoc_code {
                        Value::String(s) => Some(s.to_str()?.to_string()),
                        _ => None,
                    };
                    let role_filter: Option<String> = match role_code {
                        Value::String(s) => Some(s.to_str()?.to_string()),
                        _ => None,
                    };

                    if let Ok(assocs) = info_assocs.read() {
                        if let Some(info_assocs) = assocs.get(&feature_id) {
                            let mut idx = 1;
                            for (info_id, ac, rc) in info_assocs {
                                let matches = assoc_filter
                                    .as_ref()
                                    .map_or(true, |f| f == ac)
                                    && role_filter.as_ref().map_or(true, |f| f == rc);

                                if matches {
                                    table.set(idx, *info_id)?;
                                    idx += 1;
                                }
                            }
                        }
                    }

                    Ok(table)
                },
            )?,
        )?;

        // ========================================
        // Information Type Functions
        // ========================================

        // HostInformationTypeGetCode - Get information type code
        let info_types = self.information_types.clone();
        globals.set(
            "HostInformationTypeGetCode",
            lua.create_function(move |_, info_id: i64| {
                let code = info_types
                    .read()
                    .ok()
                    .and_then(|i| i.get(&info_id).map(|ii| ii.code.clone()))
                    .unwrap_or_default();
                Ok(code)
            })?,
        )?;

        // HostInformationTypeGetSimpleAttribute
        // Returns: table with values (empty table {} if attribute not found)
        let info_types = self.information_types.clone();
        globals.set(
            "HostInformationTypeGetSimpleAttribute",
            lua.create_function(
                move |lua, (info_id, _attr_path, attr_code): (i64, Value, String)| {
                    let info_guard = match info_types.read() {
                        Ok(i) => i,
                        Err(_) => {
                            // Return empty table, not nil - Lua expects #values to work
                            let table = lua.create_table()?;
                            return Ok(Value::Table(table));
                        }
                    };

                    let info = match info_guard.get(&info_id) {
                        Some(i) => i,
                        None => {
                            // Return empty table, not nil
                            let table = lua.create_table()?;
                            return Ok(Value::Table(table));
                        }
                    };

                    let value = info.attributes.get(&attr_code);
                    match value {
                        Some(attr_val) => match attr_val {
                            AttributeValue::Text(s) => {
                                let table = lua.create_table()?;
                                table.set(1, s.as_str())?;
                                Ok(Value::Table(table))
                            }
                            AttributeValue::Integer(i) => {
                                let table = lua.create_table()?;
                                table.set(1, *i)?;
                                Ok(Value::Table(table))
                            }
                            AttributeValue::Real(r) => {
                                // Return as string for Lua's StringToScaledDecimal parsing
                                let table = lua.create_table()?;
                                table.set(1, format!("{}", r))?;
                                Ok(Value::Table(table))
                            }
                            AttributeValue::Boolean(b) => {
                                let table = lua.create_table()?;
                                table.set(1, *b)?;
                                Ok(Value::Table(table))
                            }
                            AttributeValue::Enumeration(val, _) => {
                                let table = lua.create_table()?;
                                table.set(1, *val)?;
                                Ok(Value::Table(table))
                            }
                            AttributeValue::List(items) => {
                                let table = lua.create_table()?;
                                for (i, item) in items.iter().enumerate() {
                                    table.set(i + 1, item.as_str())?;
                                }
                                Ok(Value::Table(table))
                            }
                        },
                        None => {
                            // Return empty table, not nil - Lua expects #values to work
                            let table = lua.create_table()?;
                            Ok(Value::Table(table))
                        }
                    }
                },
            )?,
        )?;

        // HostInformationTypeGetComplexAttributeCount
        let info_types = self.information_types.clone();
        globals.set(
            "HostInformationTypeGetComplexAttributeCount",
            lua.create_function(
                move |_, (info_id, _attr_path, attr_code): (i64, Value, String)| {
                    let count = info_types
                        .read()
                        .ok()
                        .and_then(|i| {
                            i.get(&info_id)
                                .and_then(|ii| ii.complex_attributes.get(&attr_code).map(|v| v.len()))
                        })
                        .unwrap_or(0);
                    Ok(count as i64)
                },
            )?,
        )?;

        // ========================================
        // Spatial Functions
        // ========================================

        // HostGetSpatial - Get spatial geometry
        // Spatial ID is in "Type|ID" format like "Point|123"
        // Must call Lua's CreatePoint/CreateCurve/CreateSurface to create proper objects
        let spatials = self.spatials.clone();
        globals.set(
            "HostGetSpatial",
            lua.create_function(move |lua, spatial_id_str: String| {
                // Parse "Type|ID" format
                let parts: Vec<&str> = spatial_id_str.split('|').collect();
                if parts.len() != 2 {
                    return Ok(Value::Nil);
                }

                let spatial_type = parts[0];
                let id: i64 = match parts[1].parse() {
                    Ok(id) => id,
                    Err(_) => return Ok(Value::Nil),
                };

                if let Ok(spatials) = spatials.read() {
                    if let Some(spatial) = spatials.get(&id) {
                        match spatial_type {
                            "Point" => {
                                // CreatePoint(x, y, z) - parameters as strings
                                if let Some((x, y)) = spatial.coordinates.first() {
                                    let create_fn: mlua::Function = lua.globals().get("CreatePoint")?;
                                    let point: Value = create_fn.call((
                                        format!("{:.7}", x),
                                        format!("{:.7}", y),
                                        Value::Nil, // z
                                    ))?;
                                    return Ok(point);
                                }
                            }
                            "MultiPoint" => {
                                // CreateMultiPoint(points) - array of Point objects with Z for soundings
                                // Reference: S-100 standard Sounding uses ScaledZ for depth values
                                let create_point_fn: mlua::Function = lua.globals().get("CreatePoint")?;
                                let create_fn: mlua::Function = lua.globals().get("CreateMultiPoint")?;

                                let points = lua.create_table()?;
                                for (i, (x, y)) in spatial.coordinates.iter().enumerate() {
                                    // Get Z coordinate (depth) if available
                                    let z_value: Value = if i < spatial.z_coordinates.len() {
                                        match spatial.z_coordinates[i] {
                                            Some(z) => Value::String(lua.create_string(&format!("{:.7}", z))?),
                                            None => Value::Nil,
                                        }
                                    } else {
                                        Value::Nil
                                    };

                                    let point: Value = create_point_fn.call((
                                        format!("{:.7}", x),
                                        format!("{:.7}", y),
                                        z_value,
                                    ))?;
                                    points.set(i + 1, point)?;
                                }

                                let multipoint: Value = create_fn.call((points,))?;
                                return Ok(multipoint);
                            }
                            "Curve" => {
                                // For curves, we need start/end points and segments
                                // This is complex - for now, create a simplified curve with control points
                                // Full implementation would need curve structure with start/end points

                                // Create control points for the curve segment
                                let create_point_fn: mlua::Function = lua.globals().get("CreatePoint")?;
                                let control_points = lua.create_table()?;

                                for (i, (x, y)) in spatial.coordinates.iter().enumerate() {
                                    let point: Value = create_point_fn.call((
                                        format!("{:.7}", x),
                                        format!("{:.7}", y),
                                        Value::Nil,
                                    ))?;
                                    control_points.set(i + 1, point)?;
                                }

                                // Create curve segment
                                let create_segment_fn: mlua::Function = lua.globals().get("CreateCurveSegment")?;
                                let segment: Value = create_segment_fn.call((control_points, "Loxodromic"))?;

                                let segments = lua.create_table()?;
                                segments.set(1, segment)?;

                                // Create start and end point spatial associations
                                let create_sa_fn: mlua::Function = lua.globals().get("CreateSpatialAssociation")?;

                                // Start point (first coordinate)
                                let start_point: Value = if spatial.coordinates.first().is_some() {
                                    let point_id = format!("Point|start_{}", id);
                                    create_sa_fn.call(("Point", point_id, "Forward", Value::Nil, Value::Nil))?
                                } else {
                                    Value::Nil
                                };

                                // End point (last coordinate)
                                let end_point: Value = if spatial.coordinates.last().is_some() {
                                    let point_id = format!("Point|end_{}", id);
                                    create_sa_fn.call(("Point", point_id, "Forward", Value::Nil, Value::Nil))?
                                } else {
                                    Value::Nil
                                };

                                let create_curve_fn: mlua::Function = lua.globals().get("CreateCurve")?;
                                let curve: Value = create_curve_fn.call((start_point, end_point, segments))?;
                                return Ok(curve);
                            }
                            "CompositeCurve" => {
                                // CompositeCurve is composed of curve associations
                                // Reference: S-100 standard/GISLibrary/host_data.cpp - hd_get_composite_curve()
                                // Each curve association has spatial_type, spatial_id, and orientation
                                let curve_associations = lua.create_table()?;
                                let create_sa_fn: mlua::Function = lua.globals().get("CreateSpatialAssociation")?;

                                for (i, assoc) in spatial.curve_associations.iter().enumerate() {
                                    // Determine spatial type from RCNM
                                    // Reference: S-100 standard/GISLibrary/host_data.cpp - get_spatial_association(CUCO* cuco)
                                    let assoc_spatial_type = match assoc.rcnm {
                                        120 => "Curve",
                                        125 => "CompositeCurve",
                                        _ => "Curve", // Default to Curve
                                    };

                                    // Orientation: true = Forward, false = Reverse
                                    let orientation = if assoc.orientation { "Forward" } else { "Reverse" };

                                    // Create spatial ID in "Type|ID" format
                                    let assoc_spatial_id = format!("{}|{}", assoc_spatial_type, assoc.curve_id);

                                    // CreateSpatialAssociation(type, id, orientation, scaleMin, scaleMax)
                                    let sa: Value = create_sa_fn.call((
                                        assoc_spatial_type,
                                        assoc_spatial_id,
                                        orientation,
                                        Value::Nil,
                                        Value::Nil,
                                    ))?;
                                    curve_associations.set(i + 1, sa)?;
                                }

                                let create_fn: mlua::Function = lua.globals().get("CreateCompositeCurve")?;
                                let composite: Value = create_fn.call((curve_associations,))?;
                                return Ok(composite);
                            }
                            "Surface" => {
                                // Surface has exterior ring and optional interior rings
                                // For now, create with empty exterior ring
                                let create_sa_fn: mlua::Function = lua.globals().get("CreateSpatialAssociation")?;
                                let exterior_ring: Value = create_sa_fn.call((
                                    "Curve",
                                    format!("Curve|exterior_{}", id),
                                    "Forward",
                                    Value::Nil,
                                    Value::Nil,
                                ))?;

                                let create_fn: mlua::Function = lua.globals().get("CreateSurface")?;
                                let surface: Value = create_fn.call((exterior_ring, Value::Nil))?;
                                return Ok(surface);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Value::Nil)
            })?,
        )?;

        // HostSpatialGetAssociatedFeatureIDs
        // Spatial ID is in "Type|ID" format
        let spatial_features = self.spatial_to_features.clone();
        globals.set(
            "HostSpatialGetAssociatedFeatureIDs",
            lua.create_function(move |lua, spatial_id_str: String| {
                let table = lua.create_table()?;

                // Parse "Type|ID" format to get numeric ID
                let parts: Vec<&str> = spatial_id_str.split('|').collect();
                if parts.len() != 2 {
                    return Ok(table);
                }
                let spatial_id: i64 = match parts[1].parse() {
                    Ok(id) => id,
                    Err(_) => return Ok(table),
                };

                if let Ok(mapping) = spatial_features.read() {
                    if let Some(feature_ids) = mapping.get(&spatial_id) {
                        for (i, fid) in feature_ids.iter().enumerate() {
                            table.set(i + 1, *fid)?;
                        }
                    }
                }

                Ok(table)
            })?,
        )?;

        // HostSpatialGetAssociatedInformationIDs (not commonly used, return empty)
        // Spatial ID is in "Type|ID" format
        globals.set(
            "HostSpatialGetAssociatedInformationIDs",
            lua.create_function(move |lua, (_spatial_id, _assoc_code, _role_code): (String, Value, Value)| {
                // Spatials don't typically have direct information associations
                let table = lua.create_table()?;
                Ok(table)
            })?,
        )?;

        // ========================================
        // Type Catalogue Functions (FC integration)
        // ========================================

        // HostGetFeatureTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetFeatureTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    for (i, code) in cat.feature_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetInformationTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetInformationTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    for (i, code) in cat.information_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetSimpleAttributeTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetSimpleAttributeTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    // DEBUG: Verify sectorBearing IS in simple attributes
                    let has_sector_bearing = cat.simple_attribute_codes.contains(&"sectorBearing".to_string());
                    if !has_sector_bearing {
                        tracing::error!("BUG: sectorBearing is NOT in simple_attribute_codes!");
                    } else {
                        tracing::warn!("HostGetSimpleAttributeTypeCodes: sectorBearing IS in list (correct)");
                    }
                    tracing::info!(
                        "HostGetSimpleAttributeTypeCodes: {} codes",
                        cat.simple_attribute_codes.len()
                    );
                    for (i, code) in cat.simple_attribute_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetComplexAttributeTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetComplexAttributeTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    // DEBUG: Verify sectorLimitOne IS in complex attributes
                    let has_sector_limit_one = cat.complex_attribute_codes.contains(&"sectorLimitOne".to_string());
                    if !has_sector_limit_one {
                        tracing::error!("BUG: sectorLimitOne is NOT in complex_attribute_codes!");
                    } else {
                        tracing::warn!("HostGetComplexAttributeTypeCodes: sectorLimitOne IS in list (correct)");
                    }
                    tracing::info!(
                        "HostGetComplexAttributeTypeCodes: {} codes",
                        cat.complex_attribute_codes.len()
                    );
                    for (i, code) in cat.complex_attribute_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetRoleTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetRoleTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    for (i, code) in cat.role_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetInformationAssociationTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetInformationAssociationTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    for (i, code) in cat.information_association_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetFeatureAssociationTypeCodes
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetFeatureAssociationTypeCodes",
            lua.create_function(move |lua, ()| {
                let table = lua.create_table()?;
                if let Ok(cat) = type_cat.read() {
                    for (i, code) in cat.feature_association_codes.iter().enumerate() {
                        table.set(i + 1, code.as_str())?;
                    }
                }
                Ok(table)
            })?,
        )?;

        // HostGetFeatureTypeInfo
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetFeatureTypeInfo",
            lua.create_function(move |lua, code: String| {
                if let Ok(cat) = type_cat.read() {
                    if let Some(info) = cat.feature_type_info.get(&code) {
                        // Debug: log bindings for LightSectored
                        if code == "LightSectored" {
                            let binding_codes: Vec<_> = info.bindings.iter().map(|b| b.attribute_code.as_str()).collect();
                            tracing::warn!(
                                "HostGetFeatureTypeInfo: code={} bindings={:?}",
                                code, binding_codes
                            );
                        }

                        let table = lua.create_table()?;
                        table.set("Code", info.code.as_str())?;
                        if let Some(alias) = &info.alias {
                            table.set("Alias", alias.as_str())?;
                        }
                        if let Some(def) = &info.definition {
                            table.set("Definition", def.as_str())?;
                        }
                        if let Some(st) = &info.super_type {
                            table.set("SuperType", st.as_str())?;
                        }

                        // SubTypes
                        let subs = lua.create_table()?;
                        for (i, sub) in info.sub_types.iter().enumerate() {
                            subs.set(i + 1, sub.as_str())?;
                        }
                        table.set("SubTypes", subs)?;

                        // AttributeBindings - must be accessible by both numeric index and attribute code
                        // Lua code does: attributeBindings[attributeCode] to look up
                        let bindings = lua.create_table()?;
                        for (i, b) in info.bindings.iter().enumerate() {
                            let bt = lua.create_table()?;
                            bt.set("AttributeCode", b.attribute_code.as_str())?;
                            bt.set("LowerMultiplicity", b.lower)?;
                            // UpperMultiplicity: Some(n) = n, None = unbounded
                            // Lua checks: if UpperMultiplicity == 1 then single else array
                            // So unbounded (None) should NOT equal 1 → use 0
                            let upper = b.upper.unwrap_or(0);
                            bt.set("UpperMultiplicity", upper)?;
                            bt.set("Sequential", b.sequential)?;
                            // Set with numeric index
                            bindings.set(i + 1, bt.clone())?;
                            // Also set with attribute code as key (for direct lookup)
                            bindings.set(b.attribute_code.as_str(), bt)?;
                        }
                        table.set("AttributeBindings", bindings)?;

                        return Ok(Value::Table(table));
                    }
                }
                Ok(Value::Nil)
            })?,
        )?;

        // HostGetInformationTypeInfo
        globals.set(
            "HostGetInformationTypeInfo",
            lua.create_function(move |lua, code: String| {
                // Similar structure to FeatureTypeInfo
                let table = lua.create_table()?;
                table.set("Code", code.as_str())?;
                table.set("SubTypes", lua.create_table()?)?;
                table.set("AttributeBindings", lua.create_table()?)?;
                Ok(Value::Table(table))
            })?,
        )?;

        // HostGetSimpleAttributeTypeInfo
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetSimpleAttributeTypeInfo",
            lua.create_function(move |lua, code: String| {
                if let Ok(cat) = type_cat.read() {
                    if let Some(info) = cat.simple_attribute_info.get(&code) {
                        let table = lua.create_table()?;
                        table.set("Code", info.code.as_str())?;
                        if let Some(alias) = &info.alias {
                            table.set("Alias", alias.as_str())?;
                        }
                        table.set("ValueType", info.value_type.as_str())?;

                        // ListedValues for enumerations
                        let listed = lua.create_table()?;
                        for (i, (val, label, def)) in info.listed_values.iter().enumerate() {
                            let lv = lua.create_table()?;
                            lv.set("Code", *val)?;
                            lv.set("Label", label.as_str())?;
                            lv.set("Definition", def.as_str())?;
                            listed.set(i + 1, lv)?;
                        }
                        table.set("ListedValues", listed)?;

                        return Ok(Value::Table(table));
                    }
                }
                Ok(Value::Nil)
            })?,
        )?;

        // HostGetComplexAttributeTypeInfo
        let type_cat = self.type_catalogue.clone();
        globals.set(
            "HostGetComplexAttributeTypeInfo",
            lua.create_function(move |lua, code: String| {
                if let Ok(cat) = type_cat.read() {
                    if let Some(info) = cat.complex_attribute_info.get(&code) {
                        // Debug: log sector-related complex attributes
                        if code.contains("sector") || code.contains("Sector") || code.contains("light") || code.contains("Light") {
                            tracing::warn!(
                                "HostGetComplexAttributeTypeInfo: code={} sub_attrs={:?}",
                                code, info.sub_attributes
                            );
                        }

                        let table = lua.create_table()?;
                        table.set("Code", info.code.as_str())?;
                        if let Some(alias) = &info.alias {
                            table.set("Alias", alias.as_str())?;
                        }

                        let subs = lua.create_table()?;
                        for (i, sub) in info.sub_attributes.iter().enumerate() {
                            subs.set(i + 1, sub.as_str())?;
                        }
                        table.set("SubAttributes", subs)?;

                        // IMPORTANT: Add AttributeBindings for sub-attributes
                        // Lua code at PortrayalAPI.lua:130 checks containerTypeInfo.AttributeBindings[attributeCode]
                        // Lua code at PortrayalAPI.lua:184 uses UpperMultiplicity to decide array vs single
                        let bindings = lua.create_table()?;
                        for (i, sub_code) in info.sub_attributes.iter().enumerate() {
                            let binding = lua.create_table()?;
                            binding.set("AttributeCode", sub_code.as_str())?;

                            // Get actual multiplicity from FC binding info
                            let (lower, upper) = info.sub_attribute_bindings
                                .get(sub_code)
                                .copied()
                                .unwrap_or((0, Some(1)));

                            binding.set("LowerMultiplicity", lower as i64)?;
                            // UpperMultiplicity: None means unbounded → use 0 to indicate unbounded
                            // PortrayalAPI.lua:184 checks "UpperMultiplicity == 1" for single-valued
                            // So if upper > 1 or unbounded, it will be treated as array
                            match upper {
                                Some(1) => binding.set("UpperMultiplicity", 1_i64)?,
                                Some(n) => binding.set("UpperMultiplicity", n as i64)?,
                                None => binding.set("UpperMultiplicity", Value::Nil)?, // unbounded
                            }
                            binding.set("Sequential", false)?;
                            // Set both numeric index and code key
                            bindings.set(i + 1, binding.clone())?;
                            bindings.set(sub_code.as_str(), binding.clone())?;

                        }

                        // Debug: for sector-related, print all keys in bindings table
                        if code.contains("sector") || code.contains("Sector") {
                            let mut keys = Vec::new();
                            for pair in bindings.pairs::<Value, Value>() {
                                if let Ok((k, _)) = pair {
                                    let key_str = match k {
                                        Value::String(s) => format!("str:{}", s.to_str().map(|s| s.to_string()).unwrap_or_else(|_| "?".to_string())),
                                        Value::Integer(i) => format!("int:{}", i),
                                        _ => "other".to_string(),
                                    };
                                    keys.push(key_str);
                                }
                            }
                            tracing::error!(
                                "DEBUG {} bindings keys: {:?}",
                                code, keys
                            );
                        }

                        table.set("AttributeBindings", bindings)?;

                        return Ok(Value::Table(table));
                    } else if code == "featureName" {
                        tracing::warn!("HostGetComplexAttributeTypeInfo: featureName NOT FOUND in complex_attribute_info");
                    }
                }
                Ok(Value::Nil)
            })?,
        )?;

        // ========================================
        // Output and Debug Functions
        // ========================================

        // HostPortrayalEmit - Emit drawing instructions (main output)
        let results = self.results.clone();
        globals.set(
            "HostPortrayalEmit",
            lua.create_function(
                move |_, (feature_ref, instructions, observed): (String, String, String)| {
                    tracing::debug!(
                        "HostPortrayalEmit: {} -> {}",
                        feature_ref,
                        instructions.chars().take(50).collect::<String>()
                    );

                    if let Ok(result) =
                        crate::PortrayalResult::parse(&feature_ref, &instructions, &observed)
                    {
                        if let Ok(mut r) = results.write() {
                            r.push(result);
                        }
                    }
                    Ok(true)
                },
            )?,
        )?;

        // HostDebuggerEntry - Debug output with multiple actions
        globals.set(
            "HostDebuggerEntry",
            lua.create_function(|_, args: MultiValue| {
                let args_vec: Vec<Value> = args.into_iter().collect();

                let action = args_vec
                    .first()
                    .and_then(|v| match v {
                        Value::String(s) => s.to_str().ok().map(|s| s.to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "trace".to_string());

                let message = args_vec
                    .get(1)
                    .and_then(|v| match v {
                        Value::String(s) => s.to_str().ok().map(|s| s.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();

                match action.as_str() {
                    "break" => {
                        tracing::debug!("[Lua Debug] Break point");
                    }
                    "trace" => {
                        // Check if this is an error message (from main.lua error handling)
                        if message.starts_with("Error:") {
                            tracing::warn!("[Lua Debug] {}", message);
                        } else {
                            tracing::debug!("[Lua Debug] {}", message);
                        }
                    }
                    "start_performance" => {
                        tracing::trace!("[Lua Perf] Start: {}", message);
                    }
                    "stop_performance" => {
                        tracing::trace!("[Lua Perf] Stop: {}", message);
                    }
                    "reset_performance" => {
                        tracing::trace!("[Lua Perf] Reset: {}", message);
                    }
                    "first_chance_error" => {
                        // Log Lua errors at warn level
                        tracing::warn!("[Lua Error] {}", message);
                    }
                    _ => {
                        tracing::debug!("[Lua Debug] {}: {}", action, message);
                    }
                }
                Ok(())
            })?,
        )?;

        // Helper to create ScaledDecimal table for Lua
        // ScaledDecimal has: Type='ScaledDecimal', Value=int, Scale=int, ToNumber() method
        fn create_scaled_decimal(lua: &Lua, value: f64) -> mlua::Result<Value> {
            // Convert float to scaled integer representation
            // e.g., 30.0 -> Value=300, Scale=1 (or Value=30, Scale=0)
            let scale = if value.fract() == 0.0 { 0 } else { 1 };
            let int_value = if scale == 0 {
                value as i64
            } else {
                (value * 10.0).round() as i64
            };

            let table = lua.create_table()?;
            table.set("Type", "ScaledDecimal")?;
            table.set("Value", int_value)?;
            table.set("Scale", scale)?;

            // Add ToNumber method
            let val_copy = value;
            table.set(
                "ToNumber",
                lua.create_function(move |_, _: ()| Ok(val_copy))?,
            )?;

            Ok(Value::Table(table))
        }

        // HostGetContextParameter - Context parameter access
        let context = self.context.clone();
        globals.set(
            "HostGetContextParameter",
            lua.create_function(move |lua, name: String| {
                if let Ok(ctx) = context.read() {
                    let value: Option<Value> = match name.as_str() {
                        "SafetyDepth" => Some(create_scaled_decimal(lua, ctx.safety_depth)?),
                        "SafetyContour" => Some(create_scaled_decimal(lua, ctx.safety_contour)?),
                        "ShallowContour" => Some(create_scaled_decimal(lua, ctx.shallow_contour)?),
                        "DeepContour" => Some(create_scaled_decimal(lua, ctx.deep_contour)?),
                        "TwoShades" => Some(Value::Boolean(ctx.two_shades)),
                        "FourShades" => Some(Value::Boolean(!ctx.two_shades)), // Opposite of TwoShades
                        "RadarOverlay" => Some(Value::Boolean(ctx.radar_overlay)),
                        "IgnoreScamin" => Some(Value::Boolean(ctx.ignore_scamin)),
                        "IgnoreScaleMinimum" => Some(Value::Boolean(ctx.ignore_scale_minimum)),
                        "FullSectors" => Some(Value::Boolean(ctx.full_sectors)),
                        "SymbolizedBoundaries" => Some(Value::Boolean(ctx.symbolized_boundaries)),
                        "PlainBoundaries" => Some(Value::Boolean(!ctx.symbolized_boundaries)), // Opposite of SymbolizedBoundaries
                        "IsolatedDangers" => Some(Value::Boolean(ctx.isolated_dangers)),
                        "SimplifiedSymbols" => Some(Value::Boolean(ctx.simplified_symbols)),
                        "PaperChart" => Some(Value::Boolean(!ctx.simplified_symbols)), // Opposite of SimplifiedSymbols
                        "ShallowWaterDangers" => Some(Value::Boolean(ctx.shallow_water_dangers)),
                        "FullLightLines" => Some(Value::Boolean(ctx.full_light_lines)),
                        "NationalLanguage" => {
                            let s = lua.create_string(&ctx.national_language)?;
                            Some(Value::String(s))
                        }
                        "Palette" => {
                            let s = lua.create_string(&ctx.palette)?;
                            Some(Value::String(s))
                        }
                        _ => match ctx.custom.get(&name) {
                            Some(crate::ContextValue::Bool(b)) => Some(Value::Boolean(*b)),
                            Some(crate::ContextValue::Integer(i)) => Some(Value::Integer(*i)),
                            Some(crate::ContextValue::Real(r)) => Some(Value::Number(*r)),
                            Some(crate::ContextValue::Text(s)) => {
                                let ls = lua.create_string(s)?;
                                Some(Value::String(ls))
                            }
                            None => None,
                        },
                    };
                    return Ok(value);
                }
                Ok(None)
            })?,
        )?;

        Ok(())
    }
}

impl Default for HostFunctions {
    fn default() -> Self {
        Self::new()
    }
}
