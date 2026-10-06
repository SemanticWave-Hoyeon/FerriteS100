//! Product-neutral Part 9 catalogue names and signed drawing-plane orders.
use crate::{PCError, Result};
use roxmltree::{Document, Node};
use std::{collections::HashMap, num::NonZeroI32};

#[derive(Debug, Default, Clone)]
pub struct DisplayPlanes {
    pub planes: HashMap<String, NonZeroI32>,
}
impl DisplayPlanes {
    pub fn resolve(&self, reference: &str) -> Result<NonZeroI32> {
        self.planes.get(reference).copied().ok_or_else(|| {
            PCError::InvalidValue(format!("Unknown portrayal display plane: {reference}"))
        })
    }
}

fn children<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Vec<Node<'a, 'i>> {
    node.children().filter(|n| n.is_element()
        && n.tag_name().name() == name
        && (n.tag_name().namespace().is_none()
            || n.tag_name().namespace() == node.tag_name().namespace())).collect()
}

pub(crate) fn read(doc: &Document<'_>) -> Result<DisplayPlanes> {
    let containers = children(doc.root_element(), "displayPlanes");
    if containers.len() != 1 {
        return Err(PCError::InvalidValue("PC requires one displayPlanes container".into()));
    }
    let definitions = children(containers[0], "displayPlane");
    if definitions.is_empty() {
        return Err(PCError::InvalidValue("PC displayPlanes requires a plane definition".into()));
    }
    let mut result = DisplayPlanes::default();
    for n in definitions {
        let id = n.attribute("id").unwrap_or("");
        if id.is_empty() || id.trim() != id {
            return Err(PCError::InvalidValue("Display plane requires a nonempty id".into()));
        }
        let raw = n.attribute("order").ok_or_else(|| {
            PCError::InvalidValue(format!("Display plane {id} has no order"))
        })?;
        let order = raw.parse::<i32>().ok().and_then(NonZeroI32::new).ok_or_else(|| {
            PCError::InvalidValue(format!("Display plane {id} needs a supported nonzero integer order; zero is reserved for RADAR"))
        })?;
        if result.planes.insert(id.to_owned(), order).is_some() {
            return Err(PCError::InvalidValue(format!("Duplicate display plane: {id}")));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(body: &str) -> Result<DisplayPlanes> {
        let xml = format!("<portrayalCatalogue xmlns='urn:pc'>{body}</portrayalCatalogue>");
        read(&Document::parse(&xml).unwrap())
    }
    #[test]
    fn preserves_names_sparse_negative_and_same_orders_without_name_heuristics() {
        let p = parse("<displayPlanes><displayPlane id='OverRadar' order='-701'/><displayPlane id='overRadar' order='90000'/><displayPlane id='custom' order='90000'/><displayPlane id='lo' order='-2147483648'/><displayPlane id='hi' order='2147483647'/></displayPlanes>").unwrap();
        assert_eq!(p.resolve("OverRadar").unwrap().get(), -701);
        assert_eq!(p.resolve("overRadar").unwrap().get(), 90000);
        assert_eq!(p.resolve("custom").unwrap().get(), 90000);
        assert_eq!(p.resolve("lo").unwrap().get(), i32::MIN);
        assert_eq!(p.resolve("hi").unwrap().get(), i32::MAX);
        assert!(p.resolve("OVERRADAR").is_err());
    }
    #[test]
    fn missing_duplicate_reserved_and_namespace_spoofed_planes_are_rejected() {
        for body in ["", "<displayPlanes/>",
            "<displayPlanes/><displayPlanes/>",
            "<displayPlanes><displayPlane order='1'/></displayPlanes>",
            "<displayPlanes><displayPlane id='a'/></displayPlanes>",
            "<displayPlanes><displayPlane id='a' order='0'/></displayPlanes>",
            "<displayPlanes><displayPlane id='a' order='1.5'/></displayPlanes>",
            "<displayPlanes><displayPlane id='a' order='2147483648'/></displayPlanes>",
            "<displayPlanes><displayPlane id='a' order='1'/><displayPlane id='a' order='-1'/></displayPlanes>",
            "<displayPlanes xmlns='urn:evil'><displayPlane id='a' order='1'/></displayPlanes>",
            "<displayPlanes><displayPlane xmlns='urn:evil' id='a' order='1'/></displayPlanes>"] {
            assert!(parse(body).is_err(), "{body}");
        }
    }
}
