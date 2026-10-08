use crate::{
    service::{Dataset, Registry},
    Indices,
};
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_core::*;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
fn fixture(name: &str, label: &str, lon: f64) -> Dataset {
    let mut cell = S101Cell {
        file_path: format!("{name}.000").into(),
        dsid: DatasetIdentification {
            dataset_name: name.into(),
            edition_number: 2,
            update_number: 3,
            ..Default::default()
        },
        code_mappings: Default::default(),
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
    let point = RecordId::new(110, 1);
    cell.points.insert(
        point.key(),
        PointRecord {
            id: point,
            position: Coordinate::new(lon, 50.0),
            update_instruction: 1,
        },
    );
    cell.features.insert(
        1,
        FeatureRecord {
            frid: FRID {
                rcid: 1,
                nftc: 1,
                rver: 1,
                ruin: 1,
            },
            foid: None,
            feature_code: Some("TestPoint".into()),
            primitive_type: SpatialPrimitiveType::Point,
            attributes: vec![Attribute {
                natc: 1,
                atix: 1,
                paix: 0,
                atvl: label.into(),
                value: None,
                code: Some("unmappedValue".into()),
            }],
            spatial_associations: vec![SpatialAssociation {
                spatial_id: point,
                ornt: 1,
                usag: 1,
                mask: 0,
                scale_minimum: None,
                scale_maximum: None,
                update_instruction: 1,
            }],
            information_associations: vec![],
            feature_associations: vec![],
            masks: vec![],
        },
    );
    let fc = FeatureCatalogue {
        source_path: "test-fc.xml".into(),
        name: "synthetic test catalogue".into(),
        scope: String::new(),
        version: "2.0.0".into(),
        version_date: String::new(),
        product_id: "S-101".into(),
        simple_attributes: Default::default(),
        complex_attributes: Default::default(),
        feature_types: Default::default(),
        information_types: Default::default(),
    };
    Dataset {
        id: name.into(),
        product: "S-101".into(),
        metadata: json!({"edition":2,"update":3}),
        s101: Some(Arc::new(Indices::build(cell, fc))),
    }
}
#[test]
fn same_feature_id_is_scoped_to_dataset() {
    let r = Registry::new([fixture("a", "alpha", -1.0), fixture("b", "beta", 2.0)]).unwrap();
    for (id, value) in [("a", "alpha"), ("b", "beta")] {
        let out = r
            .call("feature_get", json!({"dataset_id":id,"id":1}))
            .unwrap();
        assert_eq!(out["result"]["attributes"][0]["value"], value);
    }
    assert!(r.call("feature_get", json!({"id":1})).is_err());
}
#[test]
fn spatial_type_and_count_queries_use_indexed_loaded_copy() {
    let r = Registry::new([fixture("a", "raw", -1.0)]).unwrap();
    for (tool, args) in [
        ("feature_query_bbox", json!({"w":-2,"s":49,"e":0,"n":51})),
        ("feature_query_by_type", json!({"type":"testpoint"})),
        ("feature_nearby", json!({"lat":50,"lon":-1,"radius_m":1})),
    ] {
        assert_eq!(r.call(tool, args).unwrap()["result"]["matched"], 1);
    }
    assert_eq!(
        r.call("feature_count", json!({})).unwrap()["result"]["count"],
        1
    );
}
#[test]
fn catalogue_misses_and_missing_feature_remain_explicit() {
    let r = Registry::new([fixture("a", "raw", -1.0)]).unwrap();
    for tool in ["catalogue_describe_feature", "catalogue_describe_attribute"] {
        assert_eq!(
            r.call(tool, json!({"code":"doesNotExist"})).unwrap()["result"]["found"],
            false
        );
    }
    assert_eq!(
        r.call("feature_get", json!({"id":999})).unwrap()["result"]["found"],
        false
    );
    assert!(r.call("catalogue_search", json!({"term":"absent"})).is_ok());
}
