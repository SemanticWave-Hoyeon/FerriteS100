//! Feature catalogue and product rule validation of Part 10b datasets.
//!
//! Generic checks are driven by the feature catalogue; the product's
//! [`GmlRules`] map each generic rule ([`FcRule`]) to a check identifier and
//! severity, add the product rules and decide the file naming convention
//! (the product crates: S-124 `FC/<rule>` and `S-124/<rule>`, S-129 every
//! check a warning with its S-158-129 number where one applies).
//! Association names are not encoded in Part 10b, so association checks
//! infer the association from (source class, role, target class);
//! exchange-set checks are in [`crate::exchange`].
use crate::model::*;
use crate::product::GmlRules;
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use ferrite_feature_catalog::{
    AttributeBinding, AttributeValueType, FeatureCatalogue, SpatialPrimitive,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Validates with the byte size taken from the dataset's source file, if any.
/// Returns validation issues only; reader issues stay in `dataset.issues`.
pub fn validate(dataset: &Dataset, fc: &FeatureCatalogue, rules: &dyn GmlRules) -> Vec<Issue> {
    let size = dataset
        .source
        .as_deref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    Validator::new(fc).validate(dataset, size, rules)
}

/// Generic FC rules, mapped per product ([`GmlRules::fc_rule_check`]) to a
/// check identifier and severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcRule {
    MandatoryMissing,
    MandatoryNil,
    Zeros,
    RoleNotPermitted,
    RoleUnknown,
    WrongTarget,
    WrongAssociation,
    UnknownAssociation,
    RoleOnWrongAssociation,
    UnknownClass,
    UnknownAttribute,
    AttributeNotPermitted,
    Order,
    TooMany,
    OptionalEmpty,
    FileName,
    ValueFormat,
    DateTimeFormat,
    Enumeration,
    Primitive,
    AssociationMultiplicity,
    Product,
}

impl FcRule {
    /// The rule name of `FC/<name>` check identifiers.
    pub fn name(self) -> &'static str {
        match self {
            Self::MandatoryMissing => "mandatory-missing",
            Self::MandatoryNil => "mandatory-nil",
            Self::Zeros => "non-significant-zeros",
            Self::RoleNotPermitted => "role-not-permitted",
            Self::RoleUnknown => "role-unknown",
            Self::WrongTarget => "association-target",
            Self::WrongAssociation => "association-wrong",
            Self::UnknownAssociation => "association-unknown",
            Self::RoleOnWrongAssociation => "role-association",
            Self::UnknownClass => "unknown-class",
            Self::UnknownAttribute => "unknown-attribute",
            Self::AttributeNotPermitted => "attribute-not-permitted",
            Self::Order => "order",
            Self::TooMany => "multiplicity-upper",
            Self::OptionalEmpty => "optional-empty",
            Self::FileName => "file-name",
            Self::ValueFormat | Self::DateTimeFormat => "value-format",
            Self::Enumeration => "enumeration",
            Self::Primitive => "primitive",
            Self::AssociationMultiplicity => "association-multiplicity",
            Self::Product => "product",
        }
    }
    /// Severity of a violation where the product does not override it.
    pub fn severity(self) -> Severity {
        match self {
            Self::Zeros | Self::Order | Self::OptionalEmpty => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

/// An association binding of a class: role, association (if named),
/// target class and multiplicity.
#[derive(Debug, Clone)]
pub struct AssociationBinding {
    pub role: String,
    pub association: Option<String>,
    pub target: String,
    pub lower: u32,
    pub upper: Option<u32>,
}

/// For each object of a dataset, the (source object, reference index) pairs
/// that resolve to it.
pub type Incoming = [Vec<(usize, usize)>];

/// Catalogue content the feature catalogue model does not carry (roles,
/// association bindings, permitted values, raw value types and primitives),
/// read from the catalogue source.
#[derive(Debug, Default)]
struct Supplement {
    roles: HashSet<String>,
    association_roles: HashMap<String, Vec<String>>,
    bindings: HashMap<String, Vec<AssociationBinding>>,
    permitted: HashMap<(String, String), Vec<u32>>,
    value_types: HashMap<String, String>,
    primitives: HashMap<String, Vec<String>>,
}

impl Supplement {
    fn load(path: &Path) -> Result<Self> {
        let len = std::fs::metadata(path)?.len();
        ensure!(len <= 64 * 1024 * 1024, "Feature catalogue too large");
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 64 * 1024 * 1024,
            "Feature catalogue too large"
        );
        Self::parse(std::str::from_utf8(bytes).context("Feature catalogue is not UTF-8")?)
    }
    fn parse(text: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse_with_options(
            text.trim_start_matches('\u{feff}'),
            roxmltree::ParsingOptions {
                allow_dtd: false,
                nodes_limit: 20_000_000,
            },
        )
        .context("Feature catalogue is not well-formed")?;
        let mut s = Self::default();
        for n in doc.descendants().filter(|n| n.is_element()) {
            let name = n.tag_name().name();
            match name {
                "S100_FC_Role" => s.roles.extend(text_of(n, "code")),
                "S100_FC_InformationAssociation" | "S100_FC_FeatureAssociation" => {
                    if let Some(code) = text_of(n, "code") {
                        let roles = kids(n)
                            .into_iter()
                            .filter(|c| c.tag_name().name() == "role")
                            .filter_map(|c| c.attribute("ref").map(str::to_owned))
                            .collect();
                        s.association_roles.insert(code, roles);
                    }
                }
                "S100_FC_SimpleAttribute" => {
                    if let (Some(code), Some(vt)) = (text_of(n, "code"), text_of(n, "valueType")) {
                        s.value_types.insert(code, vt);
                    }
                }
                "S100_FC_FeatureType" | "S100_FC_InformationType" | "S100_FC_ComplexAttribute" => {
                    let Some(owner) = text_of(n, "code") else {
                        continue;
                    };
                    for b in kids(n) {
                        match b.tag_name().name() {
                            "featureBinding" | "informationBinding" => {
                                let target = ref_of(b, "featureType")
                                    .or_else(|| ref_of(b, "informationType"));
                                let (Some(role), Some(target)) = (ref_of(b, "role"), target) else {
                                    continue;
                                };
                                let (lower, upper) = multiplicity(b);
                                s.bindings.entry(owner.clone()).or_default().push(
                                    AssociationBinding {
                                        role,
                                        association: ref_of(b, "association"),
                                        target,
                                        lower,
                                        upper,
                                    },
                                );
                            }
                            "attributeBinding" | "subAttributeBinding" => {
                                let Some(attr) = ref_of(b, "attribute") else {
                                    continue;
                                };
                                let values: Vec<u32> = kids(b)
                                    .into_iter()
                                    .filter(|c| c.tag_name().name() == "permittedValues")
                                    .flat_map(|c| kids(c))
                                    .filter_map(|v| v.text()?.trim().parse().ok())
                                    .collect();
                                if !values.is_empty() {
                                    s.permitted.insert((owner.clone(), attr), values);
                                }
                            }
                            "permittedPrimitives" => {
                                if let Some(t) = b.text() {
                                    s.primitives
                                        .entry(owner.clone())
                                        .or_default()
                                        .push(t.trim().to_ascii_lowercase());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(s)
    }
}

fn kids<'a, 'i>(n: roxmltree::Node<'a, 'i>) -> Vec<roxmltree::Node<'a, 'i>> {
    n.children().filter(|c| c.is_element()).collect()
}
fn text_of(n: roxmltree::Node, name: &str) -> Option<String> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
}
fn ref_of(n: roxmltree::Node, name: &str) -> Option<String> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .and_then(|c| c.attribute("ref"))
        .map(str::to_owned)
}

fn multiplicity(binding: roxmltree::Node) -> (u32, Option<u32>) {
    let Some(m) = binding
        .children()
        .find(|c| c.tag_name().name() == "multiplicity")
    else {
        return (1, Some(1));
    };
    let field = |name: &str| {
        m.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
    };
    let lower = field("lower")
        .and_then(|l| l.text()?.trim().parse().ok())
        .unwrap_or(0);
    let upper = field("upper").and_then(|u| {
        let unbounded = u.attribute("infinite") == Some("true")
            || u.attributes()
                .any(|a| a.name() == "nil" && a.value() == "true");
        if unbounded {
            None
        } else {
            u.text()?.trim().parse().ok()
        }
    });
    (lower, upper)
}

/// A prepared feature catalogue; reuse it to validate several datasets.
pub struct Validator<'a> {
    fc: &'a FeatureCatalogue,
    extra: Supplement,
}

/// Issue sink for one dataset.
pub struct IssueSink<'r> {
    rules: &'r dyn GmlRules,
    issues: Vec<Issue>,
}

impl IssueSink<'_> {
    fn rule(&mut self, rule: FcRule, object: Option<&str>, message: String) {
        let (check, severity) = self.rules.fc_rule_check(rule);
        self.issues.push(Issue {
            severity,
            check,
            object: object.map(str::to_owned),
            message,
        });
    }
    /// A product rule finding with its own check identifier.
    pub fn product(
        &mut self,
        check: &str,
        severity: Severity,
        object: Option<&str>,
        message: String,
    ) {
        self.issues.push(Issue {
            severity,
            check: check.to_string(),
            object: object.map(str::to_owned),
            message,
        });
    }
}

impl<'a> Validator<'a> {
    /// Reads roles, association bindings and permitted values from the
    /// catalogue source when the catalogue model lacks them.
    pub fn new(fc: &'a FeatureCatalogue) -> Self {
        let extra = Supplement::load(&fc.source_path).unwrap_or_else(|e| {
            tracing::warn!("Feature catalogue supplement unavailable: {e:#}");
            Supplement::default()
        });
        Self { fc, extra }
    }

    /// As [`Validator::new`], with the catalogue source supplied as bytes
    /// (a host without a file system).
    pub fn with_catalogue_bytes(fc: &'a FeatureCatalogue, source: &[u8]) -> Self {
        let extra = Supplement::from_bytes(source).unwrap_or_else(|e| {
            tracing::warn!("Feature catalogue supplement unavailable: {e:#}");
            Supplement::default()
        });
        Self { fc, extra }
    }

    /// Validates a dataset with its product's rules; `byte_size` enables
    /// the dataset size checks.
    pub fn validate(
        &self,
        dataset: &Dataset,
        byte_size: Option<u64>,
        rules: &dyn GmlRules,
    ) -> Vec<Issue> {
        let mut sink = IssueSink {
            rules,
            issues: Vec::new(),
        };
        self.dataset_checks(dataset, byte_size, &mut sink);
        let incoming = incoming(dataset);
        for (i, object) in dataset.objects.iter().enumerate() {
            self.object_checks(dataset, i, object, &incoming, &mut sink);
        }
        rules.product_checks(self, dataset, byte_size, &incoming, &mut sink);
        sink.issues
    }

    /// Roles of an association, as declared in the catalogue source.
    pub fn association_roles(&self, association: &str) -> Option<&[String]> {
        self.extra
            .association_roles
            .get(association)
            .map(Vec::as_slice)
    }

    /// The class and its supertypes, most derived first.
    fn lineage<'c>(&'c self, code: &'c str) -> Vec<&'c str> {
        let mut out = Vec::new();
        let mut current = Some(code);
        while let Some(c) = current {
            if out.contains(&c) {
                break;
            }
            out.push(c);
            current = if let Some(f) = self.fc.feature_types.get(c) {
                f.super_type.as_deref()
            } else if let Some(i) = self.fc.information_types.get(c) {
                i.super_type.as_deref()
            } else {
                None
            };
        }
        out
    }

    fn is_a(&self, code: &str, ancestor: &str) -> bool {
        self.lineage(code).contains(&ancestor)
    }

    /// Attribute bindings with their declaring owner, supertype first.
    fn attribute_bindings<'c>(&'c self, code: &'c str) -> Vec<(&'c str, &'c AttributeBinding)> {
        if let Some(c) = self.fc.complex_attributes.get(code) {
            return c.sub_attributes.iter().map(|b| (code, b)).collect();
        }
        let mut out = Vec::new();
        for owner in self.lineage(code).into_iter().rev() {
            let bindings = match (
                self.fc.feature_types.get(owner),
                self.fc.information_types.get(owner),
            ) {
                (Some(f), _) => &f.attribute_bindings,
                (_, Some(i)) => &i.attribute_bindings,
                _ => continue,
            };
            out.extend(bindings.iter().map(|b| (owner, b)));
        }
        out
    }

    /// Association bindings of the class (catalogue model first, then source).
    pub fn association_bindings(&self, code: &str) -> Vec<AssociationBinding> {
        let mut out = Vec::new();
        for owner in self.lineage(code) {
            let mut model = Vec::new();
            if let Some(f) = self.fc.feature_types.get(owner) {
                model.extend(f.feature_bindings.iter().filter_map(|b| {
                    Some(AssociationBinding {
                        role: b.role.clone()?,
                        association: b.association.clone(),
                        target: b.feature_type_code.clone(),
                        lower: b.multiplicity.lower,
                        upper: b.multiplicity.upper,
                    })
                }));
                model.extend(f.information_bindings.iter().filter_map(|b| {
                    Some(AssociationBinding {
                        role: b.role.clone()?,
                        association: b.association.clone(),
                        target: b.information_type_code.clone(),
                        lower: b.multiplicity.lower,
                        upper: b.multiplicity.upper,
                    })
                }));
            }
            if let Some(i) = self.fc.information_types.get(owner) {
                model.extend(i.information_bindings.iter().filter_map(|b| {
                    Some(AssociationBinding {
                        role: b.role.clone()?,
                        association: b.association.clone(),
                        target: b.information_type_code.clone(),
                        lower: b.multiplicity.lower,
                        upper: b.multiplicity.upper,
                    })
                }));
            }
            if model.is_empty() {
                model = self.extra.bindings.get(owner).cloned().unwrap_or_default();
            }
            out.extend(model);
        }
        out
    }

    fn permitted_values(&self, owner: &str, binding: &AttributeBinding) -> Vec<u32> {
        if !binding.permitted_values.is_empty() {
            return binding.permitted_values.clone();
        }
        self.extra
            .permitted
            .get(&(owner.to_string(), binding.attribute_code.clone()))
            .cloned()
            .unwrap_or_default()
    }

    /// Lower-case permitted primitive names (point, pointset, curve, surface, nogeometry).
    fn primitives(&self, code: &str) -> Vec<String> {
        for owner in self.lineage(code) {
            if let Some(p) = self.extra.primitives.get(owner) {
                return p.clone();
            }
            if let Some(f) = self.fc.feature_types.get(owner) {
                if !f.permitted_primitives.is_empty() {
                    return f
                        .permitted_primitives
                        .iter()
                        .map(|p| {
                            match p {
                                SpatialPrimitive::Point => "point",
                                SpatialPrimitive::Curve => "curve",
                                SpatialPrimitive::Surface => "surface",
                                SpatialPrimitive::Coverage => "coverage",
                                SpatialPrimitive::NoGeometry => "nogeometry",
                                SpatialPrimitive::PointSet => "pointset",
                                SpatialPrimitive::ArcByCenterPoint => "arcbycenterpoint",
                                SpatialPrimitive::CircleByCenterPoint => "circlebycenterpoint",
                            }
                            .to_string()
                        })
                        .collect();
                }
            }
        }
        Vec::new()
    }

    fn dataset_checks(&self, ds: &Dataset, byte_size: Option<u64>, sink: &mut IssueSink) {
        let id = &ds.identification;
        let fc_product = self.fc.product_id.trim();
        if !fc_product.is_empty() && fc_product != ds.product.as_str() {
            sink.rule(
                FcRule::Product,
                None,
                format!(
                    "Dataset is {} but the feature catalogue is {fc_product}",
                    ds.product
                ),
            );
        }
        match id.product_identifier() {
            Some(p) if sink.rules.identity().names(p) => {}
            other => sink.rule(
                FcRule::Product,
                None,
                format!("productIdentifier {other:?} is not {}", ds.product),
            ),
        }
        if let Some(edition) = id.product_edition() {
            if !self.fc.version.is_empty() && edition != self.fc.version.trim() {
                sink.product(
                    "FC/edition",
                    Severity::Warning,
                    None,
                    format!(
                        "productEdition {edition} differs from feature catalogue {}",
                        self.fc.version
                    ),
                );
            }
        }
        // Dataset file naming (S-129 18.2.3; S-100 17-4.3 for S-124).
        let source_name = ds
            .source
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let mut names: Vec<String> = source_name.iter().cloned().collect();
        if let Some(f) = id.file_identifier() {
            if source_name.as_deref() != Some(f) {
                names.push(f.to_string());
            }
        }
        for name in names {
            if !sink.rules.file_name_ok(&name) {
                sink.rule(
                    FcRule::FileName,
                    None,
                    format!("Dataset file name '{name}' does not follow the naming convention"),
                );
            }
        }
        if let (Some(source), Some(f)) = (&source_name, id.file_identifier()) {
            if !source.eq_ignore_ascii_case(f) {
                sink.product(
                    "GML/identification",
                    Severity::Warning,
                    None,
                    format!("datasetFileIdentifier '{f}' differs from the file name '{source}'"),
                );
            }
        }
        let rules = sink.rules;
        rules.dataset_checks(ds, byte_size, sink);
    }

    fn object_checks(
        &self,
        ds: &Dataset,
        index: usize,
        o: &Object,
        incoming: &Incoming,
        sink: &mut IssueSink,
    ) {
        let id = Some(o.id.as_str());
        let feature = self.fc.feature_types.get(&o.code);
        let info = self.fc.information_types.get(&o.code);
        let is_abstract = match (feature, info) {
            (Some(f), _) => f.is_abstract,
            (_, Some(i)) => i.is_abstract,
            _ => {
                sink.rule(
                    FcRule::UnknownClass,
                    id,
                    format!("{} is not a feature or information type", o.code),
                );
                return;
            }
        };
        if is_abstract {
            sink.rule(FcRule::UnknownClass, id, format!("{} is abstract", o.code));
        }
        self.attributes(&o.code, &o.attributes, &o.code, id, sink);

        // Geometry primitives.
        if feature.is_none() && !o.geometry.is_empty() {
            sink.rule(
                FcRule::Primitive,
                id,
                format!("Information type {} has geometry", o.code),
            );
        } else if feature.is_some() {
            let permitted = self.primitives(&o.code);
            for g in &o.geometry {
                let name = match g {
                    Geometry::Point(_) => "point",
                    Geometry::MultiPoint(_) => "pointset",
                    Geometry::Curve(_) => "curve",
                    Geometry::Surface { .. } => "surface",
                };
                if !permitted.is_empty() && !permitted.iter().any(|p| p == name) {
                    sink.rule(
                        FcRule::Primitive,
                        id,
                        format!(
                            "{} geometry is not permitted on {} ({})",
                            g.primitive(),
                            o.code,
                            permitted.join(", ")
                        ),
                    );
                }
            }
        }

        // Associations encoded on this object.
        let bindings = self.association_bindings(&o.code);
        for r in &o.references {
            if !self.extra.roles.is_empty() && !self.extra.roles.contains(&r.role) {
                sink.rule(
                    FcRule::RoleUnknown,
                    id,
                    format!("Role '{}' is not defined in the feature catalogue", r.role),
                );
                continue;
            }
            let target = r.target.map(|t| ds.objects[t].code.as_str());
            if let Some((rule, message)) = self.classify_role(&o.code, &r.role, target, &bindings) {
                sink.rule(rule, id, message);
            }
        }

        // Association multiplicity, counting either encoded direction.
        for b in &bindings {
            if sink.rules.checked_by_product(&o.code, &b.role, &b.target) {
                continue; // Reported by the product checks.
            }
            let n = self.associated(ds, index, o, b, incoming).len() as u32;
            if n < b.lower || b.upper.is_some_and(|u| n > u) {
                sink.rule(
                    FcRule::AssociationMultiplicity,
                    id,
                    format!(
                        "{n} {} association(s) via role '{}', expected {}..{}",
                        b.target,
                        b.role,
                        b.lower,
                        b.upper.map_or("*".to_string(), |u| u.to_string())
                    ),
                );
            }
        }
    }

    /// Classifies a reference that no binding of the source class permits.
    ///
    /// Part 10b encodes only the role, so the association is inferred from
    /// (source class, role, target class), each defect reported once:
    /// 6 the role belongs to no association, or no association links the
    /// classes through it; 5 the role belongs to the association between the
    /// classes but not to the source end; 9 the role belongs to another
    /// association of the source class; 4 the classes are associated, but
    /// the role implies an association the source class does not take part
    /// in; 8 the source class uses the role, towards other classes.
    fn classify_role(
        &self,
        source: &str,
        role: &str,
        target: Option<&str>,
        bindings: &[AssociationBinding],
    ) -> Option<(FcRule, String)> {
        let candidates: Vec<_> = bindings.iter().filter(|b| b.role == role).collect();
        match target {
            Some(t) if candidates.iter().any(|b| self.is_a(t, &b.target)) => return None,
            None if !candidates.is_empty() => return None,
            _ => {}
        }
        let evaluable = !self.extra.association_roles.is_empty();
        let mut with_role: Vec<&str> = self
            .extra
            .association_roles
            .iter()
            .filter(|(_, roles)| roles.iter().any(|r| r == role))
            .map(|(a, _)| a.as_str())
            .collect();
        with_role.sort_unstable();
        if evaluable && with_role.is_empty() {
            return Some((
                FcRule::UnknownAssociation,
                format!("Role '{role}' belongs to no association in the feature catalogue"),
            ));
        }
        let not_permitted = || {
            (
                FcRule::RoleNotPermitted,
                format!("Role '{role}' is not permitted on {source}"),
            )
        };
        let Some(target) = target else {
            return candidates.is_empty().then(not_permitted);
        };
        let wrong_target = || {
            let expected: Vec<_> = candidates.iter().map(|b| b.target.as_str()).collect();
            (
                FcRule::WrongTarget,
                format!(
                    "Role '{role}' targets {target}, expected {}",
                    expected.join(" or ")
                ),
            )
        };
        if !evaluable {
            return Some(if candidates.is_empty() {
                not_permitted()
            } else {
                wrong_target()
            });
        }
        let between = self.associations_between(source, target);
        if !between.is_empty() {
            let names: Vec<String> = between.iter().map(|(a, _)| a.clone()).collect();
            let names = names.join(", ");
            if between
                .iter()
                .any(|(_, roles)| roles.iter().any(|r| r == role))
            {
                return Some((
                    FcRule::RoleNotPermitted,
                    format!(
                        "Role '{role}' is not permitted on {source} in association {names} with {target}"
                    ),
                ));
            }
            let own = bindings.iter().any(|b| {
                b.association
                    .as_deref()
                    .is_some_and(|a| with_role.contains(&a))
            });
            let implied = with_role.join(" or ");
            return Some(if own {
                (
                    FcRule::RoleOnWrongAssociation,
                    format!(
                        "Role '{role}' of association {implied} is used on association {names} between {source} and {target}, which does not include it"
                    ),
                )
            } else {
                (
                    FcRule::WrongAssociation,
                    format!(
                        "Role '{role}' implies association {implied}, but {source} and {target} are associated by {names}"
                    ),
                )
            });
        }
        if !candidates.is_empty() {
            return Some(wrong_target());
        }
        let anywhere = self
            .fc
            .feature_types
            .keys()
            .chain(self.fc.information_types.keys())
            .flat_map(|c| self.association_bindings(c))
            .any(|b| b.role == role && self.is_a(target, &b.target));
        Some(if anywhere {
            not_permitted()
        } else {
            (
                FcRule::UnknownAssociation,
                format!(
                    "No association in the feature catalogue links {source} to {target} through role '{role}'"
                ),
            )
        })
    }

    /// Associations bound between two classes in either direction, with
    /// their roles (association code, or the binding role when unnamed).
    fn associations_between(&self, a: &str, b: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        let sides = [(a, b), (b, a)];
        for (from, to) in sides {
            for binding in self.association_bindings(from) {
                if !self.is_a(to, &binding.target) {
                    continue;
                }
                let (name, mut roles) = match &binding.association {
                    Some(code) => (
                        code.clone(),
                        self.extra
                            .association_roles
                            .get(code)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    None => (format!("({})", binding.role), Vec::new()),
                };
                roles.push(binding.role.clone());
                match out.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, r)) => r.extend(roles),
                    None => out.push((name, roles)),
                }
            }
        }
        out
    }

    /// Distinct objects associated through binding `b`, outgoing or inverse.
    pub fn associated(
        &self,
        ds: &Dataset,
        index: usize,
        o: &Object,
        b: &AssociationBinding,
        incoming: &Incoming,
    ) -> HashSet<usize> {
        let mut out: HashSet<usize> = o
            .references
            .iter()
            .filter(|r| r.role == b.role)
            .filter_map(|r| r.target)
            .filter(|t| self.is_a(&ds.objects[*t].code, &b.target))
            .collect();
        let inverse: Vec<&str> = match b
            .association
            .as_ref()
            .and_then(|a| self.extra.association_roles.get(a))
        {
            Some(roles) if roles.len() == 2 && roles[0] == roles[1] => vec![roles[0].as_str()],
            Some(roles) => roles
                .iter()
                .filter(|r| **r != b.role)
                .map(String::as_str)
                .collect(),
            None => Vec::new(),
        };
        for &(from, r) in &incoming[index] {
            let source = &ds.objects[from];
            if inverse.contains(&source.references[r].role.as_str())
                && self.is_a(&source.code, &b.target)
            {
                out.insert(from);
            }
        }
        out
    }

    /// Checks attributes against the bindings of `owner` (class or complex attribute).
    fn attributes(
        &self,
        owner: &str,
        attrs: &[Attribute],
        context: &str,
        id: Option<&str>,
        sink: &mut IssueSink,
    ) {
        let bindings = self.attribute_bindings(owner);
        let position = |code: &str| bindings.iter().position(|(_, b)| b.attribute_code == code);
        let mut counts: HashMap<&str, u32> = HashMap::new();
        let mut last = 0;
        let mut misordered = false;
        for a in attrs {
            let path = format!("{context}.{}", a.code);
            let simple = self.fc.simple_attributes.get(&a.code);
            let complex = self.fc.complex_attributes.get(&a.code);
            if simple.is_none() && complex.is_none() {
                sink.rule(
                    FcRule::UnknownAttribute,
                    id,
                    format!("{path}: attribute code is not in the feature catalogue"),
                );
                continue;
            }
            let Some(p) = position(&a.code) else {
                sink.rule(
                    FcRule::AttributeNotPermitted,
                    id,
                    format!("{path}: not permitted on {owner}"),
                );
                continue;
            };
            misordered |= p < last;
            last = last.max(p);
            *counts
                .entry(bindings[p].1.attribute_code.as_str())
                .or_default() += 1;
            let (decl, binding) = bindings[p];
            let mandatory = binding.multiplicity.lower > 0;
            let empty = |rule_if_mandatory: FcRule, what: &str, sink: &mut IssueSink| {
                if mandatory {
                    sink.rule(
                        rule_if_mandatory,
                        id,
                        format!("{path}: mandatory attribute {what}"),
                    );
                } else {
                    sink.rule(
                        FcRule::OptionalEmpty,
                        id,
                        format!("{path}: optional attribute {what}"),
                    );
                }
            };
            match (&a.value, simple, complex) {
                (Value::Nil { reason }, ..) => {
                    if mandatory && reason.is_some() {
                        // Mandatory and unknown, with nilReason: S-158-129/2 is a reminder.
                        sink.rule(
                            FcRule::MandatoryNil,
                            id,
                            format!("{path}: mandatory attribute value is unknown"),
                        );
                    } else if mandatory {
                        sink.rule(
                            FcRule::MandatoryNil,
                            id,
                            format!("{path}: mandatory attribute is nil without nilReason"),
                        );
                    } else {
                        sink.rule(
                            FcRule::OptionalEmpty,
                            id,
                            format!("{path}: optional attribute value is unknown"),
                        );
                    }
                }
                (Value::Simple { text, code }, Some(sa), _) => {
                    let value = code.as_deref().unwrap_or(text).trim();
                    if value.is_empty() {
                        empty(FcRule::MandatoryNil, "has no value", sink);
                    } else {
                        let permitted = self.permitted_values(decl, binding);
                        self.simple_value(sa, a, value, &permitted, &path, id, sink);
                    }
                }
                (Value::Complex(children), Some(sa), _) => {
                    // S100_TruncatedDate is a choice of gDay .. date elements.
                    if self.is_truncated_date(sa)
                        && children.len() == 1
                        && children[0].text().is_some()
                    {
                        let v = children[0].text().unwrap_or("");
                        if !truncated_date_ok(&children[0].code, v) {
                            sink.rule(
                                FcRule::ValueFormat,
                                id,
                                format!("{path}: '{v}' is not a valid {}", children[0].code),
                            );
                        }
                    } else {
                        sink.rule(
                            FcRule::ValueFormat,
                            id,
                            format!("{path}: simple attribute has element content"),
                        );
                    }
                }
                (Value::Simple { text, .. }, None, Some(_)) => {
                    if text.trim().is_empty() {
                        empty(FcRule::MandatoryNil, "is empty", sink);
                    } else {
                        sink.rule(
                            FcRule::ValueFormat,
                            id,
                            format!("{path}: complex attribute has text content"),
                        );
                    }
                }
                (Value::Complex(children), None, Some(_)) => {
                    if children.is_empty() {
                        empty(FcRule::MandatoryNil, "is empty", sink);
                    }
                    self.attributes(&a.code, children, &path, id, sink);
                }
                _ => {}
            }
        }
        if misordered {
            sink.rule(
                FcRule::Order,
                id,
                format!("{context}: attributes are not in feature catalogue order"),
            );
        }
        for (_, b) in &bindings {
            let n = counts.get(b.attribute_code.as_str()).copied().unwrap_or(0);
            let m = &b.multiplicity;
            if n == 0 && m.lower > 0 {
                sink.rule(
                    FcRule::MandatoryMissing,
                    id,
                    format!(
                        "{context}.{}: mandatory attribute is missing",
                        b.attribute_code
                    ),
                );
            } else if n < m.lower {
                sink.rule(
                    FcRule::MandatoryMissing,
                    id,
                    format!(
                        "{context}.{}: {n} instance(s), at least {} required",
                        b.attribute_code, m.lower
                    ),
                );
            }
            if m.upper.is_some_and(|u| n > u) {
                sink.rule(
                    FcRule::TooMany,
                    id,
                    format!(
                        "{context}.{}: {n} instances, at most {} permitted",
                        b.attribute_code,
                        m.upper.unwrap_or(0)
                    ),
                );
            }
        }
    }

    fn is_truncated_date(&self, sa: &ferrite_feature_catalog::SimpleAttribute) -> bool {
        matches!(
            sa.value_type,
            AttributeValueType::TruncatedDate | AttributeValueType::S100TruncatedDate
        ) || self.extra.value_types.get(&sa.code).is_some_and(|v| {
            v.eq_ignore_ascii_case("S100_TruncatedDate") || v.eq_ignore_ascii_case("truncatedDate")
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn simple_value(
        &self,
        sa: &ferrite_feature_catalog::SimpleAttribute,
        a: &Attribute,
        value: &str,
        permitted: &[u32],
        path: &str,
        id: Option<&str>,
        sink: &mut IssueSink,
    ) {
        let bad = |sink: &mut IssueSink, rule: FcRule, kind: &str| {
            sink.rule(rule, id, format!("{path}: '{value}' is not a valid {kind}"));
        };
        if self.is_truncated_date(sa) {
            if !truncated_date_ok("", value) {
                bad(sink, FcRule::ValueFormat, "truncated date");
            }
            return;
        }
        match sa.value_type {
            AttributeValueType::Enumeration => {
                let listed = match value.parse::<u32>() {
                    Ok(c) => sa.listed_values.iter().find(|l| l.code == c),
                    Err(_) => sa
                        .listed_values
                        .iter()
                        .find(|l| l.label.eq_ignore_ascii_case(value)),
                };
                match listed {
                    None => sink.rule(
                        FcRule::Enumeration,
                        id,
                        format!("{path}: '{value}' is not a listed value of {}", sa.code),
                    ),
                    Some(l) if !permitted.is_empty() && !permitted.contains(&l.code) => sink.rule(
                        FcRule::Enumeration,
                        id,
                        format!(
                            "{path}: value {} ({}) is not permitted here",
                            l.code, l.label
                        ),
                    ),
                    Some(l) => {
                        let label = a.text().unwrap_or("").trim();
                        if !label.is_empty()
                            && value != label
                            && !label.eq_ignore_ascii_case(&l.label)
                        {
                            sink.product(
                                "FC/enumeration-label",
                                Severity::Warning,
                                id,
                                format!(
                                    "{path}: label '{label}' does not match code {} ({})",
                                    l.code, l.label
                                ),
                            );
                        }
                    }
                }
            }
            AttributeValueType::Integer => match integer_padded(value) {
                None => bad(sink, FcRule::ValueFormat, "integer"),
                Some(true) => sink.rule(
                    FcRule::Zeros,
                    id,
                    format!("{path}: '{value}' has non-significant zeros"),
                ),
                Some(false) => {}
            },
            AttributeValueType::Real => match real_padded(value) {
                None => bad(sink, FcRule::ValueFormat, "real"),
                Some(true) => sink.rule(
                    FcRule::Zeros,
                    id,
                    format!("{path}: '{value}' has non-significant zeros"),
                ),
                Some(false) => {}
            },
            AttributeValueType::Boolean => {
                if !matches!(value, "true" | "false" | "1" | "0") {
                    bad(sink, FcRule::ValueFormat, "boolean");
                }
            }
            AttributeValueType::Date => {
                if !date_ok(strip_zone(value)) {
                    bad(sink, FcRule::ValueFormat, "date (YYYY-MM-DD)");
                }
            }
            AttributeValueType::DateTime => {
                if parse_date_time(value).is_none() {
                    bad(
                        sink,
                        FcRule::DateTimeFormat,
                        "dateTime (YYYY-MM-DDThh:mm:ss[Z|±hh:mm])",
                    );
                }
            }
            AttributeValueType::Time => {
                if !time_ok(value) {
                    bad(sink, FcRule::ValueFormat, "time (hh:mm:ss[Z|±hh:mm])");
                }
            }
            AttributeValueType::Urn => {
                let urn = value
                    .get(..4)
                    .is_some_and(|p| p.eq_ignore_ascii_case("urn:"))
                    && value[4..].contains(':');
                if !urn || value.contains(char::is_whitespace) {
                    bad(sink, FcRule::ValueFormat, "URN");
                }
            }
            AttributeValueType::Uri | AttributeValueType::Url
                if value.contains(char::is_whitespace) =>
            {
                bad(sink, FcRule::ValueFormat, "URI");
            }
            _ => {}
        }
    }
}

/// For each object, the (source object, reference index) pairs that resolve to it.
fn incoming(ds: &Dataset) -> Vec<Vec<(usize, usize)>> {
    let mut out = vec![Vec::new(); ds.objects.len()];
    for (j, o) in ds.objects.iter().enumerate() {
        for (k, r) in o.references.iter().enumerate() {
            if let Some(t) = r.target {
                out[t].push((j, k));
            }
        }
    }
    out
}

/// The S-100 dataset file naming convention `<product code><producer
/// (4 upper-case alphanumerics)><unique part>.GML`; `unique_ok` decides the
/// product's rule for the unique part.
pub fn file_name_follows(
    product: &GmlIdentity,
    name: &str,
    unique_ok: impl Fn(&str) -> bool,
) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !name.is_ascii() || !matches!(ext, "GML" | "gml") {
        return false;
    }
    let Some(rest) = stem.strip_prefix(product.code) else {
        return false;
    };
    let (producer, unique) = rest.split_at(rest.len().min(4));
    producer.len() == 4 && producer.chars().all(upper_alnum) && unique_ok(unique)
}

/// An upper-case ASCII letter or a digit.
pub fn upper_alnum(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// Some(padded) for a valid integer, None if malformed.
fn integer_padded(v: &str) -> Option<bool> {
    let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits.len() > 1 && digits.starts_with('0'))
}

/// Some(padded) for a valid real, None if malformed.
fn real_padded(v: &str) -> Option<bool> {
    let body = v.strip_prefix(['+', '-']).unwrap_or(v);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    if let Some(e) = exponent {
        integer_padded(e)?;
    }
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int)
        || !frac.is_none_or(digits)
        || (int.is_empty() && frac.is_none_or(str::is_empty))
    {
        return None;
    }
    Some(
        (int.len() > 1 && int.starts_with('0'))
            || frac.is_some_and(|f| f.is_empty() || f.ends_with('0')),
    )
}

fn strip_zone(v: &str) -> &str {
    if let Some(s) = v.strip_suffix('Z') {
        return s;
    }
    let b = v.as_bytes();
    let n = b.len();
    if n > 6 && matches!(b[n - 6], b'+' | b'-') && b[n - 3] == b':' {
        let (h, m) = (&v[n - 5..n - 3], &v[n - 2..]);
        if h.parse::<u32>().is_ok_and(|h| h <= 14) && m.parse::<u32>().is_ok_and(|m| m < 60) {
            return &v[..n - 6];
        }
    }
    v
}

fn date_ok(v: &str) -> bool {
    v.len() == 10 && NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok()
}

/// hh:mm:ss[.f][Z|±hh:mm]
fn time_ok(v: &str) -> bool {
    let t = strip_zone(v);
    let b = t.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return false;
    }
    let digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    if !(digits(&b[0..2]) && digits(&b[3..5]) && digits(&b[6..8])) {
        return false;
    }
    if b.len() > 8 && (b[8] != b'.' || !digits(&b[9..])) {
        return false;
    }
    NaiveTime::parse_from_str(t, "%H:%M:%S%.f").is_ok()
}

/// Parses an xs:dateTime (extended format); values without a zone are taken as UTC.
pub fn parse_date_time(v: &str) -> Option<NaiveDateTime> {
    let (date, time) = v.trim().split_once('T')?;
    if !date_ok(date) || !time_ok(time) {
        return None;
    }
    let v = v.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.naive_utc());
    }
    NaiveDateTime::parse_from_str(strip_zone(v), "%Y-%m-%dT%H:%M:%S%.f").ok()
}

/// S100_TruncatedDate: a date or one of the xs:g* forms (`kind` is the element, if any).
fn truncated_date_ok(kind: &str, v: &str) -> bool {
    let v = v.trim();
    if !v.is_ascii() {
        return false;
    }
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let month = |s: &str| digits(s, 2) && (1..=12).contains(&s.parse::<u32>().unwrap_or(0));
    let day = |s: &str| digits(s, 2) && (1..=31).contains(&s.parse::<u32>().unwrap_or(0));
    let g_year = || digits(v, 4);
    let g_year_month = || v.len() == 7 && digits(&v[..4], 4) && &v[4..5] == "-" && month(&v[5..]);
    let g_month_day = || {
        v.len() == 7 && v.starts_with("--") && month(&v[2..4]) && &v[4..5] == "-" && day(&v[5..])
    };
    let g_month = || v.len() == 4 && v.starts_with("--") && month(&v[2..]);
    let g_day = || v.len() == 5 && v.starts_with("---") && day(&v[3..]);
    match kind {
        "date" => date_ok(v),
        "gYear" => g_year(),
        "gYearMonth" => g_year_month(),
        "gMonthDay" => g_month_day(),
        "gMonth" => g_month(),
        "gDay" => g_day(),
        _ => date_ok(v) || g_year() || g_year_month() || g_month_day() || g_month() || g_day(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::tests::{fc, S124_FC, S129_FC};

    #[test]
    fn number_and_time_formats() {
        assert_eq!(integer_padded("0"), Some(false));
        assert_eq!(integer_padded("-12"), Some(false));
        assert_eq!(integer_padded("012"), Some(true));
        assert_eq!(integer_padded("1.0"), None);
        assert_eq!(real_padded("2.5"), Some(false));
        assert_eq!(real_padded("0.113"), Some(false));
        assert_eq!(real_padded("02.500"), Some(true));
        assert_eq!(real_padded("2.50"), Some(true));
        assert_eq!(real_padded("2."), Some(true));
        assert_eq!(real_padded("6"), Some(false));
        assert_eq!(real_padded("-1.5e3"), Some(false));
        assert_eq!(real_padded("abc"), None);
        assert_eq!(real_padded("."), None);
        assert!(parse_date_time("2024-04-17T21:41:00Z").is_some());
        assert!(parse_date_time("2024-04-17T21:41:00.5+09:30").is_some());
        assert!(parse_date_time("2024-04-17T21:41:00").is_some());
        assert!(parse_date_time("20210101T000000Z").is_none());
        assert!(parse_date_time("2024-04-17 21:41:00Z").is_none());
        assert!(parse_date_time("2024-02-30T21:41:00Z").is_none());
        assert!(parse_date_time("2024-04-17T1:41:00Z").is_none());
        assert!(
            parse_date_time("2024-04-17T23:00:00-02:00").unwrap()
                > parse_date_time("2024-04-18T00:30:00Z").unwrap()
        );
        assert!(time_ok("08:00:00Z") && time_ok("08:00:00") && !time_ok("8:00"));
        for v in ["2024-01-31", "2024", "2024-02", "--02-29", "--12", "---31"] {
            assert!(truncated_date_ok("", v), "{v}");
        }
        for v in ["2024-13", "--13", "----1", "24"] {
            assert!(!truncated_date_ok("", v), "{v}");
        }
        assert!(truncated_date_ok("gYear", "2024") && !truncated_date_ok("gYear", "2024-01-01"));
        assert!(
            !truncated_date_ok("", "é-----")
                && !file_name_follows(&crate::reader::tests::S124, "124é.GML", |_| true)
        );
    }

    #[test]
    fn catalogue_supplement_reads_bindings() {
        let Some(fc124) = fc(S124_FC) else { return };
        let v = Validator::new(&fc124);
        assert_eq!(v.extra.roles.len(), 8);
        assert_eq!(
            v.extra.association_roles["navwarnPreambleContent"],
            ["header", "theWarningPart"]
        );
        let part = v.association_bindings("NavwarnPart");
        let mut roles: Vec<_> = part
            .iter()
            .map(|b| (b.role.as_str(), b.target.as_str(), b.lower, b.upper))
            .collect();
        roles.sort();
        assert_eq!(
            roles,
            [
                ("affects", "NavwarnAreaAffected", 0, None),
                ("header", "NavwarnPreamble", 1, Some(1)),
                ("thePositionProvider", "TextPlacement", 0, None),
            ]
        );
        assert_eq!(
            v.permitted_values("NavwarnPart", v.attribute_bindings("NavwarnPart")[0].1),
            [7, 8, 14, 25, 27]
        );
        assert_eq!(v.primitives("NavwarnPart"), ["point", "curve", "surface"]);
        assert_eq!(v.extra.value_types["dateStart"], "S100_TruncatedDate");
        let Some(fc129) = fc(S129_FC) else { return };
        let v = Validator::new(&fc129);
        let plan = v.association_bindings("UnderKeelClearancePlan");
        assert_eq!(plan.len(), 4);
        assert_eq!(
            (plan[3].target.as_str(), plan[3].lower, plan[3].upper),
            ("UnderKeelClearanceControlPoint", 1, None)
        );
        assert_eq!(v.primitives("UnderKeelClearancePlan"), ["nogeometry"]);
    }
}
