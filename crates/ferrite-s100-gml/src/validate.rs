//! Feature catalogue and product rule validation of Part 10b datasets.
//!
//! Generic checks are driven by the feature catalogue. For S-129 every check
//! is a warning (S-158-129 A.1.3) and carries its S-158-129 number where one
//! applies; other checks use `FC/<rule>`, `S-124/<rule>` or `S-129/<rule>`.
//! Association names are not encoded in Part 10b, so S-158-129 checks 4, 6
//! and 9 infer the association from (source class, role, target class);
//! exchange-set checks (11, 17-20) are in [`crate::exchange`].
use crate::model::*;
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use ferrite_feature_catalog::{
    AttributeBinding, AttributeValueType, FeatureCatalogue, SpatialPrimitive,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// S-124 9.6: datasets must not exceed 50 KB.
pub const S124_MAX_BYTES: u64 = 50 * 1024;
/// S-129 18.2.2: recommended maximum dataset size.
pub const S129_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Validates with the byte size taken from the dataset's source file, if any.
/// Returns validation issues only; reader issues stay in `dataset.issues`.
pub fn validate(dataset: &Dataset, fc: &FeatureCatalogue) -> Vec<Issue> {
    let size = dataset
        .source
        .as_deref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    Validator::new(fc).validate(dataset, size)
}

/// Generic FC rules, mapped per product to a check identifier and severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
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

impl Rule {
    fn name(self) -> &'static str {
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
    fn s158_129(self) -> Option<u32> {
        Some(match self {
            Self::MandatoryMissing => 1,
            Self::MandatoryNil => 2,
            Self::Zeros => 3,
            Self::RoleNotPermitted => 5,
            Self::RoleUnknown => 7,
            Self::WrongTarget => 8,
            Self::WrongAssociation => 4,
            Self::UnknownAssociation => 6,
            Self::RoleOnWrongAssociation => 9,
            Self::ValueFormat | Self::Enumeration => 10,
            Self::UnknownClass => 12,
            Self::UnknownAttribute => 13,
            Self::AttributeNotPermitted => 14,
            Self::Order => 15,
            Self::TooMany => 16,
            Self::OptionalEmpty => 21,
            Self::FileName => 22,
            Self::DateTimeFormat => 26,
            Self::Primitive | Self::AssociationMultiplicity | Self::Product => return None,
        })
    }
    fn severity(self) -> Severity {
        match self {
            Self::Zeros | Self::Order | Self::OptionalEmpty => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

#[derive(Debug, Clone)]
struct AssociationBinding {
    role: String,
    association: Option<String>,
    target: String,
    lower: u32,
    upper: Option<u32>,
}

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
struct Sink {
    product: Product,
    issues: Vec<Issue>,
}

impl Sink {
    fn rule(&mut self, rule: Rule, object: Option<&str>, message: String) {
        let (check, severity) = match (self.product, rule.s158_129()) {
            (Product::S129, Some(n)) => (format!("S-158-129/{n}"), Severity::Warning),
            (Product::S129, None) => (format!("FC/{}", rule.name()), Severity::Warning),
            (Product::S124, _) => (format!("FC/{}", rule.name()), rule.severity()),
        };
        self.issues.push(Issue {
            severity,
            check,
            object: object.map(str::to_owned),
            message,
        });
    }
    fn product(&mut self, check: &str, severity: Severity, object: Option<&str>, message: String) {
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

    /// Validates a dataset; `byte_size` enables the dataset size checks.
    pub fn validate(&self, dataset: &Dataset, byte_size: Option<u64>) -> Vec<Issue> {
        let mut sink = Sink {
            product: dataset.product,
            issues: Vec::new(),
        };
        self.dataset_checks(dataset, byte_size, &mut sink);
        let incoming = incoming(dataset);
        for (i, object) in dataset.objects.iter().enumerate() {
            self.object_checks(dataset, i, object, &incoming, &mut sink);
        }
        match dataset.product {
            Product::S124 => s124_checks(dataset, byte_size, &mut sink),
            Product::S129 => self.s129_checks(dataset, &incoming, &mut sink),
        }
        sink.issues
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
    fn association_bindings(&self, code: &str) -> Vec<AssociationBinding> {
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

    fn dataset_checks(&self, ds: &Dataset, byte_size: Option<u64>, sink: &mut Sink) {
        let id = &ds.identification;
        let fc_product = self.fc.product_id.trim();
        if !fc_product.is_empty() && fc_product != ds.product.identifier() {
            sink.rule(
                Rule::Product,
                None,
                format!(
                    "Dataset is {} but the feature catalogue is {fc_product}",
                    ds.product.identifier()
                ),
            );
        }
        match id.product_identifier() {
            Some(p) if Product::from_identifier(p) == Some(ds.product) => {}
            other => sink.rule(
                Rule::Product,
                None,
                format!(
                    "productIdentifier {other:?} is not {}",
                    ds.product.identifier()
                ),
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
            if !file_name_ok(ds.product, &name) {
                sink.rule(
                    Rule::FileName,
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
        if let (Product::S129, Some(size)) = (ds.product, byte_size) {
            if size > S129_MAX_BYTES {
                sink.product(
                    "S-129/size",
                    Severity::Warning,
                    None,
                    format!("Dataset is {size} bytes; the recommended maximum is 20 MB"),
                );
            }
        }
    }

    fn object_checks(
        &self,
        ds: &Dataset,
        index: usize,
        o: &Object,
        incoming: &[Vec<(usize, usize)>],
        sink: &mut Sink,
    ) {
        let id = Some(o.id.as_str());
        let feature = self.fc.feature_types.get(&o.code);
        let info = self.fc.information_types.get(&o.code);
        let is_abstract = match (feature, info) {
            (Some(f), _) => f.is_abstract,
            (_, Some(i)) => i.is_abstract,
            _ => {
                sink.rule(
                    Rule::UnknownClass,
                    id,
                    format!("{} is not a feature or information type", o.code),
                );
                return;
            }
        };
        if is_abstract {
            sink.rule(Rule::UnknownClass, id, format!("{} is abstract", o.code));
        }
        self.attributes(&o.code, &o.attributes, &o.code, id, sink);

        // Geometry primitives.
        if feature.is_none() && !o.geometry.is_empty() {
            sink.rule(
                Rule::Primitive,
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
                        Rule::Primitive,
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
                    Rule::RoleUnknown,
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
            if (ds.product, o.code.as_str(), b.role.as_str())
                == (Product::S129, "UnderKeelClearancePlan", "theComponent")
                && b.target == "UnderKeelClearanceControlPoint"
                || (ds.product, o.code.as_str(), b.role.as_str())
                    == (Product::S124, "NavwarnPart", "header")
            {
                continue; // Reported by the product checks (S-158-129/27, S-124/header).
            }
            let n = self.associated(ds, index, o, b, incoming).len() as u32;
            if n < b.lower || b.upper.is_some_and(|u| n > u) {
                sink.rule(
                    Rule::AssociationMultiplicity,
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
    ) -> Option<(Rule, String)> {
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
                Rule::UnknownAssociation,
                format!("Role '{role}' belongs to no association in the feature catalogue"),
            ));
        }
        let not_permitted = || {
            (
                Rule::RoleNotPermitted,
                format!("Role '{role}' is not permitted on {source}"),
            )
        };
        let Some(target) = target else {
            return candidates.is_empty().then(not_permitted);
        };
        let wrong_target = || {
            let expected: Vec<_> = candidates.iter().map(|b| b.target.as_str()).collect();
            (
                Rule::WrongTarget,
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
                    Rule::RoleNotPermitted,
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
                    Rule::RoleOnWrongAssociation,
                    format!(
                        "Role '{role}' of association {implied} is used on association {names} between {source} and {target}, which does not include it"
                    ),
                )
            } else {
                (
                    Rule::WrongAssociation,
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
                Rule::UnknownAssociation,
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
    fn associated(
        &self,
        ds: &Dataset,
        index: usize,
        o: &Object,
        b: &AssociationBinding,
        incoming: &[Vec<(usize, usize)>],
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
        sink: &mut Sink,
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
                    Rule::UnknownAttribute,
                    id,
                    format!("{path}: attribute code is not in the feature catalogue"),
                );
                continue;
            }
            let Some(p) = position(&a.code) else {
                sink.rule(
                    Rule::AttributeNotPermitted,
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
            let empty = |rule_if_mandatory: Rule, what: &str, sink: &mut Sink| {
                if mandatory {
                    sink.rule(
                        rule_if_mandatory,
                        id,
                        format!("{path}: mandatory attribute {what}"),
                    );
                } else {
                    sink.rule(
                        Rule::OptionalEmpty,
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
                            Rule::MandatoryNil,
                            id,
                            format!("{path}: mandatory attribute value is unknown"),
                        );
                    } else if mandatory {
                        sink.rule(
                            Rule::MandatoryNil,
                            id,
                            format!("{path}: mandatory attribute is nil without nilReason"),
                        );
                    } else {
                        sink.rule(
                            Rule::OptionalEmpty,
                            id,
                            format!("{path}: optional attribute value is unknown"),
                        );
                    }
                }
                (Value::Simple { text, code }, Some(sa), _) => {
                    let value = code.as_deref().unwrap_or(text).trim();
                    if value.is_empty() {
                        empty(Rule::MandatoryNil, "has no value", sink);
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
                                Rule::ValueFormat,
                                id,
                                format!("{path}: '{v}' is not a valid {}", children[0].code),
                            );
                        }
                    } else {
                        sink.rule(
                            Rule::ValueFormat,
                            id,
                            format!("{path}: simple attribute has element content"),
                        );
                    }
                }
                (Value::Simple { text, .. }, None, Some(_)) => {
                    if text.trim().is_empty() {
                        empty(Rule::MandatoryNil, "is empty", sink);
                    } else {
                        sink.rule(
                            Rule::ValueFormat,
                            id,
                            format!("{path}: complex attribute has text content"),
                        );
                    }
                }
                (Value::Complex(children), None, Some(_)) => {
                    if children.is_empty() {
                        empty(Rule::MandatoryNil, "is empty", sink);
                    }
                    self.attributes(&a.code, children, &path, id, sink);
                }
                _ => {}
            }
        }
        if misordered {
            sink.rule(
                Rule::Order,
                id,
                format!("{context}: attributes are not in feature catalogue order"),
            );
        }
        for (_, b) in &bindings {
            let n = counts.get(b.attribute_code.as_str()).copied().unwrap_or(0);
            let m = &b.multiplicity;
            if n == 0 && m.lower > 0 {
                sink.rule(
                    Rule::MandatoryMissing,
                    id,
                    format!(
                        "{context}.{}: mandatory attribute is missing",
                        b.attribute_code
                    ),
                );
            } else if n < m.lower {
                sink.rule(
                    Rule::MandatoryMissing,
                    id,
                    format!(
                        "{context}.{}: {n} instance(s), at least {} required",
                        b.attribute_code, m.lower
                    ),
                );
            }
            if m.upper.is_some_and(|u| n > u) {
                sink.rule(
                    Rule::TooMany,
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
        sink: &mut Sink,
    ) {
        let bad = |sink: &mut Sink, rule: Rule, kind: &str| {
            sink.rule(rule, id, format!("{path}: '{value}' is not a valid {kind}"));
        };
        if self.is_truncated_date(sa) {
            if !truncated_date_ok("", value) {
                bad(sink, Rule::ValueFormat, "truncated date");
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
                        Rule::Enumeration,
                        id,
                        format!("{path}: '{value}' is not a listed value of {}", sa.code),
                    ),
                    Some(l) if !permitted.is_empty() && !permitted.contains(&l.code) => sink.rule(
                        Rule::Enumeration,
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
                None => bad(sink, Rule::ValueFormat, "integer"),
                Some(true) => sink.rule(
                    Rule::Zeros,
                    id,
                    format!("{path}: '{value}' has non-significant zeros"),
                ),
                Some(false) => {}
            },
            AttributeValueType::Real => match real_padded(value) {
                None => bad(sink, Rule::ValueFormat, "real"),
                Some(true) => sink.rule(
                    Rule::Zeros,
                    id,
                    format!("{path}: '{value}' has non-significant zeros"),
                ),
                Some(false) => {}
            },
            AttributeValueType::Boolean => {
                if !matches!(value, "true" | "false" | "1" | "0") {
                    bad(sink, Rule::ValueFormat, "boolean");
                }
            }
            AttributeValueType::Date => {
                if !date_ok(strip_zone(value)) {
                    bad(sink, Rule::ValueFormat, "date (YYYY-MM-DD)");
                }
            }
            AttributeValueType::DateTime => {
                if parse_date_time(value).is_none() {
                    bad(
                        sink,
                        Rule::DateTimeFormat,
                        "dateTime (YYYY-MM-DDThh:mm:ss[Z|±hh:mm])",
                    );
                }
            }
            AttributeValueType::Time => {
                if !time_ok(value) {
                    bad(sink, Rule::ValueFormat, "time (hh:mm:ss[Z|±hh:mm])");
                }
            }
            AttributeValueType::Urn => {
                let urn = value
                    .get(..4)
                    .is_some_and(|p| p.eq_ignore_ascii_case("urn:"))
                    && value[4..].contains(':');
                if !urn || value.contains(char::is_whitespace) {
                    bad(sink, Rule::ValueFormat, "URN");
                }
            }
            AttributeValueType::Uri | AttributeValueType::Url
                if value.contains(char::is_whitespace) =>
            {
                bad(sink, Rule::ValueFormat, "URI");
            }
            _ => {}
        }
    }

    fn s129_checks(&self, ds: &Dataset, incoming: &[Vec<(usize, usize)>], sink: &mut Sink) {
        for o in &ds.objects {
            let id = Some(o.id.as_str());
            for r in o.attributes_named("fixedTimeRange") {
                let value = |name: &str| {
                    r.children()
                        .iter()
                        .find(|c| c.code == name)
                        .and_then(|c| c.text())
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                };
                match (value("timeStart"), value("timeEnd")) {
                    (Some(s), Some(e)) => {
                        if let (Some(s), Some(e)) = (parse_date_time(s), parse_date_time(e)) {
                            if s > e {
                                sink.product(
                                    "S-158-129/23",
                                    Severity::Warning,
                                    id,
                                    "fixedTimeRange: timeStart is later than timeEnd".into(),
                                );
                            }
                        }
                    }
                    (Some(_), None) => sink.product(
                        "S-158-129/24",
                        Severity::Warning,
                        id,
                        "fixedTimeRange: timeStart without timeEnd".into(),
                    ),
                    (None, Some(_)) => sink.product(
                        "S-158-129/25",
                        Severity::Warning,
                        id,
                        "fixedTimeRange: timeEnd without timeStart".into(),
                    ),
                    (None, None) => {}
                }
            }
        }
        let binding = AssociationBinding {
            role: "theComponent".into(),
            association: Some("UnderKeelClearanceControlPointComposition".into()),
            target: "UnderKeelClearanceControlPoint".into(),
            lower: 1,
            upper: None,
        };
        let mut binding = binding;
        if let Some(b) = self
            .association_bindings("UnderKeelClearancePlan")
            .into_iter()
            .find(|b| b.target == "UnderKeelClearanceControlPoint")
        {
            binding = b;
        }
        for (i, plan) in ds.objects_of("UnderKeelClearancePlan") {
            if self.associated(ds, i, plan, &binding, incoming).is_empty() {
                sink.product(
                    "S-158-129/27",
                    Severity::Warning,
                    Some(&plan.id),
                    "An UnderKeelClearancePlan must consist of at least 1 UnderKeelClearanceControlPoint association".into(),
                );
            }
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

fn s124_checks(ds: &Dataset, byte_size: Option<u64>, sink: &mut Sink) {
    let err = Severity::Error;
    if let Some(size) = byte_size {
        if size > S124_MAX_BYTES {
            sink.product(
                "S-124/size",
                err,
                None,
                format!("Dataset is {size} bytes; S-124 datasets must not exceed 50 KB"),
            );
        }
    }
    let preambles: Vec<_> = ds.objects_of("NavwarnPreamble").map(|(i, _)| i).collect();
    if preambles.len() != 1 {
        sink.product(
            "S-124/preamble",
            err,
            None,
            format!(
                "Dataset has {} NavwarnPreamble instances; exactly one is required",
                preambles.len()
            ),
        );
    }
    let parts: Vec<_> = ds.objects_of("NavwarnPart").collect();
    for (_, part) in &parts {
        let headers: HashSet<usize> = part
            .references
            .iter()
            .filter(|r| r.role == "header")
            .filter_map(|r| r.target)
            .filter(|t| ds.objects[*t].code == "NavwarnPreamble")
            .collect();
        if headers.len() != 1
            || part
                .references
                .iter()
                .filter(|r| r.role == "header")
                .count()
                != 1
        {
            sink.product(
                "S-124/header",
                err,
                Some(&part.id),
                "NavwarnPart must have exactly one header reference to the NavwarnPreamble".into(),
            );
        }
        for wi in part.attributes_named("warningInformation") {
            let has = |c: &str| wi.children().iter().any(|a| a.code == c);
            if !has("information") && !has("navwarnTypeDetails") {
                sink.product(
                    "S-124/warning-information",
                    err,
                    Some(&part.id),
                    "warningInformation must contain information or navwarnTypeDetails".into(),
                );
            }
            let mut languages = HashSet::new();
            for info in wi.children().iter().filter(|a| a.code == "information") {
                let lang = info
                    .children()
                    .iter()
                    .find(|c| c.code == "language")
                    .and_then(|c| c.text());
                if !languages.insert(lang.unwrap_or("").trim().to_string()) {
                    sink.product(
                        "S-124/warning-information",
                        err,
                        Some(&part.id),
                        format!(
                            "warningInformation has more than one information in language {lang:?}"
                        ),
                    );
                }
            }
        }
        for range in part.attributes_named("fixedDateRange") {
            let has = |c: &str| range.children().iter().any(|a| a.code == c);
            for (time, date) in [("timeOfDayStart", "dateStart"), ("timeOfDayEnd", "dateEnd")] {
                if has(time) && !has(date) {
                    sink.product(
                        "S-124/fixed-date-range",
                        err,
                        Some(&part.id),
                        format!("fixedDateRange: {time} requires {date}"),
                    );
                }
            }
        }
    }
    for &p in &preambles {
        let pre = &ds.objects[p];
        // Associated with at least one References or NavwarnPart (either direction).
        let linked = pre
            .references
            .iter()
            .filter_map(|r| r.target)
            .chain(ds.objects.iter().enumerate().filter_map(|(j, o)| {
                o.references
                    .iter()
                    .any(|r| r.target == Some(p))
                    .then_some(j)
            }))
            .any(|t| matches!(ds.objects[t].code.as_str(), "References" | "NavwarnPart"));
        if !linked {
            sink.product(
                "S-124/preamble-content",
                err,
                Some(&pre.id),
                "NavwarnPreamble must be associated with at least one References or NavwarnPart"
                    .into(),
            );
        }
        if let (Some(c), Some(p)) = (
            pre.value(&["cancellationDate"]),
            pre.value(&["publicationTime"]),
        ) {
            if let (Some(c), Some(p)) = (parse_date_time(c), parse_date_time(p)) {
                if c <= p {
                    sink.product(
                        "S-124/cancellation-date",
                        Severity::Warning,
                        Some(&pre.id),
                        "cancellationDate is not later than publicationTime".into(),
                    );
                }
            }
        }
    }

    // References content (S-124 4.x constraints, 8.1.2, 8.1.4, 9.3).
    let references: Vec<_> = ds.objects_of("References").collect();
    let mut in_force = Vec::new();
    let mut cancellations = 0;
    for (i, r) in &references {
        let id = Some(r.id.as_str());
        let no_message = r
            .value(&["noMessageOnHand"])
            .map(|v| matches!(v.trim(), "true" | "1"));
        let category = r.value(&["referenceCategory"]).map(str::trim);
        let series = r.attributes_named("messageSeriesIdentifier").count();
        match no_message {
            Some(true) if series > 0 => sink.product(
                "S-124/references",
                err,
                id,
                "messageSeriesIdentifier is prohibited when noMessageOnHand is true".into(),
            ),
            Some(false) if series == 0 => sink.product(
                "S-124/references",
                err,
                id,
                "messageSeriesIdentifier is mandatory when noMessageOnHand is false".into(),
            ),
            _ => {}
        }
        if no_message == Some(true) && category != Some("3") {
            sink.product(
                "S-124/references",
                Severity::Warning,
                id,
                "noMessageOnHand=true is only meaningful in an in-force bulletin (referenceCategory 3)".into(),
            );
        }
        match category {
            Some("3") => in_force.push((*i, no_message == Some(true))),
            Some("1") => cancellations += 1,
            _ => {}
        }
    }
    if !in_force.is_empty() {
        if in_force.len() > 1 {
            sink.product(
                "S-124/in-force-bulletin",
                err,
                None,
                format!("In-force bulletin has {} References with referenceCategory 3; exactly one is required", in_force.len()),
            );
        }
        for o in &ds.objects {
            if matches!(o.code.as_str(), "NavwarnAreaAffected" | "TextPlacement")
                || !o.geometry.is_empty()
            {
                sink.product(
                    "S-124/in-force-bulletin",
                    err,
                    Some(&o.id),
                    format!("In-force bulletin must not contain geometry, NavwarnAreaAffected or TextPlacement ({})", o.code),
                );
            }
        }
        if let Some((keep, _)) = in_force.iter().find(|(_, nmoh)| *nmoh) {
            for (j, o) in ds.objects.iter().enumerate() {
                if j != *keep && o.code != "NavwarnPreamble" {
                    sink.product(
                        "S-124/in-force-bulletin",
                        err,
                        Some(&o.id),
                        format!("In-force bulletin with noMessageOnHand=true permits no other content ({})", o.code),
                    );
                }
            }
        }
    } else if parts.is_empty() && cancellations > 1 {
        sink.product(
            "S-124/cancellation",
            Severity::Warning,
            None,
            format!("Cancellation dataset should contain only one References instance, found {cancellations}"),
        );
    }
    for code in ["NavwarnAreaAffected", "TextPlacement"] {
        for (_, o) in ds.objects_of(code) {
            if o.geometry.is_empty() {
                sink.product(
                    "S-124/geometry",
                    err,
                    Some(&o.id),
                    format!("{code} must have geometry"),
                );
            }
        }
    }
}

pub(crate) fn file_name_ok(product: Product, name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !name.is_ascii() || !matches!(ext, "GML" | "gml") {
        return false;
    }
    let Some(rest) = stem.strip_prefix(product.code()) else {
        return false;
    };
    let (producer, unique) = rest.split_at(rest.len().min(4));
    let upper_alnum = |c: char| c.is_ascii_uppercase() || c.is_ascii_digit();
    producer.len() == 4
        && producer.chars().all(upper_alnum)
        && match product {
            Product::S129 => {
                unique.len() <= 8 && unique.chars().all(|c| upper_alnum(c) || c == '_')
            }
            Product::S124 => unique.chars().all(upper_alnum),
        }
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
fn parse_date_time(v: &str) -> Option<NaiveDateTime> {
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
    use crate::reader::tests::{
        fc, fc as fc_of, read, workspace_file, S124_DIR, S124_FC, S129_130, S129_200, S129_FC,
    };
    use crate::reader::{read_bytes, ReadLimits};

    fn count(issues: &[Issue], check: &str) -> usize {
        issues.iter().filter(|i| i.check == check).count()
    }
    fn checks(issues: &[Issue]) -> std::collections::BTreeMap<&str, usize> {
        let mut m = std::collections::BTreeMap::new();
        for i in issues {
            *m.entry(i.check.as_str()).or_default() += 1;
        }
        m
    }

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
        assert!(!truncated_date_ok("", "é-----") && !file_name_ok(Product::S124, "124é.GML"));
    }

    #[test]
    fn file_names() {
        assert!(file_name_ok(Product::S129, "12900MCTDS130TS.gml"));
        assert!(file_name_ok(Product::S129, "129ABCD.GML"));
        assert!(file_name_ok(Product::S129, "129ABCD_1234567.GML"));
        assert!(!file_name_ok(Product::S129, "129ABCD123456789.GML"));
        assert!(!file_name_ok(Product::S129, "129abcd.GML"));
        assert!(!file_name_ok(Product::S129, "129ABCD.xml"));
        assert!(!file_name_ok(Product::S129, "124ABCD.GML"));
        assert!(file_name_ok(Product::S124, "124GB00S1Y2026N36.GML"));
        assert!(!file_name_ok(Product::S124, "124GB0.GML"));
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

    #[test]
    fn s124_ukho_datasets() {
        let Some(fc) = fc(S124_FC) else { return };
        let v = Validator::new(&fc);
        let mut all = Vec::new();
        for name in [
            "124GB00S1Y2026N36.GML",
            "124GB00S2Y2026N43.GML",
            "124GB00S2Y2026N44.GML",
            "124GB00S2Y2026N45.GML",
            "124GB00S2Y2026N46.GML",
        ] {
            let Some(ds) = read(&format!("{S124_DIR}/{name}"), &fc) else {
                return;
            };
            let issues = validate(&ds, &fc);
            assert_eq!(
                issues,
                v.validate(
                    &ds,
                    std::fs::metadata(ds.source.as_ref().unwrap())
                        .ok()
                        .map(|m| m.len())
                )
            );
            let per_file = checks(&issues);
            // Every file uses countryName (FC: nationality), so nationality is
            // also reported missing only if mandatory (it is optional).
            assert!(
                count(&issues, "FC/unknown-attribute") >= 1,
                "{name}: {per_file:?}"
            );
            assert!(
                !per_file.contains_key("S-124/header"),
                "{name}: {per_file:?}"
            );
            assert!(!per_file.contains_key("S-124/preamble"), "{name}");
            assert!(!per_file.contains_key("S-124/preamble-content"), "{name}");
            assert!(!per_file.contains_key("S-124/size"), "{name}");
            assert!(!per_file.contains_key("FC/file-name"), "{name}");
            assert!(
                !per_file.contains_key("FC/association-multiplicity"),
                "{name}: {issues:#?}"
            );
            all.extend(issues);
        }
        let totals = checks(&all);
        // countryName in all 5 files; featureName/Name in N43, N44, N45.
        assert_eq!(totals.get("FC/unknown-attribute"), Some(&8), "{totals:?}");
        let names: Vec<_> = all
            .iter()
            .filter(|i| i.check == "FC/unknown-attribute")
            .map(|i| i.message.split(':').next().unwrap())
            .collect();
        assert_eq!(
            names.iter().filter(|n| n.ends_with(".countryName")).count(),
            5
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| **n == "NavwarnPart.featureName.Name")
                .count(),
            3
        );
        // ... which leaves featureName without its mandatory name.
        assert_eq!(totals.get("FC/mandatory-missing"), Some(&3), "{totals:?}");
        assert!(all
            .iter()
            .filter(|i| i.check == "FC/mandatory-missing")
            .all(|i| i.message.starts_with("NavwarnPart.featureName.name")));
        assert_eq!(totals.len(), 2, "{totals:?}");
        assert!(all.iter().all(|i| i.severity == Severity::Error));
    }

    #[test]
    fn s129_iho_datasets() {
        let Some(fc) = fc(S129_FC) else { return };
        let Some(a) = read(S129_130, &fc) else { return };
        let Some(b) = read(S129_200, &fc) else { return };
        for (ds, empty_ranges) in [(&a, 7), (&b, 0)] {
            let issues = validate(ds, &fc);
            assert!(issues.iter().all(|i| i.severity == Severity::Warning));
            let c = checks(&issues);
            // The test datasets encode no associations at all.
            assert_eq!(c.get("S-158-129/27"), Some(&1), "{c:?}");
            // Every area and control point lacks its theCollection link to the plan.
            assert_eq!(
                c.get("FC/association-multiplicity"),
                Some(&(1 + 87 + 200 + 15)),
                "{c:?}"
            );
            // 130TS has seven empty <fixedTimeRange/> elements.
            assert_eq!(
                c.get("S-158-129/21").copied().unwrap_or(0),
                empty_ranges,
                "{c:?}"
            );
            for n in [1, 2, 3, 5, 7, 8, 12, 13, 14, 15, 16, 22, 23, 24, 25, 26] {
                assert_eq!(count(&issues, &format!("S-158-129/{n}")), 0, "{n}: {c:?}");
            }
            assert_eq!(issues.len(), 1 + 303 + empty_ranges, "{c:?}");
        }
    }

    fn s129(members: &str) -> Dataset {
        let xml = format!(
            r#"<Dataset xmlns="http://www.iho.int/S129/2.0" xmlns:S100="http://www.iho.int/s100gml/5.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" gml:id="DS">
<S100:DatasetIdentificationInformation><S100:productIdentifier>S-129</S100:productIdentifier><S100:productEdition>2.0.0</S100:productEdition><S100:datasetFileIdentifier>129ABCD01.GML</S100:datasetFileIdentifier></S100:DatasetIdentificationInformation>
<members>{members}</members></Dataset>"#
        );
        read_bytes(xml.as_bytes(), None, ReadLimits::default()).unwrap()
    }

    #[test]
    fn s129_synthetic_violations() {
        let Some(fc) = fc(S129_FC) else { return };
        let ds = s129(
            r##"<UnderKeelClearancePlan gml:id="PLAN">
  <generationTime>2024-04-15 23:11</generationTime>
  <fixedTimeRange><timeStart>2024-04-18T01:00:00Z</timeStart><timeEnd>2024-04-17T01:00:00Z</timeEnd></fixedTimeRange>
  <vesselID>1</vesselID><vesselID>2</vesselID>
  <maximumDraught>012.20</maximumDraught>
  <underKeelClearancePurpose code="9">Bogus</underKeelClearancePurpose>
  <scaleMinimum>1</scaleMinimum>
  <madeUp>1</madeUp>
  <theComponent xlink:href="#CP"/>
</UnderKeelClearancePlan>
<UnderKeelClearanceControlPoint gml:id="CP">
  <featureName><language>eng</language><nameUsage code="3"/></featureName>
  <expectedPassingSpeed xsi:nil="true" nilReason="unknown"/>
  <fixedTimeRange><timeEnd>2024-04-17T01:00:00Z</timeEnd></fixedTimeRange>
  <theCollection xlink:href="#PLAN"/><theNothing xlink:href="#PLAN"/><theComponent xlink:href="#PLAN"/>
</UnderKeelClearanceControlPoint>
<UnderKeelClearanceControlPoint gml:id="CP2"><fixedTimeRange><timeStart>2024-04-17T01:00:00Z</timeStart></fixedTimeRange><theCollection xlink:href="#CP"/></UnderKeelClearanceControlPoint>
<Bogus gml:id="X"/>"##,
        );
        let issues = Validator::new(&fc).validate(&ds, None);
        let c = checks(&issues);
        let expect = [
            ("S-158-129/1", 1),  // featureName.name missing
            ("S-158-129/3", 1),  // 012.20
            ("S-158-129/5", 1),  // theComponent on a control point
            ("S-158-129/7", 1),  // theNothing
            ("S-158-129/8", 1),  // CP2 theCollection -> control point
            ("S-158-129/10", 2), // enumeration codes 9 and 3
            ("S-158-129/12", 1), // Bogus
            ("S-158-129/13", 1), // madeUp
            ("S-158-129/14", 1), // scaleMinimum on the plan
            ("S-158-129/15", 1), // generationTime before fixedTimeRange
            ("S-158-129/16", 1), // two vesselID
            ("S-158-129/21", 1), // nil expectedPassingSpeed
            ("S-158-129/23", 1),
            ("S-158-129/24", 1),
            ("S-158-129/25", 1),
            ("S-158-129/26", 1),
            ("FC/association-multiplicity", 1), // CP2 lacks a plan
        ];
        for (check, n) in expect {
            assert_eq!(
                c.get(check).copied().unwrap_or(0),
                n,
                "{check}: {issues:#?}"
            );
        }
        assert_eq!(
            issues.len(),
            expect.iter().map(|e| e.1).sum::<usize>(),
            "{c:?}"
        );
        // The plan's control point arrives through the inverse theCollection role.
        assert_eq!(count(&issues, "S-158-129/27"), 0);
    }

    /// The S-129 catalogue with an orphan role and a second association
    /// (ControlPointMonitoring: theMonitor/theMonitored, bound on control
    /// points towards plan areas), so that checks 4, 6 and 9 are reachable.
    pub(crate) fn patched_s129_fc(tag: &str) -> Option<FeatureCatalogue> {
        let text = std::fs::read_to_string(workspace_file(S129_FC)?).unwrap();
        let role = |code: &str| {
            format!("<S100FC:S100_FC_Role><S100FC:name>{code}</S100FC:name><S100FC:definition>Test role.</S100FC:definition><S100FC:code>{code}</S100FC:code></S100FC:S100_FC_Role>")
        };
        let mut patched = text.replacen(
            "</S100FC:S100_FC_Roles>",
            &format!(
                "{}{}{}</S100FC:S100_FC_Roles>",
                role("theMonitor"),
                role("theMonitored"),
                role("theOrphan")
            ),
            1,
        );
        patched = patched.replacen(
            "</S100FC:S100_FC_FeatureAssociations>",
            r#"<S100FC:S100_FC_FeatureAssociation isAbstract="false"><S100FC:name>Monitoring</S100FC:name><S100FC:definition>Test association.</S100FC:definition><S100FC:code>ControlPointMonitoring</S100FC:code><S100FC:role ref="theMonitor"/><S100FC:role ref="theMonitored"/></S100FC:S100_FC_FeatureAssociation></S100FC:S100_FC_FeatureAssociations>"#,
            1,
        );
        // The last binding in the catalogue is the control point's plan link.
        let at =
            patched.rfind("</S100FC:featureBinding>").unwrap() + "</S100FC:featureBinding>".len();
        assert!(patched[..at]
            .trim_end_matches("</S100FC:featureBinding>")
            .trim_end()
            .ends_with("<S100FC:featureType ref=\"UnderKeelClearancePlan\"/>"));
        patched.insert_str(
            at,
            r#"<S100FC:featureBinding roleType="association"><S100FC:multiplicity><S100Base:lower>0</S100Base:lower><S100Base:upper xsi:nil="true" infinite="true"/></S100FC:multiplicity><S100FC:association ref="ControlPointMonitoring"/><S100FC:role ref="theMonitored"/><S100FC:featureType ref="UnderKeelClearancePlanArea"/></S100FC:featureBinding>"#,
        );
        let dir = std::env::temp_dir().join(format!("ferrite-gml-fc-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("129_FC_patched.xml");
        std::fs::write(&path, patched).unwrap();
        Some(FeatureCatalogue::load(&path).unwrap())
    }

    #[test]
    fn s129_association_inference() {
        let Some(fc) = patched_s129_fc("assoc") else {
            return;
        };
        let v = Validator::new(&fc);
        assert_eq!(v.extra.association_roles["ControlPointMonitoring"].len(), 2);
        let ds = s129(
            r##"<UnderKeelClearancePlan gml:id="PLAN">
  <theComponent xlink:href="#CP"/>
  <theOrphan xlink:href="#CP"/>
  <theMonitored xlink:href="#CP"/>
</UnderKeelClearancePlan>
<UnderKeelClearanceControlPoint gml:id="CP">
  <theCollection xlink:href="#PLAN"/>
  <theMonitored xlink:href="#PLAN"/>
  <theComponent xlink:href="#PLAN"/>
</UnderKeelClearanceControlPoint>"##,
        );
        let issues: Vec<_> = v
            .validate(&ds, None)
            .into_iter()
            .filter(|i| {
                ["/4", "/5", "/6", "/7", "/8", "/9"]
                    .iter()
                    .any(|n| i.check == format!("S-158-129{n}"))
            })
            .collect();
        let found: Vec<_> = issues
            .iter()
            .map(|i| (i.check.as_str(), i.object.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            found,
            [
                ("S-158-129/6", "PLAN"), // theOrphan: in no association
                ("S-158-129/4", "PLAN"), // theMonitored: plan is not monitored
                ("S-158-129/9", "CP"),   // CP's monitoring role on the composition
                ("S-158-129/5", "CP"),   // composition role, wrong end
            ],
            "{issues:#?}"
        );
        assert!(issues.iter().all(|i| i.severity == Severity::Warning));
        // The released S-129 catalogue: both roles belong to every association,
        // so a known role can only be the wrong end (5) or target (8).
        let Some(plain) = fc_of(S129_FC) else { return };
        let issues = Validator::new(&plain).validate(&ds, None);
        let c = checks(&issues);
        assert_eq!(c.get("S-158-129/7"), Some(&3), "{c:?}");
        assert_eq!(c.get("S-158-129/5"), Some(&1), "{c:?}");
        for n in [4, 6, 9] {
            assert_eq!(count(&issues, &format!("S-158-129/{n}")), 0, "{c:?}");
        }
    }

    #[test]
    fn s124_association_inference() {
        let Some(fc) = fc_of(S124_FC) else { return };
        let ds = s124(&format!(
            r##"{PREAMBLE}</S124:NavwarnPreamble>
<S124:References gml:id="R"><S124:referenceCategory code="1"/><S124:theWarning xlink:href="#PRE"/><S124:affects xlink:href="#P"/></S124:References>
<S124:NavwarnPart gml:id="P"><S124:header xlink:href="#PRE"/><S124:affects xlink:href="#A"/><S124:theWarning xlink:href="#A"/><S124:header xlink:href="#A"/><S124:impacts xlink:href="#A"/></S124:NavwarnPart>
<S124:NavwarnAreaAffected gml:id="A"><S124:impacts xlink:href="#P"/></S124:NavwarnAreaAffected>"##
        ));
        let issues = Validator::new(&fc).validate(&ds, None);
        let found: Vec<_> = issues
            .iter()
            .filter(|i| {
                i.check.starts_with("FC/association-") && i.check != "FC/association-multiplicity"
                    || i.check.starts_with("FC/role-")
            })
            .map(|i| (i.check.as_str(), i.object.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            found,
            [
                ("FC/association-unknown", "R"), // affects: References - NavwarnPart
                ("FC/association-wrong", "P"),   // theWarning on Part - AreaAffected
                ("FC/role-association", "P"),    // header (Part's own) on areaAffected
                ("FC/role-not-permitted", "P"),  // impacts: the area's end
            ],
            "{issues:#?}"
        );
        assert!(issues
            .iter()
            .filter(|i| i.check.starts_with("FC/association-u")
                || i.check.starts_with("FC/association-w")
                || i.check == "FC/role-association")
            .all(|i| i.severity == Severity::Error));
    }

    fn s124(members: &str) -> Dataset {
        let xml = format!(
            r#"<S124:Dataset xmlns:S124="http://www.iho.int/S124/gml/2.0" xmlns:S100="http://www.iho.int/s100gml/5.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:xlink="http://www.w3.org/1999/xlink" gml:id="DS">
<S100:DatasetIdentificationInformation><S100:productIdentifier>S-124</S100:productIdentifier><S100:productEdition>2.0.0</S100:productEdition><S100:datasetFileIdentifier>124GB00IFB1.GML</S100:datasetFileIdentifier></S100:DatasetIdentificationInformation>
<S124:members>{members}</S124:members></S124:Dataset>"#
        );
        read_bytes(xml.as_bytes(), None, ReadLimits::default()).unwrap()
    }
    const PREAMBLE: &str = r#"<S124:NavwarnPreamble gml:id="PRE"><S124:generalArea><S124:locationName><S124:language>eng</S124:language><S124:text>WIGHT</S124:text></S124:locationName></S124:generalArea><S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction><S124:nameOfSeries>WZ</S124:nameOfSeries><S124:warningNumber>1</S124:warningNumber><S124:warningType code="2"/><S124:year>2026</S124:year></S124:messageSeriesIdentifier><S124:cancellationDate>2026-01-01T00:00:00Z</S124:cancellationDate><S124:intService>true</S124:intService><S124:navwarnTypeGeneral code="8"/><S124:publicationTime>2026-02-13T08:42:41Z</S124:publicationTime>"#;

    #[test]
    fn s124_in_force_bulletin_rules() {
        let Some(fc) = fc(S124_FC) else { return };
        let v = Validator::new(&fc);
        // No message on hand: only the preamble and one References.
        let ok = s124(&format!(
            r##"{PREAMBLE}<S124:theReferences xlink:href="#R"/></S124:NavwarnPreamble>
<S124:References gml:id="R"><S124:noMessageOnHand>true</S124:noMessageOnHand><S124:referenceCategory code="3"/></S124:References>"##
        ));
        let issues = v.validate(&ok, Some(1000));
        assert_eq!(
            checks(&issues).into_iter().collect::<Vec<_>>(),
            [("S-124/cancellation-date", 1)],
            "{issues:#?}"
        );

        let bad = s124(&format!(
            r##"{PREAMBLE}</S124:NavwarnPreamble>
<S124:References gml:id="R"><S124:messageSeriesIdentifier><S124:agencyResponsibleForProduction>UKHO</S124:agencyResponsibleForProduction><S124:nameOfSeries>WZ</S124:nameOfSeries><S124:warningNumber>1</S124:warningNumber><S124:warningType code="2"/><S124:year>2026</S124:year></S124:messageSeriesIdentifier><S124:noMessageOnHand>true</S124:noMessageOnHand><S124:referenceCategory code="3"/><S124:theWarning xlink:href="#PRE"/></S124:References>
<S124:NavwarnPart gml:id="P"><S124:fixedDateRange><S124:timeOfDayStart>08:00:00</S124:timeOfDayStart></S124:fixedDateRange><S124:warningInformation/><S124:header xlink:href="#PRE"/><S124:header xlink:href="#PRE"/><S124:geometry><S100:pointProperty><S100:Point gml:id="G"><gml:pos>50 -1</gml:pos></S100:Point></S100:pointProperty></S124:geometry></S124:NavwarnPart>
<S124:NavwarnAreaAffected gml:id="A"><S124:impacts xlink:href="#P"/></S124:NavwarnAreaAffected>"##
        ));
        let issues = v.validate(&bad, Some(60_000));
        let c = checks(&issues);
        assert_eq!(c.get("S-124/size"), Some(&1));
        assert_eq!(c.get("S-124/references"), Some(&1), "{issues:#?}");
        assert_eq!(c.get("S-124/header"), Some(&1));
        assert_eq!(c.get("S-124/warning-information"), Some(&1));
        assert_eq!(c.get("S-124/fixed-date-range"), Some(&1));
        assert_eq!(c.get("S-124/geometry"), Some(&1));
        // P has geometry, A is an area; with noMessageOnHand both are extra content.
        assert_eq!(c.get("S-124/in-force-bulletin"), Some(&4), "{issues:#?}");
        // warningInformation is mandatory and present but empty.
        assert_eq!(c.get("FC/mandatory-nil"), Some(&1), "{issues:#?}");
    }

    #[test]
    fn s124_preamble_rules() {
        let Some(fc) = fc(S124_FC) else { return };
        let ds = s124(&format!(
            "{PREAMBLE}</S124:NavwarnPreamble>{}",
            PREAMBLE.replace("\"PRE\"", "\"PRE2\"") + "</S124:NavwarnPreamble>"
        ));
        let issues = Validator::new(&fc).validate(&ds, None);
        let c = checks(&issues);
        assert_eq!(c.get("S-124/preamble"), Some(&1));
        assert_eq!(c.get("S-124/preamble-content"), Some(&2), "{issues:#?}");
        assert!(workspace_file(S124_FC).is_some());
    }
}
