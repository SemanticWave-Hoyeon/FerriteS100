//! Independent original extraction path; test-only.
use super::*;
fn legacy_context(cell: &S101Cell, parameters: ContextParameters) -> PortrayalContext {
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

    PortrayalContext {
        parameters,
        features,
        cell: Arc::new(RwLock::new(cell_data)),
    }
}

fn ordered<K: Ord + Clone, V>(map: &HashMap<K, V>, f: impl Fn(&V) -> String) -> Vec<(K, String)> {
    let mut values: Vec<_> = map.iter().map(|(k, v)| (k.clone(), f(v))).collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}
fn complex(value: &ComplexAttribute) -> String {
    format!(
        "{:?}",
        (
            &value.code,
            ordered(&value.simple_attrs, |v| format!("{v:?}")),
            ordered(&value.complex_attrs, |v| format!(
                "{:?}",
                v.iter().map(complex).collect::<Vec<_>>()
            ))
        )
    )
}
fn canonical(data: &CellData) -> String {
    format!(
        "{:?}",
        (
            ordered(&data.features, |v| format!(
                "{:?}",
                (
                    v.id,
                    &v.code,
                    v.primitive_type,
                    ordered(&v.attributes, |a| format!("{a:?}")),
                    ordered(&v.complex_attributes, |a| format!(
                        "{:?}",
                        a.iter().map(complex).collect::<Vec<_>>()
                    )),
                    &v.spatial_refs
                )
            )),
            ordered(&data.information_types, |v| format!(
                "{:?}",
                (
                    v.id,
                    &v.code,
                    ordered(&v.attributes, |a| format!("{a:?}")),
                    ordered(&v.complex_attributes, |a| format!(
                        "{:?}",
                        a.iter().map(complex).collect::<Vec<_>>()
                    ))
                )
            )),
            ordered(&data.spatials, |v| format!("{v:?}")),
            ordered(&data.feature_associations, |v| format!("{v:?}")),
            ordered(&data.information_associations, |v| format!("{v:?}")),
            ordered(&data.spatial_to_features, |v| format!("{v:?}"))
        )
    )
}
fn fixture() -> S101Cell {
    use ferrite_s100_core::*;
    let mut cell = S101Cell {
        file_path: Default::default(),
        dsid: Default::default(),
        code_mappings: DatasetCodeMappings::new(),
        coord_factor: 1.,
        coord_factor_y: 1.,
        coord_factor_z: 1.,
        coord_origin_x: 0.,
        coord_origin_y: 0.,
        coord_origin_z: 0.,
        minimum_display_scale: None,
        maximum_display_scale: None,
        points: Default::default(),
        multi_points: Default::default(),
        curves: Default::default(),
        composite_curves: Default::default(),
        surfaces: Default::default(),
        features: Default::default(),
        information: Default::default(),
        spatial_information_associations: Default::default(),
    };
    let point = RecordId::new(110, 1);
    cell.points.insert(
        point.key(),
        PointRecord {
            id: point,
            position: Coordinate::new(-0., 51.2),
            update_instruction: 0,
        },
    );
    let attrs = vec![
        Attribute {
            natc: 1,
            atix: 1,
            paix: 0,
            atvl: String::new(),
            value: None,
            code: Some("featureName".into()),
        },
        Attribute {
            natc: 2,
            atix: 2,
            paix: 1,
            atvl: "한국&name".into(),
            value: None,
            code: Some("name".into()),
        },
        Attribute {
            natc: 3,
            atix: 3,
            paix: 0,
            atvl: "7".into(),
            value: None,
            code: Some("valueOfSounding".into()),
        },
    ];
    for (id, code) in [(7, Some("Wreck".to_string())), (3, None)] {
        cell.features.insert(
            id,
            FeatureRecord {
                frid: FRID {
                    rcid: id as u32,
                    nftc: 19,
                    rver: 1,
                    ruin: 0,
                },
                foid: None,
                attributes: attrs.clone(),
                spatial_associations: vec![
                    SpatialAssociation {
                        spatial_id: point,
                        ornt: 1,
                        usag: 1,
                        mask: 0,
                        scale_minimum: Some(10),
                        scale_maximum: Some(1000),
                        update_instruction: 0,
                    },
                    SpatialAssociation {
                        spatial_id: point,
                        ornt: 2,
                        usag: 1,
                        mask: 0,
                        scale_minimum: None,
                        scale_maximum: None,
                        update_instruction: 0,
                    },
                ],
                information_associations: vec![InformationAssociation {
                    niac: 1,
                    narc: 2,
                    info_id: RecordId::new(150, 2),
                    update_instruction: 0,
                    attributes: Vec::new(),
                }],
                feature_associations: vec![FeatureAssociation {
                    nfac: 5,
                    narc: 4,
                    feature_id: RecordId::new(100, 3),
                    update_instruction: 0,
                    attributes: Vec::new(),
                }],
                masks: Vec::new(),
                feature_code: code,
                primitive_type: SpatialPrimitiveType::Point,
            },
        );
    }
    cell.information.insert(
        2,
        InformationRecord {
            irid: IRID {
                rcid: 2,
                nitc: 11,
                rver: 1,
                ruin: 0,
            },
            attributes: attrs,
            information_associations: Vec::new(),
            info_code: None,
        },
    );
    cell
}
#[test]
fn independent_original_owned_fields_order_complex_attributes_associations_and_fallback_codes_match(
) {
    let cell = fixture();
    let before = format!("{cell:?}");
    let original = legacy_context(&cell, ContextParameters::default());
    let legacy_items = format!("{:?}", original.features);
    let old = original.into_cell_data().unwrap();
    let direct = CellData::from_cell(&cell);
    assert_eq!(canonical(&direct), canonical(&old));
    assert_eq!(direct.features[&3].code, "UNKNOWN_19");
    assert_eq!(direct.information_types[&2].code, "UNKNOWN_11");
    let context = PortrayalContext::from_cell(&cell, ContextParameters::default());
    assert_eq!(format!("{:?}", context.features), legacy_items);
    assert_eq!(context.features.len(), 2);
    let (unused, items) = build_owned_cell_data(&cell, false);
    assert_eq!(items.capacity(), 0);
    assert_eq!(canonical(&unused), canonical(&direct));
    assert_eq!(format!("{cell:?}"), before);
}
#[test]
fn direct_owned_mutation_and_rebuild_do_not_alias_source_or_another_host_snapshot() {
    let cell = fixture();
    let original = canonical(
        &legacy_context(&cell, ContextParameters::default())
            .into_cell_data()
            .unwrap(),
    );
    let mut first = CellData::from_cell(&cell);
    let second = CellData::from_cell(&cell);
    first.features.get_mut(&7).unwrap().code = "Changed".into();
    first
        .spatial_to_features
        .values_mut()
        .next()
        .unwrap()
        .clear();
    first
        .features
        .get_mut(&7)
        .unwrap()
        .complex_attributes
        .clear();
    assert_eq!(canonical(&second), original);
    assert_eq!(canonical(&CellData::from_cell(&cell)), original);
    assert_ne!(canonical(&first), original);
}

#[test]
fn fresh_host_legacy_and_direct_normal_failed_exception_and_recovery_results_agree() {
    use crate::{PortrayalEngine, TypeCatalogue};
    let root = std::env::temp_dir().join(format!(
        "ferrite-direct-data-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    std::fs::create_dir_all(root.join("Rules")).unwrap();
    std::fs::write(root.join("Rules/main.lua"),r#"
        iteration=0
        function PortrayalCreateContextParameter(name,kind,value) return {name=name,kind=kind,value=value} end
        function PortrayalInitializeContextParameters(params) end
        function PortrayalMain()
            iteration=iteration+1
            local decimal = HostGetContextParameter('SafetyDepth')
            assert(type(decimal) == 'table' and decimal.Type == 'ScaledDecimal')
            assert(type(decimal.ToNumber) == 'function')
            local depth = decimal.ToNumber()
            assert(type(depth) == 'number' and depth == decimal.Value / (10 ^ decimal.Scale))
            for _,id in ipairs(HostGetFeatureIDs()) do
                HostPortrayalEmit(tostring(id),'TextInstruction:'..HostFeatureGetCode(id)..':'..tostring(iteration)..':'..tostring(depth),'IsolatedDangers')
            end
            if depth < 0 then error('injected conversion lifecycle exception') end
            if not HostGetContextParameter('IsolatedDangers') then return false end
            return true
        end
    "#).unwrap();
    let sources = ferrite_portrayal_catalog::CatalogueSources::capture(&root).unwrap();
    let mut legacy = PortrayalEngine::new_with_sources(Arc::clone(&sources)).unwrap();
    let mut direct = PortrayalEngine::new_with_sources(sources).unwrap();
    for engine in [&mut legacy, &mut direct] {
        engine.set_type_catalogue(TypeCatalogue::default());
        engine.initialize().unwrap();
    }
    let cell = fixture();
    let before = format!("{cell:?}");
    for (danger, depth) in [
        (true, 30.),
        (true, 30.5),
        (false, 30.),
        (true, -1.),
        (true, 30.),
    ] {
        let ctx = ContextParameters {
            isolated_dangers: danger,
            safety_depth: depth,
            ..Default::default()
        };
        let old = legacy.process_owned_cell(
            legacy_context(&cell, ctx.clone()).into_cell_data().unwrap(),
            ctx.clone(),
        );
        let new = direct.process_owned_cell(CellData::from_cell(&cell), ctx);
        if danger && depth >= 0. {
            let old = old.unwrap();
            let new = new.unwrap();
            assert_eq!(format!("{old:?}"), format!("{new:?}"));
            assert_eq!(new.len(), 2);
            let typed = format!("{new:?}");
            assert!(typed.contains(":1:"));
            assert!(typed.contains(&format!(":{}", depth)));
        } else {
            let old_error = old.unwrap_err().to_string();
            let new_error = new.unwrap_err().to_string();
            assert_eq!(old_error, new_error);
            if depth < 0. {
                assert!(new_error.contains("injected conversion lifecycle exception"));
            } else {
                assert!(new_error.contains("PortrayalMain terminated before completion"));
            }
            assert!(!new_error.contains("attempt to compare table with number"));
        }
        assert_eq!(format!("{cell:?}"), before);
    }
}
