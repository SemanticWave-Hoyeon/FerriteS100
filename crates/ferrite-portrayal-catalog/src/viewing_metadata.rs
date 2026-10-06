//! Preserve catalogue identifiers while assigning deterministic local render handles.
use crate::viewing::{ViewingGroup, ViewingGroupLayer, ViewingGroupLayers, ViewingGroups};
use crate::{PCError, Result};
use roxmltree::{Document, Node};
use std::collections::{BTreeMap, HashSet};
fn children<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Vec<Node<'a, 'i>> {
    node.children()
        .filter(|n| {
            n.is_element()
                && n.tag_name().name() == name
                && (n.tag_name().namespace().is_none()
                    || n.tag_name().namespace() == node.tag_name().namespace())
        })
        .collect()
}
fn container<'a, 'i>(root: Node<'a, 'i>, name: &str) -> Result<Option<Node<'a, 'i>>> {
    let found = children(root, name);
    if found.len() > 1 {
        return Err(PCError::InvalidValue(format!("Duplicate {name}")));
    }
    Ok(found.first().copied())
}
fn id(node: Node<'_, '_>) -> Result<String> {
    let value = node.attribute("id").unwrap_or("");
    if value.is_empty() || value.trim() != value {
        return Err(PCError::InvalidValue(
            "Missing or padded catalogue identifier".into(),
        ));
    }
    Ok(value.into())
}
fn text(node: Node<'_, '_>, name: &str) -> Option<String> {
    children(node, name)
        .first()
        .and_then(|n| n.text())
        .map(|s| s.trim().to_owned())
}
fn description(node: Node<'_, '_>) -> (String, Option<String>) {
    let descriptions = children(node, "description");
    let selected = descriptions
        .iter()
        .find(|n| text(**n, "language").as_deref() == Some("eng"))
        .or(descriptions.first());
    selected
        .map(|n| {
            (
                text(*n, "name").unwrap_or_default(),
                text(*n, "description"),
            )
        })
        .unwrap_or_default()
}
fn numeric(id: &str) -> Option<u32> {
    id.parse::<u32>().ok().filter(|v| v.to_string() == id)
}
pub(crate) fn read(doc: &Document<'_>) -> Result<(ViewingGroups, ViewingGroupLayers)> {
    let root = doc.root_element();
    let mut groups = ViewingGroups::new();
    let mut definitions = BTreeMap::new();
    if let Some(container) = container(root, "viewingGroups")? {
        for node in children(container, "viewingGroup") {
            let key = id(node)?;
            if definitions.insert(key.clone(), node).is_some() {
                return Err(PCError::InvalidValue(format!(
                    "Duplicate viewing group {key:?}"
                )));
            }
        }
    }
    let mut used: HashSet<u32> = definitions.keys().filter_map(|k| numeric(k)).collect();
    groups.numeric_ids = used.clone();
    let mut next = u32::MAX;
    for (key, node) in definitions {
        let handle = if let Some(value) = numeric(&key) {
            value
        } else {
            while used.contains(&next) {
                next = next.checked_sub(1).ok_or_else(|| {
                    PCError::InvalidValue("Viewing group handle space exhausted".into())
                })?;
            }
            used.insert(next);
            next
        };
        let (name, description) = description(node);
        groups.identifiers.insert(key.clone(), handle);
        groups.groups.insert(
            handle,
            ViewingGroup {
                id: handle,
                catalogue_id: key,
                name,
                description,
                parent_id: None,
            },
        );
    }
    let mut layers = ViewingGroupLayers::new();
    if let Some(container) = container(root, "viewingGroupLayers")? {
        for node in children(container, "viewingGroupLayer") {
            let key = id(node)?;
            let mut refs = Vec::new();
            for reference in children(node, "viewingGroup") {
                if reference.children().any(|n| n.is_element()) {
                    return Err(PCError::InvalidValue(
                        "Nested viewing group reference".into(),
                    ));
                }
                let value = reference.text().unwrap_or("").trim();
                let handle = groups.runtime_id(value).ok_or_else(|| {
                    PCError::InvalidValue(format!("Undeclared layer viewing group {value:?}"))
                })?;
                refs.push(handle);
            }
            if refs.is_empty() {
                return Err(PCError::InvalidValue(format!(
                    "Empty viewing group layer {key:?}"
                )));
            }
            let (name, _) = description(node);
            let default_on = match text(node, "defaultOn").as_deref() {
                None | Some("true") | Some("1") => true,
                Some("false") | Some("0") => false,
                Some(v) => return Err(PCError::InvalidValue(format!("Invalid defaultOn {v:?}"))),
            };
            if layers
                .layers
                .insert(
                    key.clone(),
                    ViewingGroupLayer {
                        id: key.clone(),
                        name,
                        viewing_group_ids: refs,
                        default_on,
                    },
                )
                .is_some()
            {
                return Err(PCError::InvalidValue(format!(
                    "Duplicate viewing group layer {key:?}"
                )));
            }
        }
    }
    Ok((groups, layers))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn load(defs: &str, refs: &str) -> Result<(ViewingGroups, ViewingGroupLayers)> {
        let xml=format!("<pc><viewingGroups>{defs}</viewingGroups><viewingGroupLayers><viewingGroupLayer id=\"layer\">{refs}</viewingGroupLayer></viewingGroupLayers></pc>");
        read(&Document::parse(&xml).unwrap())
    }
    #[test]
    fn stable_interning_preserves_identifiers_and_numeric_fast_path() {
        let a=load(r#"<viewingGroup id="accuracy"/><viewingGroup id="01"/><viewingGroup id="1"/><viewingGroup id="4294967295"/>"#, "<viewingGroup>accuracy</viewingGroup><viewingGroup>01</viewingGroup>").unwrap();
        let b=load(r#"<viewingGroup id="4294967295"/><viewingGroup id="1"/><viewingGroup id="01"/><viewingGroup id="accuracy"/>"#, "<viewingGroup>01</viewingGroup>").unwrap();
        assert_eq!(a.0.identifiers, b.0.identifiers);
        assert_eq!(a.0.runtime_id("01"), Some(u32::MAX - 1));
        assert_eq!(a.0.runtime_id("accuracy"), Some(u32::MAX - 2));
        assert_eq!(a.0.runtime_id("1"), Some(1));
        assert!(matches!(
            a.0.resolve_drawing_groups(&[1], &[]).unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(
            a.0.resolve_drawing_groups(&[1], &["accuracy".into()])
                .unwrap()
                .as_ref(),
            [1, u32::MAX - 2]
        );
        assert!(a.0.resolve_drawing_groups(&[u32::MAX - 2], &[]).is_err());
        assert!(a
            .0
            .resolve_drawing_groups(&[], &["Accuracy".into()])
            .is_err());
        let doc = Document::parse(
            "<pc><foundationMode><viewingGroup>accuracy</viewingGroup></foundationMode></pc>",
        )
        .unwrap();
        assert_eq!(
            crate::viewing::read_foundation_mode(&doc, &a.0).unwrap(),
            [u32::MAX - 2]
        );
    }
    #[test]
    fn invalid_definitions_and_references_fail() {
        let repeated = load(
            r#"<viewingGroup id="1"/>"#,
            "<viewingGroup>1</viewingGroup><viewingGroup>1</viewingGroup>",
        )
        .unwrap();
        assert_eq!(repeated.1.layers["layer"].viewing_group_ids, [1, 1]);
        for (defs, refs) in [
            (
                r#"<viewingGroup id="1"/><viewingGroup id="1"/>"#,
                "<viewingGroup>1</viewingGroup>",
            ),
            (r#"<viewingGroup id=""/>"#, "<viewingGroup>1</viewingGroup>"),
            (
                r#"<viewingGroup id="1"/>"#,
                "<viewingGroup>unknown</viewingGroup>",
            ),
        ] {
            assert!(load(defs, refs).is_err());
        }
    }
}
