//! S-100 Part 9-12.4: preserve composite components and resolve catalogue IDs.
use std::collections::HashMap;
#[cfg(test)]
use std::io::Read;
#[cfg(test)]
use std::path::Path;

use roxmltree::{Document, Node, ParsingOptions};

use crate::{
    CapStyle, CompositeLineStyle, Dash, JoinStyle, LineStyle, LineSymbol, PCError, Pen, Result,
    SimpleLineStyle,
};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_COMPONENTS: usize = 4096;
const MAX_TOTAL_COMPONENTS: usize = 65536;
// Account materialized strings, symbols and dash arrays before cloning them.
const MAX_STYLE_PAYLOAD: usize = 4 * 1024 * 1024;
const MAX_TOTAL_PAYLOAD: usize = 16 * 1024 * 1024;
fn payload(style: &SimpleLineStyle) -> usize {
    std::mem::size_of::<SimpleLineStyle>()
        + style.id.len()
        + style.pen.color_token.len()
        + style.dashes.len() * std::mem::size_of::<Dash>()
        + style.symbols.len() * std::mem::size_of::<LineSymbol>()
        + style
            .symbols
            .iter()
            .map(|s| s.reference.len())
            .sum::<usize>()
}

#[derive(Debug)]
pub(crate) enum Definition {
    Simple(SimpleLineStyle),
    Composite(Vec<Definition>),
    Reference(String),
}

fn invalid(message: impl Into<String>) -> PCError {
    PCError::InvalidValue(message.into())
}

#[cfg(test)]
pub(crate) fn parse(path: &Path, id: &str) -> Result<Definition> {
    let mut xml = String::new();
    std::fs::File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_string(&mut xml)?;
    parse_text(&xml, id)
}

pub(crate) fn parse_bytes(bytes: &[u8], id: &str) -> Result<Definition> {
    let xml =
        std::str::from_utf8(bytes).map_err(|e| invalid(format!("LineStyle XML UTF-8: {e}")))?;
    parse_text(xml, id)
}

fn parse_text(xml: &str, id: &str) -> Result<Definition> {
    if xml.len() > MAX_BYTES {
        return Err(invalid("LineStyle XML byte budget exceeded"));
    }
    let doc = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 65536,
        },
    )
    .map_err(|e| invalid(format!("LineStyle XML: {e}")))?;
    parse_node(doc.root_element(), id, 0, &mut 0)
}

fn number(value: &str) -> Result<f64> {
    let number = value
        .trim()
        .parse::<f64>()
        .map_err(|_| invalid("Invalid LineStyle number"))?;
    if !number.is_finite() {
        return Err(invalid("Nonfinite LineStyle number"));
    }
    Ok(number)
}

fn required_attribute<'a>(node: Node<'a, 'a>, name: &str) -> Result<&'a str> {
    node.attribute(name)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("Missing LineStyle {} attribute", name)))
}

fn child<'a>(node: Node<'a, 'a>, name: &str, required: bool) -> Result<Option<Node<'a, 'a>>> {
    let mut children = node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == name);
    let first = children.next();
    if children.next().is_some() || (required && first.is_none()) {
        return Err(invalid(format!("Missing or duplicate LineStyle {name}")));
    }
    Ok(first)
}

fn child_text<'a>(node: Node<'a, 'a>, name: &str) -> Result<&'a str> {
    let c = child(node, name, true)?.unwrap();
    if c.children().any(|n| n.is_element()) {
        return Err(invalid("Nested LineStyle scalar"));
    }
    c.text()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("Empty LineStyle {name}")))
}

fn parse_node(
    node: Node<'_, '_>,
    id: &str,
    depth: usize,
    components: &mut usize,
) -> Result<Definition> {
    if depth >= MAX_DEPTH {
        return Err(invalid("LineStyle XML nesting budget exceeded"));
    }
    *components += 1;
    if *components > MAX_COMPONENTS {
        return Err(invalid("LineStyle component budget exceeded"));
    }
    match node.tag_name().name() {
        "compositeLineStyle" => {
            let parts: Vec<_> = node
                .children()
                .filter(|n| n.is_element())
                .map(|n| parse_node(n, id, depth + 1, components))
                .collect::<Result<_>>()?;
            if parts.is_empty() {
                return Err(invalid("Empty CompositeLineStyle"));
            }
            Ok(Definition::Composite(parts))
        }
        "lineStyleReference" => {
            if node.children().any(|n| n.is_element()) {
                return Err(invalid("Nested LineStyleReference"));
            }
            Ok(Definition::Reference(
                required_attribute(node, "reference")?.into(),
            ))
        }
        "lineStyle" => {
            let pen_node = child(node, "pen", true)?.unwrap();
            let cap_style = match node.attribute("capStyle").unwrap_or("Butt") {
                "Butt" => CapStyle::Butt,
                "Square" => CapStyle::Square,
                "Round" => CapStyle::Round,
                _ => return Err(invalid("Invalid line cap")),
            };
            let join_style = match node.attribute("joinStyle").unwrap_or("Miter") {
                "Miter" => JoinStyle::Miter,
                "Bevel" => JoinStyle::Bevel,
                "Round" => JoinStyle::Round,
                _ => return Err(invalid("Invalid line join")),
            };
            let mut style = SimpleLineStyle {
                id: id.into(),
                offset_mm: number(node.attribute("offset").unwrap_or("0"))?,
                interval_length: match child(node, "intervalLength", false)? {
                    Some(_) => number(child_text(node, "intervalLength")?)?,
                    None => 0.,
                },
                pen: Pen {
                    width: number(required_attribute(pen_node, "width")?)?,
                    color_token: child_text(pen_node, "color")?.into(),
                    cap_style,
                    join_style,
                },
                ..Default::default()
            };
            if style.pen.width < 0. || style.interval_length < 0. {
                return Err(invalid("Invalid line width/interval"));
            }
            for c in node.children().filter(|n| n.is_element()) {
                match c.tag_name().name() {
                    "pen" | "intervalLength" => {}
                    "dash" => {
                        if style.dashes.len() >= MAX_COMPONENTS {
                            return Err(invalid("Dash budget exceeded"));
                        }
                        style.dashes.push(Dash {
                            start: number(child_text(c, "start")?)?,
                            length: number(child_text(c, "length")?)?,
                        });
                    }
                    "symbol" => {
                        if style.symbols.len() >= MAX_COMPONENTS {
                            return Err(invalid("LineSymbol budget exceeded"));
                        }
                        style.symbols.push(LineSymbol {
                            reference: required_attribute(c, "reference")?.into(),
                            position: number(child_text(c, "position")?)?,
                            rotation: number(c.attribute("rotation").unwrap_or("0"))?,
                            scale_factor: number(c.attribute("scaleFactor").unwrap_or("1"))?,
                            crs_type: c
                                .attribute("crsType")
                                .unwrap_or("LocalCRS")
                                .parse()
                                .map_err(|e: &str| invalid(e))?,
                        });
                    }
                    other => return Err(invalid(format!("Unexpected LineStyle child {other}"))),
                }
            }
            Ok(Definition::Simple(style))
        }
        other => Err(invalid(format!("Unexpected line style element {other}"))),
    }
}

/// Flatten only after all XML files have been read, so forward references work.
/// Catalogue mutation happens after resolution succeeds for the whole set.
pub(crate) fn resolve(
    definitions: &HashMap<String, Definition>,
) -> Result<HashMap<String, LineStyle>> {
    if definitions.len() > MAX_COMPONENTS {
        return Err(invalid("LineStyle catalogue budget exceeded"));
    }
    let mut resolver = Resolver {
        definitions,
        cache: HashMap::new(),
        stack: Vec::new(),
        total: 0,
        total_payload: 0,
    };
    let mut ids: Vec<_> = definitions.keys().collect();
    ids.sort();
    for id in ids {
        resolver.named(id, 0)?;
    }
    Ok(resolver.cache)
}

struct Resolver<'a> {
    definitions: &'a HashMap<String, Definition>,
    cache: HashMap<String, LineStyle>,
    stack: Vec<String>,
    total: usize,
    total_payload: usize,
}

impl Resolver<'_> {
    fn named(&mut self, id: &str, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(invalid("Resolved LineStyle nesting budget exceeded"));
        }
        if self.cache.contains_key(id) {
            return Ok(());
        }
        if self.stack.len() >= MAX_DEPTH || self.stack.iter().any(|s| s == id) {
            return Err(invalid(format!("LineStyle reference cycle/depth at {id}")));
        }
        let definition = self
            .definitions
            .get(id)
            .ok_or_else(|| PCError::ResourceNotFound(format!("LineStyleReference {id}")))?;
        self.stack.push(id.into());
        let mut parts = Vec::new();
        let mut bytes = 0;
        self.components(definition, &mut parts, &mut bytes, depth)?;
        self.stack.pop();
        self.total += parts.len();
        self.total_payload += bytes;
        if self.total_payload > MAX_TOTAL_PAYLOAD {
            return Err(invalid("Resolved catalogue payload budget exceeded"));
        }
        if self.total > MAX_TOTAL_COMPONENTS {
            return Err(invalid("Resolved catalogue component budget exceeded"));
        }
        let style = match definition {
            Definition::Simple(_) => LineStyle::Simple(parts.pop().unwrap()),
            _ => LineStyle::Composite(CompositeLineStyle {
                id: id.into(),
                components: parts,
            }),
        };
        self.cache.insert(id.into(), style);
        Ok(())
    }

    fn components(
        &mut self,
        definition: &Definition,
        out: &mut Vec<SimpleLineStyle>,
        bytes: &mut usize,
        depth: usize,
    ) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(invalid("Resolved LineStyle nesting budget exceeded"));
        }
        match definition {
            Definition::Simple(s) => {
                if out.len() >= MAX_COMPONENTS {
                    return Err(invalid("Resolved line component budget exceeded"));
                }
                let size = payload(s);
                if size > MAX_STYLE_PAYLOAD - *bytes {
                    return Err(invalid("Resolved line payload budget exceeded"));
                }
                *bytes += size;
                out.push(s.clone());
            }
            Definition::Composite(parts) => {
                for p in parts {
                    self.components(p, out, bytes, depth + 1)?;
                }
            }
            Definition::Reference(id) => {
                self.named(id, depth + 1)?;
                let parts: &[SimpleLineStyle] = match &self.cache[id] {
                    LineStyle::Simple(s) => std::slice::from_ref(s),
                    LineStyle::Composite(c) => &c.components,
                    LineStyle::Complex(c) => &c.strokes,
                };
                if parts.len() > MAX_COMPONENTS - out.len() {
                    return Err(invalid("Resolved line component budget exceeded"));
                }
                let size = parts.iter().map(payload).sum::<usize>();
                if size > MAX_STYLE_PAYLOAD - *bytes {
                    return Err(invalid("Resolved line payload budget exceeded"));
                }
                *bytes += size;
                out.extend_from_slice(parts);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str =
        r#"<lineStyle offset="-1"><pen width="1.28"><color>BKAJ1</color></pen></lineStyle>"#;
    const B: &str =
        r#"<lineStyle offset="2"><pen width="0.64"><color>CHYLW</color></pen></lineStyle>"#;
    fn set(xml: &[(&str, &str)]) -> HashMap<String, Definition> {
        xml.iter()
            .map(|(id, s)| ((*id).into(), parse_text(s, id).unwrap()))
            .collect()
    }
    #[test]
    fn nested_components_preserve_order_and_physical_metadata() {
        let xml = format!("<compositeLineStyle>{A}<compositeLineStyle>{B}</compositeLineStyle>{A}</compositeLineStyle>");
        let result = resolve(&set(&[("C", &xml)])).unwrap();
        let LineStyle::Composite(c) = &result["C"] else {
            panic!()
        };
        assert_eq!(
            c.components
                .iter()
                .map(|s| (s.pen.width, s.offset_mm, s.pen.color_token.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1.28, -1., "BKAJ1"),
                (0.64, 2., "CHYLW"),
                (1.28, -1., "BKAJ1")
            ]
        );
    }
    #[test]
    fn forward_and_repeated_references_resolve_without_deduplicating_components() {
        let d = set(&[
            ("Z", B),
            (
                "A",
                r#"<compositeLineStyle><lineStyleReference reference="Z"/><lineStyleReference reference="Z"></lineStyleReference></compositeLineStyle>"#,
            ),
        ]);
        let result = resolve(&d).unwrap();
        let LineStyle::Composite(c) = &result["A"] else {
            panic!()
        };
        assert_eq!(c.id, "A");
        assert_eq!(c.components.len(), 2);
        assert_eq!(c.components[0].pen.width, 0.64);
        assert_eq!(c.components[1].pen.width, 0.64);
    }
    #[test]
    fn missing_and_cyclic_references_fail_instead_of_silently_dropping_parts() {
        assert!(resolve(&set(&[(
            "A",
            r#"<lineStyleReference reference="Missing"/>"#
        )]))
        .is_err());
        assert!(resolve(&set(&[
            ("A", r#"<lineStyleReference reference="B"/>"#),
            ("B", r#"<lineStyleReference reference="A"/>"#)
        ]))
        .is_err());
    }
    #[test]
    fn empty_malformed_and_incomplete_styles_fail() {
        for xml in ["<compositeLineStyle/>", "<lineStyle/>", "<lineStyle><pen width=\"1\"/></lineStyle>", "<lineStyleReference/>", "<lineStyle><pen width=\"1\"><color>C</color></pen><dash><start>0</start></dash></lineStyle>", "<lineStyle><pen width=\"1\"><color>C</color></pen></lineStyle><lineStyle/>"] {
            assert!(parse_text(xml, "A").is_err(), "{xml}");
        }
    }
    #[test]
    fn nesting_expansion_and_xml_budgets_are_bounded() {
        let deep = format!(
            "{}{}{}",
            "<compositeLineStyle>".repeat(65),
            A,
            "</compositeLineStyle>".repeat(65)
        );
        assert!(parse_text(&deep, "A").is_err());
        assert!(parse_text(&" ".repeat(MAX_BYTES + 1), "A").is_err());
        assert!(parse_text("<!DOCTYPE lineStyle [<!ENTITY a 'C'>]><lineStyle/>", "A").is_err());
        let mut d = set(&[("0", A)]);
        for i in 1..=13 {
            d.insert(
                i.to_string(),
                Definition::Composite(vec![
                    Definition::Reference((i - 1).to_string()),
                    Definition::Reference((i - 1).to_string()),
                ]),
            );
        }
        assert!(
            resolve(&d).is_err(),
            "Exponential references must stop at the output budget"
        );
    }
    #[test]
    fn symbol_payload_is_budgeted_before_reference_expansion() {
        let mut style = SimpleLineStyle::default();
        style.symbols.push(LineSymbol {
            reference: "R".repeat(40000),
            ..Default::default()
        });
        let mut definitions = HashMap::new();
        definitions.insert("S".into(), Definition::Simple(style));
        definitions.insert(
            "C".into(),
            Definition::Composite(
                (0..128)
                    .map(|_| Definition::Reference("S".into()))
                    .collect(),
            ),
        );
        assert!(
            resolve(&definitions).is_err(),
            "A small stroke count with large symbol strings still exceeds the payload limit"
        );
    }
    #[test]
    fn reference_ids_are_xml_decoded_and_never_used_as_file_paths() {
        let result = resolve(&set(&[
            ("A&B", A),
            ("C", r#"<lineStyleReference reference="A&amp;B"/>"#),
        ]))
        .unwrap();
        let LineStyle::Composite(c) = &result["C"] else {
            panic!()
        };
        assert_eq!(c.components.len(), 1);
        assert!(resolve(&set(&[(
            "C",
            r#"<lineStyleReference reference="../outside"/>"#
        )]))
        .is_err());
    }
}
