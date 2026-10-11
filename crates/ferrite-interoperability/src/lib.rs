//! Bounded reader for the display-plane subset of S-100 Part 16.
//! This is not an XSD validator, a trust verifier, or a complete S-98 processor.
//! Callers must validate/authenticate catalogues and select compatible editions before activation.
//! Unsupported constructs are errors; no suppression or replacement is silently discarded.
use anyhow::{bail, ensure, Context, Result};
use ferrite_kernel::{CompositionPlane, CompositionStage};
use roxmltree::{Document, Node, ParsingOptions};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    num::NonZeroI32,
};
const IC: &str = "http://www.iho.int/S100/IC/5.0";
const CAT: &str = "http://standards.iso.org/iso/19115/-3/cat/1.0";
const GCO: &str = "http://standards.iso.org/iso/19115/-3/gco/1.0";
const MAX_BYTES: usize = 8 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Decimal {
    negative: bool,
    whole: String,
    fraction: String,
}
impl Decimal {
    pub fn parse(s: &str) -> Result<Self> {
        let (negative, body) = if let Some(s) = s.strip_prefix('-') {
            (true, s)
        } else {
            (false, s.strip_prefix('+').unwrap_or(s))
        };
        let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
        ensure!(!whole.is_empty() || !fraction.is_empty(), "Empty decimal");
        ensure!(
            whole
                .bytes()
                .chain(fraction.bytes())
                .all(|c| c.is_ascii_digit()),
            "Invalid decimal {s:?}"
        );
        let whole = whole.trim_start_matches('0');
        let fraction = fraction.trim_end_matches('0');
        Ok(Self {
            negative: negative && (!whole.is_empty() || !fraction.is_empty()),
            whole: if whole.is_empty() {
                "0".into()
            } else {
                whole.into()
            },
            fraction: fraction.into(),
        })
    }
    /// Exact decimal expansion for ISO 8211 Real values, including exponential notation.
    /// Reject non-finite/out-of-domain values and cap exponent expansion before allocation.
    pub fn parse_real(s: &str) -> Result<Self> {
        ensure!(s.len() <= 8192, "Real lexical value exceeds limit");
        let finite: f64 = s.parse().context("Invalid Real encoding")?;
        ensure!(finite.is_finite(), "Non-finite Real encoding");
        let Some((mantissa, exponent)) = s.split_once(['e', 'E']) else {
            let value = Self::parse(s)?;
            ensure!(
                finite != 0. || (value.whole == "0" && value.fraction.is_empty()),
                "Real underflows IEEE-754 domain"
            );
            return Ok(value);
        };
        let exponent: i32 = exponent.parse().context("Invalid Real exponent")?;
        ensure!(
            exponent.abs_diff(0) <= 4096,
            "Real exponent exceeds expansion limit"
        );
        let value = Self::parse(mantissa)?;
        if value.whole == "0" && value.fraction.is_empty() {
            return Ok(value);
        }
        ensure!(finite != 0., "Real underflows IEEE-754 domain");
        let digits = format!("{}{}", value.whole, value.fraction);
        let point = value.whole.len() as i64 + exponent as i64;
        ensure!(
            digits.len() <= 8192,
            "Real mantissa exceeds expansion limit"
        );
        let decimal = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point >= digits.len() as i64 {
            format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        Self::parse(&format!(
            "{}{}",
            if value.negative { "-" } else { "" },
            decimal
        ))
    }
}
impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.negative != other.negative {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let magnitude = self
            .whole
            .len()
            .cmp(&other.whole.len())
            .then_with(|| self.whole.cmp(&other.whole))
            .then_with(|| {
                let n = self.fraction.len().max(other.fraction.len());
                self.fraction
                    .bytes()
                    .chain(std::iter::repeat(b'0'))
                    .take(n)
                    .cmp(
                        other
                            .fraction
                            .bytes()
                            .chain(std::iter::repeat(b'0'))
                            .take(n),
                    )
            });
        if self.negative {
            magnitude.reverse()
        } else {
            magnitude
        }
    }
}
impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scalar {
    Number(Decimal),
    Text(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Operator {
    Equal,
    NotEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttributeFilter {
    pub path: Vec<String>,
    op: Operator,
    value: Scalar,
}
impl AttributeFilter {
    pub fn parse(s: &str) -> Result<Self> {
        let split = |s: &str| s.find([' ', '\t']);
        let a =
            split(s).context("Filter requires whitespace-separated attribute/operator/value")?;
        let path = &s[..a];
        let rest = s[a..].trim_start_matches([' ', '\t']);
        let b = split(rest).context("Filter requires a value")?;
        let op = match &rest[..b] {
            "=" => Operator::Equal,
            "!=" => Operator::NotEqual,
            "gt" => Operator::Greater,
            "ge" => Operator::GreaterEqual,
            "lt" => Operator::Less,
            "le" => Operator::LessEqual,
            x => {
                bail!("Unsupported filter operator {x:?}: list/null semantics are not implemented")
            }
        };
        let v = rest[b..].trim_matches([' ', '\t']);
        let value = if v.starts_with('"') {
            let mut chars = v.chars();
            chars.next();
            let mut out = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '"' => {
                        ensure!(chars.next().is_none(), "Trailing filter input");
                        closed = true;
                        break;
                    }
                    '\\' => {
                        let c = chars.next().context("Unterminated escape")?;
                        ensure!(c == '"' || c == '\\', "Invalid string escape");
                        out.push(c)
                    }
                    _ => out.push(c),
                }
            }
            ensure!(closed, "Unterminated quoted value");
            ensure!(
                matches!(op, Operator::Equal | Operator::NotEqual),
                "Ordering on strings is not implemented"
            );
            Scalar::Text(out)
        } else {
            Scalar::Number(Decimal::parse(v)?)
        };
        let path: Vec<String> = path.split('/').map(str::to_owned).collect();
        ensure!(
            path.iter()
                .all(|p| p.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                    && p.bytes().all(|c| c.is_ascii_alphanumeric())),
            "Only relative camelCase child paths are supported"
        );
        Ok(Self { path, op, value })
    }
    /// Missing values do not match inequality. Callers must reject ambiguous repeated values.
    pub fn matches(&self, value: Option<&Scalar>) -> Result<bool> {
        let Some(value) = value else { return Ok(false) };
        let c = match (&self.value, value) {
            (Scalar::Number(a), Scalar::Number(b)) => b.cmp(a),
            (Scalar::Text(a), Scalar::Text(b)) => b.cmp(a),
            _ => bail!("Attribute type does not match filter operand"),
        };
        Ok(match self.op {
            Operator::Equal => c.is_eq(),
            Operator::NotEqual => !c.is_eq(),
            Operator::Greater => c.is_gt(),
            Operator::GreaterEqual => !c.is_lt(),
            Operator::Less => c.is_lt(),
            Operator::LessEqual => !c.is_gt(),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub plane: CompositionPlane,
    pub priority: i32,
    pub viewing_group: u32,
}
#[derive(Debug, Clone)]
enum RuleTarget {
    Feature,
    DrawingInstruction,
}
#[derive(Debug, Clone)]
struct Rule {
    // Plane order is a drawing key, not plane identity. Different planes may
    // carry the same order and still require unambiguous feature partitioning.
    plane_index: usize,
    target: RuleTarget,
    identifier: String,
    geometry: Vec<String>,
    filter: Option<AttributeFilter>,
    assignment: Assignment,
}
/// Expected O(R) time and O(R) memory within each product/feature rule bucket.
fn validate_selector_consistency(rules: &[Rule]) -> Result<()> {
    type Selector = (usize, Vec<String>, Option<AttributeFilter>);
    let mut seen: HashMap<Selector, [Option<&Rule>; 2]> = HashMap::new();
    for rule in rules {
        // geometryType is a set of selected primitives; XML order is irrelevant.
        let mut geometry = rule.geometry.clone();
        geometry.sort_unstable();
        geometry.dedup();
        let key = (rule.plane_index, geometry, rule.filter.clone());
        let kind = match rule.target {
            RuleTarget::Feature => 0,
            RuleTarget::DrawingInstruction => 1,
        };
        let pair = seen.entry(key).or_insert([None, None]);
        if let Some(other) = pair[1 - kind] {
            ensure!(
                other.assignment.priority == rule.assignment.priority
                    && other.assignment.viewing_group == rule.assignment.viewing_group,
                "Inconsistent IC Feature/DrawingInstruction selectors in plane {}: {} and {} must have equal drawingPriority and viewingGroup",
                rule.assignment.plane.order, other.identifier, rule.identifier
            );
        }
        // Retain each kind; check duplicate-kind conflicts too, otherwise a later
        // entry could conceal an inconsistent pair depending on document order.
        if let Some(other) = pair[kind] {
            ensure!(
                other.assignment == rule.assignment,
                "Conflicting duplicate IC selector: {} and {}",
                other.identifier,
                rule.identifier
            );
        }
        pair[kind] = Some(rule);
    }
    Ok(())
}
#[derive(Debug)]
pub struct ProductReference {
    pub code_list: String,
    pub code: String,
}
#[derive(Debug)]
pub struct Catalogue {
    pub name: String,
    pub version: String,
    pub version_date: String,
    pub products: Vec<ProductReference>,
    pub plane_count: usize,
    rules: HashMap<String, HashMap<String, Vec<Rule>>>,
}
fn children<'a, 'i>(n: Node<'a, 'i>, ns: Option<&str>, name: &str) -> Vec<Node<'a, 'i>> {
    n.children()
        .filter(|c| c.is_element() && c.tag_name().namespace() == ns && c.tag_name().name() == name)
        .collect()
}
fn one<'a, 'i>(n: Node<'a, 'i>, ns: Option<&str>, name: &str) -> Result<Node<'a, 'i>> {
    let c = children(n, ns, name);
    ensure!(c.len() == 1, "Expected one {name}, found {}", c.len());
    Ok(c[0])
}
fn text(n: Node<'_, '_>) -> Result<String> {
    ensure!(
        !n.children().any(|n| n.is_element()),
        "Expected scalar text in {}",
        n.tag_name().name()
    );
    Ok(n.text().unwrap_or("").trim().to_owned())
}
fn required(n: Node<'_, '_>, name: &str) -> Result<String> {
    text(one(n, None, name)?)
}
fn nonempty(s: String) -> Result<String> {
    ensure!(!s.is_empty(), "Empty required value");
    Ok(s)
}
fn allowed(n: Node<'_, '_>, names: &[&str]) -> Result<()> {
    for c in n.children().filter(Node::is_element) {
        ensure!(
            c.tag_name().namespace().is_none() && names.contains(&c.tag_name().name()),
            "Unsupported or wrong-namespace element {}",
            c.tag_name().name()
        );
    }
    Ok(())
}
fn product(n: Node<'_, '_>) -> Result<ProductReference> {
    ensure!(
        n.attribute("codeList").is_some_and(|x| !x.is_empty()),
        "Product requires codeList"
    );
    let code = n
        .attribute("codeListValue")
        .context("Product requires codeListValue")?;
    ensure!(!code.is_empty(), "Empty product code");
    text(n)?; // The label is not the identity. Preserve the codeListValue exactly.
    Ok(ProductReference {
        code_list: n.attribute("codeList").unwrap().into(),
        code: code.into(),
    })
}
fn level(n: Node<'_, '_>) -> Result<()> {
    let v = children(n, None, "interoperabilityLevel");
    ensure!(v.len() <= 1, "Duplicate interoperabilityLevel");
    if let Some(v) = v.first() {
        ensure!(
            text(*v)? == "1",
            "Only level 1 display-plane catalogues are supported"
        );
    }
    Ok(())
}
impl Catalogue {
    pub fn parse(xml: &str) -> Result<Self> {
        ensure!(xml.len() <= MAX_BYTES, "IC exceeds byte limit");
        let d = Document::parse_with_options(
            xml,
            ParsingOptions {
                allow_dtd: false,
                nodes_limit: 100_000,
            },
        )?;
        let root = d.root_element();
        ensure!(
            root.tag_name().namespace() == Some(IC)
                && root.tag_name().name() == "S100_IC_InteroperabilityCatalogue",
            "Unsupported IC root/namespace"
        );
        for c in root.children().filter(Node::is_element) {
            let t = c.tag_name();
            ensure!(
                (t.namespace() == Some(CAT)
                    && [
                        "name",
                        "scope",
                        "fieldOfApplication",
                        "versionNumber",
                        "versionDate",
                        "language",
                        "characterSet",
                        "locale"
                    ]
                    .contains(&t.name()))
                    || (t.namespace().is_none()
                        && [
                            "description",
                            "comment",
                            "interoperabilityLevel",
                            "requirementType",
                            "requirementDescription",
                            "productCovered",
                            "displayPlanes"
                        ]
                        .contains(&t.name())),
                "Unsupported IC element {} (PDC/suppression/substitution are not implemented)",
                t.name()
            );
        }
        let meta = |name, leaf| -> Result<String> {
            nonempty(text(one(one(root, Some(CAT), name)?, Some(GCO), leaf)?)?)
        };
        let name = meta("name", "CharacterString")?;
        let version = meta("versionNumber", "CharacterString")?;
        let version_date = meta("versionDate", "Date")?;
        ensure!(
            !children(root, Some(CAT), "scope").is_empty(),
            "Missing catalogue scope"
        );
        required(root, "description")?;
        required(root, "requirementType")?;
        level(root)?;
        for name in ["comment", "requirementDescription"] {
            ensure!(children(root, None, name).len() <= 1, "Duplicate {name}");
        }
        let products: Vec<ProductReference> = children(root, None, "productCovered")
            .into_iter()
            .map(product)
            .collect::<Result<_>>()?;
        ensure!(products.len() >= 2, "IC must cover at least two products");
        let product_set: HashMap<_, _> = products
            .iter()
            .map(|p| (p.code.as_str(), p.code_list.as_str()))
            .collect();
        ensure!(
            product_set.len() == products.len(),
            "Duplicate product code"
        );
        let container = one(root, None, "displayPlanes")?;
        allowed(container, &["S100_IC_DisplayPlane"])?;
        let planes = children(container, None, "S100_IC_DisplayPlane");
        ensure!(!planes.is_empty(), "No display planes");
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut rule_ids = HashSet::new();
        let mut rules: HashMap<String, HashMap<String, Vec<Rule>>> = HashMap::new();
        for (plane_index, p) in planes.iter().enumerate() {
            allowed(
                *p,
                &[
                    "identifier",
                    "name",
                    "order",
                    "description",
                    "interoperabilityLevel",
                    "features",
                    "drawingInstructions",
                ],
            )?;
            ensure!(
                ids.insert(nonempty(required(*p, "identifier")?)?),
                "Duplicate plane identifier"
            );
            ensure!(
                names.insert(nonempty(required(*p, "name")?)?),
                "Duplicate plane name"
            );
            required(*p, "description")?;
            level(*p)?;
            let order = NonZeroI32::new(required(*p, "order")?.parse()?)
                .context("Plane zero is reserved for RADAR")?;
            let mut count = 0;
            for (container, tag) in [
                ("features", "S100_IC_Feature"),
                ("drawingInstructions", "S100_IC_DrawingInstruction"),
            ] {
                let c = one(*p, None, container)?;
                allowed(c, &[tag])?;
                for r in children(c, None, tag) {
                    allowed(
                        r,
                        &[
                            "identifier",
                            "featureCode",
                            "product",
                            "geometryType",
                            "attributeCombination",
                            "drawingPriority",
                            "viewingGroup",
                        ],
                    )?;
                    let identifier = nonempty(required(r, "identifier")?)?;
                    ensure!(
                        rule_ids.insert(identifier.clone()),
                        "Duplicate rule identifier {identifier}"
                    );
                    let prod = product(one(r, None, "product")?)?;
                    ensure!(
                        product_set
                            .get(prod.code.as_str())
                            .is_some_and(|list| *list == prod.code_list),
                        "Rule product/code-list is not covered: {}",
                        prod.code
                    );
                    let feature = nonempty(required(r, "featureCode")?)?;
                    let geometry = children(r, None, "geometryType")
                        .into_iter()
                        .map(text)
                        .collect::<Result<Vec<_>>>()?;
                    ensure!(
                        geometry.iter().all(|s| [
                            "noGeometry",
                            "point",
                            "pointSet",
                            "curve",
                            "surface",
                            "coverage"
                        ]
                        .contains(&s.as_str())),
                        "Invalid spatial primitive"
                    );
                    let filters = children(r, None, "attributeCombination");
                    ensure!(filters.len()<=1,"Multiple attributeCombination conditions require an explicit combination policy, which is not implemented");
                    let filter = filters
                        .first()
                        .map(|n| AttributeFilter::parse(&text(*n)?))
                        .transpose()?;
                    let priority: i32 = required(r, "drawingPriority")?.parse()?;
                    ensure!(priority >= 0, "Negative priority");
                    let viewing_group = required(r, "viewingGroup")?.parse()?;
                    rules
                        .entry(prod.code)
                        .or_default()
                        .entry(feature)
                        .or_default()
                        .push(Rule {
                            plane_index,
                            target: if tag == "S100_IC_Feature" {
                                RuleTarget::Feature
                            } else {
                                RuleTarget::DrawingInstruction
                            },
                            identifier,
                            geometry,
                            filter,
                            assignment: Assignment {
                                plane: CompositionPlane::new(CompositionStage::Chart, order),
                                priority,
                                viewing_group,
                            },
                        });
                    count += 1;
                }
            }
            ensure!(count > 0, "Empty display plane");
        }
        // Part 16-4.2: identical selectors in the same plane must assign the same
        // priority/viewing group across Feature and DrawingInstruction entries.
        // Validate before any feature query: source and portrayal geometries differ,
        // so runtime conflict detection alone cannot enforce this catalogue invariant.
        for features in rules.values() {
            for selectors in features.values() {
                validate_selector_consistency(selectors)?;
            }
        }
        Ok(Self {
            name,
            version,
            version_date,
            products,
            plane_count: planes.len(),
            rules,
        })
    }
    /// Query only this product/feature's rules. Conflicting matches fail instead of depending on XML order.
    /// An absent assignment means ordinary product portrayal, never feature suppression.
    pub fn resolve(
        &self,
        product: &str,
        feature: &str,
        geometry: &str,
        attribute: impl FnMut(&[String]) -> Result<Option<Scalar>>,
    ) -> Result<Option<Assignment>> {
        self.resolve_portrayal(product, feature, geometry, geometry, attribute)
    }

    pub fn has_rules_for(&self, product: &str, feature: &str) -> bool {
        self.rules
            .get(product)
            .is_some_and(|m| m.contains_key(feature))
    }

    /// Feature selectors use the source primitive; drawing selectors use the portrayal primitive.
    pub fn resolve_portrayal(
        &self,
        product: &str,
        feature: &str,
        feature_geometry: &str,
        drawing_geometry: &str,
        mut attribute: impl FnMut(&[String]) -> Result<Option<Scalar>>,
    ) -> Result<Option<Assignment>> {
        let Some(rules) = self.rules.get(product).and_then(|m| m.get(feature)) else {
            return Ok(None);
        };
        let mut found: Option<&Rule> = None;
        for r in rules {
            let geometry = match r.target {
                RuleTarget::Feature => feature_geometry,
                RuleTarget::DrawingInstruction => drawing_geometry,
            };
            if !r.geometry.is_empty() && !r.geometry.iter().any(|g| g == geometry) {
                continue;
            }
            if let Some(filter) = &r.filter {
                if !filter.matches(attribute(&filter.path)?.as_ref())? {
                    continue;
                }
            }
            if let Some(old) = found {
                // Part 16-4.4.2.2 requires instances to be partitioned
                // unambiguously between display planes. Equal drawing keys
                // cannot turn matches in two distinct planes into one match.
                ensure!(
                    old.plane_index == r.plane_index,
                    "Ambiguous IC display-plane match for {product}/{feature}: {} and {}",
                    old.identifier,
                    r.identifier
                );
                ensure!(
                    old.assignment == r.assignment,
                    "Ambiguous IC match for {product}/{feature}: {}",
                    r.identifier
                );
            } else {
                found = Some(r);
            }
        }
        Ok(found.map(|rule| rule.assignment.clone()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> String {
        include_str!("../tests/display-plane.xml").into()
    }
    #[test]
    fn decimal_and_filter_exactness() {
        assert!(
            Decimal::parse("9007199254740993").unwrap()
                > Decimal::parse("9007199254740992").unwrap()
        );
        assert_eq!(
            Decimal::parse("-00.00").unwrap(),
            Decimal::parse("0").unwrap()
        );
        assert!(Decimal::parse("-.01").unwrap() < Decimal::parse("-.001").unwrap());
        assert_eq!(
            Decimal::parse_real("1E-5").unwrap(),
            Decimal::parse("0.00001").unwrap()
        );
        assert_eq!(
            Decimal::parse_real("-2.45E7").unwrap(),
            Decimal::parse("-24500000").unwrap()
        );
        for invalid in ["NaN", "INF", "1e9999", "1e-9999", "1e-324"] {
            assert!(Decimal::parse_real(invalid).is_err());
        }

        for s in ["NaN", "1e3", "1.2.3", ""] {
            assert!(Decimal::parse(s).is_err());
        }
        let f = AttributeFilter::parse("featureName/language = \"eng\"").unwrap();
        assert!(f.matches(Some(&Scalar::Text("eng".into()))).unwrap());
        assert!(!f.matches(None).unwrap());
        let f = AttributeFilter::parse("depth gt 30").unwrap();
        assert!(f
            .matches(Some(&Scalar::Number(Decimal::parse("30.00001").unwrap())))
            .unwrap());
        assert!(f.matches(Some(&Scalar::Text("31".into()))).is_err());
        assert!(AttributeFilter::parse("language = \"a\\\"b\\\\c\"")
            .unwrap()
            .matches(Some(&Scalar::Text("a\"b\\c".into())))
            .unwrap());
        for s in [
            "x = \"unterminated",
            "x = \"a\" tail",
            "x in 1,2",
            "x null",
            "child::x = 2",
            "x[1] = 2",
            "x = \"a\\n\"",
            "x gt \"a\"",
        ] {
            assert!(AttributeFilter::parse(s).is_err(), "{s}");
        }
    }
    #[test]
    fn catalogue_selection_and_fail_closed() {
        let c = Catalogue::parse(&fixture()).unwrap();
        assert_eq!(c.plane_count, 2);
        assert_eq!(
            c.resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
                .unwrap()
                .unwrap()
                .plane
                .order
                .get(),
            -500
        );
        assert_eq!(
            c.resolve("S-101", "Wreck", "point", |_| Ok(Some(Scalar::Number(
                Decimal::parse("1").unwrap()
            ))))
            .unwrap()
            .unwrap()
            .priority,
            4
        );
        assert!(c
            .resolve("S-101", "Wreck", "surface", |_| panic!())
            .unwrap()
            .is_none());
        assert!(c
            .resolve("S-129", "Wreck", "point", |_| panic!())
            .unwrap()
            .is_none());
        for x in [
            fixture().replace("<order>-500</order>", "<order>0</order>"),
            fixture().replace("<features>", "<features><substituteSymbolization/>"),
            fixture().replace(
                "<drawingPriority>4</drawingPriority>",
                "<drawingPriority>-1</drawingPriority>",
            ),
            fixture().replace("geometryType>point", "geometryType>arcByCenterPoint"),
            fixture().replace(
                "<displayPlanes>",
                "<predefinedProductCombinations/><displayPlanes>",
            ),
            fixture().replace("http://www.iho.int/S100/IC/5.0", "urn:spoof"),
            fixture().replace("<features>", "<features xmlns=\"urn:spoof\">"),
            fixture().replace("<order>-10</order>", "<order>2147483648</order>"),
            fixture().replace("<interoperabilityLevel>1", "<interoperabilityLevel>2"),
            fixture().replace("<viewingGroup>11010", "<viewingGroup>"),
            fixture().replace("<name>Danger", "<name>Coverage"),
        ] {
            assert!(Catalogue::parse(&x).is_err(), "{x}");
        }
        assert!(
            Catalogue::parse(&format!("<!DOCTYPE foo [<!ENTITY a 'x'>]>{}", fixture())).is_err()
        );
        let mut x = fixture();
        let start = x.find("<S100_IC_DisplayPlane>").unwrap();
        let end = x.find("</S100_IC_DisplayPlane>").unwrap() + "</S100_IC_DisplayPlane>".len();
        let duplicate = x[start..end]
            .replace("CoveragePlane", "ConflictPlane")
            .replace("<name>Coverage", "<name>Conflict")
            .replace("coverageRule", "conflictRule")
            .replace("<order>-500", "<order>-600");
        x.insert_str(end, &duplicate);
        let c = Catalogue::parse(&x).unwrap();
        assert!(c
            .resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
            .is_err());
    }
    #[test]
    fn identical_cross_target_selectors_are_validated_before_queries() {
        let xml = fixture();
        let begin = xml.find("<S100_IC_DrawingInstruction>").unwrap();
        let end = xml.find("</S100_IC_DrawingInstruction>").unwrap()
            + "</S100_IC_DrawingInstruction>".len();
        let feature = xml[begin..end]
            .replace("S100_IC_DrawingInstruction", "S100_IC_Feature")
            .replace("wreckRule", "wreckFeatureRule")
            .replace("categoryOfWreck = 1", "categoryOfWreck = +01.00");
        let matching = xml.replace(
            "<features/><drawingInstructions>",
            &format!("<features>{feature}</features><drawingInstructions>"),
        );
        assert!(Catalogue::parse(&matching).is_ok());
        for changed in [
            feature.replace("<drawingPriority>4", "<drawingPriority>5"),
            feature.replace("<viewingGroup>11010", "<viewingGroup>11011"),
        ] {
            let invalid = xml.replace(
                "<features/><drawingInstructions>",
                &format!("<features>{changed}</features><drawingInstructions>"),
            );
            let error = Catalogue::parse(&invalid).unwrap_err().to_string();
            assert!(
                error.contains("equal drawingPriority and viewingGroup"),
                "{error}"
            );
        }
        // Different attribute-value partitions are not the same selector.
        let different = feature
            .replace("categoryOfWreck = +01.00", "categoryOfWreck = 2")
            .replace("<drawingPriority>4", "<drawingPriority>5");
        assert!(Catalogue::parse(&xml.replace(
            "<features/><drawingInstructions>",
            &format!("<features>{different}</features><drawingInstructions>")
        ))
        .is_ok());
        // Equal geometry sets encoded in different order still have the same selector.
        let both = matching.replace(
            "<geometryType>point</geometryType>",
            "<geometryType>surface</geometryType><geometryType>point</geometryType>",
        );
        let flipped = both.replacen(
            "<geometryType>surface</geometryType><geometryType>point</geometryType>",
            "<geometryType>point</geometryType><geometryType>surface</geometryType>",
            1,
        );
        assert!(Catalogue::parse(&flipped).is_ok());
        assert!(
            Catalogue::parse(&flipped.replacen("<drawingPriority>4", "<drawingPriority>5", 1))
                .is_err()
        );
    }
    fn duplicate_coverage_plane(xml: &str, transform: impl FnOnce(String) -> String) -> String {
        let start = xml.find("<S100_IC_DisplayPlane>").unwrap();
        let end = xml.find("</S100_IC_DisplayPlane>").unwrap() + "</S100_IC_DisplayPlane>".len();
        let duplicate = xml[start..end]
            .replace("CoveragePlane", "OtherCoveragePlane")
            .replace("<name>Coverage", "<name>Other coverage")
            .replace("coverageRule", "otherCoverageRule");
        let mut result = xml.to_owned();
        result.insert_str(end, &transform(duplicate));
        result
    }

    #[test]
    fn identical_drawing_keys_do_not_merge_distinct_display_planes() {
        let xml = duplicate_coverage_plane(&fixture(), |plane| plane);
        let catalogue = Catalogue::parse(&xml).unwrap();
        let error = catalogue
            .resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Ambiguous IC display-plane match"),
            "{error}"
        );
    }

    #[test]
    fn overlapping_geometry_selectors_in_equal_order_planes_are_still_ambiguous() {
        let xml = duplicate_coverage_plane(&fixture(), |plane| {
            plane.replace("<geometryType>coverage</geometryType>", "")
        });
        let catalogue = Catalogue::parse(&xml).unwrap();
        assert!(catalogue
            .resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
            .is_err());
        // A non-overlapping primitive selects only the unrestricted second plane.
        assert!(catalogue
            .resolve("S-102", "BathymetryCoverage", "surface", |_| panic!())
            .unwrap()
            .is_some());
    }

    #[test]
    fn separate_plane_selectors_can_partition_instances_at_the_same_drawing_order() {
        let xml = fixture().replace("<geometryType>coverage</geometryType>",
            "<geometryType>coverage</geometryType><attributeCombination>depth lt 30</attributeCombination>");
        let xml = duplicate_coverage_plane(&xml, |plane| {
            plane
                .replace("depth lt 30", "depth ge 30")
                .replace("<drawingPriority>3", "<drawingPriority>7")
        });
        let catalogue = Catalogue::parse(&xml).unwrap();
        for (depth, priority) in [("29.99999", 3), ("30", 7), ("9007199254740993", 7)] {
            let resolved = catalogue
                .resolve("S-102", "BathymetryCoverage", "coverage", |_| {
                    Ok(Some(Scalar::Number(Decimal::parse(depth)?)))
                })
                .unwrap()
                .unwrap();
            assert_eq!(resolved.priority, priority);
        }
        assert!(catalogue
            .resolve("S-102", "BathymetryCoverage", "coverage", |_| Ok(None))
            .unwrap()
            .is_none());
    }

    #[test]
    fn equal_assignments_from_overlapping_rules_in_the_same_plane_remain_valid() {
        let mut xml = fixture();
        let start = xml.find("<S100_IC_Feature>").unwrap();
        let end = xml.find("</S100_IC_Feature>").unwrap() + "</S100_IC_Feature>".len();
        let duplicate = xml[start..end]
            .replace("coverageRule", "secondRule")
            .replace("<geometryType>coverage</geometryType>", "");
        xml.insert_str(end, &duplicate);
        let catalogue = Catalogue::parse(&xml).unwrap();
        assert_eq!(
            catalogue
                .resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
                .unwrap()
                .unwrap()
                .priority,
            3
        );
    }

    #[test]
    fn source_and_portrayal_primitive_selectors_are_distinct() {
        let c = Catalogue::parse(&fixture()).unwrap();
        // Wreck rule is a DrawingInstruction selector: an area source can emit a point symbol.
        assert!(c
            .resolve_portrayal("S-101", "Wreck", "surface", "point", |_| Ok(Some(
                Scalar::Number(Decimal::parse("1").unwrap())
            )))
            .unwrap()
            .is_some());
        let xml = fixture()
            .replace(
                "<features/><drawingInstructions><S100_IC_DrawingInstruction>",
                "<features><S100_IC_Feature>",
            )
            .replace(
                "</S100_IC_DrawingInstruction></drawingInstructions>",
                "</S100_IC_Feature></features><drawingInstructions/>",
            );
        let c = Catalogue::parse(&xml).unwrap();
        assert!(c
            .resolve_portrayal("S-101", "Wreck", "surface", "point", |_| panic!())
            .unwrap()
            .is_none());
    }
}

mod authenticated;
pub use authenticated::AuthenticatedCatalogue;
