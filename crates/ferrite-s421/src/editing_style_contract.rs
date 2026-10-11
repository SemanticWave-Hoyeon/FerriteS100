//! Declared public-PC resources for a HOST editing illustration, not XSLT portrayal.
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct EditingStyleContract {
    product: String,
    version: String,
    svg_tokens: BTreeSet<String>,
}
impl EditingStyleContract {
    pub fn from_declared_pc(pc: &str, waypoint_svg: &str) -> Result<Self, String> {
        let doc = bounded_xml(pc)?;
        let root = doc.root_element();
        if root.tag_name().name() != "portrayalCatalog" || root.tag_name().namespace().is_some() {
            return Err("Unsupported S421 editing PC root/namespace".into());
        }
        let product = root.attribute("productId").unwrap_or_default();
        if !product.is_empty() && product != "S421" && product != "S-421" {
            return Err("Contradictory editing PC product identity".into());
        }
        require_declaration(root, "symbols", "symbol", "RTEWPT01", "RTEWPT01.svg")?;
        require_declaration(
            root,
            "lineStyles",
            "lineStyle",
            "RTEACTLEGLINE",
            "RTEACTLEGLINE.xml",
        )?;
        require_declaration(
            root,
            "colorProfiles",
            "colorProfile",
            "COLOR01",
            "colorProfile.xml",
        )?;
        let svg = bounded_xml(waypoint_svg)?;
        if svg.root_element().tag_name().name() != "svg"
            || svg.root_element().tag_name().namespace() != Some("http://www.w3.org/2000/svg")
        {
            return Err("Waypoint resource is not SVG".into());
        }
        let mut tokens = BTreeSet::new();
        for node in svg.descendants().filter(|n| n.is_element()) {
            if matches!(
                node.tag_name().name(),
                "script" | "foreignObject" | "image" | "use" | "style"
            ) {
                return Err("Editing SVG embedded/external resources unsupported".into());
            }
            for attr in node.attributes() {
                let compact: String = attr
                    .value()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let compact = compact.to_ascii_lowercase();
                if attr.name() == "href"
                    || compact.contains("url(")
                    || (attr.name() == "style"
                        && (compact.contains("@import") || compact.contains('\\')))
                {
                    return Err("Editing SVG external/reference resolution unsupported".into());
                }
            }
            for class in node
                .attribute("class")
                .unwrap_or_default()
                .split_whitespace()
            {
                if let Some(token) = class.strip_prefix('f').or_else(|| class.strip_prefix('s')) {
                    if token.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
                        if token.len() > 32
                            || !token
                                .bytes()
                                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                        {
                            return Err("Unsupported SVG color class".into());
                        }
                        tokens.insert(token.to_owned());
                    }
                }
            }
        }
        Ok(Self {
            product: product.to_owned(),
            version: root.attribute("version").unwrap_or_default().to_owned(),
            svg_tokens: tokens,
        })
    }
    pub fn product(&self) -> &str {
        &self.product
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn svg_color_tokens(&self) -> impl Iterator<Item = &str> {
        self.svg_tokens.iter().map(String::as_str)
    }
}
fn bounded_xml(xml: &str) -> Result<roxmltree::Document<'_>, String> {
    if xml.len() > 1024 * 1024 {
        return Err("Editing XML exceeds 1 MiB".into());
    }
    let doc = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 20_000,
        },
    )
    .map_err(|e| e.to_string())?;
    if doc
        .descendants()
        .any(|n| n.ancestors().take(130).count() > 128)
    {
        return Err("Editing XML depth exceeds 128".into());
    }
    Ok(doc)
}
// Scalar declarations must not silently accept only the first text fragment.
fn scalar_text<'a, 'input>(node: roxmltree::Node<'a, 'input>) -> Result<&'a str, String> {
    let mut children = node.children();
    let value = children.next().ok_or("Empty editing XML scalar")?;
    if !value.is_text() || children.next().is_some() {
        return Err("Mixed/split editing XML scalar".into());
    }
    value
        .text()
        .ok_or_else(|| "Empty editing XML scalar".into())
}
fn require_declaration(
    root: roxmltree::Node<'_, '_>,
    section: &str,
    element: &str,
    id: &str,
    file: &str,
) -> Result<(), String> {
    let sections: Vec<_> = root
        .children()
        .filter(|n| {
            n.is_element() && n.tag_name().namespace().is_none() && n.tag_name().name() == section
        })
        .collect();
    if sections.len() != 1 {
        return Err(format!("Missing/duplicate {section}"));
    }
    let entries: Vec<_> = sections[0]
        .children()
        .filter(|n| {
            n.is_element()
                && n.tag_name().namespace().is_none()
                && n.tag_name().name() == element
                && n.attribute("id") == Some(id)
        })
        .collect();
    if entries.len() != 1 {
        return Err(format!("Missing/duplicate declared {id}"));
    }
    let names: Vec<_> = entries[0]
        .children()
        .filter(|n| {
            n.is_element()
                && n.tag_name().namespace().is_none()
                && n.tag_name().name() == "fileName"
        })
        .collect();
    if names.len() != 1 || scalar_text(names[0])? != file {
        return Err(format!("Unsupported declared filename for {id}"));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    const PC: &str = r#"<portrayalCatalog productId="" version=""><symbols><symbol id="RTEWPT01"><fileName>RTEWPT01.svg</fileName></symbol></symbols><lineStyles><lineStyle id="RTEACTLEGLINE"><fileName>RTEACTLEGLINE.xml</fileName></lineStyle></lineStyles><colorProfiles><colorProfile id="COLOR01"><fileName>colorProfile.xml</fileName></colorProfile></colorProfiles></portrayalCatalog>"#;
    const SVG: &str =
        r#"<svg xmlns="http://www.w3.org/2000/svg"><circle class="f0 sPLRTE"/></svg>"#;
    #[test]
    fn blank_metadata_retained_and_exact_token_declared() {
        let c = EditingStyleContract::from_declared_pc(PC, SVG).unwrap();
        assert_eq!(c.product(), "");
        assert_eq!(c.version(), "");
        assert_eq!(c.svg_color_tokens().collect::<Vec<_>>(), vec!["PLRTE"]);
    }
    #[test]
    fn undeclared_or_path_alias_and_external_svg_rejected() {
        assert!(EditingStyleContract::from_declared_pc(
            &PC.replace("RTEWPT01.svg", "../RTEWPT01.svg"),
            SVG
        )
        .is_err());
        assert!(EditingStyleContract::from_declared_pc(
            &PC.replace("id=\"RTEWPT01\"", "id=\"OTHER\""),
            SVG
        )
        .is_err());
        assert!(EditingStyleContract::from_declared_pc(
            PC,
            &SVG.replace("<circle", "<use href=\"other.svg\"")
        )
        .is_err());
    }
    #[test]
    fn split_filename_and_embedded_css_resources_rejected() {
        for filename in ["RTEWPT01.svg<!-- split -->.extra", "RTEWPT01.svg<x/>"] {
            assert!(EditingStyleContract::from_declared_pc(
                &PC.replace("RTEWPT01.svg", filename),
                SVG
            )
            .is_err());
        }
        for extra in [
            "<style>@import 'external.css';</style>",
            "<circle style=\"fill:URL (external.svg)\"/>",
            "<circle style=\"fill:u\\72l(external.svg)\"/>",
        ] {
            assert!(EditingStyleContract::from_declared_pc(
                PC,
                &SVG.replace("</svg>", &format!("{extra}</svg>"))
            )
            .is_err());
        }
    }
    #[test]
    fn dtd_and_foreign_root_rejected() {
        assert!(EditingStyleContract::from_declared_pc(
            &format!("<!DOCTYPE portrayalCatalog [<!ENTITY x 'x'>]>{PC}"),
            SVG
        )
        .is_err());
        assert!(EditingStyleContract::from_declared_pc(
            &PC.replace("productId=\"\"", "productId=\"S101\""),
            SVG
        )
        .is_err());
    }
}

/// Exact requested palette from declared COLOR01 bytes, never directory enumeration.
pub fn read_editing_palette(
    xml: &str,
    requested: &str,
) -> Result<std::collections::BTreeMap<String, [u8; 3]>, String> {
    let doc = bounded_xml(xml)?;
    let root = doc.root_element();
    if root.tag_name().namespace().is_some() || root.tag_name().name() != "colorProfile" {
        return Err("Unsupported editing color profile root".into());
    }
    let palettes: Vec<_> = root
        .children()
        .filter(|n| {
            n.is_element()
                && n.tag_name().namespace().is_none()
                && n.tag_name().name() == "palette"
                && n.attribute("name") == Some(requested)
        })
        .collect();
    if palettes.len() != 1 {
        return Err(format!(
            "Absent/duplicate requested S421 palette {requested}"
        ));
    }
    let mut result = std::collections::BTreeMap::new();
    for item in palettes[0]
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "item")
    {
        if item.tag_name().namespace().is_some() {
            return Err("Foreign palette item namespace".into());
        }
        let token = item.attribute("token").ok_or("Missing palette token")?;
        if token.is_empty()
            || token.len() > 32
            || !token
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Err("Invalid palette token".into());
        }
        let srgb: Vec<_> = item
            .children()
            .filter(|n| {
                n.is_element()
                    && n.tag_name().name() == "srgb"
                    && n.tag_name().namespace().is_none()
            })
            .collect();
        if srgb.len() != 1 {
            return Err(format!("Missing/duplicate explicit sRGB for {token}"));
        }
        let mut rgb = [0; 3];
        for (i, name) in ["red", "green", "blue"].into_iter().enumerate() {
            let channels: Vec<_> = srgb[0]
                .children()
                .filter(|n| {
                    n.is_element()
                        && n.tag_name().namespace().is_none()
                        && n.tag_name().name() == name
                })
                .collect();
            if channels.len() != 1 {
                return Err("Invalid palette channel".into());
            }
            let text = scalar_text(channels[0])?.trim();
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err("Nondecimal sRGB channel".into());
            }
            rgb[i] = text
                .parse::<u8>()
                .map_err(|_| "sRGB channel out of range")?;
        }
        if result.insert(token.to_owned(), rgb).is_some() {
            return Err("Duplicate palette token".into());
        }
    }
    if result.is_empty() {
        return Err("Empty editing palette".into());
    }
    Ok(result)
}
#[cfg(test)]
mod palette_tests {
    use super::*;
    const DAY: &str = r#"<colorProfile><palette name="Day"><item token="PLRTE"><srgb><red>214</red><green>63</green><blue>36</blue></srgb></item></palette></colorProfile>"#;
    #[test]
    fn actual_palette_name_not_catalogue_id_no_fallback() {
        assert_eq!(
            read_editing_palette(DAY, "Day").unwrap()["PLRTE"],
            [214, 63, 36]
        );
        assert!(read_editing_palette(DAY, "Dusk").is_err());
        assert!(read_editing_palette(DAY, "COLOR01").is_err());
    }
    #[test]
    fn split_channel_text_is_not_truncated() {
        for channel in ["<red>214<!--split-->0</red>", "<red>214<x/></red>"] {
            assert!(read_editing_palette(&DAY.replace("<red>214</red>", channel), "Day").is_err());
        }
    }
    #[test]
    fn duplicate_or_partial_channels_rejected() {
        assert!(read_editing_palette(
            &DAY.replace("<red>214</red>", "<red>214</red><red>0</red>"),
            "Day"
        )
        .is_err());
        assert!(read_editing_palette(&DAY.replace("<blue>36</blue>", ""), "Day").is_err());
    }
}

#[cfg(test)]
mod published_fixture_tests {
    use super::*;
    #[test]
    fn public_pc_blank_identity_day_and_waypoint_use_exact_declared_resources() {
        let c = EditingStyleContract::from_declared_pc(
            include_str!("../tests/fixtures/editing/portrayal_catalogue.xml"),
            include_str!("../tests/fixtures/editing/RTEWPT01.svg"),
        )
        .unwrap();
        assert_eq!(c.product(), "");
        assert_eq!(c.version(), "");
        assert_eq!(c.svg_color_tokens().collect::<Vec<_>>(), vec!["PLRTE"]);
        let colors = read_editing_palette(
            include_str!("../tests/fixtures/editing/colorProfile.xml"),
            "Day",
        )
        .unwrap();
        assert!(colors.contains_key("PLRTE"));
        assert!(read_editing_palette(
            include_str!("../tests/fixtures/editing/colorProfile.xml"),
            "Night"
        )
        .is_err());
    }
}
