//! Parser regression tests against the Feature Catalogues shipped in `Catalogues/FC`.

use ferrite_feature_catalog::*;

fn fc_path(rel: &str) -> String {
    format!("{}/../../Catalogues/FC/{rel}", env!("CARGO_MANIFEST_DIR"))
}

fn load(rel: &str) -> (FeatureCatalogue, FeatureCatalogueExtras) {
    FeatureCatalogue::load_with_extras(fc_path(rel)).unwrap()
}

#[test]
fn s101_counts_unchanged_and_new_fields_populated() {
    let (fc, x) = load("S-101/101_Feature_Catalogue_2.0.0.xml");
    // Counts/metadata as parsed before binding/permitted-value support.
    assert_eq!(fc.name, "Electronic Navigational Chart");
    assert_eq!(fc.product_id, "S-101");
    assert_eq!(fc.version, "2.0.0");
    assert_eq!(fc.feature_types.len(), 190);
    assert_eq!(fc.simple_attributes.len(), 236);
    assert_eq!(fc.complex_attributes.len(), 42);
    assert_eq!(fc.information_types.len(), 5);
    let attr_bindings: usize = fc
        .feature_types
        .values()
        .map(|f| f.attribute_bindings.len())
        .sum::<usize>()
        + fc.information_types
            .values()
            .map(|i| i.attribute_bindings.len())
            .sum::<usize>();
    assert!(attr_bindings > 0);
    assert!(fc
        .feature_types
        .values()
        .flat_map(|f| &f.attribute_bindings)
        .all(|b| !b.attribute_code.is_empty()));

    // Newly populated.
    let feature_bindings: usize = fc
        .feature_types
        .values()
        .map(|f| f.feature_bindings.len())
        .sum();
    let info_bindings: usize = fc
        .feature_types
        .values()
        .map(|f| f.information_bindings.len())
        .sum::<usize>()
        + fc.information_types
            .values()
            .map(|i| i.information_bindings.len())
            .sum::<usize>();
    assert_eq!(feature_bindings, 532);
    assert_eq!(info_bindings, 180);
    assert!(fc
        .feature_types
        .values()
        .flat_map(|f| &f.feature_bindings)
        .all(|b| !b.feature_type_code.is_empty() && b.association.is_some() && b.role.is_some()));
    let permitted: usize = fc
        .feature_types
        .values()
        .flat_map(|f| &f.attribute_bindings)
        .chain(
            fc.information_types
                .values()
                .flat_map(|i| &i.attribute_bindings),
        )
        .chain(
            fc.complex_attributes
                .values()
                .flat_map(|c| &c.sub_attributes),
        )
        .filter(|b| !b.permitted_values.is_empty())
        .count();
    assert_eq!(permitted, 588);
    let point_sets = fc
        .feature_types
        .values()
        .filter(|f| f.permitted_primitives.contains(&SpatialPrimitive::PointSet))
        .count();
    assert_eq!(point_sets, 3);
    let truncated = fc
        .simple_attributes
        .values()
        .filter(|a| a.value_type == AttributeValueType::S100TruncatedDate)
        .count();
    assert_eq!(truncated, 8);
    assert_eq!(x.roles.len(), 14);
    assert_eq!(x.feature_associations.len(), 18);
    assert_eq!(x.information_associations.len(), 3);
    assert_eq!(x.units_of_measure.len(), 52);
    assert_eq!(x.attribute_constraints.len(), 28);
    assert!(fc
        .simple_attributes
        .values()
        .filter(|a| a.uom.is_some())
        .all(|a| x.units_of_measure.contains_key(&a.code)));
}

#[test]
fn s124_navwarn_part_bindings() {
    let (fc, x) = load("S-124/124_FC_2.0.0.xml");
    assert_eq!(fc.name, "Navigational Warnings");
    let part = &fc.feature_types["NavwarnPart"];

    assert_eq!(part.information_bindings.len(), 1);
    let header = &part.information_bindings[0];
    assert_eq!(header.information_type_code, "NavwarnPreamble");
    assert_eq!(header.role.as_deref(), Some("header"));
    assert_eq!(
        header.association.as_deref(),
        Some("navwarnPreambleContent")
    );
    assert_eq!(header.role_type, RoleType::Association);
    assert_eq!(
        (header.multiplicity.lower, header.multiplicity.upper),
        (1, Some(1))
    );

    let affects = part
        .feature_bindings
        .iter()
        .find(|b| b.role.as_deref() == Some("affects"))
        .unwrap();
    assert_eq!(affects.feature_type_code, "NavwarnAreaAffected");
    assert_eq!(affects.association.as_deref(), Some("areaAffected"));
    assert_eq!(
        (affects.multiplicity.lower, affects.multiplicity.upper),
        (0, None)
    );
    let text = part
        .feature_bindings
        .iter()
        .find(|b| b.role.as_deref() == Some("thePositionProvider"))
        .unwrap();
    assert_eq!(text.feature_type_code, "TextPlacement");
    assert_eq!(text.association.as_deref(), Some("TextAssociation"));

    let restriction = part
        .attribute_bindings
        .iter()
        .find(|b| b.attribute_code == "restriction")
        .unwrap();
    assert_eq!(restriction.permitted_values, vec![7, 8, 14, 25, 27]);

    let references = &fc.information_types["References"];
    assert_eq!(references.information_bindings.len(), 1);
    assert_eq!(
        references.information_bindings[0].information_type_code,
        "NavwarnPreamble"
    );
    assert_eq!(
        references.information_bindings[0].role.as_deref(),
        Some("theWarning")
    );

    assert_eq!(x.roles.len(), 8);
    assert_eq!(
        x.roles["header"].definition.as_deref(),
        Some("The header of a navigational warning.")
    );
    assert_eq!(
        x.information_associations["navwarnPreambleContent"].roles,
        vec!["header", "theWarningPart"]
    );
    assert_eq!(
        x.feature_associations["areaAffected"].roles,
        vec!["impacts", "affects"]
    );
    assert_eq!(
        fc.simple_attributes["dateStart"].value_type,
        AttributeValueType::S100TruncatedDate
    );
}

#[test]
fn s129_bindings_uom_and_constraints() {
    let (fc, x) = load("S-129/129_Feature_Catalogue_2.0.0.XML");
    assert_eq!(fc.name, "Under Keel Clearance Management");
    let plan = &fc.feature_types["UnderKeelClearancePlan"];
    assert_eq!(plan.feature_bindings.len(), 4);
    assert!(plan
        .feature_bindings
        .iter()
        .all(|b| b.role.as_deref() == Some("theComponent")));
    let cp = plan
        .feature_bindings
        .iter()
        .find(|b| b.feature_type_code == "UnderKeelClearanceControlPoint")
        .unwrap();
    assert_eq!((cp.multiplicity.lower, cp.multiplicity.upper), (1, None));
    assert_eq!(
        cp.association.as_deref(),
        Some("UnderKeelClearanceControlPointComposition")
    );
    let area = &fc.feature_types["UnderKeelClearancePlanArea"];
    assert_eq!(area.feature_bindings[0].role_type, RoleType::Composition);
    assert_eq!(
        area.feature_bindings[0].feature_type_code,
        "UnderKeelClearancePlan"
    );

    let draught = &fc.simple_attributes["maximumDraught"];
    assert_eq!(draught.uom.as_deref(), Some("m"));
    assert_eq!(
        x.units_of_measure["maximumDraught"].name.as_deref(),
        Some("metre")
    );
    let range = draught.quantitative_range.as_ref().unwrap();
    assert_eq!((range.minimum, range.maximum), (Some(0.0), None));
    assert_eq!(
        x.attribute_constraints["maximumDraught"]
            .range
            .as_ref()
            .unwrap()
            .closure
            .as_deref(),
        Some("gtSemiInterval")
    );
    assert_eq!(x.roles.len(), 2);
    assert_eq!(x.feature_associations.len(), 4);
}

fn parse(xml: &str) -> (FeatureCatalogue, FeatureCatalogueExtras) {
    FeatureCatalogue::from_bytes_with_extras("inline.xml", xml.as_bytes()).unwrap()
}

#[test]
fn catalogue_name_only_from_top_level() {
    let (fc, _) = parse(
        "<S100FC:S100_FC_FeatureCatalogue xmlns:S100FC=\"x\" xmlns:S100CI=\"y\">\
         <S100FC:producer><S100CI:party><S100CI:CI_Organisation><S100CI:name>KMOU</S100CI:name>\
         </S100CI:CI_Organisation></S100CI:party></S100FC:producer>\
         <S100FC:S100_FC_Roles><S100FC:S100_FC_Role><S100FC:name>r</S100FC:name><S100FC:code>r</S100FC:code></S100FC:S100_FC_Role></S100FC:S100_FC_Roles>\
         <S100FC:productId>S-129</S100FC:productId></S100FC:S100_FC_FeatureCatalogue>",
    );
    assert_eq!(fc.name, "");
    assert_eq!(fc.product_id, "S-129");
}

#[test]
fn split_text_uom_and_value_types() {
    let (fc, x) = parse(
        "<S100_FC_FeatureCatalogue><name>A &amp; <![CDATA[B]]></name>\
         <S100_FC_SimpleAttributes>\
         <S100_FC_SimpleAttribute><code>depth</code><name>Depth</name><valueType>real</valueType>\
         <uom><name>metre</name><symbol>m</symbol></uom>\
         <constraints><range><lowerBound>-30.0</lowerBound><upperBound>12500</upperBound><closure>closedInterval</closure></range></constraints>\
         </S100_FC_SimpleAttribute>\
         <S100_FC_SimpleAttribute><code>legacy</code><uom>kn</uom><valueType>S100_TruncatedDate</valueType></S100_FC_SimpleAttribute>\
         </S100_FC_SimpleAttributes>\
         <S100_FC_FeatureTypes><S100_FC_FeatureType><code>F</code>\
         <permittedPrimitives>pointSet</permittedPrimitives><permittedPrimitives>arcByCenterPoint</permittedPrimitives>\
         <permittedPrimitives>circleByCenterPoint</permittedPrimitives>\
         <attributeBinding><multiplicity><lower>0</lower><upper infinite=\"true\"></upper></multiplicity><attribute ref=\"depth\"/></attributeBinding>\
         </S100_FC_FeatureType></S100_FC_FeatureTypes>\
         </S100_FC_FeatureCatalogue>",
    );
    assert_eq!(fc.name, "A & B");
    let depth = &fc.simple_attributes["depth"];
    assert_eq!(depth.name, "Depth");
    assert_eq!(depth.uom.as_deref(), Some("m"));
    let r = depth.quantitative_range.as_ref().unwrap();
    assert_eq!((r.minimum, r.maximum), (Some(-30.0), Some(12500.0)));
    assert_eq!(
        x.attribute_constraints["depth"]
            .range
            .as_ref()
            .unwrap()
            .closure
            .as_deref(),
        Some("closedInterval")
    );
    let legacy = &fc.simple_attributes["legacy"];
    assert_eq!(legacy.uom.as_deref(), Some("kn"));
    assert_eq!(legacy.value_type, AttributeValueType::S100TruncatedDate);
    let f = &fc.feature_types["F"];
    assert_eq!(
        f.permitted_primitives,
        vec![
            SpatialPrimitive::PointSet,
            SpatialPrimitive::ArcByCenterPoint,
            SpatialPrimitive::CircleByCenterPoint
        ]
    );
    assert_eq!(f.attribute_bindings.len(), 1);
    assert_eq!(f.attribute_bindings[0].attribute_code, "depth");
    assert_eq!(f.attribute_bindings[0].multiplicity.upper, None);
}
