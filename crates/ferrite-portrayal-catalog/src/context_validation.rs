//! Product-independent S-100 9-10.1 context input validation.
use crate::{
    ContextEnumeration, ContextExpression, ContextParamType, ContextParameter, ContextText,
    ContextValidation, PCError, Result,
};
use std::collections::{BTreeMap, HashMap, HashSet};
fn invalid(message: impl Into<String>) -> PCError {
    PCError::InvalidValue(message.into())
}
fn children<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Vec<roxmltree::Node<'a, 'i>> {
    n.children()
        .filter(|c| {
            c.is_element()
                && c.tag_name().namespace() == n.tag_name().namespace()
                && c.tag_name().name() == name
        })
        .collect()
}
fn text(n: roxmltree::Node<'_, '_>) -> Result<String> {
    if n.children().any(|c| c.is_element()) {
        return Err(invalid("Expected scalar context metadata"));
    }
    Ok(n.children()
        .filter(|c| c.is_text())
        .filter_map(|c| c.text())
        .collect())
}
fn single_text(n: roxmltree::Node<'_, '_>, name: &str) -> Result<Option<String>> {
    let c = children(n, name);
    if c.len() > 1 {
        return Err(invalid(format!("Duplicate context {name}")));
    }
    c.first().copied().map(text).transpose()
}
fn texts(n: roxmltree::Node<'_, '_>, name: &str) -> Result<Vec<ContextText>> {
    children(n, name)
        .into_iter()
        .map(|n| {
            Ok(ContextText {
                text: text(n)?,
                language: n.attribute("language").map(str::to_owned),
            })
        })
        .collect()
}
/// Decode supported delivered XML encodings without lossy substitution.
/// Historical IHO PC 1.0.2 declares ISO-8859-1 and contains accented description text.
pub(crate) fn decode_metadata_xml(bytes: Vec<u8>) -> Result<String> {
    let mut reader = quick_xml::Reader::from_reader(bytes.as_slice());
    let encoding = match reader.read_event().map_err(PCError::from)? {
        quick_xml::events::Event::Decl(decl) => decl
            .encoding()
            .transpose()
            .map_err(|e| invalid(format!("XML encoding: {e}")))?
            .map(|x| String::from_utf8_lossy(&x).to_ascii_lowercase()),
        _ => None,
    };
    match encoding.as_deref() {
        Some("iso-8859-1") => Ok(bytes.into_iter().map(char::from).collect()),
        None | Some("utf-8" | "utf8") => {
            String::from_utf8(bytes).map_err(|e| invalid(format!("Context UTF-8: {e}")))
        }
        Some(e) => Err(invalid(format!("Unsupported PC XML encoding: {e}"))),
    }
}
#[cfg(test)]
pub(crate) fn read_metadata(
    xml: &str,
    parameters: &mut HashMap<String, ContextParameter>,
) -> Result<()> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| invalid(format!("Context XML: {e}")))?;
    read_metadata_document(&doc, parameters)
}
pub(crate) fn read_metadata_document(
    doc: &roxmltree::Document<'_>,
    parameters: &mut HashMap<String, ContextParameter>,
) -> Result<()> {
    let ns = doc.root_element().tag_name().namespace();
    let mut seen = HashSet::new();
    for node in doc.descendants().filter(|n| {
        n.is_element()
            && n.tag_name().name() == "parameter"
            && n.parent().is_some_and(|p| {
                p.is_element()
                    && p.tag_name().name() == "context"
                    && [None, ns].contains(&p.tag_name().namespace())
                    && p.tag_name().namespace() == n.tag_name().namespace()
            })
    }) {
        if node.parent().and_then(|p| p.parent()) != Some(doc.root_element()) {
            return Err(invalid("Context must be directly under the catalogue"));
        }
        let id = node
            .attribute("id")
            .ok_or_else(|| invalid("Context parameter lacks id"))?;
        if !seen.insert(id) {
            return Err(invalid(format!("Duplicate context parameter {id}")));
        }
        let p = parameters
            .get_mut(id)
            .ok_or_else(|| invalid(format!("Context parameter {id} was not read")))?;
        p.enable = node.attribute("enable").map(str::to_owned);
        let t = single_text(node, "type")?
            .ok_or_else(|| invalid(format!("Missing context type: {id}")))?;
        p.param_type = match t.as_str() {
            "Boolean" => ContextParamType::Boolean,
            "Integer" => ContextParamType::Integer,
            "Double" => ContextParamType::Double,
            "String" => ContextParamType::String,
            "Date" => ContextParamType::Date,
            _ => return Err(invalid(format!("Unknown context type {t}: {id}"))),
        };
        p.default_value = Some(
            single_text(node, "default")?
                .ok_or_else(|| invalid(format!("Missing context default: {id}")))?,
        );
        canonical(p.param_type, p.default_value.as_deref().unwrap())?;
        let constraints = children(node, "constrain");
        if constraints.len() > 1 {
            return Err(invalid("Multiple constrain blocks"));
        }
        if let Some(c) = constraints.first() {
            for e in children(*c, "enumeration") {
                let labels = texts(e, "label")?;
                if labels.is_empty() {
                    return Err(invalid("Enumeration requires labels"));
                }
                p.constraints.push(ContextEnumeration {
                    value: e
                        .attribute("value")
                        .ok_or_else(|| invalid("Enumeration lacks value"))?
                        .into(),
                    labels,
                    icon: e.attribute("icon").map(str::to_owned),
                });
            }
            if p.constraints.is_empty() {
                return Err(invalid("Empty context constraint"));
            }
        }
        for v in children(node, "validate") {
            let expr = match (single_text(v, "xpath")?, single_text(v, "regex")?) {
                (Some(x), None) => ContextExpression::XPath(x),
                (None, Some(x)) => ContextExpression::Regex(x),
                _ => {
                    return Err(invalid(format!(
                        "Validation requires exactly one xpath/regex: {id}"
                    )))
                }
            };
            let messages = children(v, "errorMessage");
            if messages.len() != 1 {
                return Err(invalid("Validation requires one errorMessage"));
            }
            let errors = texts(messages[0], "text")?;
            if errors.is_empty() {
                return Err(invalid("Validation requires error text"));
            }
            p.validations.push(ContextValidation {
                enable: v.attribute("enable").map(str::to_owned),
                expression: expr,
                errors,
                icon: messages[0].attribute("icon").map(str::to_owned),
            });
        }
    }
    Ok(())
}
#[derive(Debug, Clone, Default)]
pub struct ContextValidationReport {
    pub enabled: BTreeMap<String, bool>,
    pub failures: Vec<ContextInputFailure>,
    pub checked_rules: usize,
}
#[derive(Debug, Clone)]
pub struct ContextInputFailure {
    pub parameter: String,
    pub message: String,
}
impl ContextValidationReport {
    pub fn ensure_valid(&self) -> Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(invalid(
                self.failures
                    .iter()
                    .map(|x| format!("{}: {}", x.parameter, x.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ))
        }
    }
}
fn canonical(t: ContextParamType, v: &str) -> Result<String> {
    let v = if matches!(t, ContextParamType::String | ContextParamType::Enumeration) {
        v
    } else {
        v.trim_matches([' ', '\t', '\r', '\n'])
    };
    match t {
        ContextParamType::Boolean => match v {
            "true" | "1" => Ok("true".into()),
            "false" | "0" => Ok("false".into()),
            _ => Err(invalid("Expected Boolean true/false/1/0")),
        },
        ContextParamType::Integer => {
            let (negative, d) = if let Some(d) = v.strip_prefix('-') {
                (true, d)
            } else {
                (false, v.strip_prefix('+').unwrap_or(v))
            };
            if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("Expected Integer"));
            }
            let d = d.trim_start_matches('0');
            Ok(format!(
                "{}{}",
                if negative && !d.is_empty() { "-" } else { "" },
                if d.is_empty() { "0" } else { d }
            ))
        }
        ContextParamType::Double => {
            let n = v.parse::<f64>().map_err(|_| invalid("Expected Double"))?;
            if !n.is_finite() {
                return Err(invalid("Non-finite context Double"));
            }
            Ok(n.to_string())
        }
        ContextParamType::Date => {
            if !v.is_ascii() {
                return Err(invalid("Invalid Gregorian date"));
            }
            let (negative, body) = if let Some(v) = v.strip_prefix('-') {
                (true, v)
            } else {
                (false, v)
            };
            let dash = body
                .find('-')
                .ok_or_else(|| invalid("Expected Gregorian date"))?;
            let year = &body[..dash];
            if year.len() < 4
                || !year.bytes().all(|b| b.is_ascii_digit())
                || (year.len() > 4 && year.starts_with('0'))
                || year.bytes().all(|b| b == b'0')
            {
                return Err(invalid("Invalid Gregorian year"));
            }
            let rest = &body[dash + 1..];
            if rest.len() < 5
                || rest.as_bytes()[2] != b'-'
                || !rest[..2]
                    .bytes()
                    .chain(rest[3..5].bytes())
                    .all(|b| b.is_ascii_digit())
            {
                return Err(invalid("Invalid Gregorian month/day"));
            }
            let m: u32 = rest[..2]
                .parse()
                .map_err(|_| invalid("Invalid date month"))?;
            let d: u32 = rest[3..5]
                .parse()
                .map_err(|_| invalid("Invalid date day"))?;
            let modulo = year
                .bytes()
                .fold(0u32, |a, b| (a * 10 + (b - b'0') as u32) % 400);
            let days = match m {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                2 => {
                    if modulo % 4 == 0 && (modulo % 100 != 0 || modulo == 0) {
                        29
                    } else {
                        28
                    }
                }
                _ => 0,
            };
            let zone = &rest[5..];
            let zone_valid = if matches!(zone, "" | "Z") {
                true
            } else if zone.len() == 6
                && matches!(zone.as_bytes()[0], b'+' | b'-')
                && zone.as_bytes()[3] == b':'
                && zone[1..3]
                    .bytes()
                    .chain(zone[4..6].bytes())
                    .all(|b| b.is_ascii_digit())
            {
                match (zone[1..3].parse::<u32>(), zone[4..6].parse::<u32>()) {
                    (Ok(h), Ok(m)) => h <= 14 && m < 60 && (h < 14 || m == 0),
                    _ => false,
                }
            } else {
                false
            };
            if d == 0 || d > days || !zone_valid {
                return Err(invalid("Invalid Gregorian date/timezone"));
            }
            Ok(format!("{}{}", if negative { "-" } else { "" }, body))
        }
        ContextParamType::String | ContextParamType::Enumeration => Ok(v.into()),
    }
}
fn xpath_boolean(expression: &str, document: sxd_document::dom::Document<'_>) -> Result<bool> {
    if expression.len() > 2048 {
        return Err(invalid("Context XPath exceeds bound"));
    }
    let xpath = sxd_xpath::Factory::new()
        .build(expression)
        .map_err(|e| invalid(format!("Context XPath: {e}")))?
        .ok_or_else(|| invalid("Empty context XPath"))?;
    match xpath
        .evaluate(&sxd_xpath::Context::new(), document.root())
        .map_err(|e| invalid(format!("Context XPath evaluation: {e}")))?
    {
        sxd_xpath::Value::Boolean(b) => Ok(b),
        _ => Err(invalid("Context XPath must return Boolean")),
    }
}
/// Check XSD syntax first, then anchor the equivalent XPath regex to the entire value.
/// In XML Schema '^' and '$' outside classes are literals, not anchors.
fn full_regex(pattern: &str, value: &str) -> Result<bool> {
    if pattern.len() > 2048 {
        return Err(invalid("Context regex exceeds bound"));
    }
    regexml::Regex::xsd(pattern, "").map_err(|e| invalid(format!("Context XML regex: {e:?}")))?;
    let mut converted = String::new();
    let mut escaped = false;
    let mut depth = 0usize;
    for c in pattern.chars() {
        if escaped {
            converted.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                converted.push(c);
                escaped = true;
            }
            '[' => {
                depth += 1;
                converted.push(c);
            }
            ']' => {
                depth = depth.saturating_sub(1);
                converted.push(c);
            }
            '^' | '$' if depth == 0 => {
                converted.push('\\');
                converted.push(c);
            }
            _ => converted.push(c),
        }
    }
    let re = regexml::Regex::xpath(&format!("^({converted})$"), "")
        .map_err(|e| invalid(format!("Context full regex: {e:?}")))?;
    Ok(re.is_match(value))
}
/// Values are the complete candidate portrayal context, including PC defaults. No state is changed.
pub fn validate_context_values(
    parameters: &HashMap<String, ContextParameter>,
    values: &HashMap<String, String>,
) -> Result<ContextValidationReport> {
    if parameters.len() > 256 || values.len() > 512 {
        return Err(invalid("Too many context parameters"));
    }
    let package = sxd_document::Package::new();
    let document = package.as_document();
    let root = document.create_element("context");
    document.root().append_child(root);
    for (id, value) in values {
        if id.is_empty()
            || !id
                .bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()))
            || value.len() > 4096
        {
            return Err(invalid("Invalid/bounded context input"));
        }
        let e = document.create_element(id.as_str());
        e.append_child(document.create_text(value));
        root.append_child(e);
    }
    let mut report = ContextValidationReport::default();
    let mut ids: Vec<_> = parameters.keys().collect();
    ids.sort();
    for id in ids {
        let p = &parameters[id];
        let enabled = p
            .enable
            .as_ref()
            .map(|x| xpath_boolean(x, document))
            .transpose()?
            .unwrap_or(true);
        report.enabled.insert(id.clone(), enabled);
        if !enabled {
            continue;
        }
        let Some(value) = values.get(id) else {
            report.failures.push(ContextInputFailure {
                parameter: id.clone(),
                message: "Missing context value".into(),
            });
            continue;
        };
        let typed = match canonical(p.param_type, value) {
            Ok(v) => v,
            Err(e) => {
                report.failures.push(ContextInputFailure {
                    parameter: id.clone(),
                    message: e.to_string(),
                });
                continue;
            }
        };
        if !p.constraints.is_empty() {
            let allowed = p
                .constraints
                .iter()
                .map(|c| canonical(p.param_type, &c.value))
                .collect::<Result<Vec<_>>>()?;
            if !allowed.contains(&typed) {
                report.failures.push(ContextInputFailure {
                    parameter: id.clone(),
                    message: "Value is outside the PC enumeration constraint".into(),
                });
            }
        }
        for rule in &p.validations {
            if !rule
                .enable
                .as_ref()
                .map(|x| xpath_boolean(x, document))
                .transpose()?
                .unwrap_or(true)
            {
                continue;
            }
            report.checked_rules += 1;
            let matched = match &rule.expression {
                ContextExpression::XPath(x) => xpath_boolean(x, document)?,
                ContextExpression::Regex(x) => full_regex(x, value)?,
            };
            if !matched {
                let message = rule
                    .errors
                    .iter()
                    .find(|x| x.language.as_deref().is_none_or(|x| x == "eng"))
                    .or(rule.errors.first())
                    .map(|x| x.text.clone())
                    .unwrap_or_else(|| "PC validation failed".into());
                report.failures.push(ContextInputFailure {
                    parameter: id.clone(),
                    message,
                });
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definitions(xml: &str) -> HashMap<String, ContextParameter> {
        let d = roxmltree::Document::parse(xml).unwrap();
        let mut map = HashMap::new();
        for n in d.descendants().filter(|n| n.has_tag_name("parameter")) {
            let id = n.attribute("id").unwrap().to_owned();
            map.insert(
                id.clone(),
                ContextParameter {
                    id,
                    param_type: ContextParamType::String,
                    default_value: None,
                    description: None,
                    enable: None,
                    constraints: vec![],
                    validations: vec![],
                },
            );
        }
        read_metadata(xml, &mut map).unwrap();
        map
    }
    #[test]
    fn conditional_parameters_and_individual_rules_use_actual_candidate_values() {
        let p = definitions(
            r#"<pc><context><parameter id="Shallow" enable="//FourShades='true' or //FourShades='1'"><type>Double</type><default>2</default><validate enable="//Safety &gt; 0"><xpath>//Shallow &lt;= //Safety</xpath><errorMessage><text language="eng">Too shallow</text></errorMessage></validate></parameter><parameter id="Deep"><type>Double</type><default>30</default><validate><xpath>//Deep &gt;= //Safety</xpath><errorMessage><text>Too deep</text></errorMessage></validate></parameter></context></pc>"#,
        );
        let mut v = HashMap::from([
            ("FourShades".into(), "true".into()),
            ("Shallow".into(), "12".into()),
            ("Safety".into(), "10".into()),
            ("Deep".into(), "5".into()),
        ]);
        let report = validate_context_values(&p, &v).unwrap();
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.checked_rules, 2);
        v.insert("FourShades".into(), "false".into());
        let report = validate_context_values(&p, &v).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert!(!report.enabled["Shallow"]);
        v.insert("Deep".into(), "10".into());
        assert!(validate_context_values(&p, &v)
            .unwrap()
            .ensure_valid()
            .is_ok());
        v.insert("FourShades".into(), "1".into());
        v.insert("Safety".into(), "0".into());
        let report = validate_context_values(&p, &v).unwrap();
        assert!(report.failures.is_empty());
        assert_eq!(report.checked_rules, 1);
    }
    #[test]
    fn prefixed_root_with_unqualified_children_and_legacy_encoding_keep_rules() {
        let xml = r#"<pc:portrayalCatalog xmlns:pc="http://www.iho.int/S100PortrayalCatalog/5.2"><context><parameter id="Flag"><type>Boolean</type><default>true</default><validate><xpath>//Flag='true'</xpath><errorMessage><text>Check flag</text></errorMessage></validate></parameter></context></pc:portrayalCatalog>"#;
        let mut m = HashMap::from([(
            "Flag".into(),
            ContextParameter {
                id: "Flag".into(),
                param_type: ContextParamType::String,
                default_value: None,
                description: None,
                enable: None,
                constraints: vec![],
                validations: vec![],
            },
        )]);
        read_metadata(xml, &mut m).unwrap();
        assert_eq!(m["Flag"].validations.len(), 1);
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><pc><!-- Moir".to_vec();
        bytes.push(0xe9);
        bytes.extend_from_slice(b" --></pc>");
        assert!(decode_metadata_xml(bytes).unwrap().contains("Moiré"));
        assert!(decode_metadata_xml(vec![0xff]).is_err());
    }
    #[test]
    fn xml_regex_is_full_match_and_preserves_xml_dialect() {
        for (pattern, yes, no) in [
            ("[a-z]{3}", "eng", "xeng"),
            ("[a-z-[aeiou]]+", "bcdf", "abc"),
            (r"\p{L}+", "한국어", "한국1"),
            ("^a$", "^a$", "a"),
            (r"\i\c*", "name_1", "1name"),
        ] {
            assert!(full_regex(pattern, yes).unwrap(), "{pattern}");
            assert!(!full_regex(pattern, no).unwrap(), "{pattern}");
        }
        assert!(!full_regex("[a-z]{3}", "eng\n").unwrap());
        assert!(full_regex("[", "").is_err());
    }
    #[test]
    fn constraints_types_xpath_and_metadata_failures_are_explicit() {
        let p = definitions(
            r#"<pc><context><parameter id="Flag"><type>Boolean</type><default>false</default><constrain><enumeration value="1"><label language="eng">On</label></enumeration></constrain></parameter></context></pc>"#,
        );
        for v in ["1", "true"] {
            assert!(
                validate_context_values(&p, &HashMap::from([("Flag".into(), v.into())]))
                    .unwrap()
                    .ensure_valid()
                    .is_ok()
            );
        }
        for v in ["false", "yes"] {
            assert!(
                validate_context_values(&p, &HashMap::from([("Flag".into(), v.into())]))
                    .unwrap()
                    .ensure_valid()
                    .is_err()
            );
        }
        assert!(xpath_boolean("unknown()", sxd_document::Package::new().as_document()).is_err());
        assert!(xpath_boolean("1", sxd_document::Package::new().as_document()).is_err());
        for v in ["NaN", "inf", "-inf"] {
            assert!(canonical(ContextParamType::Double, v).is_err());
        }
        for v in [
            "2000-02-29",
            "2024-02-29+14:00",
            "-0004-02-29Z",
            "120000-02-29",
        ] {
            assert!(canonical(ContextParamType::Date, v).is_ok(), "{v}");
        }
        for v in [
            "1900-02-29",
            "0000-01-01",
            "2024-02-29+14:01",
            "2024-02-29+15:00",
            "한글-02-01",
        ] {
            assert!(canonical(ContextParamType::Date, v).is_err(), "{v}");
        }
        let mut m = HashMap::new();
        assert!(read_metadata(r#"<pc><context><parameter id="x"><type>Alien</type><default>0</default></parameter></context></pc>"#,&mut m).is_err());
    }
}
