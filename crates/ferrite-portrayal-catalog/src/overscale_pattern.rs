//! Narrow active-PC resource capability for S-98 OVERSC01 (not a product instruction).
use crate::{CatalogueSources, PCError, Result};
use std::path::Path;
#[derive(Debug)]
pub struct OverscalePatternDefinition {
    digest: [u8; 32],
}
impl OverscalePatternDefinition {
    pub fn from_sources(sources: &CatalogueSources, path: &Path) -> Result<Self> {
        let bytes = sources.read_path(path)?;
        Self::check_xml(&bytes)?;
        Ok(Self {
            digest: *sources.digest(),
        })
    }
    pub fn source_digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub fn symbol_ref(&self) -> &'static str {
        "OVERSC01P"
    }
    pub fn lattice_mm(&self) -> [[f64; 2]; 2] {
        [[9., 0.], [0., 4.]]
    }
    fn check_xml(bytes: &[u8]) -> Result<()> {
        let bad = || PCError::InvalidValue("Unsupported active-PC OVERSC01 definition".into());
        if bytes.len() > 4096 {
            return Err(bad());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| bad())?;
        let doc = roxmltree::Document::parse_with_options(
            text,
            roxmltree::ParsingOptions {
                allow_dtd: false,
                nodes_limit: 128,
            },
        )
        .map_err(|_| bad())?;
        let root = doc.root_element();
        if root.tag_name().name() != "symbolFill"
            || !matches!(
                root.tag_name().namespace(),
                None | Some("http://www.iho.int/S100AreaFill/5.2")
            )
        {
            return Err(bad());
        }
        let elements: Vec<_> = root.children().filter(|n| n.is_element()).collect();
        if elements.len() != 4
            || root.attributes().len() != 0
            || elements.iter().any(|n| n.tag_name().namespace().is_some())
        {
            return Err(bad());
        }
        // S-100 SymbolFill: absent clipSymbols means true. Unknown controls
        // cannot silently turn an authored whole-motif policy into clipping.
        for node in &elements {
            if node.tag_name().name() == "symbol" {
                if node.attributes().len() != 1 || node.children().any(|n| n.is_element()) {
                    return Err(bad());
                }
            } else if node.attributes().len() != 0 {
                return Err(bad());
            }
        }
        let child = |name| {
            let mut found = elements.iter().filter(|n| n.tag_name().name() == name);
            let first = found.next().copied();
            if found.next().is_some() {
                None
            } else {
                first
            }
        };
        if child("areaCRS").and_then(|n| n.text()).map(str::trim) != Some("GlobalGeometry")
            || child("symbol").and_then(|n| n.attribute("reference")) != Some("OVERSC01P")
        {
            return Err(bad());
        }
        for (name, expected) in [("v1", [9., 0.]), ("v2", [0., 4.])] {
            let vector = child(name).ok_or_else(bad)?;
            let parts: Vec<_> = vector.children().filter(|n| n.is_element()).collect();
            if parts.len() != 2 {
                return Err(bad());
            }
            for (axis, value) in [("x", expected[0]), ("y", expected[1])] {
                let values: Vec<_> = parts
                    .iter()
                    .filter(|n| n.tag_name().name() == axis)
                    .collect();
                if values.len() != 1
                    || values[0].text().and_then(|s| s.trim().parse::<f64>().ok()) != Some(value)
                {
                    return Err(bad());
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const XML: &str = r#"<af:symbolFill xmlns:af="http://www.iho.int/S100AreaFill/5.2"><areaCRS>GlobalGeometry</areaCRS><symbol reference="OVERSC01P"/><v1><x>9</x><y>0.0</y></v1><v2><x>0</x><y>4</y></v2></af:symbolFill>"#;
    #[test]
    fn official_contract_and_invalid_lattice_or_anchor() {
        assert!(OverscalePatternDefinition::check_xml(XML.as_bytes()).is_ok());
        // Actual 1.0.2/1.1.0 PC definitions use the same XML without namespace.
        let legacy = XML
            .replace("af:", "")
            .replace(" xmlns:af=\"http://www.iho.int/S100AreaFill/5.2\"", "");
        assert!(OverscalePatternDefinition::check_xml(legacy.as_bytes()).is_ok());
        for edited in [
            XML.replace("GlobalGeometry", "Global"),
            XML.replace(
                "http://www.iho.int/S100AreaFill/5.2",
                "http://example.invalid/areaFill",
            ),
            XML.replace("OVERSC01P", "DIAMOND1P"),
            XML.replace("<x>9</x>", "<x>NaN</x>"),
            XML.replace("<x>9</x>", "<x>8</x>"),
            XML.replace("<areaCRS>", "<areaCRS unsupported='1'>"),
            XML.replace(
                "reference=\"OVERSC01P\"",
                "reference=\"OVERSC01P\" clipSymbols=\"false\"",
            ),
        ] {
            assert!(OverscalePatternDefinition::check_xml(edited.as_bytes()).is_err());
        }
    }
    #[test]
    fn definition_is_bounded_and_dtd_not_allowed() {
        assert!(OverscalePatternDefinition::check_xml(&vec![b' '; 4097]).is_err());
        assert!(OverscalePatternDefinition::check_xml(
            format!("<!DOCTYPE af:symbolFill [<!ENTITY x '9'>]>{XML}").as_bytes()
        )
        .is_err());
    }
}
