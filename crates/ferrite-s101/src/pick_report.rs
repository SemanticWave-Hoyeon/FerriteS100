//! S-101 end-user Pick Report policy (Product Specification 4.3.6.3).
use ferrite_feature_catalog::{AttributeVisibility, FeatureCatalogue};
use ferrite_s100_core::Attribute;
use std::collections::HashSet;

// Mandatory S-101 suppression also covers older catalogues missing binding metadata.
const PRIVATE: &[&str] = &[
    "defaultClearanceDepth",
    "displayPriority",
    "drawingIndex",
    "drawingInstruction",
    "fileLocator",
    "flareBearing",
    "inTheWater",
    "interoperabilityIdentifier",
    "majorLight",
    "nameUsage",
    "sectorArcExtension",
    "sectorLineLength",
    "surroundingDepth",
];

/// Return display attributes without modifying the source or portrayal inputs.
/// A private complex binding hides its descendants. PAIX addresses the 1-based
/// tuple position in ATTR; ATIX is the occurrence of a code under the same parent,
/// and may legitimately repeat for different codes (S-100 10a-5.1.5).
/// Missing or cyclic parent paths are omitted.
pub fn pick_report_attributes(
    catalogue: &FeatureCatalogue,
    feature_type: &str,
    attributes: &[Attribute],
) -> Vec<(String, String)> {
    attributes
        .iter()
        .enumerate()
        .filter(|(position, _)| {
            let mut index = *position;
            let mut visited = HashSet::new();
            loop {
                if !visited.insert(index) {
                    return false;
                }
                let current = &attributes[index];
                if current
                    .code
                    .as_deref()
                    .is_some_and(|code| PRIVATE.contains(&code))
                {
                    return false;
                }
                let parent = if current.paix == 0 {
                    None
                } else {
                    let parent_index = current.paix as usize - 1;
                    if parent_index >= attributes.len() {
                        return false;
                    }
                    Some(parent_index)
                };
                let owner = match parent {
                    Some(p) => match attributes[p].code.as_deref() {
                        Some(code) => code,
                        None => return false,
                    },
                    None => feature_type,
                };
                if current.code.as_deref().is_some_and(|code| {
                    catalogue.attribute_visibility(owner, code)
                        == Some(AttributeVisibility::Private)
                }) {
                    return false;
                }
                match parent {
                    Some(p) => index = p,
                    None => return true,
                }
            }
        })
        .map(|(_, a)| {
            (
                a.code
                    .clone()
                    .unwrap_or_else(|| format!("Attribute {}", a.natc)),
                a.value
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| a.atvl.clone()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_feature_catalog::{AttributeBinding, ComplexAttribute, FeatureType, Multiplicity};
    use std::collections::HashMap;
    fn catalogue() -> FeatureCatalogue {
        FeatureCatalogue {
            source_path: Default::default(),
            name: String::new(),
            scope: String::new(),
            version: String::new(),
            version_date: String::new(),
            product_id: String::new(),
            simple_attributes: HashMap::new(),
            complex_attributes: HashMap::new(),
            feature_types: HashMap::new(),
            information_types: HashMap::new(),
        }
    }
    fn binding(code: &str, visibility: AttributeVisibility) -> AttributeBinding {
        AttributeBinding {
            attribute_code: code.into(),
            visibility,
            multiplicity: Multiplicity::default(),
            sequential: false,
            permitted_values: vec![],
        }
    }
    fn attr(code: &str, index: u16, parent: u16) -> Attribute {
        Attribute {
            natc: index,
            atix: index,
            paix: parent,
            atvl: "source value".into(),
            value: None,
            code: Some(code.into()),
        }
    }
    fn feature(code: &str, parent: Option<&str>, bindings: Vec<AttributeBinding>) -> FeatureType {
        FeatureType {
            feature_use_type: Some(ferrite_feature_catalog::FeatureUseType::Geographic),
            code: code.into(),
            name: code.into(),
            definition: None,
            is_abstract: false,
            super_type: parent.map(str::to_owned),
            attribute_bindings: bindings,
            information_bindings: vec![],
            feature_bindings: vec![],
            permitted_primitives: vec![],
        }
    }
    #[test]
    fn mandatory_attributes_hidden_public_values_preserved_source_unchanged() {
        let fc = catalogue();
        let mut attrs: Vec<_> = PRIVATE
            .iter()
            .enumerate()
            .map(|(i, code)| attr(code, i as u16 + 1, 0))
            .collect();
        attrs.push(attr("valueOfSounding", 50, 0));
        let report = pick_report_attributes(&fc, "Wreck", &attrs);
        assert_eq!(
            report,
            vec![("valueOfSounding".into(), "source value".into())]
        );
        assert_eq!(attrs.len(), 14);
        assert_eq!(attrs[0].atvl, "source value");
    }
    #[test]
    fn catalogue_context_inheritance_and_private_ancestors_are_respected() {
        let mut fc = catalogue();
        fc.feature_types.insert(
            "Parent".into(),
            feature(
                "Parent",
                None,
                vec![binding("internal", AttributeVisibility::Private)],
            ),
        );
        fc.feature_types.insert(
            "Wreck".into(),
            feature(
                "Wreck",
                Some("Parent"),
                vec![
                    binding("publicGroup", AttributeVisibility::Public),
                    binding("privateGroup", AttributeVisibility::Private),
                ],
            ),
        );
        fc.complex_attributes.insert(
            "publicGroup".into(),
            ComplexAttribute {
                code: "publicGroup".into(),
                name: "group".into(),
                definition: None,
                sub_attributes: vec![
                    binding("nestedInternal", AttributeVisibility::Private),
                    binding("internal", AttributeVisibility::Public),
                ],
            },
        );
        // Parent references address tuple positions, while ATIX can repeat.
        let attrs = vec![
            attr("visibleChild", 1, 2),
            attr("privateGroup", 1, 0),
            attr("nestedInternal", 1, 5),
            attr("internal", 1, 5),
            attr("publicGroup", 1, 0),
            attr("internal", 1, 0),
        ];
        let names: Vec<_> = pick_report_attributes(&fc, "Wreck", &attrs)
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(names, vec!["internal", "publicGroup"]);
        fc.feature_types.get_mut("Parent").unwrap().super_type = Some("Wreck".into());
        assert_eq!(fc.attribute_visibility("Wreck", "absent"), None);
    }
    #[test]
    fn malformed_parent_paths_are_bounded_and_suppressed() {
        let fc = catalogue();
        let attrs = vec![
            attr("cycle", 1, 2),
            attr("cycle", 2, 1),
            attr("missing", 3, 99),
            attr("privateGroup", 1, 0),
            attr("defaultClearanceDepth", 1, 0),
            attr("child", 1, 5),
            attr("public", 6, 0),
        ];
        assert_eq!(
            pick_report_attributes(&fc, "Wreck", &attrs),
            vec![
                ("privateGroup".into(), "source value".into()),
                ("public".into(), "source value".into())
            ]
        );
    }
}
