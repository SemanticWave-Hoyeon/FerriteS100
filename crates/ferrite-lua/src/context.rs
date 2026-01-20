//! Portrayal Context
//!
//! Contains context parameters and feature data for portrayal rule execution.
//! Based on S-100 standard's context parameter system.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use ferrite_portrayal_catalog::{ContextParamType, ContextParameter};
use ferrite_s100_core::S101Cell;

/// Context parameters for portrayal (mariner settings, display options)
#[derive(Debug, Clone)]
pub struct ContextParameters {
    /// Safety depth in meters
    pub safety_depth: f64,
    /// Safety contour in meters
    pub safety_contour: f64,
    /// Shallow contour in meters
    pub shallow_contour: f64,
    /// Deep contour in meters
    pub deep_contour: f64,
    /// Display isolated dangers in shallow water
    pub isolated_dangers: bool,
    /// Two shades (simple) depth display
    pub two_shades: bool,
    /// Show full light sectors
    pub full_sectors: bool,
    /// Show symbolized boundaries
    pub symbolized_boundaries: bool,
    /// Radar overlay mode
    pub radar_overlay: bool,
    /// Ignore SCAMIN (scale minimum)
    pub ignore_scamin: bool,
    /// Ignore scale minimum attribute
    pub ignore_scale_minimum: bool,
    /// Use simplified point symbols (vs paper chart symbols)
    pub simplified_symbols: bool,
    /// Show shallow water dangers
    pub shallow_water_dangers: bool,
    /// Show full light lines
    pub full_light_lines: bool,
    /// National language for text display
    pub national_language: String,
    /// Active palette name (Day, Dusk, Night)
    pub palette: String,
    /// Custom parameters
    pub custom: HashMap<String, ContextValue>,
}

/// Context parameter value types
#[derive(Debug, Clone)]
pub enum ContextValue {
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
}

impl Default for ContextParameters {
    fn default() -> Self {
        ContextParameters {
            safety_depth: 30.0,
            safety_contour: 30.0,
            shallow_contour: 2.0,
            deep_contour: 30.0,
            isolated_dangers: true,
            two_shades: false,
            full_sectors: true,
            symbolized_boundaries: true,
            radar_overlay: false,
            ignore_scamin: false,
            ignore_scale_minimum: false,
            simplified_symbols: false, // Paper chart symbols by default
            shallow_water_dangers: true,
            full_light_lines: true,
            national_language: "eng".to_string(),
            palette: "Day".to_string(),
            custom: HashMap::new(),
        }
    }
}

impl ContextParameters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create ContextParameters from PC context parameters (dynamically loaded from XML)
    /// This follows the S-100 standard pattern where context parameters are read from PC, not hardcoded.
    pub fn from_pc_context(pc_params: &HashMap<String, ContextParameter>) -> Self {
        let mut params = ContextParameters::default();

        // Apply PC-defined parameters
        for (id, param) in pc_params {
            if let Some(ref default_val) = param.default_value {
                match id.as_str() {
                    "SafetyDepth" => {
                        params.safety_depth = default_val.parse().unwrap_or(30.0);
                    }
                    "SafetyContour" => {
                        params.safety_contour = default_val.parse().unwrap_or(30.0);
                    }
                    "ShallowContour" => {
                        params.shallow_contour = default_val.parse().unwrap_or(2.0);
                    }
                    "DeepContour" => {
                        params.deep_contour = default_val.parse().unwrap_or(30.0);
                    }
                    "FourShades" => {
                        // FourShades = true means NOT two shades
                        params.two_shades = default_val != "true" && default_val != "1";
                    }
                    "ShallowWaterDangers" => {
                        params.shallow_water_dangers = default_val == "true" || default_val == "1";
                    }
                    "PlainBoundaries" => {
                        // PlainBoundaries = true means symbolized_boundaries = false
                        params.symbolized_boundaries = default_val != "true" && default_val != "1";
                    }
                    "SimplifiedSymbols" => {
                        params.simplified_symbols = default_val == "true" || default_val == "1";
                    }
                    "FullLightLines" => {
                        params.full_light_lines = default_val == "true" || default_val == "1";
                    }
                    "RadarOverlay" => {
                        params.radar_overlay = default_val == "true" || default_val == "1";
                    }
                    "IgnoreScaleMinimum" => {
                        params.ignore_scale_minimum = default_val == "true" || default_val == "1";
                        params.ignore_scamin = params.ignore_scale_minimum;
                    }
                    "NationalLanguage" => {
                        params.national_language = default_val.clone();
                    }
                    // Store any unknown parameters as custom
                    _ => {
                        let value = match param.param_type {
                            ContextParamType::Boolean => {
                                ContextValue::Bool(default_val == "true" || default_val == "1")
                            }
                            ContextParamType::Integer => {
                                ContextValue::Integer(default_val.parse().unwrap_or(0))
                            }
                            ContextParamType::Double => {
                                ContextValue::Real(default_val.parse().unwrap_or(0.0))
                            }
                            _ => ContextValue::Text(default_val.clone()),
                        };
                        params.custom.insert(id.clone(), value);
                    }
                }
            }
        }

        tracing::debug!(
            "ContextParameters from PC: SafetyDepth={}, SafetyContour={}, ShallowContour={}, DeepContour={}",
            params.safety_depth, params.safety_contour, params.shallow_contour, params.deep_contour
        );

        params
    }

    /// Get all context parameters as a list for Lua initialization
    /// Returns (id, type_str, default_value_str) tuples for each parameter
    pub fn to_lua_params(&self) -> Vec<(String, String, String)> {
        let mut params = Vec::new();

        params.push((
            "SafetyDepth".to_string(),
            "real".to_string(),
            format!("{}", self.safety_depth),
        ));
        params.push((
            "SafetyContour".to_string(),
            "real".to_string(),
            format!("{}", self.safety_contour),
        ));
        params.push((
            "ShallowContour".to_string(),
            "real".to_string(),
            format!("{}", self.shallow_contour),
        ));
        params.push((
            "DeepContour".to_string(),
            "real".to_string(),
            format!("{}", self.deep_contour),
        ));
        params.push((
            "TwoShades".to_string(),
            "boolean".to_string(),
            if self.two_shades { "true" } else { "false" }.to_string(),
        ));
        params.push((
            "FourShades".to_string(),
            "boolean".to_string(),
            if !self.two_shades { "true" } else { "false" }.to_string(),
        ));
        params.push((
            "RadarOverlay".to_string(),
            "boolean".to_string(),
            if self.radar_overlay { "true" } else { "false" }.to_string(),
        ));
        params.push((
            "IgnoreScamin".to_string(),
            "boolean".to_string(),
            if self.ignore_scamin { "true" } else { "false" }.to_string(),
        ));
        params.push((
            "IgnoreScaleMinimum".to_string(),
            "boolean".to_string(),
            if self.ignore_scale_minimum {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "FullSectors".to_string(),
            "boolean".to_string(),
            if self.full_sectors { "true" } else { "false" }.to_string(),
        ));
        params.push((
            "SymbolizedBoundaries".to_string(),
            "boolean".to_string(),
            if self.symbolized_boundaries {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "PlainBoundaries".to_string(),
            "boolean".to_string(),
            if !self.symbolized_boundaries {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "IsolatedDangers".to_string(),
            "boolean".to_string(),
            if self.isolated_dangers {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "SimplifiedSymbols".to_string(),
            "boolean".to_string(),
            if self.simplified_symbols {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "PaperChart".to_string(),
            "boolean".to_string(),
            if !self.simplified_symbols {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "ShallowWaterDangers".to_string(),
            "boolean".to_string(),
            if self.shallow_water_dangers {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "FullLightLines".to_string(),
            "boolean".to_string(),
            if self.full_light_lines {
                "true"
            } else {
                "false"
            }
            .to_string(),
        ));
        params.push((
            "NationalLanguage".to_string(),
            "text".to_string(),
            self.national_language.clone(),
        ));

        // Add custom parameters
        for (key, value) in &self.custom {
            let (type_str, value_str) = match value {
                ContextValue::Bool(b) => ("boolean".to_string(), b.to_string()),
                ContextValue::Integer(i) => ("integer".to_string(), i.to_string()),
                ContextValue::Real(r) => ("real".to_string(), r.to_string()),
                ContextValue::Text(s) => ("text".to_string(), s.clone()),
            };
            params.push((key.clone(), type_str, value_str));
        }

        params
    }

    pub fn set_palette(&mut self, palette: &str) {
        self.palette = palette.to_string();
    }

    pub fn set_custom(&mut self, key: &str, value: ContextValue) {
        self.custom.insert(key.to_string(), value);
    }

    pub fn get_custom(&self, key: &str) -> Option<&ContextValue> {
        self.custom.get(key)
    }
}

/// Feature item for portrayal processing
#[derive(Debug)]
pub struct FeaturePortrayalItem {
    /// Feature ID (from cell)
    pub feature_id: i64,
    /// Feature type code
    pub feature_code: String,
    /// Reference to feature record
    pub feature: FeatureRef,
    /// Observed context parameters during portrayal
    pub observed_parameters: Vec<String>,
}

/// Reference to a feature (for Lua access)
#[derive(Debug, Clone)]
pub struct FeatureRef {
    pub id: i64,
    pub code: String,
    pub primitive_type: PrimitiveType,
}

/// Primitive type for geometry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveType {
    None,
    Point,
    MultiPoint,
    Curve,
    CompositeCurve,
    Surface,
}

impl From<ferrite_s100_core::SpatialPrimitiveType> for PrimitiveType {
    fn from(spt: ferrite_s100_core::SpatialPrimitiveType) -> Self {
        match spt {
            ferrite_s100_core::SpatialPrimitiveType::Point => PrimitiveType::Point,
            ferrite_s100_core::SpatialPrimitiveType::MultiPoint => PrimitiveType::MultiPoint,
            ferrite_s100_core::SpatialPrimitiveType::Curve => PrimitiveType::Curve,
            ferrite_s100_core::SpatialPrimitiveType::CompositeCurve => {
                PrimitiveType::CompositeCurve
            }
            ferrite_s100_core::SpatialPrimitiveType::Surface => PrimitiveType::Surface,
            ferrite_s100_core::SpatialPrimitiveType::NoGeometry => PrimitiveType::None,
        }
    }
}

/// Portrayal context for a cell
pub struct PortrayalContext {
    /// Context parameters (mariner settings)
    pub parameters: ContextParameters,
    /// Features to portray
    pub features: Vec<FeaturePortrayalItem>,
    /// Reference to the cell
    cell: Arc<RwLock<CellData>>,
}

/// Cell data holder for Lua access
pub struct CellData {
    /// Feature ID → Feature Record mapping
    pub features: HashMap<i64, FeatureInfo>,
    /// Information Type ID → Information Record mapping
    pub information_types: HashMap<i64, InformationInfo>,
    /// Spatial data cache
    pub spatials: HashMap<i64, SpatialInfo>,
    /// Feature associations (source_id → [(target_id, association_code, role_code)])
    pub feature_associations: HashMap<i64, Vec<FeatureAssociation>>,
    /// Information associations (source_id → [(info_id, association_code, role_code)])
    pub information_associations: HashMap<i64, Vec<InformationAssociation>>,
    /// Spatial to feature reverse mapping
    pub spatial_to_features: HashMap<i64, Vec<i64>>,
}

/// Spatial reference with orientation (for Lua)
#[derive(Debug, Clone)]
pub struct SpatialRef {
    pub spatial_id: i64,
    pub spatial_type: PrimitiveType,
    /// Orientation: 1 = Forward, 2 = Reverse
    pub orientation: i8,
}

/// Feature info for Lua
#[derive(Debug, Clone)]
pub struct FeatureInfo {
    pub id: i64,
    pub code: String,
    pub primitive_type: PrimitiveType,
    /// Simple attributes (code → value)
    pub attributes: HashMap<String, AttributeValue>,
    /// Nested complex attributes (code → instances)
    /// Reference: S-100 standard's GF::ObjectType with recursive ComplexAttributeType
    pub complex_attributes: HashMap<String, Vec<ComplexAttribute>>,
    pub spatial_refs: Vec<SpatialRef>,
}

/// Information type info for Lua
#[derive(Debug, Clone)]
pub struct InformationInfo {
    pub id: i64,
    pub code: String,
    /// Simple attributes (code → value)
    pub attributes: HashMap<String, AttributeValue>,
    /// Nested complex attributes (code → instances)
    pub complex_attributes: HashMap<String, Vec<ComplexAttribute>>,
}

/// Attribute value that can be various types
#[derive(Debug, Clone)]
pub enum AttributeValue {
    Text(String),
    Integer(i64),
    Real(f64),
    Boolean(bool),
    Enumeration(i32, String), // (numeric_value, label)
    List(Vec<String>),
}

impl AttributeValue {
    pub fn as_string(&self) -> String {
        match self {
            AttributeValue::Text(s) => s.clone(),
            AttributeValue::Integer(i) => i.to_string(),
            AttributeValue::Real(r) => r.to_string(),
            AttributeValue::Boolean(b) => b.to_string(),
            AttributeValue::Enumeration(_, label) => label.clone(),
            AttributeValue::List(items) => items.join(","),
        }
    }
}

/// Nested complex attribute that can contain both simple and complex sub-attributes
/// Reference: S-100 standard's GF_ComplexAttributeType with recursive 'carries' vector
#[derive(Debug, Clone)]
pub struct ComplexAttribute {
    pub code: String,
    /// Simple sub-attributes (code → values). Uses Vec to support multi-valued attributes.
    /// Reference: S-101 allows UpperMultiplicity > 1 for simple attributes like 'colour'
    pub simple_attrs: HashMap<String, Vec<AttributeValue>>,
    /// Nested complex sub-attributes (code → instances)
    pub complex_attrs: HashMap<String, Vec<ComplexAttribute>>,
}

impl ComplexAttribute {
    pub fn new(code: String) -> Self {
        ComplexAttribute {
            code,
            simple_attrs: HashMap::new(),
            complex_attrs: HashMap::new(),
        }
    }

    /// Get count of sub-attributes with given code (simple or complex)
    pub fn get_sub_attribute_count(&self, code: &str) -> usize {
        // Check complex attributes first
        if let Some(instances) = self.complex_attrs.get(code) {
            return instances.len();
        }
        // Check simple attributes - return count of values
        if let Some(values) = self.simple_attrs.get(code) {
            return values.len();
        }
        0
    }

    /// Get nested complex attribute by code and 0-based index
    pub fn get_complex_attribute(&self, code: &str, index: usize) -> Option<&ComplexAttribute> {
        self.complex_attrs.get(code).and_then(|v| v.get(index))
    }

    /// Get all simple attribute values for a code
    pub fn get_simple_values(&self, code: &str) -> Option<&Vec<AttributeValue>> {
        self.simple_attrs.get(code)
    }

    /// Get first simple attribute value (for compatibility)
    pub fn get_simple_value(&self, code: &str) -> Option<&AttributeValue> {
        self.simple_attrs.get(code).and_then(|v| v.first())
    }
}

/// Feature association
#[derive(Debug, Clone)]
pub struct FeatureAssociation {
    pub target_id: i64,
    pub association_code: String,
    pub role_code: String,
}

/// Information association
#[derive(Debug, Clone)]
pub struct InformationAssociation {
    pub info_id: i64,
    pub association_code: String,
    pub role_code: String,
}

/// Curve association for composite curves
#[derive(Debug, Clone)]
pub struct CurveAssociation {
    pub curve_id: i64,
    pub rcnm: u8,          // 120=Curve, 125=CompositeCurve
    pub orientation: bool, // true=Forward, false=Reverse
}

/// Spatial info for Lua
#[derive(Debug, Clone)]
pub struct SpatialInfo {
    pub id: i64,
    pub spatial_type: PrimitiveType,
    /// 2D coordinates (x, y)
    pub coordinates: Vec<(f64, f64)>,
    /// Z coordinates (depth) for MultiPoint soundings - same length as coordinates
    pub z_coordinates: Vec<Option<f64>>,
    /// Curve associations for CompositeCurve types
    pub curve_associations: Vec<CurveAssociation>,
}

/// Extract attributes from S-100 attribute list, building nested complex attribute trees
///
/// IMPORTANT: The S-100 `paix` (parent attribute index) refers to the 1-based POSITION INDEX
/// of the parent attribute in the attribute list, NOT the `atix` value.
///
/// Reference: S-100 standard/GISLibrary/GF_ObjectType.cpp - builds recursive ComplexAttributeType tree
fn extract_attributes(
    attrs: &[ferrite_s100_core::Attribute],
    _feature_code: Option<&str>,
) -> (
    HashMap<String, AttributeValue>,
    HashMap<String, Vec<ComplexAttribute>>,
) {
    let mut simple_attrs = HashMap::new();
    let mut complex_attrs: HashMap<String, Vec<ComplexAttribute>> = HashMap::new();

    if attrs.is_empty() {
        return (simple_attrs, complex_attrs);
    }

    // Build children mapping using POSITION INDEX (1-based) as the parent reference
    // paix refers to the 1-based position of the parent in the attribute list
    let mut children_by_position: HashMap<usize, Vec<(usize, &ferrite_s100_core::Attribute)>> =
        HashMap::new();

    for (pos, attr) in attrs.iter().enumerate() {
        if attr.paix != 0 {
            // paix is the 1-based position index of the parent
            children_by_position
                .entry(attr.paix as usize)
                .or_default()
                .push((pos + 1, attr)); // Store position with attribute
        }
    }

    // Recursive helper to build complex attribute tree
    fn build_complex_attribute(
        attr: &ferrite_s100_core::Attribute,
        position: usize,
        children_by_position: &HashMap<usize, Vec<(usize, &ferrite_s100_core::Attribute)>>,
    ) -> ComplexAttribute {
        let code = attr.code.clone().unwrap_or_default();
        let mut complex = ComplexAttribute::new(code);

        // Process all children of this attribute
        if let Some(children) = children_by_position.get(&position) {
            for (child_pos, child) in children {
                let child_code = match &child.code {
                    Some(c) => c.clone(),
                    None => continue,
                };

                // Check if this child has its own children (making it a nested complex attribute)
                if children_by_position.contains_key(child_pos) {
                    // Nested complex attribute
                    let nested = build_complex_attribute(child, *child_pos, children_by_position);
                    complex
                        .complex_attrs
                        .entry(child_code)
                        .or_default()
                        .push(nested);
                } else {
                    // Simple sub-attribute - use push to collect multiple values
                    // S-101 allows multiple values for same attribute (e.g., colour = [1, 3])
                    complex
                        .simple_attrs
                        .entry(child_code)
                        .or_default()
                        .push(AttributeValue::Text(child.atvl.clone()));
                }
            }
        }

        complex
    }

    // Process root attributes (paix == 0)
    for (pos, attr) in attrs.iter().enumerate() {
        if attr.paix != 0 {
            continue; // Skip non-root attributes (children)
        }

        let Some(code) = &attr.code else {
            continue;
        };

        let position = pos + 1; // 1-based position for this attribute

        // Check if this root attribute has children using position index
        if children_by_position.contains_key(&position) {
            // Complex attribute - build the full tree recursively
            let complex = build_complex_attribute(attr, position, &children_by_position);
            complex_attrs.entry(code.clone()).or_default().push(complex);
        } else {
            // Simple attribute - no children
            simple_attrs.insert(code.clone(), AttributeValue::Text(attr.atvl.clone()));
        }
    }

    (simple_attrs, complex_attrs)
}

impl PortrayalContext {
    /// Create new portrayal context from S101 cell
    pub fn from_cell(cell: &S101Cell, parameters: ContextParameters) -> Self {
        let mut features = Vec::new();
        let mut cell_data = CellData {
            features: HashMap::new(),
            information_types: HashMap::new(),
            spatials: HashMap::new(),
            feature_associations: HashMap::new(),
            information_associations: HashMap::new(),
            spatial_to_features: HashMap::new(),
        };

        // Extract features
        for (key, feature) in &cell.features {
            let feature_code = feature
                .feature_code
                .clone()
                .unwrap_or_else(|| format!("UNKNOWN_{}", feature.frid.nftc));

            let primitive_type = PrimitiveType::from(feature.primitive_type);

            // Extract attributes - handle both simple and complex attributes
            // Complex attributes have child attributes (paix points to parent's atix)
            let (attributes, complex_attributes) =
                extract_attributes(&feature.attributes, Some(&feature_code));

            // Debug: trace LightSectored attribute structure
            if feature_code == "LightSectored" && *key == 429496838244 {
                tracing::warn!(
                    "LightSectored ID={}: {} simple attrs, {} complex attrs",
                    key,
                    attributes.len(),
                    complex_attributes.len()
                );
                for (code, instances) in &complex_attributes {
                    tracing::warn!("  complex attr '{}': {} instances", code, instances.len());
                    for (i, inst) in instances.iter().enumerate() {
                        tracing::warn!(
                            "    [{}] {} simple sub-attrs: {:?}",
                            i,
                            inst.simple_attrs.len(),
                            inst.simple_attrs.keys().collect::<Vec<_>>()
                        );
                        tracing::warn!(
                            "    [{}] {} complex sub-attrs: {:?}",
                            i,
                            inst.complex_attrs.len(),
                            inst.complex_attrs.keys().collect::<Vec<_>>()
                        );
                        // Show lightSector details
                        if let Some(light_sectors) = inst.complex_attrs.get("lightSector") {
                            for (j, ls) in light_sectors.iter().enumerate() {
                                tracing::warn!(
                                    "      lightSector[{}]: {} simple, {} complex",
                                    j,
                                    ls.simple_attrs.len(),
                                    ls.complex_attrs.len()
                                );
                                tracing::warn!(
                                    "        simple attrs: {:?}",
                                    ls.simple_attrs.keys().collect::<Vec<_>>()
                                );
                                tracing::warn!(
                                    "        complex attrs: {:?}",
                                    ls.complex_attrs.keys().collect::<Vec<_>>()
                                );
                                // Check for sectorLimit
                                if let Some(sector_limits) = ls.complex_attrs.get("sectorLimit") {
                                    for (k, sl) in sector_limits.iter().enumerate() {
                                        tracing::warn!(
                                            "          sectorLimit[{}]: {} simple, {} complex",
                                            k,
                                            sl.simple_attrs.len(),
                                            sl.complex_attrs.len()
                                        );
                                        tracing::warn!(
                                            "            simple: {:?}, complex: {:?}",
                                            sl.simple_attrs.keys().collect::<Vec<_>>(),
                                            sl.complex_attrs.keys().collect::<Vec<_>>()
                                        );
                                        // Check inside sectorLimitOne
                                        if let Some(sector_limit_ones) =
                                            sl.complex_attrs.get("sectorLimitOne")
                                        {
                                            for (l, slo) in sector_limit_ones.iter().enumerate() {
                                                tracing::warn!("              sectorLimitOne[{}]: {} simple, {} complex", l, slo.simple_attrs.len(), slo.complex_attrs.len());
                                                tracing::warn!(
                                                    "                simple: {:?}",
                                                    slo.simple_attrs.keys().collect::<Vec<_>>()
                                                );
                                            }
                                        } else {
                                            tracing::warn!(
                                                "              NO sectorLimitOne in sectorLimit"
                                            );
                                        }
                                    }
                                } else {
                                    tracing::warn!(
                                        "          NO sectorLimit found in lightSector!"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Extract spatial references with orientation
            let spatial_refs: Vec<SpatialRef> = feature
                .spatial_associations
                .iter()
                .map(|sa| {
                    // Determine spatial type from the RCNM in spatial_id
                    let spatial_type = match sa.spatial_id.rcnm {
                        110 => PrimitiveType::Point,
                        115 => PrimitiveType::MultiPoint,
                        120 => PrimitiveType::Curve,
                        125 => PrimitiveType::CompositeCurve,
                        130 => PrimitiveType::Surface,
                        _ => PrimitiveType::None,
                    };
                    SpatialRef {
                        spatial_id: sa.spatial_id.key(),
                        spatial_type,
                        orientation: sa.ornt,
                    }
                })
                .collect();

            // Build spatial → feature reverse mapping
            for spatial_ref in &spatial_refs {
                cell_data
                    .spatial_to_features
                    .entry(spatial_ref.spatial_id)
                    .or_default()
                    .push(*key);
            }

            // Extract feature associations (FASC field)
            let mut feat_assocs = Vec::new();
            for fasc in &feature.feature_associations {
                feat_assocs.push(FeatureAssociation {
                    target_id: fasc.feature_id.key(),
                    // Use numeric codes as strings (will be mapped later via FC if needed)
                    association_code: format!("{}", fasc.nfac),
                    role_code: format!("{}", fasc.narc),
                });
            }
            if !feat_assocs.is_empty() {
                cell_data.feature_associations.insert(*key, feat_assocs);
            }

            // Extract information associations (INAS field)
            let mut info_assocs = Vec::new();
            for inas in &feature.information_associations {
                info_assocs.push(InformationAssociation {
                    info_id: inas.info_id.key(),
                    // Use numeric codes as strings (will be mapped later via FC if needed)
                    association_code: format!("{}", inas.niac),
                    role_code: format!("{}", inas.narc),
                });
            }
            if !info_assocs.is_empty() {
                cell_data.information_associations.insert(*key, info_assocs);
            }

            let feature_info = FeatureInfo {
                id: *key,
                code: feature_code.clone(),
                primitive_type,
                attributes,
                complex_attributes,
                spatial_refs,
            };

            cell_data.features.insert(*key, feature_info);

            features.push(FeaturePortrayalItem {
                feature_id: *key,
                feature_code: feature_code.clone(),
                feature: FeatureRef {
                    id: *key,
                    code: feature_code,
                    primitive_type,
                },
                observed_parameters: Vec::new(),
            });
        }

        // Extract information types
        for (key, info) in &cell.information {
            let info_code = info
                .info_code
                .clone()
                .unwrap_or_else(|| format!("UNKNOWN_{}", info.irid.nitc));

            let (attributes, complex_attributes) =
                extract_attributes(&info.attributes, Some(&info_code));

            cell_data.information_types.insert(
                *key,
                InformationInfo {
                    id: *key,
                    code: info_code,
                    attributes,
                    complex_attributes,
                },
            );
        }

        // Extract point spatials
        for (key, point) in &cell.points {
            cell_data.spatials.insert(
                *key,
                SpatialInfo {
                    id: *key,
                    spatial_type: PrimitiveType::Point,
                    coordinates: vec![(point.position.x, point.position.y)],
                    z_coordinates: vec![point.position.z],
                    curve_associations: Vec::new(),
                },
            );
        }

        // Extract multi-point spatials (for Sounding features)
        // Reference: S-100 standard uses multiPoint for Sounding with multiple depth values
        for (key, multi_point) in &cell.multi_points {
            let coords: Vec<(f64, f64)> =
                multi_point.positions.iter().map(|c| (c.x, c.y)).collect();
            let z_coords: Vec<Option<f64>> = multi_point.positions.iter().map(|c| c.z).collect();
            cell_data.spatials.insert(
                *key,
                SpatialInfo {
                    id: *key,
                    spatial_type: PrimitiveType::MultiPoint,
                    coordinates: coords,
                    z_coordinates: z_coords,
                    curve_associations: Vec::new(),
                },
            );
        }

        // Extract curve spatials
        for (key, curve) in &cell.curves {
            let coords: Vec<(f64, f64)> =
                curve.all_positions().iter().map(|c| (c.x, c.y)).collect();
            cell_data.spatials.insert(
                *key,
                SpatialInfo {
                    id: *key,
                    spatial_type: PrimitiveType::Curve,
                    coordinates: coords,
                    z_coordinates: Vec::new(), // Curves don't have Z
                    curve_associations: Vec::new(),
                },
            );
        }

        // Extract composite curve spatials (following S-100 standard's host_data.cpp pattern)
        // Reference: S-100 standard/GISLibrary/host_data.cpp - hd_get_composite_curve()
        for (key, composite) in &cell.composite_curves {
            // Build curve associations from CUCO records (oriented curves)
            let associations: Vec<CurveAssociation> = composite
                .curves
                .iter()
                .map(|oriented_curve| CurveAssociation {
                    curve_id: oriented_curve.curve_id.key(),
                    rcnm: oriented_curve.curve_id.rcnm,
                    orientation: oriented_curve.orientation,
                })
                .collect();

            cell_data.spatials.insert(
                *key,
                SpatialInfo {
                    id: *key,
                    spatial_type: PrimitiveType::CompositeCurve,
                    coordinates: Vec::new(), // CompositeCurve derives coords from member curves
                    z_coordinates: Vec::new(),
                    curve_associations: associations,
                },
            );
        }

        // Extract surface spatials (for Surface primitive type)
        for key in cell.surfaces.keys() {
            // Surface coordinates are derived from curves, just register the ID
            cell_data.spatials.insert(
                *key,
                SpatialInfo {
                    id: *key,
                    spatial_type: PrimitiveType::Surface,
                    coordinates: Vec::new(), // Surfaces derive coords from ring curves
                    z_coordinates: Vec::new(),
                    curve_associations: Vec::new(),
                },
            );
        }

        PortrayalContext {
            parameters,
            features,
            cell: Arc::new(RwLock::new(cell_data)),
        }
    }

    /// Get feature info by ID
    pub fn get_feature(&self, id: i64) -> Option<FeatureInfo> {
        self.cell.read().ok()?.features.get(&id).cloned()
    }

    /// Get information type by ID
    pub fn get_information(&self, id: i64) -> Option<InformationInfo> {
        self.cell.read().ok()?.information_types.get(&id).cloned()
    }

    /// Get spatial info by ID
    pub fn get_spatial(&self, id: i64) -> Option<SpatialInfo> {
        self.cell.read().ok()?.spatials.get(&id).cloned()
    }

    /// Get all feature IDs
    pub fn feature_ids(&self) -> Vec<i64> {
        self.features.iter().map(|f| f.feature_id).collect()
    }

    /// Get number of features
    pub fn feature_count(&self) -> usize {
        self.features.len()
    }

    /// Get associated feature IDs for a feature
    pub fn get_feature_associations(
        &self,
        feature_id: i64,
        association_code: Option<&str>,
        role_code: Option<&str>,
    ) -> Vec<i64> {
        self.cell
            .read()
            .ok()
            .and_then(|data| {
                data.feature_associations.get(&feature_id).map(|assocs| {
                    assocs
                        .iter()
                        .filter(|a| {
                            association_code.is_none_or(|c| a.association_code == c)
                                && role_code.is_none_or(|r| a.role_code == r)
                        })
                        .map(|a| a.target_id)
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// Get associated information IDs for a feature
    pub fn get_information_associations(
        &self,
        feature_id: i64,
        association_code: Option<&str>,
        role_code: Option<&str>,
    ) -> Vec<i64> {
        self.cell
            .read()
            .ok()
            .and_then(|data| {
                data.information_associations
                    .get(&feature_id)
                    .map(|assocs| {
                        assocs
                            .iter()
                            .filter(|a| {
                                association_code.is_none_or(|c| a.association_code == c)
                                    && role_code.is_none_or(|r| a.role_code == r)
                            })
                            .map(|a| a.info_id)
                            .collect()
                    })
            })
            .unwrap_or_default()
    }

    /// Get feature IDs associated with a spatial
    pub fn get_spatial_features(&self, spatial_id: i64) -> Vec<i64> {
        self.cell
            .read()
            .ok()
            .and_then(|data| data.spatial_to_features.get(&spatial_id).cloned())
            .unwrap_or_default()
    }

    /// Get access to cell data (for host functions)
    pub fn cell_data(&self) -> Arc<RwLock<CellData>> {
        self.cell.clone()
    }
}
