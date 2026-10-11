//! Bounded reader for the Level 1 and Level 2 subset of S-100 Part 16: display
//! planes, predefined combinations and suppressed feature layers, plus the
//! S-98 2.0.0 clause 3.3 profile, and substituteSymbolization in its Part 9
//! XML form (symbol, lineStyleReference). Level 3/4 hybridization, derived
//! features and other substitute fragments are refused. This is not an XSD validator, a trust
//! verifier, or a complete S-98 processor. Callers must validate/authenticate
//! catalogues and select compatible editions before activation.
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
/// S-98 2.0.0 Parts A/B clause 3.3 name the first; Annex A 4.1.1 names the second.
const S98_PRODUCT_DICTIONARIES: [&str; 2] = [
    "urn:mrn:iho:prod:s98:1:0:0:products",
    "urn:mrn:iho:prod:s98:1:1:0:products",
];
/// S-98 2.0.0 Annex A Table 2-1.
const S98_PRODUCTS: [&str; 5] = ["S-101", "S-102", "S-104", "S-111", "S-129"];
/// S-98 2.0.0 Table A-3.1 / B-3-1; "other: ..." values are also permitted.
const S98_REQUIREMENT_TYPES: [&str; 8] = [
    "IHO", "OEM", "national", "local", "port", "company", "pilot", "master",
];
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scalar {
    Number(Decimal),
    Text(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Operator {
    Equal,
    NotEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
            // S-100 Part 16-4.4.3 / S-98 Annex A 4.3 list these operators but
            // define neither a list-value syntax nor the operand of "null".
            // Refuse rather than invent an encoding.
            x @ ("in" | "notIn" | "null") => bail!(
                "Filter operator {x:?} has no normative value encoding in S-100 5.2.1 Part 16-4.4.3; not supported"
            ),
            x => bail!("Unknown filter operator {x:?}"),
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
    /// S100_IC_DrawingInstruction substituteSymbolization, if any.
    pub substitution: Option<std::sync::Arc<Substitution>>,
}
/// Part 9 PointInstruction `symbol` (S100SymbolDefinition Symbol) without
/// child elements. Real numbers are kept as exact bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSubstitute {
    pub reference: String,
    rotation: Option<u64>,
    pub rotation_crs: Option<String>,
    scale_factor: Option<u64>,
}
impl SymbolSubstitute {
    pub fn rotation(&self) -> Option<f64> {
        self.rotation.map(f64::from_bits)
    }
    pub fn scale_factor(&self) -> Option<f64> {
        self.scale_factor.map(f64::from_bits)
    }
}
/// S-100 Part 16-4.4.2.4 substituteSymbolization in its XML form: fragments
/// of the Part 9 Presentation Schema elements that descendants of
/// DrawingInstruction add. Supported: PointInstruction `symbol`,
/// LineInstruction `lineStyleReference`, AreaInstruction `colorFill` and
/// `areaFillReference`, TextInstruction `textPoint` (one element). Inline or
/// composite line styles, pixmap/symbol/hatch fills given inline, textLine,
/// text flags/fonts/offsets and non-XML rule syntaxes are refused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Substitution {
    pub symbol: Option<SymbolSubstitute>,
    pub line_style: Option<String>,
    pub area_fill: Option<AreaFillSubstitute>,
    pub text: Option<TextSubstitute>,
}
/// Part 9 Color: a colour token with transparency in [0, 1] (exact bits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorSubstitute {
    pub token: String,
    transparency: u64,
}
impl ColorSubstitute {
    pub fn transparency(&self) -> f64 {
        f64::from_bits(self.transparency)
    }
    fn parse(node: Node<'_, '_>) -> Result<Self> {
        ensure!(
            node.attributes().all(|a| a.name() == "transparency"),
            "Unsupported color attribute"
        );
        let token = text(node)?;
        ensure!(
            !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric()),
            "Invalid colour token"
        );
        let transparency: f64 = node
            .attribute("transparency")
            .unwrap_or("0")
            .trim()
            .parse()?;
        ensure!(
            (0.0..=1.0).contains(&transparency),
            "Colour transparency outside [0, 1]"
        );
        Ok(Self {
            token,
            transparency: transparency.to_bits(),
        })
    }
}
/// Part 9 AreaFillGroup subset: colorFill or areaFillReference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AreaFillSubstitute {
    Color(ColorSubstitute),
    Reference(String),
}
/// Part 9 TextPoint with one TextElement (text, bodySize, foreground and
/// optional background), alignments and rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSubstitute {
    pub text: String,
    body_size: u64,
    pub foreground: ColorSubstitute,
    pub background: Option<ColorSubstitute>,
    /// Start, Center or End (Part 9 default Start).
    pub horizontal_alignment: String,
    /// Top, Center or Bottom (Part 9 default Bottom).
    pub vertical_alignment: String,
    rotation: u64,
}
impl TextSubstitute {
    pub fn body_size(&self) -> f64 {
        f64::from_bits(self.body_size)
    }
    pub fn rotation(&self) -> f64 {
        f64::from_bits(self.rotation)
    }
    fn parse(node: Node<'_, '_>) -> Result<Self> {
        for a in node.attributes() {
            ensure!(
                ["horizontalAlignment", "verticalAlignment", "rotation"].contains(&a.name()),
                "Unsupported textPoint attribute {}",
                a.name()
            );
        }
        let horizontal_alignment = node.attribute("horizontalAlignment").unwrap_or("Start");
        ensure!(
            ["Start", "Center", "End"].contains(&horizontal_alignment),
            "Invalid horizontalAlignment"
        );
        let vertical_alignment = node.attribute("verticalAlignment").unwrap_or("Bottom");
        ensure!(
            ["Top", "Center", "Bottom"].contains(&vertical_alignment),
            "Invalid verticalAlignment"
        );
        let rotation: f64 = node.attribute("rotation").unwrap_or("0").trim().parse()?;
        ensure!(rotation.is_finite(), "Non-finite text rotation");
        // offset/areaPlacement and multiple elements are not supported.
        let elements: Vec<_> = node.children().filter(Node::is_element).collect();
        ensure!(
            elements.len() == 1 && elements[0].tag_name().name() == "element",
            "Substitute textPoint must hold exactly one element (offset/areaPlacement unsupported)"
        );
        let element = elements[0];
        ensure!(
            element.attributes().next().is_none(),
            "Substitute text verticalOffset is not supported"
        );
        let mut text_value = None;
        let mut body_size = None;
        let mut foreground = None;
        let mut background = None;
        for c in element.children().filter(Node::is_element) {
            match c.tag_name().name() {
                "text" if text_value.is_none() => text_value = Some(text(c)?),
                "bodySize" if body_size.is_none() => {
                    let v: f64 = text(c)?.parse()?;
                    ensure!(v.is_finite() && v > 0., "Invalid text bodySize");
                    body_size = Some(v);
                }
                "foreground" if foreground.is_none() => {
                    foreground = Some(ColorSubstitute::parse(c)?)
                }
                "background" if background.is_none() => {
                    background = Some(ColorSubstitute::parse(c)?)
                }
                other => bail!("Substitute text element {other} is not supported or repeated"),
            }
        }
        Ok(Self {
            text: text_value.context("Substitute text requires text")?,
            body_size: body_size
                .context("Substitute text requires bodySize")?
                .to_bits(),
            foreground: foreground.context("Substitute text requires foreground")?,
            background,
            horizontal_alignment: horizontal_alignment.to_owned(),
            vertical_alignment: vertical_alignment.to_owned(),
            rotation: rotation.to_bits(),
        })
    }
}
impl Substitution {
    fn parse(node: Node<'_, '_>) -> Result<Self> {
        let elements: Vec<_> = node.children().filter(Node::is_element).collect();
        let fragment;
        let document;
        let items: Vec<Node<'_, '_>> = if elements.is_empty() {
            // Text or CDATA holding XML fragments.
            fragment = format!(
                "<substitute>{}</substitute>",
                node.text().unwrap_or("").trim()
            );
            document = Document::parse_with_options(
                &fragment,
                ParsingOptions {
                    allow_dtd: false,
                    nodes_limit: 1_000,
                },
            )
            .context("substituteSymbolization is not an XML fragment; non-XML syntaxes are not supported")?;
            let root = document.root_element();
            ensure!(
                !root
                    .children()
                    .any(|n| n.is_text() && !n.text().unwrap_or("").trim().is_empty()),
                "substituteSymbolization mixes text with elements"
            );
            root.children().filter(Node::is_element).collect()
        } else {
            ensure!(
                !node
                    .children()
                    .any(|n| n.is_text() && !n.text().unwrap_or("").trim().is_empty()),
                "substituteSymbolization mixes text with elements"
            );
            elements
        };
        ensure!(!items.is_empty(), "Empty substituteSymbolization");
        let mut out = Self::default();
        for item in items {
            let attribute = |name: &str| item.attribute(name).map(str::trim);
            let reference = || -> Result<String> {
                let r = attribute("reference").context("Substitute requires a reference")?;
                ensure!(
                    !r.is_empty() && !r.contains(char::is_whitespace),
                    "Invalid substitute reference"
                );
                Ok(r.to_owned())
            };
            let name = item.tag_name().name();
            ensure!(
                matches!(name, "colorFill" | "textPoint")
                    || !item.children().any(|n| n.is_element()),
                "Substitute {name} with child elements is not supported"
            );
            match name {
                "colorFill" | "areaFillReference" => {
                    ensure!(out.area_fill.is_none(), "Duplicate substitute area fill");
                    out.area_fill = Some(if name == "colorFill" {
                        ensure!(
                            item.attributes().next().is_none(),
                            "Unsupported colorFill attribute"
                        );
                        allowed(item, &["color"])?;
                        AreaFillSubstitute::Color(ColorSubstitute::parse(one(
                            item, None, "color",
                        )?)?)
                    } else {
                        ensure!(
                            item.attributes().all(|a| a.name() == "reference"),
                            "Unsupported areaFillReference attribute"
                        );
                        AreaFillSubstitute::Reference(reference()?)
                    });
                }
                "textPoint" => {
                    ensure!(out.text.is_none(), "Duplicate substitute text");
                    out.text = Some(TextSubstitute::parse(item)?);
                }
                "symbol" => {
                    ensure!(out.symbol.is_none(), "Duplicate substitute symbol");
                    for a in item.attributes() {
                        ensure!(
                            ["reference", "rotation", "rotationCRS", "scaleFactor"]
                                .contains(&a.name()),
                            "Unsupported symbol attribute {}",
                            a.name()
                        );
                    }
                    let real = |name| -> Result<Option<u64>> {
                        attribute(name)
                            .map(|v| {
                                let v: f64 = v.parse()?;
                                ensure!(v.is_finite(), "Non-finite symbol {name}");
                                Ok(v.to_bits())
                            })
                            .transpose()
                    };
                    let rotation_crs = attribute("rotationCRS").map(str::to_owned);
                    ensure!(
                        rotation_crs.as_deref().is_none_or(|c| {
                            ["GeographicCRS", "PortrayalCRS", "LocalCRS", "LineCRS"].contains(&c)
                        }),
                        "Invalid symbol rotationCRS"
                    );
                    let scale_factor = real("scaleFactor")?;
                    ensure!(
                        scale_factor.is_none_or(|b| f64::from_bits(b) > 0.),
                        "Symbol scaleFactor must be positive"
                    );
                    out.symbol = Some(SymbolSubstitute {
                        reference: reference()?,
                        rotation: real("rotation")?,
                        rotation_crs,
                        scale_factor,
                    });
                }
                "lineStyleReference" => {
                    ensure!(out.line_style.is_none(), "Duplicate substitute line style");
                    ensure!(
                        item.attributes().all(|a| a.name() == "reference"),
                        "Unsupported lineStyleReference attribute"
                    );
                    out.line_style = Some(reference()?);
                }
                other => bail!(
                    "Substitute element {other} is not supported (supported: symbol, \
                     lineStyleReference, colorFill, areaFillReference, textPoint)"
                ),
            }
        }
        Ok(out)
    }
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
    /// Every attributeCombination must hold (S-100 Part 16 Annex D 16-D-2.5/2.6:
    /// the filter attributes apply in conjunction). Sorted and deduplicated,
    /// so XML order never changes selector identity.
    filters: Vec<AttributeFilter>,
    assignment: Assignment,
}
/// Expected O(R) time and O(R) memory within each product/feature rule bucket.
fn validate_selector_consistency(rules: &[Rule]) -> Result<()> {
    type Selector = (usize, Vec<String>, Vec<AttributeFilter>);
    let mut seen: HashMap<Selector, [Option<&Rule>; 2]> = HashMap::new();
    for rule in rules {
        // geometryType is a set of selected primitives; XML order is irrelevant.
        let mut geometry = rule.geometry.clone();
        geometry.sort_unstable();
        geometry.dedup();
        let key = (rule.plane_index, geometry, rule.filters.clone());
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
    /// Catalogue-level interoperabilityLevel; optional in Part 16, mandatory in S-98.
    pub level: Option<u8>,
    /// interoperabilityLevel of each display plane in document order.
    pub plane_levels: Vec<Option<u8>>,
    pub requirement_type: String,
    /// Level 2 predefined combinations in document order.
    pub combinations: Vec<PredefinedCombination>,
    /// For each display plane, the combinations that reference it. A plane no
    /// combination references applies to every display (Level 1 behaviour).
    plane_combinations: Vec<Vec<usize>>,
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
fn level(n: Node<'_, '_>) -> Result<Option<u8>> {
    let v = children(n, None, "interoperabilityLevel");
    ensure!(v.len() <= 1, "Duplicate interoperabilityLevel");
    let Some(v) = v.first() else {
        return Ok(None);
    };
    match text(*v)?.as_str() {
        "1" => Ok(Some(1)),
        "2" => Ok(Some(2)),
        other => bail!("Interoperability level {other} is not supported (Levels 1 and 2 only)"),
    }
}
/// S-100 Part 16-4.4.2.5. All instances of `feature` in `product` are hidden
/// while the owning combination is active, where both products have coverage.
#[derive(Debug, Clone)]
pub struct SuppressedFeatureLayer {
    pub identifier: String,
    pub product: String,
    pub feature: String,
    /// Products of the referenced replacement rules; empty means the layer is
    /// suppressed without replacement.
    pub replacement_products: Vec<String>,
    /// Referenced S100_IC_Feature or S100_IC_DrawingInstruction identifiers.
    pub replacement_rules: Vec<String>,
}
/// S-100 Part 16-4.4.2.6 / S-98 Part B B-3.2.1.5.
#[derive(Debug, Clone)]
pub struct PredefinedCombination {
    pub identifier: String,
    pub name: String,
    pub description: String,
    pub use_conditions: String,
    pub level: Option<u8>,
    pub products: Vec<String>,
    planes: Vec<usize>,
    pub suppressed: Vec<SuppressedFeatureLayer>,
}
/// Which interoperability operations apply to the current display.
/// The default applies Level 1 operations only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Selection {
    combination: Option<usize>,
}
impl Selection {
    pub fn combination(self) -> Option<usize> {
        self.combination
    }
}
fn parse_combinations(
    container: Node<'_, '_>,
    catalogue_level: Option<u8>,
    covered: &HashMap<&str, &str>,
    planes: &HashMap<String, usize>,
    features: &HashMap<String, String>,
    instructions: &HashMap<String, String>,
    element_ids: &mut HashSet<String>,
) -> Result<Vec<PredefinedCombination>> {
    allowed(container, &["S100_IC_PredefinedCombination"])?;
    let mut names = HashSet::new();
    let mut out = Vec::new();
    for n in children(container, None, "S100_IC_PredefinedCombination") {
        allowed(
            n,
            &[
                "identifier",
                "name",
                "description",
                "useConditions",
                "interoperabilityLevel",
                "includedProduct",
                "displayPlaneRef",
                "suppressedFeatureLayers",
                "derivedFeatures",
            ],
        )?;
        let identifier = nonempty(required(n, "identifier")?)?;
        ensure!(
            element_ids.insert(identifier.clone()),
            "Duplicate catalogue element identifier {identifier}"
        );
        let name = nonempty(required(n, "name")?)?;
        ensure!(
            names.insert(name.clone()),
            "Duplicate combination name {name}"
        );
        let level = level(n)?;
        if let (Some(pdc), Some(catalogue)) = (level, catalogue_level) {
            ensure!(
                pdc <= catalogue,
                "Combination {identifier} level {pdc} exceeds catalogue level {catalogue}"
            );
        }
        // Derived features (instance suppression, hybridization) are Level 3/4;
        // S-98 Part B B-3.3 9) forbids them at Level 2.
        ensure!(
            children(n, None, "derivedFeatures")
                .iter()
                .all(|d| !d.children().any(|c| c.is_element())),
            "Combination {identifier}: derivedFeatures (Level 3/4) are not supported"
        );
        let mut products = Vec::new();
        for p in children(n, None, "includedProduct") {
            let p = product(p)?;
            ensure!(
                covered
                    .get(p.code.as_str())
                    .is_some_and(|list| *list == p.code_list),
                "Combination {identifier} product is not covered: {}",
                p.code
            );
            ensure!(
                !products.contains(&p.code),
                "Duplicate product {} in combination {identifier}",
                p.code
            );
            products.push(p.code);
        }
        ensure!(
            products.len() >= 2,
            "Combination {identifier} must include at least two products"
        );
        let mut plane_refs = Vec::new();
        for r in children(n, None, "displayPlaneRef") {
            let r = text(r)?;
            let index = *planes
                .get(&r)
                .with_context(|| format!("Combination {identifier}: unknown display plane {r}"))?;
            if !plane_refs.contains(&index) {
                plane_refs.push(index);
            }
        }
        let mut suppressed = Vec::new();
        match children(n, None, "suppressedFeatureLayers").as_slice() {
            [] => {}
            [layers] => {
                allowed(*layers, &["S100_IC_SuppressedFeatureLayer"])?;
                for l in children(*layers, None, "S100_IC_SuppressedFeatureLayer") {
                    suppressed.push(parse_suppressed_layer(
                        l,
                        &identifier,
                        &products,
                        features,
                        instructions,
                        element_ids,
                    )?);
                }
            }
            _ => bail!("Duplicate suppressedFeatureLayers in {identifier}"),
        }
        // Suppression is a Level 2 operation (S-98 Part B B-7 Table B-7.1).
        ensure!(
            suppressed.is_empty() || level.is_none_or(|l| l >= 2),
            "Combination {identifier} suppresses feature layers but declares level 1"
        );
        let mut seen = HashSet::new();
        for l in &suppressed {
            ensure!(
                seen.insert((l.product.as_str(), l.feature.as_str())),
                "Combination {identifier} suppresses {}/{} twice",
                l.product,
                l.feature
            );
        }
        out.push(PredefinedCombination {
            identifier,
            name,
            description: required(n, "description")?,
            use_conditions: required(n, "useConditions")?,
            level,
            products,
            planes: plane_refs,
            suppressed,
        });
    }
    Ok(out)
}
fn parse_suppressed_layer(
    n: Node<'_, '_>,
    combination: &str,
    included: &[String],
    features: &HashMap<String, String>,
    instructions: &HashMap<String, String>,
    element_ids: &mut HashSet<String>,
) -> Result<SuppressedFeatureLayer> {
    allowed(
        n,
        &[
            "identifier",
            "featureCode",
            "product",
            "featureRef",
            "drawingInstructionRef",
        ],
    )?;
    let identifier = nonempty(required(n, "identifier")?)?;
    ensure!(
        element_ids.insert(identifier.clone()),
        "Duplicate catalogue element identifier {identifier}"
    );
    let feature = nonempty(required(n, "featureCode")?)?;
    let product = product(one(n, None, "product")?)?.code;
    // Part 16-D-2.8: every product referenced directly or indirectly is included.
    ensure!(
        included.contains(&product),
        "Suppressed layer {identifier} product {product} is not included in {combination}"
    );
    let feature_refs = children(n, None, "featureRef");
    let instruction_refs = children(n, None, "drawingInstructionRef");
    // Part 16-4.4.2.5 NOTE: only one of the two reference kinds per instance.
    ensure!(
        feature_refs.is_empty() || instruction_refs.is_empty(),
        "Suppressed layer {identifier} uses both featureRef and drawingInstructionRef"
    );
    let (refs, targets, kind) = if instruction_refs.is_empty() {
        (feature_refs, features, "S100_IC_Feature")
    } else {
        (instruction_refs, instructions, "S100_IC_DrawingInstruction")
    };
    let mut replacement_rules = Vec::new();
    let mut replacement_products = Vec::new();
    for r in refs {
        let r = text(r)?;
        let target = targets
            .get(&r)
            .with_context(|| format!("Suppressed layer {identifier}: unknown {kind} {r}"))?;
        // Part 16-4.4.2.5: replacement comes "from another product".
        ensure!(
            *target != product,
            "Suppressed layer {identifier} is replaced by its own product {product}"
        );
        ensure!(
            included.contains(target),
            "Replacement product {target} of {identifier} is not included in {combination}"
        );
        if !replacement_products.contains(target) {
            replacement_products.push(target.clone());
        }
        replacement_rules.push(r);
    }
    Ok(SuppressedFeatureLayer {
        identifier,
        product,
        feature,
        replacement_products,
        replacement_rules,
    })
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
                            "displayPlanes",
                            "predefinedProductCombinations"
                        ]
                        .contains(&t.name())),
                "Unsupported IC element {} (Level 3/4 hybridization is not implemented)",
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
        let requirement_type = nonempty(required(root, "requirementType")?)?;
        let catalogue_level = level(root)?;
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
        let mut plane_ids: HashMap<String, usize> = HashMap::new();
        let mut names = HashSet::new();
        // XSD ELEMENTID covers DrawingInstruction, SuppressedFeatureLayer and
        // PredefinedCombination; S100_IC_Feature identifiers are a separate
        // namespace that featureRef resolves against.
        let mut element_ids = HashSet::new();
        let mut feature_ids: HashMap<String, String> = HashMap::new();
        let mut instruction_ids: HashMap<String, String> = HashMap::new();
        let mut rules: HashMap<String, HashMap<String, Vec<Rule>>> = HashMap::new();
        let mut plane_levels = Vec::with_capacity(planes.len());
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
                plane_ids
                    .insert(nonempty(required(*p, "identifier")?)?, plane_index)
                    .is_none(),
                "Duplicate plane identifier"
            );
            ensure!(
                names.insert(nonempty(required(*p, "name")?)?),
                "Duplicate plane name"
            );
            required(*p, "description")?;
            let plane_level = level(*p)?;
            if let (Some(plane), Some(catalogue)) = (plane_level, catalogue_level) {
                ensure!(
                    plane <= catalogue,
                    "Display plane level {plane} exceeds catalogue level {catalogue}"
                );
            }
            plane_levels.push(plane_level);
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
                    // Only S100_IC_DrawingInstruction carries substituteSymbolization.
                    let substitute = if tag == "S100_IC_DrawingInstruction" {
                        match children(r, None, "substituteSymbolization").as_slice() {
                            [] => None,
                            [node] => Some(std::sync::Arc::new(Substitution::parse(*node)?)),
                            _ => bail!("Duplicate substituteSymbolization"),
                        }
                    } else {
                        None
                    };
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
                            "substituteSymbolization",
                        ][..if substitute.is_some() { 8 } else { 7 }],
                    )?;
                    let identifier = nonempty(required(r, "identifier")?)?;
                    let prod = product(one(r, None, "product")?)?;
                    let fresh = if tag == "S100_IC_Feature" {
                        feature_ids
                            .insert(identifier.clone(), prod.code.clone())
                            .is_none()
                    } else {
                        element_ids.insert(identifier.clone())
                            && instruction_ids
                                .insert(identifier.clone(), prod.code.clone())
                                .is_none()
                    };
                    ensure!(fresh, "Duplicate rule identifier {identifier}");
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
                    let mut filters = children(r, None, "attributeCombination")
                        .into_iter()
                        .map(|n| AttributeFilter::parse(&text(n)?))
                        .collect::<Result<Vec<_>>>()?;
                    filters.sort();
                    filters.dedup();
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
                            filters,
                            assignment: Assignment {
                                plane: CompositionPlane::new(CompositionStage::Chart, order),
                                priority,
                                viewing_group,
                                substitution: substitute,
                            },
                        });
                    count += 1;
                }
            }
            ensure!(count > 0, "Empty display plane");
        }
        let combinations = match children(root, None, "predefinedProductCombinations").as_slice() {
            [] => Vec::new(),
            [container] => parse_combinations(
                *container,
                catalogue_level,
                &product_set,
                &plane_ids,
                &feature_ids,
                &instruction_ids,
                &mut element_ids,
            )?,
            _ => bail!("Duplicate predefinedProductCombinations"),
        };
        let mut plane_combinations = vec![Vec::new(); planes.len()];
        for (index, combination) in combinations.iter().enumerate() {
            for plane in &combination.planes {
                plane_combinations[*plane].push(index);
            }
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
            level: catalogue_level,
            plane_levels,
            combinations,
            plane_combinations,
            requirement_type,
            rules,
        })
    }
    /// S-98 2.0.0 Part A/B clause 3.3 profile of the generic Part 16 reader.
    /// Returns non-fatal findings; violations are errors. Apply before activation.
    pub fn validate_s98_profile(&self) -> Result<Vec<String>> {
        let mut findings = Vec::new();
        let level = self
            .level
            .context("S-98 requires the catalogue interoperabilityLevel")?;
        ensure!(
            (1..=2).contains(&level),
            "Unsupported S-98 interoperabilityLevel {level}"
        );
        for (index, plane) in self.plane_levels.iter().enumerate() {
            let plane = plane.with_context(|| {
                format!(
                    "S-98 requires interoperabilityLevel on display plane {}",
                    index + 1
                )
            })?;
            // Part A: only 1 at Level 1. Part B: 1 or 2 at Level 2.
            ensure!(
                (1..=level).contains(&plane),
                "Display plane {} level {plane} exceeds catalogue level {level}",
                index + 1
            );
        }
        let requirement = self.requirement_type.as_str();
        ensure!(
            S98_REQUIREMENT_TYPES.contains(&requirement)
                || requirement
                    .strip_prefix("other:")
                    .is_some_and(|rest| !rest.trim().is_empty()),
            "requirementType {requirement:?} is not permitted by S-98 Table A-3.1"
        );
        for p in &self.products {
            ensure!(
                S98_PRODUCT_DICTIONARIES.contains(&p.code_list.as_str()),
                "productCovered {} must use the S-98 products dictionary, found {}",
                p.code,
                p.code_list
            );
            // Closed dictionary (Annex A Table 2-1).
            ensure!(
                S98_PRODUCTS.contains(&p.code.as_str()),
                "Product {} is not in the S-98 products dictionary",
                p.code
            );
            if p.code_list != S98_PRODUCT_DICTIONARIES[0] {
                findings.push(format!(
                    "productCovered {} uses the Annex A 4.1.1 dictionary MRN {}; Parts A/B \
                     clause 3.3 specify {}",
                    p.code, p.code_list, S98_PRODUCT_DICTIONARIES[0]
                ));
            }
        }
        Ok(findings)
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
    /// Level 1 operations only; see `resolve_portrayal_in`.
    pub fn resolve_portrayal(
        &self,
        product: &str,
        feature: &str,
        feature_geometry: &str,
        drawing_geometry: &str,
        attribute: impl FnMut(&[String]) -> Result<Option<Scalar>>,
    ) -> Result<Option<Assignment>> {
        self.resolve_portrayal_in(
            Selection::default(),
            product,
            feature,
            feature_geometry,
            drawing_geometry,
            attribute,
        )
    }
    /// The combination the mariner selected (S-98 Part B Table B-7.1 "Select Load
    /// Sets"). Its operations apply only while every included product is part of
    /// the display (Part B B-1); otherwise only Level 1 operations apply.
    pub fn select(&self, combination: Option<&str>, displayed: &[&str]) -> Result<Selection> {
        let Some(identifier) = combination else {
            return Ok(Selection::default());
        };
        let index = self
            .combinations
            .iter()
            .position(|c| c.identifier == identifier)
            .with_context(|| format!("Unknown predefined combination {identifier}"))?;
        let active = self.combinations[index]
            .products
            .iter()
            .all(|p| displayed.contains(&p.as_str()));
        Ok(Selection {
            combination: active.then_some(index),
        })
    }
    pub fn active_combination(&self, selection: Selection) -> Option<&PredefinedCombination> {
        selection.combination.map(|i| &self.combinations[i])
    }
    /// Level 2 "Suppress Feature Types": the layer hiding every instance of
    /// `product`/`feature` under this selection. Callers restrict the effect to
    /// areas covered by both products (S-98 Part B B-5.1).
    pub fn suppressed_layer(
        &self,
        selection: Selection,
        product: &str,
        feature: &str,
    ) -> Option<&SuppressedFeatureLayer> {
        self.active_combination(selection)?
            .suppressed
            .iter()
            .find(|l| l.product == product && l.feature == feature)
    }
    fn plane_applies(&self, selection: Selection, plane: usize) -> bool {
        let owners = &self.plane_combinations[plane];
        owners.is_empty() || selection.combination.is_some_and(|c| owners.contains(&c))
    }
    pub fn resolve_portrayal_in(
        &self,
        selection: Selection,
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
            // Planes referenced by a combination apply only while it is active.
            if !self.plane_applies(selection, r.plane_index) {
                continue;
            }
            let geometry = match r.target {
                RuleTarget::Feature => feature_geometry,
                RuleTarget::DrawingInstruction => drawing_geometry,
            };
            if !r.geometry.is_empty() && !r.geometry.iter().any(|g| g == geometry) {
                continue;
            }
            let mut matched = true;
            for filter in &r.filters {
                if !filter.matches(attribute(&filter.path)?.as_ref())? {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
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
            fixture().replace("<displayPlanes>", "<hybridizationRules/><displayPlanes>"),
            fixture().replace("http://www.iho.int/S100/IC/5.0", "urn:spoof"),
            fixture().replace("<features>", "<features xmlns=\"urn:spoof\">"),
            fixture().replace("<order>-10</order>", "<order>2147483648</order>"),
            fixture().replace("<interoperabilityLevel>1", "<interoperabilityLevel>3"),
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
    fn multiple_attribute_combinations_apply_in_conjunction_in_any_order() {
        // S-100 Part 16 Annex D 16-D-2.5/2.6: all filter conditions must hold.
        let both = |first: &str, second: &str| {
            fixture().replace(
                "<geometryType>coverage</geometryType>",
                &format!(
                    "<geometryType>coverage</geometryType><attributeCombination>{first}</attributeCombination><attributeCombination>{second}</attributeCombination>"
                ),
            )
        };
        for xml in [
            both("depth ge 10", "depth lt 30"),
            both("depth lt 30", "depth ge 10"),
        ] {
            let catalogue = Catalogue::parse(&xml).unwrap();
            for (depth, selected) in [("9.99", false), ("10", true), ("29.9", true), ("30", false)]
            {
                let resolved = catalogue
                    .resolve("S-102", "BathymetryCoverage", "coverage", |_| {
                        Ok(Some(Scalar::Number(Decimal::parse(depth)?)))
                    })
                    .unwrap();
                assert_eq!(resolved.is_some(), selected, "{depth}");
            }
            // A missing attribute fails the first condition: ordinary portrayal.
            assert!(catalogue
                .resolve("S-102", "BathymetryCoverage", "coverage", |_| Ok(None))
                .unwrap()
                .is_none());
        }
    }

    fn s98_fixture() -> String {
        fixture()
            .replace("urn:ferrite:test:products", S98_PRODUCT_DICTIONARIES[0])
            .replace(
                "<description>Bathymetry plane</description>",
                "<description>Bathymetry plane</description><interoperabilityLevel>1</interoperabilityLevel>",
            )
            .replace(
                "<order>-10</order>",
                "<order>-10</order><interoperabilityLevel>1</interoperabilityLevel>",
            )
    }
    #[test]
    fn s98_profile_requires_levels_dictionary_and_requirement_type() {
        let c = Catalogue::parse(&s98_fixture()).unwrap();
        assert_eq!(c.level, Some(1));
        assert!(c.validate_s98_profile().unwrap().is_empty());
        // Part 16 permits the generic test catalogue; S-98 does not.
        assert!(Catalogue::parse(&fixture())
            .unwrap()
            .validate_s98_profile()
            .is_err());
        let annex = Catalogue::parse(
            &s98_fixture().replace(S98_PRODUCT_DICTIONARIES[0], S98_PRODUCT_DICTIONARIES[1]),
        )
        .unwrap();
        assert_eq!(annex.validate_s98_profile().unwrap().len(), 2);
        for (from, to) in [
            (
                "<interoperabilityLevel>1</interoperabilityLevel><requirementType>",
                "<requirementType>",
            ),
            (
                "<order>-10</order><interoperabilityLevel>1</interoperabilityLevel>",
                "<order>-10</order>",
            ),
            ("<requirementType>OEM", "<requirementType>vendor"),
            ("<requirementType>OEM", "<requirementType>other:"),
            ("codeListValue=\"S-102\"", "codeListValue=\"S-999\""),
        ] {
            let x = s98_fixture().replace(from, to);
            assert_ne!(x, s98_fixture(), "{from}");
            let profile = Catalogue::parse(&x).and_then(|c| c.validate_s98_profile());
            assert!(profile.is_err(), "{from} -> {to}");
        }
        let other = Catalogue::parse(
            &s98_fixture().replace("<requirementType>OEM", "<requirementType>other: trial"),
        )
        .unwrap();
        assert!(other.validate_s98_profile().is_ok());
    }
    fn level2_fixture(pdc: &str) -> String {
        const PRODUCT: &str = "codeList=\"urn:ferrite:test:products\"";
        let pdc = pdc.replace("PRODUCT", PRODUCT);
        fixture()
            .replace("<interoperabilityLevel>1", "<interoperabilityLevel>2")
            .replace(
                "</displayPlanes>",
                &format!(
                    "</displayPlanes><predefinedProductCombinations>{pdc}</predefinedProductCombinations>"
                ),
            )
    }
    const BATHYMETRY_PDC: &str = "<S100_IC_PredefinedCombination><identifier>pdc1</identifier>\
        <name>Bathymetry</name><description>d</description><useConditions>u</useConditions>\
        <interoperabilityLevel>2</interoperabilityLevel>\
        <includedProduct PRODUCT codeListValue=\"S-101\">ENC</includedProduct>\
        <includedProduct PRODUCT codeListValue=\"S-102\">Bathymetry</includedProduct>\
        <displayPlaneRef>DangerPlane</displayPlaneRef>\
        <suppressedFeatureLayers><S100_IC_SuppressedFeatureLayer><identifier>sfl1</identifier>\
        <featureCode>DepthArea</featureCode><product PRODUCT codeListValue=\"S-101\">ENC</product>\
        <featureRef>coverageRule</featureRef></S100_IC_SuppressedFeatureLayer></suppressedFeatureLayers>\
        </S100_IC_PredefinedCombination>";
    #[test]
    fn level2_combination_scopes_planes_and_suppression_to_its_products() {
        let c = Catalogue::parse(&level2_fixture(BATHYMETRY_PDC)).unwrap();
        assert_eq!(c.level, Some(2));
        let wreck = |selection| {
            c.resolve_portrayal_in(selection, "S-101", "Wreck", "point", "point", |_| {
                Ok(Some(Scalar::Number(Decimal::parse("1").unwrap())))
            })
            .unwrap()
        };
        // Level 1 only: the plane referenced by the combination does not apply.
        let none = Selection::default();
        assert!(wreck(none).is_none());
        assert!(c.suppressed_layer(none, "S-101", "DepthArea").is_none());
        // Unreferenced planes always apply.
        assert!(c
            .resolve("S-102", "BathymetryCoverage", "coverage", |_| panic!())
            .unwrap()
            .is_some());
        // Selected, but S-102 is not displayed: still Level 1.
        let partial = c.select(Some("pdc1"), &["S-101"]).unwrap();
        assert_eq!(partial, none);
        let active = c.select(Some("pdc1"), &["S-102", "S-101"]).unwrap();
        assert_eq!(wreck(active).unwrap().priority, 4);
        let layer = c.suppressed_layer(active, "S-101", "DepthArea").unwrap();
        assert_eq!(layer.replacement_products, ["S-102"]);
        assert!(c.suppressed_layer(active, "S-101", "Wreck").is_none());
        assert!(c.select(Some("missing"), &["S-101", "S-102"]).is_err());
    }
    #[test]
    fn level2_combination_constraints_are_enforced() {
        for (from, to) in [
            // Part 16-4.4.2.5: only one reference kind per instance.
            ("<featureRef>coverageRule</featureRef>", "<featureRef>coverageRule</featureRef><drawingInstructionRef>wreckRule</drawingInstructionRef>"),
            // Unresolved references.
            ("<featureRef>coverageRule", "<featureRef>wreckRule"),
            ("<displayPlaneRef>DangerPlane", "<displayPlaneRef>Nowhere"),
            // Replacement must come from another product.
            ("<product PRODUCT codeListValue=\"S-101\">ENC</product>", "<product PRODUCT codeListValue=\"S-102\">B</product>"),
            // Schematron S98.pdcproducts and minimum of two products.
            ("codeListValue=\"S-102\">Bathymetry</includedProduct>", "codeListValue=\"S-101\">ENC</includedProduct>"),
            // Suppression is a Level 2 operation; derivedFeatures are Level 3/4.
            ("<interoperabilityLevel>2</interoperabilityLevel>\
", "<interoperabilityLevel>1</interoperabilityLevel>"),
            ("</suppressedFeatureLayers>", "</suppressedFeatureLayers><derivedFeatures><S100_IC_HybridFeature/></derivedFeatures>"),
            ("<identifier>sfl1", "<identifier>wreckRule"),
        ] {
            let pdc = BATHYMETRY_PDC.replace(from, to);
            assert_ne!(pdc, BATHYMETRY_PDC, "{from}");
            assert!(Catalogue::parse(&level2_fixture(&pdc)).is_err(), "{from} -> {to}");
        }
        // A level 2 combination in a catalogue that declares level 1.
        assert!(Catalogue::parse(&level2_fixture(BATHYMETRY_PDC).replacen(
            "<interoperabilityLevel>2",
            "<interoperabilityLevel>1",
            1
        ))
        .is_err());
        // Two PDCs with the same name.
        let twice = format!(
            "{BATHYMETRY_PDC}{}",
            BATHYMETRY_PDC
                .replace("pdc1", "pdc2")
                .replace("sfl1", "sfl2")
        );
        assert!(Catalogue::parse(&level2_fixture(&twice)).is_err());
        assert!(Catalogue::parse(&level2_fixture(&twice.replacen(
            "<name>Bathymetry</name>",
            "<name>Other</name>",
            1
        )))
        .is_ok());
    }
    #[test]
    fn feature_and_instruction_identifiers_are_separate_namespaces() {
        // XSD ELEMENTID covers DrawingInstruction but not S100_IC_Feature.
        let x = fixture().replace("<identifier>coverageRule", "<identifier>wreckRule");
        assert!(Catalogue::parse(&x).is_ok());
        let x = fixture().replace(
            "<features/>",
            "<features><S100_IC_Feature><identifier>coverageRule</identifier><featureCode>Wreck</featureCode>\
             <product codeList=\"urn:ferrite:test:products\" codeListValue=\"S-101\">ENC</product>\
             <drawingPriority>4</drawingPriority><viewingGroup>11010</viewingGroup></S100_IC_Feature></features>",
        );
        assert!(Catalogue::parse(&x).is_err());
    }
    #[test]
    fn substitute_symbolization_accepts_part9_symbol_and_line_style_fragments() {
        let with = |content: &str| {
            fixture().replace(
                "<viewingGroup>11010</viewingGroup></S100_IC_DrawingInstruction>",
                &format!(
                    "<viewingGroup>11010</viewingGroup><substituteSymbolization>{content}</substituteSymbolization></S100_IC_DrawingInstruction>"
                ),
            )
        };
        let wreck = |c: &Catalogue| {
            c.resolve_portrayal("S-101", "Wreck", "point", "point", |_| {
                Ok(Some(Scalar::Number(Decimal::parse("1").unwrap())))
            })
            .unwrap()
            .unwrap()
            .substitution
            .unwrap()
        };
        // Element content and the CDATA form are equivalent.
        for content in [
            "<symbol reference=\"WRECKS99\" rotation=\"45\" rotationCRS=\"GeographicCRS\"/><lineStyleReference reference=\"NEWLINE\"/>",
            "<![CDATA[<symbol reference=\"WRECKS99\" rotation=\"45\" rotationCRS=\"GeographicCRS\"/> <lineStyleReference reference=\"NEWLINE\"/>]]>",
        ] {
            let c = Catalogue::parse(&with(content)).unwrap();
            let s = wreck(&c);
            let symbol = s.symbol.as_ref().unwrap();
            assert_eq!(symbol.reference, "WRECKS99");
            assert_eq!(symbol.rotation(), Some(45.));
            assert_eq!(symbol.rotation_crs.as_deref(), Some("GeographicCRS"));
            assert_eq!(symbol.scale_factor(), None);
            assert_eq!(s.line_style.as_deref(), Some("NEWLINE"));
        }
        let c = Catalogue::parse(&with(
            "<colorFill><color transparency=\"0.25\">DEPDW</color></colorFill><textPoint horizontalAlignment=\"Center\"><element><text>WK</text><bodySize>12</bodySize><foreground>CHBLK</foreground></element></textPoint>",
        ))
        .unwrap();
        let s = wreck(&c);
        let Some(AreaFillSubstitute::Color(fill)) = &s.area_fill else {
            panic!()
        };
        assert_eq!((fill.token.as_str(), fill.transparency()), ("DEPDW", 0.25));
        let text = s.text.as_ref().unwrap();
        assert_eq!((text.text.as_str(), text.body_size()), ("WK", 12.));
        assert_eq!(text.horizontal_alignment, "Center");
        assert_eq!(text.vertical_alignment, "Bottom");
        assert!(matches!(
            wreck(&Catalogue::parse(&with("<areaFillReference reference=\"DIAMOND1\"/>")).unwrap()).area_fill,
            Some(AreaFillSubstitute::Reference(ref r)) if r == "DIAMOND1"
        ));
        for refused in [
            "<colorFill><color transparency=\"2\">CHBLK</color></colorFill>",
            "<colorFill><color>CHBLK</color><extra/></colorFill>",
            "<pixmapFill reference=\"P\"/>",
            "<textLine><element><text>A</text><bodySize>10</bodySize><foreground>CHBLK</foreground></element><startOffset>0</startOffset></textLine>",
            "<textPoint><element><text>A</text><bodySize>10</bodySize></element></textPoint>",
            "<textPoint><element><text>A</text><bodySize>10</bodySize><foreground>CHBLK</foreground><flags/></element></textPoint>",
            "<symbol reference=\"A\"><offset/></symbol>",
            "<symbol reference=\"A\"/><symbol reference=\"B\"/>",
            "<symbol reference=\"A\" scaleFactor=\"0\"/>",
            "<symbol/>",
            "replace WRECKS01 with WRECKS99",
            "",
        ] {
            assert!(Catalogue::parse(&with(refused)).is_err(), "{refused}");
        }
        // S100_IC_Feature has no substituteSymbolization.
        let feature = fixture().replace(
            "<viewingGroup>11010</viewingGroup></S100_IC_Feature>",
            "<viewingGroup>11010</viewingGroup><substituteSymbolization><symbol reference=\"A\"/></substituteSymbolization></S100_IC_Feature>",
        );
        assert!(Catalogue::parse(&feature).is_err());
    }
    #[test]
    fn list_and_null_operators_are_refused_without_a_normative_encoding() {
        for s in ["x in 1", "x notIn \"a\"", "x null 1"] {
            let error = AttributeFilter::parse(s).unwrap_err().to_string();
            assert!(
                error.contains("no normative value encoding"),
                "{s}: {error}"
            );
        }
        assert!(AttributeFilter::parse("x like 1")
            .unwrap_err()
            .to_string()
            .contains("Unknown filter operator"));
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
