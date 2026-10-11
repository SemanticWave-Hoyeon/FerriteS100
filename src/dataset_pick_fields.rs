//! Pick-report fields require the source cell's validated catalogue owner.
//! Missing owner or feature type withholds FC-derived/public attributes;
//! a global catalogue is never an implicit fallback.
use ferrite_feature_catalog::FeatureCatalogue;
use ferrite_s100_core::Attribute;
#[derive(Default)]
pub(crate) struct CataloguePickFields {
    pub definition: Option<String>,
    pub attributes: Vec<(String, String)>,
    pub resolved: bool,
}
pub(crate) fn resolve(
    catalogue: Option<&FeatureCatalogue>,
    feature_code: &str,
    attributes: &[Attribute],
) -> CataloguePickFields {
    let Some(catalogue) = catalogue else {
        return CataloguePickFields::default();
    };
    let Some(feature) = catalogue.feature_types.get(feature_code) else {
        return CataloguePickFields::default();
    };
    CataloguePickFields {
        definition: feature.definition.clone(),
        attributes: ferrite_s101::pick_report_attributes(catalogue, feature_code, attributes),
        resolved: true,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_feature_catalog::{
        AttributeBinding, AttributeVisibility, FeatureType, Multiplicity,
    };
    use std::collections::HashMap;
    fn catalogue(definition: &str, visibility: AttributeVisibility) -> FeatureCatalogue {
        let feature = FeatureType {
            feature_use_type: None,
            code: "Wreck".into(),
            name: "Wreck".into(),
            definition: Some(definition.into()),
            is_abstract: false,
            super_type: None,
            attribute_bindings: vec![AttributeBinding {
                attribute_code: "status".into(),
                visibility,
                multiplicity: Multiplicity::default(),
                sequential: false,
                permitted_values: vec![],
            }],
            information_bindings: vec![],
            feature_bindings: vec![],
            permitted_primitives: vec![],
        };
        FeatureCatalogue {
            source_path: Default::default(),
            name: definition.into(),
            scope: String::new(),
            version: String::new(),
            version_date: String::new(),
            product_id: "S-101".into(),
            simple_attributes: HashMap::new(),
            complex_attributes: HashMap::new(),
            feature_types: HashMap::from([("Wreck".into(), feature)]),
            information_types: HashMap::new(),
        }
    }
    #[test]
    fn source_owner_changes_definition_and_privacy_without_mutating_raw_values() {
        let a = catalogue("edition one wreck", AttributeVisibility::Public);
        let b = catalogue("edition two wreck", AttributeVisibility::Private);
        let input = vec![Attribute {
            natc: 1,
            atix: 1,
            paix: 0,
            atvl: "source value".into(),
            value: None,
            code: Some("status".into()),
        }];
        let one = resolve(Some(&a), "Wreck", &input);
        let two = resolve(Some(&b), "Wreck", &input);
        assert_eq!(one.definition.as_deref(), Some("edition one wreck"));
        assert_eq!(two.definition.as_deref(), Some("edition two wreck"));
        assert_eq!(
            one.attributes,
            vec![("status".into(), "source value".into())]
        );
        assert!(two.attributes.is_empty());
        assert_eq!(input[0].atvl, "source value");
        assert!(one.resolved && two.resolved);
    }
    #[test]
    fn missing_owner_and_unknown_type_never_guess_public_attributes() {
        let fc = catalogue("owner", AttributeVisibility::Public);
        let missing = resolve(None, "Wreck", &[]);
        let unknown = resolve(Some(&fc), "OtherEditionType", &[]);
        assert!(!missing.resolved && !unknown.resolved);
        assert!(missing.definition.is_none() && unknown.definition.is_none());
    }
}
