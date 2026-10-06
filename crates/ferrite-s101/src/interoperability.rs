//! FC-typed S-101 attribute paths and atomic portrayal composition plans.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::{AttributeValueType, FeatureCatalogue};
use ferrite_interoperability::{Assignment, Catalogue, Decimal, Scalar};
use ferrite_render::{DisplayPlane, DrawingInstruction, GeometryType};
use ferrite_s100_core::{Attribute, FeatureRecord, S101Cell, SpatialPrimitiveType};
use std::collections::HashMap;
struct AttributeForest<'a> {
    attributes: &'a [Attribute],
    children: HashMap<(usize, &'a str), Vec<usize>>,
}
impl<'a> AttributeForest<'a> {
    fn new(attributes: &'a [Attribute]) -> Result<Self> {
        let mut children: HashMap<(usize, &str), Vec<usize>> = HashMap::new();
        for (i, a) in attributes.iter().enumerate() {
            let parent = a.paix as usize;
            ensure!(
                parent <= attributes.len() && parent != i + 1,
                "Invalid S-101 attribute parent at {}",
                i + 1
            );
            children
                .entry((
                    parent,
                    a.code.as_deref().context("Unmapped attribute code")?,
                ))
                .or_default()
                .push(i + 1);
        }
        // Each parent chain is visited once. PAIX is a vector position, not ATIX.
        let mut colors = vec![0u8; attributes.len() + 1];
        colors[0] = 2;
        for start in 1..=attributes.len() {
            let mut chain = Vec::new();
            let mut current = start;
            while colors[current] == 0 {
                colors[current] = 1;
                chain.push(current);
                current = attributes[current - 1].paix as usize;
            }
            ensure!(colors[current] == 2, "Cyclic S-101 attribute hierarchy");
            for i in chain {
                colors[i] = 2;
            }
        }
        Ok(Self {
            attributes,
            children,
        })
    }
    fn scalar(&self, path: &[String], fc: &FeatureCatalogue) -> Result<Option<Scalar>> {
        let code = path.last().context("Empty attribute path")?;
        let definition = fc
            .simple_attributes
            .get(code)
            .context("Filter leaf is not a simple FC attribute")?;
        for segment in &path[..path.len() - 1] {
            ensure!(
                fc.complex_attributes.contains_key(segment),
                "Filter parent is not a complex FC attribute: {segment}"
            );
        }
        for pair in path.windows(2) {
            ensure!(
                fc.complex_attributes[&pair[0]]
                    .sub_attributes
                    .iter()
                    .any(|b| b.attribute_code == pair[1]),
                "Path is not bound by FC: {}/{}",
                pair[0],
                pair[1]
            );
        }
        let mut parents = vec![0usize];
        for segment in path {
            parents = parents
                .iter()
                .flat_map(|p| {
                    self.children
                        .get(&(*p, segment.as_str()))
                        .into_iter()
                        .flatten()
                        .copied()
                })
                .collect();
            if parents.is_empty() {
                return Ok(None);
            }
        }
        ensure!(
            parents.len() == 1,
            "Repeated attribute path requires a multiplicity policy: {}",
            path.join("/")
        );
        let raw = &self.attributes[parents[0] - 1].atvl;
        // S-100 10a-5.1.3: a present code with empty ATVL means unknown, including text.
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(Some(match definition.value_type {
            AttributeValueType::Integer => {
                let digits = raw.strip_prefix('-').unwrap_or(raw);
                ensure!(
                    !digits.is_empty()
                        && digits.bytes().all(|c| c.is_ascii_digit())
                        && (digits == "0" || !digits.starts_with('0')),
                    "Invalid arbitrary-length integer attribute"
                );
                Scalar::Number(Decimal::parse(raw)?)
            }
            AttributeValueType::Real => Scalar::Number(Decimal::parse_real(raw)?),
            AttributeValueType::Enumeration => {
                let code = raw.parse::<u32>().context("Invalid enumeration code")?;
                ensure!(
                    definition.get_listed_value(code).is_some(),
                    "Enumeration code {code} is not in FC"
                );
                Scalar::Number(Decimal::parse(raw)?)
            }
            AttributeValueType::Boolean => {
                ensure!(raw == "0" || raw == "1", "Invalid S-101 boolean");
                Scalar::Number(Decimal::parse(raw)?)
            }
            _ => Scalar::Text(raw.clone()),
        }))
    }
}
fn feature_geometry(f: &FeatureRecord) -> &'static str {
    match f.primitive_type {
        SpatialPrimitiveType::Point => "point",
        SpatialPrimitiveType::MultiPoint => "pointSet",
        SpatialPrimitiveType::Curve | SpatialPrimitiveType::CompositeCurve => "curve",
        SpatialPrimitiveType::Surface => "surface",
        SpatialPrimitiveType::NoGeometry => "noGeometry",
    }
}
fn drawing_geometry(i: &DrawingInstruction) -> &'static str {
    match i {
        DrawingInstruction::Point(_) | DrawingInstruction::Text(_) => "point",
        DrawingInstruction::Line(_) => "curve",
        DrawingInstruction::Area(_) => "surface",
    }
}
struct Change {
    index: usize,
    cell: u32,
    feature: i64,
    geometry: GeometryType,
    assignment: Assignment,
}
pub struct InteroperabilityPlan {
    changes: Vec<Change>,
}
impl InteroperabilityPlan {
    pub fn len(&self) -> usize {
        self.changes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
    /// Validate every target first. A stale or invalid plan changes no instructions.
    pub fn apply(&self, instructions: &mut [DrawingInstruction]) -> Result<()> {
        for c in &self.changes {
            let i = instructions
                .get(c.index)
                .context("Composition plan is stale")?;
            ensure!(
                i.cell_index() == Some(c.cell)
                    && i.feature_id() == Some(c.feature)
                    && i.geometry_type() == c.geometry,
                "Composition plan identity mismatch"
            );
            ensure!(
                c.assignment.plane.stage == ferrite_kernel::CompositionStage::Chart
                    && c.assignment.priority >= 0,
                "Invalid vector composition assignment"
            );
        }
        for c in &self.changes {
            instructions[c.index].set_display_parameters(
                DisplayPlane::Interoperability(c.assignment.plane.order),
                c.assignment.priority,
                c.assignment.viewing_group,
            );
        }
        Ok(())
    }
}
/// Resolve once per (source feature, portrayal primitive), then share among its symbols/text/layers.
/// The caller retains the original PC instructions and can rebuild when IC is disabled/changed.
pub fn plan_interoperability(
    catalogue: &Catalogue,
    cell: &S101Cell,
    fc: &FeatureCatalogue,
    instructions: &[DrawingInstruction],
    cell_index: u32,
) -> Result<InteroperabilityPlan> {
    let mut assignments: HashMap<(i64, &str), Option<Assignment>> = HashMap::new();
    let mut changes = Vec::new();
    for (index, i) in instructions
        .iter()
        .enumerate()
        .filter(|(_, i)| i.cell_index() == Some(cell_index))
    {
        let id = i
            .feature_id()
            .context("S-101 portrayal is missing its feature identity")?;
        let feature = cell
            .features
            .get(&id)
            .context("S-101 portrayal references an unknown feature")?;
        let code = feature
            .feature_code
            .as_deref()
            .context("Unmapped feature code")?;
        if !catalogue.has_rules_for("S-101", code) {
            continue;
        }
        let definition = fc
            .feature_types
            .get(code)
            .with_context(|| format!("Feature code is not in FC: {code}"))?;
        let use_type = definition
            .feature_use_type
            .with_context(|| format!("featureUseType is required before IC processing: {code}"))?;
        // S-98 4.2.2.2: meta features retain their own product's PC portrayal.
        // Do this before evaluating selectors, including selectors with unsupported attributes.
        if use_type == ferrite_feature_catalog::FeatureUseType::Meta {
            continue;
        }
        let geometry = drawing_geometry(i);
        if let std::collections::hash_map::Entry::Vacant(entry) = assignments.entry((id, geometry))
        {
            let mut forest = None;
            let assignment = catalogue
                .resolve_portrayal("S-101", code, feature_geometry(feature), geometry, |path| {
                    ensure!(
                        fc.attribute_visibility(code, &path[0]).is_some(),
                        "IC filter root is not bound to feature in FC: {}",
                        path[0]
                    );
                    if forest.is_none() {
                        forest = Some(AttributeForest::new(&feature.attributes)?);
                    }
                    forest.as_ref().unwrap().scalar(path, fc)
                })
                .with_context(|| format!("IC selection for {code} {id}"))?;
            entry.insert(assignment);
        }
        if let Some(assignment) = assignments[&(id, geometry)].clone() {
            changes.push(Change {
                index,
                cell: cell_index,
                feature: id,
                geometry: i.geometry_type(),
                assignment,
            });
        }
    }
    Ok(InteroperabilityPlan { changes })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fc() -> FeatureCatalogue {
        let mut fc = FeatureCatalogue {
            source_path: Default::default(),
            name: "Test".into(),
            scope: String::new(),
            version: "1".into(),
            version_date: String::new(),
            product_id: "S-101".into(),
            simple_attributes: HashMap::new(),
            complex_attributes: HashMap::new(),
            feature_types: HashMap::new(),
            information_types: HashMap::new(),
        };
        fc.simple_attributes.insert(
            "language".into(),
            ferrite_feature_catalog::SimpleAttribute {
                code: "language".into(),
                name: "Language".into(),
                definition: None,
                value_type: AttributeValueType::Text,
                uom: None,
                listed_values: vec![],
                quantitative_range: None,
            },
        );
        fc.complex_attributes.insert(
            "featureName".into(),
            ferrite_feature_catalog::ComplexAttribute {
                code: "featureName".into(),
                name: "Name".into(),
                definition: None,
                sub_attributes: vec![ferrite_feature_catalog::AttributeBinding {
                    attribute_code: "language".into(),
                    visibility: Default::default(),
                    multiplicity: Default::default(),
                    sequential: false,
                    permitted_values: vec![],
                }],
            },
        );
        fc
    }
    fn a(code: &str, parent: u16, value: &str) -> Attribute {
        Attribute {
            natc: 1,
            atix: 99,
            paix: parent,
            atvl: value.into(),
            value: None,
            code: Some(code.into()),
        }
    }
    #[test]
    fn paths_use_parent_positions_and_preserve_text() {
        let attrs = [
            a("language", 0, "root"),
            a("featureName", 0, ""),
            a("language", 2, "eng"),
        ];
        let f = AttributeForest::new(&attrs).unwrap();
        let fc = fc();
        assert_eq!(
            f.scalar(&["featureName".into(), "language".into()], &fc)
                .unwrap(),
            Some(Scalar::Text("eng".into()))
        );
        assert_eq!(
            f.scalar(&["language".into()], &fc).unwrap(),
            Some(Scalar::Text("root".into()))
        );
        let unknown = [a("language", 0, "")];
        assert_eq!(
            AttributeForest::new(&unknown)
                .unwrap()
                .scalar(&["language".into()], &fc)
                .unwrap(),
            None
        );
        let repeated = [a("language", 0, "eng"), a("language", 0, "fra")];
        assert!(AttributeForest::new(&repeated)
            .unwrap()
            .scalar(&["language".into()], &fc)
            .is_err());
        assert!(AttributeForest::new(&[a("x", 2, ""), a("y", 1, "")]).is_err());
        assert!(AttributeForest::new(&[a("x", 9, "")]).is_err());
    }

    #[test]
    fn meta_portrayal_is_unchanged_even_when_ic_targets_it() {
        use ferrite_feature_catalog::{FeatureType, FeatureUseType};
        use ferrite_render::{PointInstruction, WorldPoint};
        let mut fc = fc();
        fc.feature_types.insert(
            "Wreck".into(),
            FeatureType {
                code: "Wreck".into(),
                name: "Test".into(),
                definition: None,
                is_abstract: false,
                super_type: None,
                feature_use_type: Some(FeatureUseType::Meta),
                attribute_bindings: vec![],
                information_bindings: vec![],
                feature_bindings: vec![],
                permitted_primitives: vec![],
            },
        );
        let mut cell = S101Cell {
            file_path: Default::default(),
            dsid: Default::default(),
            code_mappings: ferrite_s100_core::DatasetCodeMappings::new(),
            coord_factor: 1.,
            coord_factor_y: 1.,
            coord_factor_z: 1.,
            coord_origin_x: 0.,
            coord_origin_y: 0.,
            coord_origin_z: 0.,
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
        cell.features.insert(
            1,
            FeatureRecord {
                frid: ferrite_s100_core::FRID {
                    rcid: 1,
                    nftc: 1,
                    rver: 1,
                    ruin: 1,
                },
                foid: None,
                attributes: vec![],
                spatial_associations: vec![],
                information_associations: vec![],
                feature_associations: vec![],
                masks: vec![],
                feature_code: Some("Wreck".into()),
                primitive_type: SpatialPrimitiveType::Point,
            },
        );
        let mut instructions = vec![DrawingInstruction::Point(
            PointInstruction::new("PC_SYMBOL".into(), WorldPoint::new(0., 0.))
                .with_cell_index(0)
                .with_feature_id(1),
        )];
        let original = serde_json::to_value(&instructions).unwrap();
        let xml = include_str!("../../ferrite-interoperability/tests/display-plane.xml");
        let catalogue = Catalogue::parse(xml).unwrap();
        let plan = plan_interoperability(&catalogue, &cell, &fc, &instructions, 0).unwrap();
        assert!(plan.is_empty());
        plan.apply(&mut instructions).unwrap();
        assert_eq!(serde_json::to_value(&instructions).unwrap(), original);
        fc.feature_types.get_mut("Wreck").unwrap().feature_use_type = None;
        assert!(
            plan_interoperability(&catalogue, &cell, &fc, &instructions, 0)
                .err()
                .unwrap()
                .to_string()
                .contains("featureUseType")
        );
        fc.feature_types.get_mut("Wreck").unwrap().feature_use_type =
            Some(FeatureUseType::Geographic);
        // The same ordinary feature without a filter is eligible for composition.
        let catalogue = Catalogue::parse(&xml.replace(
            "<attributeCombination>categoryOfWreck = 1</attributeCombination>",
            "",
        ))
        .unwrap();
        assert_eq!(
            plan_interoperability(&catalogue, &cell, &fc, &instructions, 0)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn stale_plan_is_atomic() {
        use ferrite_render::{PointInstruction, WorldPoint};
        let assignment = Assignment {
            plane: ferrite_kernel::CompositionPlane::new(
                ferrite_kernel::CompositionStage::Chart,
                std::num::NonZeroI32::new(-10).unwrap(),
            ),
            priority: 2,
            viewing_group: 11010,
        };
        let plan = InteroperabilityPlan {
            changes: vec![
                Change {
                    index: 0,
                    cell: 0,
                    feature: 1,
                    geometry: GeometryType::Point,
                    assignment: assignment.clone(),
                },
                Change {
                    index: 1,
                    cell: 0,
                    feature: 2,
                    geometry: GeometryType::Point,
                    assignment,
                },
            ],
        };
        let mut instructions = vec![
            DrawingInstruction::Point(
                PointInstruction::new("TEST".into(), WorldPoint::new(0., 0.))
                    .with_cell_index(0)
                    .with_feature_id(1),
            ),
            DrawingInstruction::Point(
                PointInstruction::new("TEST".into(), WorldPoint::new(0., 0.))
                    .with_cell_index(1)
                    .with_feature_id(2),
            ),
        ];
        assert!(plan.apply(&mut instructions).is_err());
        assert_eq!(instructions[0].display_plane(), DisplayPlane::UnderRadar);
    }
    #[test]
    fn numeric_values_use_fc_type_and_original_encoding() {
        let mut fc = fc();
        let mut integer = fc.simple_attributes["language"].clone();
        integer.code = "integer".into();
        integer.value_type = AttributeValueType::Integer;
        fc.simple_attributes.insert("integer".into(), integer);
        let mut real = fc.simple_attributes["language"].clone();
        real.code = "real".into();
        real.value_type = AttributeValueType::Real;
        fc.simple_attributes.insert("real".into(), real);
        let huge = "90071992547409930000000000000000000000000";
        let attributes = [a("integer", 0, huge), a("real", 0, "1E-5")];
        let forest = AttributeForest::new(&attributes).unwrap();
        assert_eq!(
            forest.scalar(&["integer".into()], &fc).unwrap(),
            Some(Scalar::Number(Decimal::parse(huge).unwrap()))
        );
        assert_eq!(
            forest.scalar(&["real".into()], &fc).unwrap(),
            Some(Scalar::Number(Decimal::parse("0.00001").unwrap()))
        );
    }
}
