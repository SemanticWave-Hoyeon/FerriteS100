//! Immutable XML trees in document order (XPath 1.0 data model) and a
//! builder for result trees.
use std::cell::Cell;
use std::fmt;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Root,
    Element,
    Attribute,
    Text,
    Comment,
    ProcessingInstruction,
}

/// Expanded name. `prefix` is kept only for output.
#[derive(Clone, Debug)]
pub struct QName {
    pub namespace: Option<Rc<str>>,
    pub local: Rc<str>,
    pub prefix: Option<Rc<str>>,
}
impl QName {
    pub fn local(local: &str) -> Self {
        Self {
            namespace: None,
            local: local.into(),
            prefix: None,
        }
    }
    /// Same expanded name (namespace URI and local part).
    pub fn same(&self, other: &QName) -> bool {
        self.local == other.local && self.namespace == other.namespace
    }
    pub fn qualified(&self) -> String {
        match &self.prefix {
            Some(p) if !p.is_empty() => format!("{p}:{}", self.local),
            _ => self.local.to_string(),
        }
    }
}
impl PartialEq for QName {
    fn eq(&self, other: &Self) -> bool {
        self.same(other)
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: NodeKind,
    pub name: Option<QName>,
    /// Text, attribute value, comment or processing-instruction data.
    pub value: String,
    pub parent: Option<u32>,
    pub children: Vec<u32>,
    pub attributes: Vec<u32>,
    /// In-scope namespace declarations (prefix, uri) of an element.
    pub namespaces: Vec<(Rc<str>, Rc<str>)>,
}

pub struct Document {
    pub id: u64,
    pub nodes: Vec<Node>,
    pub base: Option<std::path::PathBuf>,
}

thread_local! {
    static NEXT_DOCUMENT: Cell<u64> = const { Cell::new(1) };
}
fn next_document_id() -> u64 {
    NEXT_DOCUMENT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Document#{}({} nodes)", self.id, self.nodes.len())
    }
}

/// A node of a shared document; ordered by document then position.
#[derive(Clone)]
pub struct NodeRef {
    pub doc: Rc<Document>,
    pub index: u32,
}
impl fmt::Debug for NodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Node({}:{})", self.doc.id, self.index)
    }
}
impl PartialEq for NodeRef {
    fn eq(&self, other: &Self) -> bool {
        self.doc.id == other.doc.id && self.index == other.index
    }
}
impl Eq for NodeRef {}
impl PartialOrd for NodeRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NodeRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.doc.id, self.index).cmp(&(other.doc.id, other.index))
    }
}
impl std::hash::Hash for NodeRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.doc.id, self.index).hash(state)
    }
}

impl NodeRef {
    pub fn root_of(doc: Rc<Document>) -> Self {
        Self { doc, index: 0 }
    }
    pub fn node(&self) -> &Node {
        &self.doc.nodes[self.index as usize]
    }
    fn at(&self, index: u32) -> NodeRef {
        NodeRef {
            doc: Rc::clone(&self.doc),
            index,
        }
    }
    pub fn kind(&self) -> &NodeKind {
        &self.node().kind
    }
    pub fn name(&self) -> Option<&QName> {
        self.node().name.as_ref()
    }
    pub fn parent(&self) -> Option<NodeRef> {
        self.node().parent.map(|p| self.at(p))
    }
    pub fn children(&self) -> impl Iterator<Item = NodeRef> + '_ {
        self.node().children.iter().map(|c| self.at(*c))
    }
    pub fn attributes(&self) -> impl Iterator<Item = NodeRef> + '_ {
        self.node().attributes.iter().map(|c| self.at(*c))
    }
    pub fn attribute(&self, local: &str) -> Option<String> {
        self.attributes()
            .find(|a| {
                a.name()
                    .is_some_and(|n| n.namespace.is_none() && &*n.local == local)
            })
            .map(|a| a.node().value.clone())
    }
    pub fn root(&self) -> NodeRef {
        self.at(0)
    }
    /// XPath string-value.
    pub fn string_value(&self) -> String {
        match self.kind() {
            NodeKind::Root | NodeKind::Element => {
                let mut out = String::new();
                self.collect_text(&mut out);
                out
            }
            _ => self.node().value.clone(),
        }
    }
    fn collect_text(&self, out: &mut String) {
        for child in self.children() {
            match child.kind() {
                NodeKind::Text => out.push_str(&child.node().value),
                NodeKind::Element => child.collect_text(out),
                _ => {}
            }
        }
    }
    /// Descendants in document order (excluding self and attributes).
    pub fn descendants(&self) -> Vec<NodeRef> {
        let mut out = Vec::new();
        let mut stack: Vec<u32> = self.node().children.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            out.push(self.at(i));
            stack.extend(self.doc.nodes[i as usize].children.iter().rev());
        }
        out
    }
    pub fn following_siblings(&self) -> Vec<NodeRef> {
        if matches!(self.kind(), NodeKind::Attribute) {
            return Vec::new();
        }
        let Some(parent) = self.parent() else {
            return Vec::new();
        };
        let siblings = &parent.node().children;
        let pos = siblings.iter().position(|c| *c == self.index).unwrap_or(0);
        siblings[pos + 1..].iter().map(|c| self.at(*c)).collect()
    }
    pub fn preceding_siblings(&self) -> Vec<NodeRef> {
        if matches!(self.kind(), NodeKind::Attribute) {
            return Vec::new();
        }
        let Some(parent) = self.parent() else {
            return Vec::new();
        };
        let siblings = &parent.node().children;
        let pos = siblings.iter().position(|c| *c == self.index).unwrap_or(0);
        // Reverse document order (axis order).
        siblings[..pos].iter().rev().map(|c| self.at(*c)).collect()
    }
    pub fn ancestors(&self) -> Vec<NodeRef> {
        let mut out = Vec::new();
        let mut current = self.parent();
        while let Some(node) = current {
            current = node.parent();
            out.push(node);
        }
        out
    }
    /// Resolve a prefix in the in-scope namespaces of this element (or its
    /// nearest element ancestor).
    pub fn lookup_namespace(&self, prefix: &str) -> Option<Rc<str>> {
        let mut current = Some(self.clone());
        while let Some(node) = current {
            if matches!(node.kind(), NodeKind::Element) {
                return node
                    .node()
                    .namespaces
                    .iter()
                    .rev()
                    .find(|(p, _)| &**p == prefix)
                    .map(|(_, uri)| Rc::clone(uri))
                    .or_else(|| {
                        (prefix == "xml").then(|| "http://www.w3.org/XML/1998/namespace".into())
                    });
            }
            current = node.parent();
        }
        None
    }
    pub fn is_element_named(&self, namespace: Option<&str>, local: &str) -> bool {
        matches!(self.kind(), NodeKind::Element)
            && self
                .name()
                .is_some_and(|n| &*n.local == local && n.namespace.as_deref() == namespace)
    }
}

#[derive(Debug)]
pub struct ParseError(pub String);
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse XML (no DTD). `strip_space` removes whitespace-only text nodes
/// (used for stylesheets, except under xsl:text and xml:space="preserve").
pub fn parse(
    text: &str,
    base: Option<std::path::PathBuf>,
    strip_space: Option<&dyn Fn(&roxmltree::Node) -> bool>,
) -> Result<Rc<Document>, ParseError> {
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: 4_000_000,
    };
    let source =
        roxmltree::Document::parse_with_options(text.trim_start_matches('\u{feff}'), options)
            .map_err(|e| ParseError(e.to_string()))?;
    let mut nodes = vec![Node {
        kind: NodeKind::Root,
        name: None,
        value: String::new(),
        parent: None,
        children: Vec::new(),
        attributes: Vec::new(),
        namespaces: Vec::new(),
    }];
    let mut interned = Interner::default();
    fn qname(interned: &mut Interner, n: roxmltree::ExpandedName, prefix: Option<&str>) -> QName {
        QName {
            namespace: n.namespace().map(|u| interned.get(u)),
            local: interned.get(n.name()),
            prefix: prefix.map(|p| interned.get(p)),
        }
    }
    // Iterative pre-order copy keeps document order equal to index order.
    let mut stack: Vec<(roxmltree::Node, u32)> = source
        .root()
        .children()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|c| (c, 0))
        .collect();
    while let Some((node, parent)) = stack.pop() {
        let index = nodes.len() as u32;
        match node.node_type() {
            roxmltree::NodeType::Element => {
                let prefix = node
                    .tag_name()
                    .namespace()
                    .and_then(|uri| node.lookup_prefix(uri))
                    .filter(|p| !p.is_empty());
                let name = qname(&mut interned, node.tag_name(), prefix);
                let namespaces = node
                    .namespaces()
                    .map(|ns| {
                        (
                            interned.get(ns.name().unwrap_or("")),
                            interned.get(ns.uri()),
                        )
                    })
                    .collect();
                nodes.push(Node {
                    kind: NodeKind::Element,
                    name: Some(name),
                    value: String::new(),
                    parent: Some(parent),
                    children: Vec::new(),
                    attributes: Vec::new(),
                    namespaces,
                });
                nodes[parent as usize].children.push(index);
                for attr in node.attributes() {
                    let a = nodes.len() as u32;
                    let prefix = attr
                        .namespace()
                        .and_then(|uri| node.lookup_prefix(uri))
                        .filter(|p| !p.is_empty());
                    let name = QName {
                        namespace: attr.namespace().map(|u| interned.get(u)),
                        local: interned.get(attr.name()),
                        prefix: prefix.map(|p| interned.get(p)),
                    };
                    nodes.push(Node {
                        kind: NodeKind::Attribute,
                        name: Some(name),
                        value: attr.value().to_string(),
                        parent: Some(index),
                        children: Vec::new(),
                        attributes: Vec::new(),
                        namespaces: Vec::new(),
                    });
                    nodes[index as usize].attributes.push(a);
                }
                for child in node.children().collect::<Vec<_>>().into_iter().rev() {
                    stack.push((child, index));
                }
            }
            roxmltree::NodeType::Text => {
                let text = node.text().unwrap_or("");
                if strip_space.is_some_and(|keep| text.trim().is_empty() && !keep(&node)) {
                    continue;
                }
                // Merge with a preceding text sibling (entity/CDATA splits).
                if let Some(&last) = nodes[parent as usize].children.last() {
                    if nodes[last as usize].kind == NodeKind::Text {
                        nodes[last as usize].value.push_str(text);
                        continue;
                    }
                }
                nodes.push(Node {
                    kind: NodeKind::Text,
                    name: None,
                    value: text.to_string(),
                    parent: Some(parent),
                    children: Vec::new(),
                    attributes: Vec::new(),
                    namespaces: Vec::new(),
                });
                nodes[parent as usize].children.push(index);
            }
            roxmltree::NodeType::Comment => {
                nodes.push(Node {
                    kind: NodeKind::Comment,
                    name: None,
                    value: node.text().unwrap_or("").to_string(),
                    parent: Some(parent),
                    children: Vec::new(),
                    attributes: Vec::new(),
                    namespaces: Vec::new(),
                });
                nodes[parent as usize].children.push(index);
            }
            roxmltree::NodeType::PI => {
                let pi = node.pi().expect("processing instruction");
                nodes.push(Node {
                    kind: NodeKind::ProcessingInstruction,
                    name: Some(QName::local(pi.target)),
                    value: pi.value.unwrap_or("").to_string(),
                    parent: Some(parent),
                    children: Vec::new(),
                    attributes: Vec::new(),
                    namespaces: Vec::new(),
                });
                nodes[parent as usize].children.push(index);
            }
            roxmltree::NodeType::Root => {}
        }
    }
    Ok(Rc::new(Document {
        id: next_document_id(),
        nodes,
        base,
    }))
}

#[derive(Default)]
struct Interner(std::collections::HashMap<String, Rc<str>>);
impl Interner {
    fn get(&mut self, s: &str) -> Rc<str> {
        if let Some(v) = self.0.get(s) {
            return Rc::clone(v);
        }
        let v: Rc<str> = s.into();
        self.0.insert(s.to_string(), Rc::clone(&v));
        v
    }
}

/// Incremental construction of a result tree (or result tree fragment).
pub struct Builder {
    nodes: Vec<Node>,
    open: Vec<u32>,
}
impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}
impl Builder {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeKind::Root,
                name: None,
                value: String::new(),
                parent: None,
                children: Vec::new(),
                attributes: Vec::new(),
                namespaces: Vec::new(),
            }],
            open: vec![0],
        }
    }
    fn current(&self) -> u32 {
        *self.open.last().expect("root stays open")
    }
    pub fn start_element(&mut self, name: QName) {
        let parent = self.current();
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind: NodeKind::Element,
            name: Some(name),
            value: String::new(),
            parent: Some(parent),
            children: Vec::new(),
            attributes: Vec::new(),
            namespaces: Vec::new(),
        });
        self.nodes[parent as usize].children.push(index);
        self.open.push(index);
    }
    pub fn end_element(&mut self) {
        if self.open.len() > 1 {
            self.open.pop();
        }
    }
    /// XSLT 1.0 7.1.3: attributes added after children of the element, or to a
    /// non-element, are errors that may be recovered by ignoring them.
    pub fn attribute(&mut self, name: QName, value: String) -> bool {
        let element = self.current();
        let node = &self.nodes[element as usize];
        if node.kind != NodeKind::Element || !node.children.is_empty() {
            return false;
        }
        if let Some(existing) = node.attributes.iter().copied().find(|a| {
            self.nodes[*a as usize]
                .name
                .as_ref()
                .is_some_and(|n| n.same(&name))
        }) {
            self.nodes[existing as usize].value = value;
            return true;
        }
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind: NodeKind::Attribute,
            name: Some(name),
            value,
            parent: Some(element),
            children: Vec::new(),
            attributes: Vec::new(),
            namespaces: Vec::new(),
        });
        self.nodes[element as usize].attributes.push(index);
        true
    }
    pub fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let parent = self.current();
        if let Some(&last) = self.nodes[parent as usize].children.last() {
            if self.nodes[last as usize].kind == NodeKind::Text {
                self.nodes[last as usize].value.push_str(text);
                return;
            }
        }
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind: NodeKind::Text,
            name: None,
            value: text.to_string(),
            parent: Some(parent),
            children: Vec::new(),
            attributes: Vec::new(),
            namespaces: Vec::new(),
        });
        self.nodes[parent as usize].children.push(index);
    }
    pub fn comment(&mut self, text: &str) {
        self.leaf(NodeKind::Comment, None, text);
    }
    pub fn processing_instruction(&mut self, target: &str, data: &str) {
        self.leaf(
            NodeKind::ProcessingInstruction,
            Some(QName::local(target)),
            data,
        );
    }
    fn leaf(&mut self, kind: NodeKind, name: Option<QName>, value: &str) {
        let parent = self.current();
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind,
            name,
            value: value.to_string(),
            parent: Some(parent),
            children: Vec::new(),
            attributes: Vec::new(),
            namespaces: Vec::new(),
        });
        self.nodes[parent as usize].children.push(index);
    }
    /// Deep copy of a source node (xsl:copy-of).
    pub fn copy_of(&mut self, node: &NodeRef) {
        match node.kind() {
            NodeKind::Root => {
                for child in node.children() {
                    self.copy_of(&child);
                }
            }
            NodeKind::Element => {
                self.start_element(node.name().cloned().expect("element name"));
                for attr in node.attributes() {
                    self.attribute(
                        attr.name().cloned().expect("attribute name"),
                        attr.node().value.clone(),
                    );
                }
                for child in node.children() {
                    self.copy_of(&child);
                }
                self.end_element();
            }
            NodeKind::Attribute => {
                self.attribute(
                    node.name().cloned().expect("attribute name"),
                    node.node().value.clone(),
                );
            }
            NodeKind::Text => self.text(&node.node().value),
            NodeKind::Comment => self.comment(&node.node().value),
            NodeKind::ProcessingInstruction => {
                let target = node.name().map(|n| n.local.to_string()).unwrap_or_default();
                self.processing_instruction(&target, &node.node().value)
            }
        }
    }
    /// The XSLT reordering of documents: index order must be document order,
    /// which holds because attributes are appended directly after their
    /// element is opened and children only after them. Late attributes
    /// (allowed above only while the element has no children) also hold.
    pub fn finish(self) -> Rc<Document> {
        // Renumber in pre-order (element, attributes, children) so that index
        // order is document order even if attributes were replaced.
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![0u32];
        while let Some(i) = stack.pop() {
            order.push(i);
            let node = &self.nodes[i as usize];
            for a in &node.attributes {
                order.push(*a);
            }
            for c in node.children.iter().rev() {
                stack.push(*c);
            }
        }
        let mut remap = vec![u32::MAX; self.nodes.len()];
        for (new, old) in order.iter().enumerate() {
            remap[*old as usize] = new as u32;
        }
        let mut nodes: Vec<Node> = order
            .iter()
            .map(|o| self.nodes[*o as usize].clone())
            .collect();
        for node in &mut nodes {
            node.parent = node.parent.map(|p| remap[p as usize]);
            for c in node.children.iter_mut().chain(node.attributes.iter_mut()) {
                *c = remap[*c as usize];
            }
        }
        Rc::new(Document {
            id: next_document_id(),
            nodes,
            base: None,
        })
    }
}

/// Serialize a tree as XML (for diagnostics and tests).
pub fn serialize(node: &NodeRef) -> String {
    fn escape(s: &str, attr: bool) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' if attr => out.push_str("&quot;"),
                _ => out.push(c),
            }
        }
        out
    }
    fn write(node: &NodeRef, out: &mut String) {
        match node.kind() {
            NodeKind::Root => node.children().for_each(|c| write(&c, out)),
            NodeKind::Element => {
                let name = node.name().map(QName::qualified).unwrap_or_default();
                out.push('<');
                out.push_str(&name);
                for a in node.attributes() {
                    out.push(' ');
                    out.push_str(&a.name().map(QName::qualified).unwrap_or_default());
                    out.push_str("=\"");
                    out.push_str(&escape(&a.node().value, true));
                    out.push('"');
                }
                if node.node().children.is_empty() {
                    out.push_str("/>");
                } else {
                    out.push('>');
                    node.children().for_each(|c| write(&c, out));
                    out.push_str("</");
                    out.push_str(&name);
                    out.push('>');
                }
            }
            NodeKind::Text => out.push_str(&escape(&node.node().value, false)),
            NodeKind::Comment => {
                out.push_str("<!--");
                out.push_str(&node.node().value);
                out.push_str("-->");
            }
            NodeKind::ProcessingInstruction => {
                out.push_str("<?");
                out.push_str(&node.name().map(|n| n.local.to_string()).unwrap_or_default());
                out.push(' ');
                out.push_str(&node.node().value);
                out.push_str("?>");
            }
            NodeKind::Attribute => out.push_str(&escape(&node.node().value, true)),
        }
    }
    let mut out = String::new();
    write(node, &mut out);
    out
}
