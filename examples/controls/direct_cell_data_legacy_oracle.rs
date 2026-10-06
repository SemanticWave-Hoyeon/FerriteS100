//! Explicit original shipping321 extraction oracle; example only, never linked by App.
//! Original algorithm and order retained; private local context substitutes for
//! original private PortrayalContext fields. No candidate CellData builder calls.
use ferrite_lua::*;
use ferrite_s100_core::S101Cell;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
#[allow(dead_code)]
pub struct LegacyContext {
    pub parameters: ContextParameters,
    pub features: Vec<FeaturePortrayalItem>,
    cell: Arc<RwLock<CellData>>,
}
impl LegacyContext {
    pub fn into_cell_data(self) -> ferrite_lua::Result<CellData> {
        match Arc::try_unwrap(self.cell) {
            Ok(lock) => lock
                .into_inner()
                .map_err(|_| ferrite_lua::LuaError::Portrayal("Cell data lock poisoned".into())),
            Err(shared) => {
                let data = shared.read().map_err(|_| {
                    ferrite_lua::LuaError::Portrayal("Cell data lock poisoned".into())
                })?;
                Ok(data.clone())
            }
        }
    }
    pub fn cell_data(&self) -> Arc<RwLock<CellData>> {
        self.cell.clone()
    }
}
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
pub fn from_cell(cell: &S101Cell, parameters: ContextParameters) -> LegacyContext {
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
                    scale_minimum: sa.scale_minimum,
                    scale_maximum: sa.scale_maximum,
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
                z_coordinates: vec![point.position.depth()],
                curve_associations: Vec::new(),
                interior_curve_associations: Vec::new(),
            },
        );
    }

    // Extract multi-point spatials (for Sounding features)
    // Reference: S-100 standard uses multiPoint for Sounding with multiple depth values
    for (key, multi_point) in &cell.multi_points {
        let coords: Vec<(f64, f64)> = multi_point.positions.iter().map(|c| (c.x, c.y)).collect();
        let z_coords: Vec<Option<f64>> = multi_point.positions.iter().map(|c| c.depth()).collect();
        cell_data.spatials.insert(
            *key,
            SpatialInfo {
                id: *key,
                spatial_type: PrimitiveType::MultiPoint,
                coordinates: coords,
                z_coordinates: z_coords,
                curve_associations: Vec::new(),
                interior_curve_associations: Vec::new(),
            },
        );
    }

    // Extract curve spatials
    for (key, curve) in &cell.curves {
        let coords: Vec<(f64, f64)> = curve.positions_iter().map(|c| (c.x, c.y)).collect();
        cell_data.spatials.insert(
            *key,
            SpatialInfo {
                id: *key,
                spatial_type: PrimitiveType::Curve,
                coordinates: coords,
                z_coordinates: Vec::new(), // Curves don't have Z
                curve_associations: Vec::new(),
                interior_curve_associations: Vec::new(),
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
                interior_curve_associations: Vec::new(),
            },
        );
    }

    // Extract surface spatials (for Surface primitive type)
    for (key, surface) in &cell.surfaces {
        // Preserve real surface boundary references for Lua geometry access
        cell_data.spatials.insert(
            *key,
            SpatialInfo {
                id: *key,
                spatial_type: PrimitiveType::Surface,
                coordinates: Vec::new(), // Surfaces derive coords from ring curves
                z_coordinates: Vec::new(),
                curve_associations: surface
                    .exterior_ring
                    .iter()
                    .map(|c| CurveAssociation {
                        curve_id: c.curve_id.key(),
                        rcnm: c.curve_id.rcnm,
                        orientation: c.orientation,
                    })
                    .collect(),
                interior_curve_associations: surface
                    .interior_rings
                    .iter()
                    .map(|ring| {
                        ring.iter()
                            .map(|c| CurveAssociation {
                                curve_id: c.curve_id.key(),
                                rcnm: c.curve_id.rcnm,
                                orientation: c.orientation,
                            })
                            .collect()
                    })
                    .collect(),
            },
        );
    }

    LegacyContext {
        parameters,
        features,
        cell: Arc::new(RwLock::new(cell_data)),
    }
}
