//! XSLT 1.0: stylesheet compilation (import/include, templates, patterns)
//! and execution into a result tree.
use crate::dom::{self, Builder, NodeKind, NodeRef, QName};
use crate::eval::{self, Context, Environment, Value};
use crate::xpath::{self, Axis, Expr, NamespaceResolver, NodeTest, PathStart, XPathError};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const XSL: &str = "http://www.w3.org/1999/XSL/Transform";

#[derive(Debug, Clone, PartialEq)]
pub struct XsltError(pub String);
impl fmt::Display for XsltError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for XsltError {}
impl From<XPathError> for XsltError {
    fn from(e: XPathError) -> Self {
        XsltError(e.0)
    }
}
type Result<T> = std::result::Result<T, XsltError>;

/// Execution bounds: a portrayal catalogue is data, not trusted code.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Instructions, template invocations, XPath steps and predicates.
    pub steps: u64,
    /// Nested template/instruction depth.
    pub depth: usize,
    /// Result tree nodes.
    pub output_nodes: usize,
    /// Stylesheet modules (import/include).
    pub modules: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            steps: 200_000_000,
            depth: 2_000,
            output_nodes: 50_000_000,
            modules: 512,
        }
    }
}

/// Attribute value template parts.
#[derive(Debug, Clone)]
enum AvtPart {
    Text(String),
    Expr(Expr),
}
#[derive(Debug, Clone)]
struct Avt(Vec<AvtPart>);

#[derive(Debug, Clone)]
struct Sort {
    select: Expr,
    numeric: bool,
    descending: bool,
}

#[derive(Debug, Clone)]
struct WithParam {
    name: QName,
    value: VarValue,
}

#[derive(Debug, Clone)]
enum VarValue {
    Select(Expr),
    Content(Vec<Instr>),
    Empty,
}

#[derive(Debug, Clone)]
enum Instr {
    LiteralElement {
        name: QName,
        attributes: Vec<(QName, Avt)>,
        body: Vec<Instr>,
    },
    Text(String),
    ValueOf(Expr),
    ApplyTemplates {
        select: Option<Expr>,
        mode: Option<QName>,
        sorts: Vec<Sort>,
        params: Vec<WithParam>,
    },
    ApplyImports,
    CallTemplate {
        name: QName,
        params: Vec<WithParam>,
    },
    ForEach {
        select: Expr,
        sorts: Vec<Sort>,
        body: Vec<Instr>,
    },
    If {
        test: Expr,
        body: Vec<Instr>,
    },
    Choose {
        whens: Vec<(Expr, Vec<Instr>)>,
        otherwise: Option<Vec<Instr>>,
    },
    Element {
        name: Avt,
        namespace: Option<Avt>,
        resolver: Rc<Scope>,
        body: Vec<Instr>,
    },
    Attribute {
        name: Avt,
        namespace: Option<Avt>,
        resolver: Rc<Scope>,
        body: Vec<Instr>,
    },
    Comment(Vec<Instr>),
    ProcessingInstruction {
        name: Avt,
        body: Vec<Instr>,
    },
    Copy(Vec<Instr>),
    CopyOf(Expr),
    Variable {
        name: QName,
        value: VarValue,
    },
    Message {
        terminate: bool,
        body: Vec<Instr>,
    },
}

/// In-scope namespaces of a stylesheet element.
#[derive(Debug)]
pub struct Scope(Vec<(Rc<str>, Rc<str>)>);
impl NamespaceResolver for Scope {
    fn resolve(&self, prefix: &str) -> Option<Rc<str>> {
        self.0
            .iter()
            .rev()
            .find(|(p, _)| &**p == prefix)
            .map(|(_, u)| Rc::clone(u))
            .or_else(|| (prefix == "xml").then(|| "http://www.w3.org/XML/1998/namespace".into()))
    }
}
fn scope_of(node: &NodeRef) -> Scope {
    // Namespaces declared on the element and its ancestors (outermost first).
    let mut chain = vec![node.clone()];
    chain.extend(node.ancestors());
    let mut v = Vec::new();
    for n in chain.iter().rev() {
        v.extend(n.node().namespaces.iter().cloned());
    }
    Scope(v)
}

#[derive(Debug)]
struct Template {
    pattern: Option<Pattern>,
    name: Option<QName>,
    mode: Option<QName>,
    priority: f64,
    precedence: usize,
    /// Position in the whole stylesheet (later wins ties).
    order: usize,
    params: Vec<WithParam>,
    body: Vec<Instr>,
}

#[derive(Debug, Clone)]
struct Pattern {
    /// Alternatives of a union pattern.
    alternatives: Vec<Expr>,
}

#[derive(Debug, Clone)]
pub struct DecimalFormat {
    decimal_separator: char,
    grouping_separator: char,
    infinity: String,
    minus_sign: char,
    nan: String,
    percent: char,
    per_mille: char,
    zero_digit: char,
    digit: char,
    pattern_separator: char,
}
impl Default for DecimalFormat {
    fn default() -> Self {
        Self {
            decimal_separator: '.',
            grouping_separator: ',',
            infinity: "Infinity".into(),
            minus_sign: '-',
            nan: "NaN".into(),
            percent: '%',
            per_mille: '\u{2030}',
            zero_digit: '0',
            digit: '#',
            pattern_separator: ';',
        }
    }
}

pub struct Stylesheet {
    templates: Vec<Template>,
    globals: Vec<(QName, VarValue, bool)>,
    decimal_formats: HashMap<Option<String>, DecimalFormat>,
    strip_space: Vec<NodeTest>,
    preserve_space: Vec<NodeTest>,
    limits: Limits,
}

impl fmt::Debug for Stylesheet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Stylesheet({} templates)", self.templates.len())
    }
}

struct Loader {
    templates: Vec<Template>,
    globals: Vec<(QName, VarValue, bool)>,
    decimal_formats: HashMap<Option<String>, DecimalFormat>,
    strip_space: Vec<NodeTest>,
    preserve_space: Vec<NodeTest>,
    precedence: usize,
    order: usize,
    root: PathBuf,
    loading: Vec<PathBuf>,
    modules: usize,
    limits: Limits,
}

fn is_xsl(node: &NodeRef, local: &str) -> bool {
    node.is_element_named(Some(XSL), local)
}

fn keep_whitespace(node: &roxmltree::Node) -> bool {
    // Whitespace-only text is stripped from stylesheets except in xsl:text
    // and under xml:space="preserve".
    let parent = node.parent();
    if parent
        .is_some_and(|p| p.tag_name().namespace() == Some(XSL) && p.tag_name().name() == "text")
    {
        return true;
    }
    let mut current = node.parent();
    while let Some(n) = current {
        if let Some(v) = n.attribute(("http://www.w3.org/XML/1998/namespace", "space")) {
            return v == "preserve";
        }
        current = n.parent();
    }
    false
}

impl Stylesheet {
    /// Load a stylesheet. Imported and included files must lie within `root`.
    pub fn load(path: &Path, root: &Path, limits: Limits) -> Result<Self> {
        let root = root
            .canonicalize()
            .map_err(|e| XsltError(format!("Rule directory {}: {e}", root.display())))?;
        let mut loader = Loader {
            templates: Vec::new(),
            globals: Vec::new(),
            decimal_formats: HashMap::new(),
            strip_space: Vec::new(),
            preserve_space: Vec::new(),
            precedence: 0,
            order: 0,
            root,
            loading: Vec::new(),
            modules: 0,
            limits,
        };
        loader.load_module(path, None)?;
        loader.decimal_formats.entry(None).or_default();
        Ok(Stylesheet {
            templates: loader.templates,
            globals: loader.globals,
            decimal_formats: loader.decimal_formats,
            strip_space: loader.strip_space,
            preserve_space: loader.preserve_space,
            limits,
        })
    }

    /// Compile a stylesheet from text (imports resolved relative to `root`).
    pub fn from_str(text: &str, root: &Path, limits: Limits) -> Result<Self> {
        let dir = std::env::temp_dir();
        let _ = dir;
        let mut loader = Loader {
            templates: Vec::new(),
            globals: Vec::new(),
            decimal_formats: HashMap::new(),
            strip_space: Vec::new(),
            preserve_space: Vec::new(),
            precedence: 0,
            order: 0,
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            loading: Vec::new(),
            modules: 0,
            limits,
        };
        let doc = dom::parse(text, Some(root.join("inline.xsl")), Some(&keep_whitespace))
            .map_err(|e| XsltError(format!("Stylesheet: {e}")))?;
        loader.compile_document(doc, None)?;
        loader.decimal_formats.entry(None).or_default();
        Ok(Stylesheet {
            templates: loader.templates,
            globals: loader.globals,
            decimal_formats: loader.decimal_formats,
            strip_space: loader.strip_space,
            preserve_space: loader.preserve_space,
            limits,
        })
    }
}

impl Loader {
    fn resolve_href(&self, base: &Path, href: &str) -> Result<PathBuf> {
        let dir = base.parent().unwrap_or(Path::new("."));
        let path = dir.join(href.replace('\\', "/"));
        let canonical = path
            .canonicalize()
            .map_err(|e| XsltError(format!("Cannot open {}: {e}", path.display())))?;
        if !canonical.starts_with(&self.root) {
            return Err(XsltError(format!(
                "Stylesheet module {} is outside the rule directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Imports first (lower precedence), then this module's declarations.
    /// `precedence`: Some(p) when included into a module of precedence p.
    fn load_module(&mut self, path: &Path, include_into: Option<usize>) -> Result<()> {
        self.modules += 1;
        if self.modules > self.limits.modules {
            return Err(XsltError("Too many stylesheet modules".into()));
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| XsltError(format!("Cannot open {}: {e}", path.display())))?;
        if self.loading.contains(&canonical) {
            return Err(XsltError(format!(
                "Recursive import/include of {}",
                canonical.display()
            )));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|e| XsltError(format!("Cannot read {}: {e}", canonical.display())))?;
        let text = decode_xml_bytes(&bytes);
        let doc = dom::parse(&text, Some(canonical.clone()), Some(&keep_whitespace))
            .map_err(|e| XsltError(format!("{}: {e}", canonical.display())))?;
        self.loading.push(canonical);
        let result = self.compile_document(doc, include_into);
        self.loading.pop();
        result
    }

    fn compile_document(
        &mut self,
        doc: Rc<dom::Document>,
        include_into: Option<usize>,
    ) -> Result<()> {
        let base = doc.base.clone().unwrap_or_default();
        let root = NodeRef::root_of(Rc::clone(&doc));
        let sheet = root
            .children()
            .find(|c| matches!(c.kind(), NodeKind::Element))
            .ok_or_else(|| XsltError("Empty stylesheet".into()))?;
        if !(is_xsl(&sheet, "stylesheet") || is_xsl(&sheet, "transform")) {
            return Err(XsltError(
                "Simplified (literal result element) stylesheets are not supported".into(),
            ));
        }
        // Imports: each imported module gets lower precedence than this one.
        for child in sheet.children().filter(|c| is_xsl(c, "import")) {
            let href = child
                .attribute("href")
                .ok_or_else(|| XsltError("xsl:import without href".into()))?;
            let path = self.resolve_href(&base, &href)?;
            self.load_module(&path, None)?;
        }
        // An included module's imports are imports of the including module.
        let precedence = match include_into {
            Some(p) => p,
            None => {
                // Precedence is assigned after this module's imports and includes'
                // imports are loaded; compute includes' imports now.
                self.load_include_imports(&sheet, &base)?;
                self.precedence += 1;
                self.precedence
            }
        };
        for child in sheet.children() {
            if !matches!(child.kind(), NodeKind::Element) {
                continue;
            }
            let ns = child.name().and_then(|n| n.namespace.clone());
            if ns.as_deref() != Some(XSL) {
                continue; // top-level user data elements are ignored
            }
            let local = child
                .name()
                .map(|n| n.local.to_string())
                .unwrap_or_default();
            match local.as_str() {
                "import" => {}
                "include" => {
                    let href = child
                        .attribute("href")
                        .ok_or_else(|| XsltError("xsl:include without href".into()))?;
                    let path = self.resolve_href(&base, &href)?;
                    self.load_module_body(&path, precedence)?;
                }
                "template" => self.compile_template(&child, precedence)?,
                "variable" | "param" => {
                    let scope = scope_of(&child);
                    let name = qname_attr(&child, "name", &scope)?;
                    let value = self.var_value(&child)?;
                    self.globals.push((name, value, local == "param"));
                }
                "decimal-format" => {
                    let mut f = DecimalFormat::default();
                    let ch = |n: &str, d: char| {
                        child
                            .attribute(n)
                            .and_then(|v| v.chars().next())
                            .unwrap_or(d)
                    };
                    f.decimal_separator = ch("decimal-separator", '.');
                    f.grouping_separator = ch("grouping-separator", ',');
                    f.minus_sign = ch("minus-sign", '-');
                    f.percent = ch("percent", '%');
                    f.per_mille = ch("per-mille", '\u{2030}');
                    f.zero_digit = ch("zero-digit", '0');
                    f.digit = ch("digit", '#');
                    f.pattern_separator = ch("pattern-separator", ';');
                    if let Some(v) = child.attribute("infinity") {
                        f.infinity = v;
                    }
                    if let Some(v) = child.attribute("NaN") {
                        f.nan = v;
                    }
                    self.decimal_formats.insert(child.attribute("name"), f);
                }
                "strip-space" | "preserve-space" => {
                    let scope = scope_of(&child);
                    let tests = child
                        .attribute("elements")
                        .unwrap_or_default()
                        .split_whitespace()
                        .map(|t| name_test(t, &scope))
                        .collect::<Result<Vec<_>>>()?;
                    if local == "strip-space" {
                        self.strip_space.extend(tests);
                    } else {
                        self.preserve_space.extend(tests);
                    }
                }
                // Output method, keys, attribute sets and aliases do not affect
                // the result tree consumed by portrayal.
                "output" | "key" | "attribute-set" | "namespace-alias" => {}
                other => {
                    return Err(XsltError(format!("Unsupported top-level xsl:{other}")));
                }
            }
        }
        Ok(())
    }

    fn load_include_imports(&mut self, sheet: &NodeRef, base: &Path) -> Result<()> {
        for child in sheet.children().filter(|c| is_xsl(c, "include")) {
            let href = child
                .attribute("href")
                .ok_or_else(|| XsltError("xsl:include without href".into()))?;
            let path = self.resolve_href(base, &href)?;
            let bytes = std::fs::read(&path)
                .map_err(|e| XsltError(format!("Cannot read {}: {e}", path.display())))?;
            let doc = dom::parse(
                &decode_xml_bytes(&bytes),
                Some(path.clone()),
                Some(&keep_whitespace),
            )
            .map_err(|e| XsltError(format!("{}: {e}", path.display())))?;
            let inc = NodeRef::root_of(doc);
            let found = inc
                .children()
                .find(|c| matches!(c.kind(), NodeKind::Element));
            if let Some(inc_sheet) = found {
                for imp in inc_sheet.children().filter(|c| is_xsl(c, "import")) {
                    let href = imp
                        .attribute("href")
                        .ok_or_else(|| XsltError("xsl:import without href".into()))?;
                    let ipath = self.resolve_href(&path, &href)?;
                    self.load_module(&ipath, None)?;
                }
                self.load_include_imports(&inc_sheet, &path)?;
            }
        }
        Ok(())
    }

    /// Declarations of an included module at the including precedence (its
    /// imports were already loaded by `load_include_imports`).
    fn load_module_body(&mut self, path: &Path, precedence: usize) -> Result<()> {
        self.modules += 1;
        if self.modules > self.limits.modules {
            return Err(XsltError("Too many stylesheet modules".into()));
        }
        if self.loading.iter().any(|p| p == path) {
            return Err(XsltError(format!(
                "Recursive include of {}",
                path.display()
            )));
        }
        let bytes = std::fs::read(path)
            .map_err(|e| XsltError(format!("Cannot read {}: {e}", path.display())))?;
        let doc = dom::parse(
            &decode_xml_bytes(&bytes),
            Some(path.to_path_buf()),
            Some(&keep_whitespace),
        )
        .map_err(|e| XsltError(format!("{}: {e}", path.display())))?;
        self.loading.push(path.to_path_buf());
        // Skip its imports here: compile_document with include precedence.
        let root = NodeRef::root_of(Rc::clone(&doc));
        let result = (|| {
            let sheet = root
                .children()
                .find(|c| matches!(c.kind(), NodeKind::Element))
                .ok_or_else(|| XsltError("Empty stylesheet".into()))?;
            if !(is_xsl(&sheet, "stylesheet") || is_xsl(&sheet, "transform")) {
                return Err(XsltError("Included module is not a stylesheet".into()));
            }
            // Reuse the declaration pass without re-loading imports.
            self.compile_declarations(&sheet, path, precedence)
        })();
        self.loading.pop();
        result
    }

    fn compile_declarations(
        &mut self,
        sheet: &NodeRef,
        base: &Path,
        precedence: usize,
    ) -> Result<()> {
        for child in sheet.children() {
            if !matches!(child.kind(), NodeKind::Element) {
                continue;
            }
            if child.name().and_then(|n| n.namespace.clone()).as_deref() != Some(XSL) {
                continue;
            }
            let local = child
                .name()
                .map(|n| n.local.to_string())
                .unwrap_or_default();
            match local.as_str() {
                "import" => {}
                "include" => {
                    let href = child
                        .attribute("href")
                        .ok_or_else(|| XsltError("xsl:include without href".into()))?;
                    let path = self.resolve_href(base, &href)?;
                    self.load_module_body(&path, precedence)?;
                }
                "template" => self.compile_template(&child, precedence)?,
                "variable" | "param" => {
                    let scope = scope_of(&child);
                    let name = qname_attr(&child, "name", &scope)?;
                    let value = self.var_value(&child)?;
                    self.globals.push((name, value, local == "param"));
                }
                "decimal-format" => {
                    let mut f = DecimalFormat::default();
                    let ch = |n: &str, d: char| {
                        child
                            .attribute(n)
                            .and_then(|v| v.chars().next())
                            .unwrap_or(d)
                    };
                    f.decimal_separator = ch("decimal-separator", '.');
                    f.grouping_separator = ch("grouping-separator", ',');
                    f.minus_sign = ch("minus-sign", '-');
                    f.percent = ch("percent", '%');
                    f.per_mille = ch("per-mille", '\u{2030}');
                    f.zero_digit = ch("zero-digit", '0');
                    f.digit = ch("digit", '#');
                    f.pattern_separator = ch("pattern-separator", ';');
                    if let Some(v) = child.attribute("infinity") {
                        f.infinity = v;
                    }
                    if let Some(v) = child.attribute("NaN") {
                        f.nan = v;
                    }
                    self.decimal_formats.insert(child.attribute("name"), f);
                }
                "strip-space" | "preserve-space" => {
                    let scope = scope_of(&child);
                    let tests = child
                        .attribute("elements")
                        .unwrap_or_default()
                        .split_whitespace()
                        .map(|t| name_test(t, &scope))
                        .collect::<Result<Vec<_>>>()?;
                    if local == "strip-space" {
                        self.strip_space.extend(tests);
                    } else {
                        self.preserve_space.extend(tests);
                    }
                }
                "output" | "key" | "attribute-set" | "namespace-alias" => {}
                other => return Err(XsltError(format!("Unsupported top-level xsl:{other}"))),
            }
        }
        Ok(())
    }

    fn compile_template(&mut self, node: &NodeRef, precedence: usize) -> Result<()> {
        let scope = scope_of(node);
        let name = match node.attribute("name") {
            Some(n) => Some(parse_qname(&n, &scope)?),
            None => None,
        };
        let mode = match node.attribute("mode") {
            Some(m) => Some(parse_qname(&m, &scope)?),
            None => None,
        };
        let explicit = match node.attribute("priority") {
            Some(p) => Some(
                p.trim()
                    .parse::<f64>()
                    .map_err(|_| XsltError(format!("Bad template priority '{p}'")))?,
            ),
            None => None,
        };
        let mut params = Vec::new();
        let mut body_nodes = Vec::new();
        for child in node.children() {
            // Comments and processing instructions do not end the parameter list.
            if matches!(
                child.kind(),
                NodeKind::Comment | NodeKind::ProcessingInstruction
            ) {
                continue;
            }
            if is_xsl(&child, "param") && body_nodes.is_empty() {
                let pscope = scope_of(&child);
                params.push(WithParam {
                    name: qname_attr(&child, "name", &pscope)?,
                    value: self.var_value(&child)?,
                });
            } else {
                body_nodes.push(child);
            }
        }
        let body = self.compile_sequence(body_nodes)?;
        if let Some(m) = node.attribute("match") {
            let pattern = compile_pattern(&m, &scope)?;
            // A union pattern is equivalent to one template per alternative,
            // each with its own default priority.
            for alt in &pattern.alternatives {
                self.order += 1;
                self.templates.push(Template {
                    pattern: Some(Pattern {
                        alternatives: vec![alt.clone()],
                    }),
                    name: name.clone(),
                    mode: mode.clone(),
                    priority: explicit.unwrap_or_else(|| default_priority(alt)),
                    precedence,
                    order: self.order,
                    params: params.clone(),
                    body: body.clone(),
                });
            }
        } else if name.is_some() {
            self.order += 1;
            self.templates.push(Template {
                pattern: None,
                name,
                mode,
                priority: 0.,
                precedence,
                order: self.order,
                params,
                body,
            });
        } else {
            return Err(XsltError("xsl:template without match or name".into()));
        }
        Ok(())
    }

    fn var_value(&mut self, node: &NodeRef) -> Result<VarValue> {
        let scope = scope_of(node);
        if let Some(select) = node.attribute("select") {
            return Ok(VarValue::Select(xpath::parse(&select, &scope)?));
        }
        let children: Vec<NodeRef> = node.children().collect();
        if children.is_empty() {
            return Ok(VarValue::Empty);
        }
        Ok(VarValue::Content(self.compile_sequence(children)?))
    }

    fn compile_sequence(&mut self, nodes: Vec<NodeRef>) -> Result<Vec<Instr>> {
        let mut out = Vec::new();
        for node in nodes {
            match node.kind() {
                NodeKind::Text => out.push(Instr::Text(node.node().value.clone())),
                NodeKind::Element => {
                    if let Some(i) = self.compile_instruction(&node)? {
                        out.push(i);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn compile_avt(&self, text: &str, scope: &Scope) -> Result<Avt> {
        let mut parts = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let mut literal = String::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' if chars.get(i + 1) == Some(&'{') => {
                    literal.push('{');
                    i += 2;
                }
                '}' if chars.get(i + 1) == Some(&'}') => {
                    literal.push('}');
                    i += 2;
                }
                '{' => {
                    let start = i + 1;
                    let mut j = start;
                    let mut quote = None;
                    while j < chars.len() {
                        match (quote, chars[j]) {
                            (None, '\'' | '"') => quote = Some(chars[j]),
                            (Some(q), c) if c == q => quote = None,
                            (None, '}') => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    if j >= chars.len() {
                        return Err(XsltError(format!(
                            "Unterminated '{{' in attribute value template '{text}'"
                        )));
                    }
                    if !literal.is_empty() {
                        parts.push(AvtPart::Text(std::mem::take(&mut literal)));
                    }
                    let expr: String = chars[start..j].iter().collect();
                    parts.push(AvtPart::Expr(xpath::parse(&expr, scope)?));
                    i = j + 1;
                }
                '}' => {
                    return Err(XsltError(format!(
                        "Unmatched '}}' in attribute value template '{text}'"
                    )))
                }
                c => {
                    literal.push(c);
                    i += 1;
                }
            }
        }
        if !literal.is_empty() {
            parts.push(AvtPart::Text(literal));
        }
        Ok(Avt(parts))
    }

    fn sorts_and_params(&mut self, node: &NodeRef) -> Result<(Vec<Sort>, Vec<WithParam>)> {
        let mut sorts = Vec::new();
        let mut params = Vec::new();
        for child in node.children() {
            if is_xsl(&child, "sort") {
                let scope = scope_of(&child);
                sorts.push(Sort {
                    select: xpath::parse(
                        &child.attribute("select").unwrap_or_else(|| ".".into()),
                        &scope,
                    )?,
                    numeric: child.attribute("data-type").as_deref() == Some("number"),
                    descending: child.attribute("order").as_deref() == Some("descending"),
                });
            } else if is_xsl(&child, "with-param") {
                let scope = scope_of(&child);
                params.push(WithParam {
                    name: qname_attr(&child, "name", &scope)?,
                    value: self.var_value(&child)?,
                });
            }
        }
        Ok((sorts, params))
    }

    fn compile_instruction(&mut self, node: &NodeRef) -> Result<Option<Instr>> {
        let scope = scope_of(node);
        let name = node.name().cloned().expect("element name");
        if name.namespace.as_deref() != Some(XSL) {
            // Literal result element: attributes are AVTs; xsl:* attributes and
            // namespace declarations are not copied.
            let mut attributes = Vec::new();
            for a in node.attributes() {
                let an = a.name().cloned().expect("attribute name");
                if an.namespace.as_deref() == Some(XSL) {
                    continue;
                }
                attributes.push((an, self.compile_avt(&a.node().value, &scope)?));
            }
            let body = self.compile_sequence(node.children().collect())?;
            return Ok(Some(Instr::LiteralElement {
                name,
                attributes,
                body,
            }));
        }
        let req = |n: &str| {
            node.attribute(n)
                .ok_or_else(|| XsltError(format!("xsl:{} requires @{n}", name.local)))
        };
        let children = |s: &mut Self| {
            s.compile_sequence(
                node.children()
                    .filter(|c| !is_xsl(c, "sort") && !is_xsl(c, "with-param"))
                    .collect(),
            )
        };
        Ok(Some(match &*name.local {
            "text" => Instr::Text(node.string_value()),
            "value-of" => Instr::ValueOf(xpath::parse(&req("select")?, &scope)?),
            "apply-templates" => {
                let select = match node.attribute("select") {
                    Some(s) => Some(xpath::parse(&s, &scope)?),
                    None => None,
                };
                let mode = match node.attribute("mode") {
                    Some(m) => Some(parse_qname(&m, &scope)?),
                    None => None,
                };
                let (sorts, params) = self.sorts_and_params(node)?;
                Instr::ApplyTemplates {
                    select,
                    mode,
                    sorts,
                    params,
                }
            }
            "apply-imports" => Instr::ApplyImports,
            "call-template" => {
                let (_, params) = self.sorts_and_params(node)?;
                Instr::CallTemplate {
                    name: parse_qname(&req("name")?, &scope)?,
                    params,
                }
            }
            "for-each" => {
                let (sorts, _) = self.sorts_and_params(node)?;
                Instr::ForEach {
                    select: xpath::parse(&req("select")?, &scope)?,
                    sorts,
                    body: children(self)?,
                }
            }
            "if" => Instr::If {
                test: xpath::parse(&req("test")?, &scope)?,
                body: children(self)?,
            },
            "choose" => {
                let mut whens = Vec::new();
                let mut otherwise = None;
                for c in node.children() {
                    if is_xsl(&c, "when") {
                        let cs = scope_of(&c);
                        let test = xpath::parse(
                            &c.attribute("test")
                                .ok_or_else(|| XsltError("xsl:when requires @test".into()))?,
                            &cs,
                        )?;
                        whens.push((test, self.compile_sequence(c.children().collect())?));
                    } else if is_xsl(&c, "otherwise") {
                        otherwise = Some(self.compile_sequence(c.children().collect())?);
                    }
                }
                Instr::Choose { whens, otherwise }
            }
            "element" => Instr::Element {
                name: self.compile_avt(&req("name")?, &scope)?,
                namespace: match node.attribute("namespace") {
                    Some(n) => Some(self.compile_avt(&n, &scope)?),
                    None => None,
                },
                resolver: Rc::new(scope),
                body: children(self)?,
            },
            "attribute" => Instr::Attribute {
                name: self.compile_avt(&req("name")?, &scope)?,
                namespace: match node.attribute("namespace") {
                    Some(n) => Some(self.compile_avt(&n, &scope)?),
                    None => None,
                },
                resolver: Rc::new(scope),
                body: children(self)?,
            },
            "comment" => Instr::Comment(children(self)?),
            "processing-instruction" => Instr::ProcessingInstruction {
                name: self.compile_avt(&req("name")?, &scope)?,
                body: children(self)?,
            },
            "copy" => Instr::Copy(children(self)?),
            "copy-of" => Instr::CopyOf(xpath::parse(&req("select")?, &scope)?),
            "variable" | "param" => Instr::Variable {
                name: qname_attr(node, "name", &scope)?,
                value: self.var_value(node)?,
            },
            "message" => Instr::Message {
                terminate: node.attribute("terminate").as_deref() == Some("yes"),
                body: children(self)?,
            },
            "fallback" => return Ok(None),
            "number" => {
                return Err(XsltError(
                    "xsl:number is not supported by this processor".into(),
                ))
            }
            other => return Err(XsltError(format!("Unknown instruction xsl:{other}"))),
        }))
    }
}

fn decode_xml_bytes(bytes: &[u8]) -> String {
    // UTF-8 (with or without BOM) or a declared single-byte Latin-1 encoding.
    match std::str::from_utf8(bytes) {
        Ok(s) => s.trim_start_matches('\u{feff}').to_string(),
        Err(_) => bytes.iter().map(|b| *b as char).collect(),
    }
    .replacen("encoding=\"ISO-8859-1\"", "encoding=\"UTF-8\"", 1)
    .replacen("encoding=\"iso-8859-1\"", "encoding=\"UTF-8\"", 1)
}

fn parse_qname(text: &str, scope: &Scope) -> Result<QName> {
    let text = text.trim();
    match text.split_once(':') {
        Some((p, l)) => Ok(QName {
            namespace: Some(
                scope
                    .resolve(p)
                    .ok_or_else(|| XsltError(format!("Undeclared prefix '{p}' in '{text}'")))?,
            ),
            local: l.into(),
            prefix: Some(p.into()),
        }),
        None => Ok(QName::local(text)),
    }
}
fn qname_attr(node: &NodeRef, attr: &str, scope: &Scope) -> Result<QName> {
    let v = node
        .attribute(attr)
        .ok_or_else(|| XsltError(format!("Missing @{attr}")))?;
    parse_qname(&v, scope)
}

fn name_test(text: &str, scope: &Scope) -> Result<NodeTest> {
    Ok(if text == "*" {
        NodeTest::AnyName
    } else if let Some(p) = text.strip_suffix(":*") {
        NodeTest::NamespaceAny(
            scope
                .resolve(p)
                .ok_or_else(|| XsltError(format!("Undeclared prefix '{p}'")))?,
        )
    } else {
        let q = parse_qname(text, scope)?;
        NodeTest::Name {
            namespace: q.namespace,
            local: q.local,
        }
    })
}

fn compile_pattern(text: &str, scope: &Scope) -> Result<Pattern> {
    let expr = xpath::parse(text, scope)?;
    let mut alternatives = Vec::new();
    fn split(e: Expr, out: &mut Vec<Expr>) {
        match e {
            Expr::Binary(xpath::BinaryOp::Union, a, b) => {
                split(*a, out);
                split(*b, out);
            }
            other => out.push(other),
        }
    }
    split(expr, &mut alternatives);
    for alt in &alternatives {
        let ok = match alt {
            Expr::Path(PathStart::Root | PathStart::Context, steps) => steps.iter().all(|s| {
                matches!(
                    s.axis,
                    Axis::Child | Axis::Attribute | Axis::DescendantOrSelf
                )
            }),
            Expr::Path(PathStart::Filter(f), _) | Expr::Filter(f, _) => {
                matches!(&**f, Expr::Function { local, .. } if &**local == "id" || &**local == "key")
            }
            Expr::Function { local, .. } => &**local == "id" || &**local == "key",
            _ => false,
        };
        if !ok {
            return Err(XsltError(format!("Invalid pattern '{text}'")));
        }
    }
    Ok(Pattern { alternatives })
}

/// XSLT 1.0 5.5 default priority of one pattern alternative.
fn default_priority(alt: &Expr) -> f64 {
    if let Expr::Path(PathStart::Context, steps) = alt {
        if steps.len() == 1 && steps[0].predicates.is_empty() {
            return match &steps[0].test {
                NodeTest::Name { .. } => 0.,
                NodeTest::ProcessingInstruction(Some(_)) => 0.,
                NodeTest::NamespaceAny(_) => -0.25,
                _ => -0.5,
            };
        }
    }
    0.5
}

/// The external result of a transformation.
pub struct Output {
    pub root: NodeRef,
    pub messages: Vec<String>,
}

struct Frame {
    variables: Vec<(QName, Value)>,
}

struct Exec<'s> {
    sheet: &'s Stylesheet,
    globals: RefCell<HashMap<(Option<Rc<str>>, Rc<str>), Value>>,
    builders: RefCell<Vec<Builder>>,
    frames: RefCell<Vec<Frame>>,
    current: RefCell<Vec<NodeRef>>,
    steps: Cell<u64>,
    depth: Cell<usize>,
    output_nodes: Cell<usize>,
    messages: RefCell<Vec<String>>,
}

impl Environment for Exec<'_> {
    fn variable(&self, namespace: Option<&str>, local: &str) -> Option<Value> {
        let frames = self.frames.borrow();
        if let Some(frame) = frames.last() {
            if let Some((_, v)) = frame
                .variables
                .iter()
                .rev()
                .find(|(q, _)| &*q.local == local && q.namespace.as_deref() == namespace)
            {
                return Some(v.clone());
            }
        }
        self.globals
            .borrow()
            .get(&(namespace.map(Rc::from), Rc::from(local)))
            .cloned()
    }

    fn function(
        &self,
        namespace: Option<&str>,
        local: &str,
        args: &[Value],
        ctx: &Context,
    ) -> Option<std::result::Result<Value, XPathError>> {
        if namespace.is_some() {
            return None;
        }
        Some(match local {
            "current" => Ok(Value::Nodes(
                self.current.borrow().last().cloned().into_iter().collect(),
            )),
            "format-number" => {
                if args.len() < 2 || args.len() > 3 {
                    return Some(Err(XPathError(
                        "format-number() takes 2 or 3 arguments".into(),
                    )));
                }
                let name = args.get(2).map(Value::to_string_value);
                match self.sheet.decimal_formats.get(&name) {
                    Some(f) => format_number(args[0].to_number(), &args[1].to_string_value(), f)
                        .map(Value::string),
                    None => Err(XPathError(format!(
                        "Unknown decimal-format '{}'",
                        name.unwrap_or_default()
                    ))),
                }
            }
            "generate-id" => {
                let node = match args.first() {
                    Some(Value::Nodes(n)) => n.first().cloned(),
                    Some(_) => {
                        return Some(Err(XPathError("generate-id() requires a node-set".into())))
                    }
                    None => Some(ctx.node.clone()),
                };
                Ok(Value::string(
                    node.map(|n| format!("d{}n{}", n.doc.id, n.index))
                        .unwrap_or_default(),
                ))
            }
            "system-property" => {
                let p = args.first().map(Value::to_string_value).unwrap_or_default();
                Ok(Value::string(match p.as_str() {
                    "xsl:version" => "1.0",
                    "xsl:vendor" => "FerriteS100",
                    "xsl:vendor-url" => "",
                    _ => "",
                }))
            }
            "element-available" => {
                let n = args.first().map(Value::to_string_value).unwrap_or_default();
                Ok(Value::Boolean(matches!(
                    n.strip_prefix("xsl:").unwrap_or(""),
                    "apply-templates"
                        | "call-template"
                        | "choose"
                        | "if"
                        | "for-each"
                        | "value-of"
                        | "text"
                        | "element"
                        | "attribute"
                        | "variable"
                        | "param"
                        | "copy"
                        | "copy-of"
                        | "comment"
                        | "processing-instruction"
                        | "message"
                )))
            }
            "function-available" => {
                let n = args.first().map(Value::to_string_value).unwrap_or_default();
                Ok(Value::Boolean(matches!(
                    n.as_str(),
                    "last"
                        | "position"
                        | "count"
                        | "local-name"
                        | "namespace-uri"
                        | "name"
                        | "string"
                        | "concat"
                        | "starts-with"
                        | "contains"
                        | "substring-before"
                        | "substring-after"
                        | "substring"
                        | "string-length"
                        | "normalize-space"
                        | "translate"
                        | "boolean"
                        | "not"
                        | "true"
                        | "false"
                        | "lang"
                        | "number"
                        | "sum"
                        | "floor"
                        | "ceiling"
                        | "round"
                        | "current"
                        | "format-number"
                        | "generate-id"
                        | "system-property"
                        | "element-available"
                        | "function-available"
                )))
            }
            "document" | "key" | "unparsed-entity-uri" => Err(XPathError(format!(
                "{local}() is not available to portrayal rules"
            ))),
            _ => return None,
        })
    }

    fn charge(&self) -> std::result::Result<(), XPathError> {
        let n = self.steps.get() + 1;
        self.steps.set(n);
        if n > self.sheet.limits.steps {
            Err(XPathError(
                "Portrayal rule execution budget exceeded".into(),
            ))
        } else {
            Ok(())
        }
    }
}

impl Stylesheet {
    /// Transform `source` with top-level parameters given as strings.
    pub fn transform(&self, source: Rc<dom::Document>, params: &[(&str, &str)]) -> Result<Output> {
        let source = self.strip_source(source);
        let root = NodeRef::root_of(source);
        let exec = Exec {
            sheet: self,
            globals: RefCell::new(HashMap::new()),
            builders: RefCell::new(vec![Builder::new()]),
            frames: RefCell::new(vec![Frame {
                variables: Vec::new(),
            }]),
            current: RefCell::new(vec![root.clone()]),
            steps: Cell::new(0),
            depth: Cell::new(0),
            output_nodes: Cell::new(0),
            messages: RefCell::new(Vec::new()),
        };
        // Globals in declaration order; external parameters override params.
        for (name, value, is_param) in &self.globals {
            let external = if *is_param && name.namespace.is_none() {
                params
                    .iter()
                    .find(|(n, _)| *n == &*name.local)
                    .map(|(_, v)| *v)
            } else {
                None
            };
            let v = match external {
                Some(v) => Value::string(v),
                None => exec.var_value(value, &root, 1, 1)?,
            };
            exec.globals
                .borrow_mut()
                .insert((name.namespace.clone(), Rc::clone(&name.local)), v);
        }
        exec.apply(&[root.clone()], None, &[], &root, None)?;
        let builder = exec.builders.borrow_mut().pop().expect("result builder");
        Ok(Output {
            root: NodeRef::root_of(builder.finish()),
            messages: exec.messages.into_inner(),
        })
    }

    fn strip_source(&self, source: Rc<dom::Document>) -> Rc<dom::Document> {
        if self.strip_space.is_empty() {
            return source;
        }
        // Rebuild without whitespace-only text children of stripped elements.
        let root = NodeRef::root_of(Rc::clone(&source));
        let mut b = Builder::new();
        fn matches(tests: &[NodeTest], node: &NodeRef) -> bool {
            tests.iter().any(|t| match t {
                NodeTest::AnyName => true,
                NodeTest::NamespaceAny(ns) => node
                    .name()
                    .is_some_and(|n| n.namespace.as_deref() == Some(ns)),
                NodeTest::Name { namespace, local } => node.name().is_some_and(|n| {
                    n.local == *local && n.namespace.as_deref() == namespace.as_deref()
                }),
                _ => false,
            })
        }
        fn copy(sheet: &Stylesheet, b: &mut Builder, node: &NodeRef) {
            match node.kind() {
                NodeKind::Root => node.children().for_each(|c| copy(sheet, b, &c)),
                NodeKind::Element => {
                    b.start_element(node.name().cloned().expect("name"));
                    for a in node.attributes() {
                        b.attribute(a.name().cloned().expect("name"), a.node().value.clone());
                    }
                    let strip =
                        matches(&sheet.strip_space, node) && !matches(&sheet.preserve_space, node);
                    for c in node.children() {
                        if strip
                            && matches!(c.kind(), NodeKind::Text)
                            && c.node().value.trim().is_empty()
                        {
                            continue;
                        }
                        copy(sheet, b, &c);
                    }
                    b.end_element();
                }
                _ => b.copy_of(node),
            }
        }
        copy(self, &mut b, &root);
        b.finish()
    }
}

impl Exec<'_> {
    fn charge(&self) -> Result<()> {
        Environment::charge(self).map_err(XsltError::from)
    }

    fn eval(&self, expr: &Expr, node: &NodeRef, position: usize, size: usize) -> Result<Value> {
        let ctx = Context {
            node: node.clone(),
            position,
            size,
            env: self,
        };
        Ok(eval::evaluate(expr, &ctx)?)
    }

    fn with_builder<F: FnOnce(&mut Builder)>(&self, f: F) -> Result<()> {
        let n = self.output_nodes.get() + 1;
        self.output_nodes.set(n);
        if n > self.sheet.limits.output_nodes {
            return Err(XsltError("Portrayal result tree too large".into()));
        }
        let mut builders = self.builders.borrow_mut();
        f(builders.last_mut().expect("builder"));
        Ok(())
    }

    /// Evaluate a variable/param value (content gives a result tree fragment).
    fn var_value(
        &self,
        value: &VarValue,
        node: &NodeRef,
        position: usize,
        size: usize,
    ) -> Result<Value> {
        match value {
            VarValue::Select(e) => self.eval(e, node, position, size),
            VarValue::Empty => Ok(Value::string("")),
            VarValue::Content(body) => {
                self.builders.borrow_mut().push(Builder::new());
                let result = self.run(body, node, position, size);
                let builder = self.builders.borrow_mut().pop().expect("fragment builder");
                result?;
                // A result tree fragment behaves as a node-set of its root.
                Ok(Value::Nodes(vec![NodeRef::root_of(builder.finish())]))
            }
        }
    }

    fn find_template(
        &self,
        node: &NodeRef,
        mode: Option<&QName>,
        below: Option<usize>,
    ) -> Result<Option<&Template>> {
        let mut best: Option<&Template> = None;
        for t in &self.sheet.templates {
            let Some(pattern) = &t.pattern else { continue };
            if t.mode.as_ref() != mode {
                continue;
            }
            if below.is_some_and(|p| t.precedence >= p) {
                continue;
            }
            if let Some(b) = best {
                if (t.precedence, t.priority) < (b.precedence, b.priority) {
                    continue;
                }
            }
            if self.matches(pattern, node)? {
                best = match best {
                    None => Some(t),
                    Some(b) => {
                        if (t.precedence, t.priority, t.order) > (b.precedence, b.priority, b.order)
                        {
                            Some(t)
                        } else {
                            Some(b)
                        }
                    }
                };
            }
        }
        Ok(best)
    }

    /// XSLT 1.0 5.2: a node matches if, for some context (the node or an
    /// ancestor), evaluating the pattern selects it.
    fn matches(&self, pattern: &Pattern, node: &NodeRef) -> Result<bool> {
        for alt in &pattern.alternatives {
            if let Expr::Path(_, steps) = alt {
                if let Some(last) = steps.last() {
                    if !quick_test(&last.test, last.axis == Axis::Attribute, node) {
                        continue;
                    }
                }
            }
            let mut contexts = node.ancestors();
            if matches!(node.kind(), NodeKind::Attribute) {
                // The attribute's parent is in ancestors(); nothing else to add.
            }
            if contexts.is_empty() {
                contexts.push(node.clone());
            }
            let absolute = matches!(alt, Expr::Path(PathStart::Root, _));
            let candidates: Vec<NodeRef> = if absolute {
                vec![node.root()]
            } else {
                contexts
            };
            for ctx in candidates {
                if let Value::Nodes(selected) = self.eval(alt, &ctx, 1, 1)? {
                    if selected.binary_search(node).is_ok() {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn sort_nodes(&self, nodes: &mut Vec<NodeRef>, sorts: &[Sort]) -> Result<()> {
        if sorts.is_empty() {
            return Ok(());
        }
        let size = nodes.len();
        let mut keyed = Vec::with_capacity(size);
        for (i, n) in nodes.iter().enumerate() {
            let mut keys = Vec::with_capacity(sorts.len());
            for s in sorts {
                let v = self.eval(&s.select, n, i + 1, size)?;
                keys.push(if s.numeric {
                    SortKey::Number(v.to_number())
                } else {
                    SortKey::Text(v.to_string_value())
                });
            }
            keyed.push((keys, n.clone()));
        }
        keyed.sort_by(|a, b| {
            for (i, s) in sorts.iter().enumerate() {
                let o = a.0[i].cmp(&b.0[i]);
                let o = if s.descending { o.reverse() } else { o };
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        });
        *nodes = keyed.into_iter().map(|(_, n)| n).collect();
        Ok(())
    }

    fn bind_params(
        &self,
        params: &[WithParam],
        node: &NodeRef,
        position: usize,
        size: usize,
    ) -> Result<Vec<(QName, Value)>> {
        params
            .iter()
            .map(|p| {
                Ok((
                    p.name.clone(),
                    self.var_value(&p.value, node, position, size)?,
                ))
            })
            .collect()
    }

    fn invoke(
        &self,
        template: &Template,
        node: &NodeRef,
        position: usize,
        size: usize,
        passed: Vec<(QName, Value)>,
    ) -> Result<()> {
        self.charge()?;
        let depth = self.depth.get() + 1;
        if depth > self.sheet.limits.depth {
            return Err(XsltError("Portrayal rule recursion too deep".into()));
        }
        self.depth.set(depth);
        // New variable frame: only globals and the template's params are visible.
        self.frames.borrow_mut().push(Frame {
            variables: Vec::new(),
        });
        self.current.borrow_mut().push(node.clone());
        let result = (|| {
            for p in &template.params {
                let value = match passed.iter().find(|(n, _)| n.same(&p.name)) {
                    Some((_, v)) => v.clone(),
                    None => self.var_value(&p.value, node, position, size)?,
                };
                self.frames
                    .borrow_mut()
                    .last_mut()
                    .expect("frame")
                    .variables
                    .push((p.name.clone(), value));
            }
            self.run(&template.body, node, position, size)
        })();
        self.current.borrow_mut().pop();
        self.frames.borrow_mut().pop();
        self.depth.set(depth - 1);
        result
    }

    fn apply(
        &self,
        nodes: &[NodeRef],
        mode: Option<&QName>,
        params: &[(QName, Value)],
        _context: &NodeRef,
        below: Option<usize>,
    ) -> Result<()> {
        let size = nodes.len();
        for (i, node) in nodes.iter().enumerate() {
            self.charge()?;
            match self.find_template(node, mode, below)? {
                Some(t) => self.invoke(t, node, i + 1, size, params.to_vec())?,
                None => self.builtin(node, mode, i + 1, size)?,
            }
        }
        Ok(())
    }

    /// XSLT 1.0 5.8 built-in template rules.
    fn builtin(
        &self,
        node: &NodeRef,
        mode: Option<&QName>,
        _position: usize,
        _size: usize,
    ) -> Result<()> {
        match node.kind() {
            NodeKind::Root | NodeKind::Element => {
                let children: Vec<NodeRef> = node.children().collect();
                self.current.borrow_mut().push(node.clone());
                let r = self.apply(&children, mode, &[], node, None);
                self.current.borrow_mut().pop();
                r
            }
            NodeKind::Text | NodeKind::Attribute => {
                let text = node.node().value.clone();
                self.with_builder(|b| b.text(&text))
            }
            NodeKind::Comment | NodeKind::ProcessingInstruction => Ok(()),
        }
    }

    fn avt(&self, avt: &Avt, node: &NodeRef, position: usize, size: usize) -> Result<String> {
        let mut out = String::new();
        for part in &avt.0 {
            match part {
                AvtPart::Text(t) => out.push_str(t),
                AvtPart::Expr(e) => {
                    out.push_str(&self.eval(e, node, position, size)?.to_string_value())
                }
            }
        }
        Ok(out)
    }

    fn content_string(
        &self,
        body: &[Instr],
        node: &NodeRef,
        position: usize,
        size: usize,
    ) -> Result<String> {
        match self.var_value(&VarValue::Content(body.to_vec()), node, position, size)? {
            v => Ok(v.to_string_value()),
        }
    }

    fn run(&self, body: &[Instr], node: &NodeRef, position: usize, size: usize) -> Result<()> {
        // Variables declared in this sequence are visible to following siblings.
        let mark = self
            .frames
            .borrow()
            .last()
            .map(|f| f.variables.len())
            .unwrap_or(0);
        let result = (|| {
            for instr in body {
                self.charge()?;
                self.exec(instr, node, position, size)?;
            }
            Ok(())
        })();
        if let Some(f) = self.frames.borrow_mut().last_mut() {
            f.variables.truncate(mark);
        }
        result
    }

    fn exec(&self, instr: &Instr, node: &NodeRef, position: usize, size: usize) -> Result<()> {
        match instr {
            Instr::Text(t) => self.with_builder(|b| b.text(t)),
            Instr::ValueOf(e) => {
                let s = self.eval(e, node, position, size)?.to_string_value();
                self.with_builder(|b| b.text(&s))
            }
            Instr::LiteralElement {
                name,
                attributes,
                body,
            } => {
                let mut values = Vec::with_capacity(attributes.len());
                for (n, avt) in attributes {
                    values.push((n.clone(), self.avt(avt, node, position, size)?));
                }
                self.with_builder(|b| {
                    b.start_element(name.clone());
                    for (n, v) in values {
                        b.attribute(n, v);
                    }
                })?;
                let r = self.run(body, node, position, size);
                self.with_builder(|b| b.end_element())?;
                r
            }
            Instr::Element {
                name,
                namespace,
                resolver,
                body,
            } => {
                let qname =
                    self.computed_name(name, namespace.as_ref(), resolver, node, position, size)?;
                self.with_builder(|b| b.start_element(qname))?;
                let r = self.run(body, node, position, size);
                self.with_builder(|b| b.end_element())?;
                r
            }
            Instr::Attribute {
                name,
                namespace,
                resolver,
                body,
            } => {
                let qname =
                    self.computed_name(name, namespace.as_ref(), resolver, node, position, size)?;
                let value = self.content_string(body, node, position, size)?;
                self.with_builder(|b| {
                    b.attribute(qname, value);
                })
            }
            Instr::Comment(body) => {
                let text = self.content_string(body, node, position, size)?;
                self.with_builder(|b| b.comment(&text))
            }
            Instr::ProcessingInstruction { name, body } => {
                let target = self.avt(name, node, position, size)?;
                let text = self.content_string(body, node, position, size)?;
                self.with_builder(|b| b.processing_instruction(&target, &text))
            }
            Instr::Copy(body) => match node.kind() {
                NodeKind::Element => {
                    let name = node.name().cloned().expect("name");
                    self.with_builder(|b| b.start_element(name))?;
                    let r = self.run(body, node, position, size);
                    self.with_builder(|b| b.end_element())?;
                    r
                }
                NodeKind::Root => self.run(body, node, position, size),
                _ => self.with_builder(|b| b.copy_of(node)),
            },
            Instr::CopyOf(e) => match self.eval(e, node, position, size)? {
                Value::Nodes(nodes) => {
                    for n in nodes {
                        self.with_builder(|b| b.copy_of(&n))?;
                    }
                    Ok(())
                }
                other => {
                    let s = other.to_string_value();
                    self.with_builder(|b| b.text(&s))
                }
            },
            Instr::Variable { name, value } => {
                let v = self.var_value(value, node, position, size)?;
                self.frames
                    .borrow_mut()
                    .last_mut()
                    .expect("frame")
                    .variables
                    .push((name.clone(), v));
                Ok(())
            }
            Instr::If { test, body } => {
                if self.eval(test, node, position, size)?.to_bool() {
                    self.run(body, node, position, size)?;
                }
                Ok(())
            }
            Instr::Choose { whens, otherwise } => {
                for (test, body) in whens {
                    if self.eval(test, node, position, size)?.to_bool() {
                        return self.run(body, node, position, size);
                    }
                }
                if let Some(body) = otherwise {
                    self.run(body, node, position, size)?;
                }
                Ok(())
            }
            Instr::ForEach {
                select,
                sorts,
                body,
            } => {
                let Value::Nodes(mut nodes) = self.eval(select, node, position, size)? else {
                    return Err(XsltError("xsl:for-each select is not a node-set".into()));
                };
                self.sort_nodes(&mut nodes, sorts)?;
                let n = nodes.len();
                for (i, item) in nodes.iter().enumerate() {
                    self.current.borrow_mut().push(item.clone());
                    let r = self.run(body, item, i + 1, n);
                    self.current.borrow_mut().pop();
                    r?;
                }
                Ok(())
            }
            Instr::ApplyTemplates {
                select,
                mode,
                sorts,
                params,
            } => {
                let mut nodes = match select {
                    Some(e) => match self.eval(e, node, position, size)? {
                        Value::Nodes(n) => n,
                        _ => {
                            return Err(XsltError(
                                "xsl:apply-templates select is not a node-set".into(),
                            ))
                        }
                    },
                    None => node.children().collect(),
                };
                self.sort_nodes(&mut nodes, sorts)?;
                let bound = self.bind_params(params, node, position, size)?;
                self.apply(&nodes, mode.as_ref(), &bound, node, None)
            }
            Instr::ApplyImports => {
                // Templates of lower precedence than the current template's.
                let current = self.current.borrow().last().cloned().expect("current node");
                let precedence = self
                    .find_template(&current, None, None)?
                    .map(|t| t.precedence)
                    .unwrap_or(usize::MAX);
                match self.find_template(&current, None, Some(precedence))? {
                    Some(t) => self.invoke(t, &current, position, size, Vec::new()),
                    None => self.builtin(&current, None, position, size),
                }
            }
            Instr::CallTemplate { name, params } => {
                let template = self
                    .sheet
                    .templates
                    .iter()
                    .filter(|t| t.name.as_ref().is_some_and(|n| n.same(name)))
                    .max_by(|a, b| (a.precedence, a.order).cmp(&(b.precedence, b.order)))
                    .ok_or_else(|| XsltError(format!("No template named '{}'", name.local)))?;
                let bound = self.bind_params(params, node, position, size)?;
                // The current node is unchanged by call-template.
                self.invoke(template, node, position, size, bound)
            }
            Instr::Message { terminate, body } => {
                let text = self.content_string(body, node, position, size)?;
                self.messages.borrow_mut().push(text.clone());
                if *terminate {
                    return Err(XsltError(format!("xsl:message terminated: {text}")));
                }
                Ok(())
            }
        }
    }

    fn computed_name(
        &self,
        name: &Avt,
        namespace: Option<&Avt>,
        resolver: &Scope,
        node: &NodeRef,
        position: usize,
        size: usize,
    ) -> Result<QName> {
        let text = self.avt(name, node, position, size)?;
        let text = text.trim();
        if text.is_empty() {
            return Err(XsltError("Empty computed element/attribute name".into()));
        }
        let (prefix, local) = match text.split_once(':') {
            Some((p, l)) => (Some(p), l),
            None => (None, text),
        };
        let namespace = match namespace {
            Some(ns) => {
                let uri = self.avt(ns, node, position, size)?;
                (!uri.is_empty()).then(|| Rc::from(uri.as_str()))
            }
            None => match prefix {
                Some(p) => Some(resolver.resolve(p).ok_or_else(|| {
                    XsltError(format!("Undeclared prefix '{p}' in computed name"))
                })?),
                None => None,
            },
        };
        Ok(QName {
            namespace,
            local: local.into(),
            prefix: prefix.map(Rc::from),
        })
    }
}

fn quick_test(test: &NodeTest, attribute: bool, node: &NodeRef) -> bool {
    let principal = if attribute {
        NodeKind::Attribute
    } else {
        NodeKind::Element
    };
    match test {
        NodeTest::Node => true,
        NodeTest::Text => matches!(node.kind(), NodeKind::Text),
        NodeTest::Comment => matches!(node.kind(), NodeKind::Comment),
        NodeTest::ProcessingInstruction(_) => {
            matches!(node.kind(), NodeKind::ProcessingInstruction)
        }
        NodeTest::AnyName | NodeTest::NamespaceAny(_) => *node.kind() == principal,
        NodeTest::Name { namespace, local } => {
            *node.kind() == principal
                && node.name().is_some_and(|n| {
                    n.local == *local && n.namespace.as_deref() == namespace.as_deref()
                })
        }
    }
}

#[derive(Debug, PartialEq, PartialOrd)]
enum SortKey {
    Number(f64),
    Text(String),
}
impl SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            // NaN sorts before all numbers (XSLT 1.0 10).
            (SortKey::Number(a), SortKey::Number(b)) => match (a.is_nan(), b.is_nan()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
            },
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// XSLT 1.0 12.3 format-number with JDK 1.1 DecimalFormat pattern semantics
/// (rounding half-even).
pub fn format_number(
    n: f64,
    pattern: &str,
    f: &DecimalFormat,
) -> std::result::Result<String, XPathError> {
    if n.is_nan() {
        return Ok(f.nan.clone());
    }
    let (positive, negative) = match pattern.split_once(f.pattern_separator) {
        Some((p, q)) => (p, Some(q)),
        None => (pattern, None),
    };
    let parse = |sub: &str| -> std::result::Result<(String, String, String, Sub), XPathError> {
        // prefix, suffix, body
        let is_body = |c: char| {
            c == f.digit
                || c == f.zero_digit
                || c == f.grouping_separator
                || c == f.decimal_separator
        };
        let start = sub.find(is_body).unwrap_or(sub.len());
        let end = sub
            .rfind(is_body)
            .map(|i| i + sub[i..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(start);
        let prefix = sub[..start].to_string();
        let body = &sub[start..end];
        let suffix = sub[end..].to_string();
        let (int, frac) = match body.split_once(f.decimal_separator) {
            Some((i, fr)) => (i, fr),
            None => (body, ""),
        };
        let min_int = int.chars().filter(|c| *c == f.zero_digit).count();
        let grouping = int.rfind(f.grouping_separator).map(|i| {
            int[i + 1..]
                .chars()
                .filter(|c| *c == f.digit || *c == f.zero_digit)
                .count()
        });
        let min_frac = frac.chars().filter(|c| *c == f.zero_digit).count();
        let max_frac = frac
            .chars()
            .filter(|c| *c == f.zero_digit || *c == f.digit)
            .count();
        Ok((
            prefix,
            suffix,
            body.to_string(),
            Sub {
                min_int,
                grouping,
                min_frac,
                max_frac,
            },
        ))
    };
    let (pprefix, psuffix, _, sub) = parse(positive)?;
    let (nprefix, nsuffix) = match negative {
        Some(q) => {
            let (a, b, _, _) = parse(q)?;
            (a, b)
        }
        None => (format!("{}{}", f.minus_sign, pprefix), psuffix.clone()),
    };
    let negative_value = n < 0. || (n == 0. && n.is_sign_negative() && false);
    let (prefix, suffix) = if negative_value {
        (nprefix, nsuffix)
    } else {
        (pprefix, psuffix)
    };
    let mut value = n.abs();
    let scale = if prefix.contains(f.percent) || suffix.contains(f.percent) {
        100.
    } else if prefix.contains(f.per_mille) || suffix.contains(f.per_mille) {
        1000.
    } else {
        1.
    };
    value *= scale;
    if value.is_infinite() {
        return Ok(format!("{prefix}{}{suffix}", f.infinity));
    }
    // Round half-even at max_frac digits using a decimal string.
    let digits = round_half_even(value, sub.max_frac);
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, fr)) => (i.to_string(), fr.to_string()),
        None => (digits, String::new()),
    };
    let mut frac = frac_part.trim_end_matches('0').to_string();
    while frac.len() < sub.min_frac {
        frac.push('0');
    }
    let mut int = int_part.trim_start_matches('0').to_string();
    while int.len() < sub.min_int {
        int.insert(0, '0');
    }
    if let Some(g) = sub.grouping.filter(|g| *g > 0) {
        let chars: Vec<char> = int.chars().collect();
        let mut grouped = String::new();
        for (i, c) in chars.iter().enumerate() {
            if i > 0 && (chars.len() - i) % g == 0 {
                grouped.push(f.grouping_separator);
            }
            grouped.push(*c);
        }
        int = grouped;
    }
    let map_digits = |s: &str| -> String {
        s.chars()
            .map(|c| match c.to_digit(10) {
                Some(d) => char::from_u32(f.zero_digit as u32 + d).unwrap_or(c),
                None => c,
            })
            .collect()
    };
    let mut out = prefix;
    out.push_str(&map_digits(&int));
    if !frac.is_empty() {
        out.push(f.decimal_separator);
        out.push_str(&map_digits(&frac));
    }
    if int.is_empty() && frac.is_empty() {
        out.push(f.zero_digit);
    }
    out.push_str(&suffix);
    Ok(out)
}

struct Sub {
    min_int: usize,
    grouping: Option<usize>,
    min_frac: usize,
    max_frac: usize,
}

fn round_half_even(value: f64, frac_digits: usize) -> String {
    // Exact decimal expansion of the double, then round half-even.
    let exact = format!("{value:.40}");
    let (int, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut digits: Vec<u8> = int.bytes().chain(frac.bytes()).map(|b| b - b'0').collect();
    let point = int.len();
    let keep = point + frac_digits;
    if keep < digits.len() {
        let rest = &digits[keep..];
        let first = rest[0];
        let tail_nonzero = rest[1..].iter().any(|d| *d != 0);
        let last_kept_odd = keep > 0 && digits[keep - 1] % 2 == 1;
        let round_up = first > 5 || (first == 5 && (tail_nonzero || last_kept_odd));
        digits.truncate(keep);
        if round_up {
            let mut i = digits.len();
            loop {
                if i == 0 {
                    digits.insert(0, 1);
                    break;
                }
                i -= 1;
                if digits[i] == 9 {
                    digits[i] = 0;
                } else {
                    digits[i] += 1;
                    break;
                }
            }
        }
    }
    let point = digits.len() - frac_digits.min(digits.len());
    let s: String = digits.iter().map(|d| (b'0' + d) as char).collect();
    if frac_digits == 0 {
        s
    } else {
        format!("{}.{}", &s[..point], &s[point..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(xsl: &str, xml: &str, params: &[(&str, &str)]) -> String {
        let dir = std::env::temp_dir();
        let sheet = Stylesheet::from_str(xsl, &dir, Limits::default()).unwrap();
        let doc = dom::parse(xml, None, None).unwrap();
        let out = sheet.transform(doc, params).unwrap();
        dom::serialize(&out.root)
    }
    const HEAD: &str =
        r#"<xsl:transform xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">"#;

    #[test]
    fn templates_priorities_modes_and_builtins() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:template match="/"><out><xsl:apply-templates select="d/*"/></out></xsl:template>
            <xsl:template match="*[@p='S']"><any/></xsl:template>
            <xsl:template match="f[@p='S']" priority="1"><f-s/></xsl:template>
            <xsl:template match="f[@p='S' and r=1]" priority="2"><f-r/></xsl:template>
            <xsl:template match="g"><xsl:apply-templates select="n" mode="m"><xsl:with-param name="x" select="'!'"/></xsl:apply-templates></xsl:template>
            <xsl:template match="n" mode="m"><xsl:param name="x"/><xsl:value-of select="concat(., $x)"/></xsl:template>
            </xsl:transform>"#
        );
        let xml =
            "<d><f p='S'/><f p='S'><r>1</r></f><h p='S'/><g><n>a</n><n>b</n></g><z>text</z></d>";
        assert_eq!(run(&xsl, xml, &[]), "<out><f-s/><f-r/><any/>a!b!text</out>");
    }

    #[test]
    fn variables_current_count_and_call_template() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:param name="P" select="'def'"/>
            <xsl:template match="/"><l><xsl:apply-templates select="d/f"/><xsl:value-of select="$P"/></l></xsl:template>
            <xsl:template match="f">
              <xsl:if test="../f[p/@ref = current()/p/@ref][1]/@id = @id">
                <xsl:variable name="n" select="count(../f[p/@ref = current()/p/@ref])"/>
                <i id="{{@id}}"><xsl:call-template name="t"><xsl:with-param name="v" select="$n"/><xsl:with-param name="w">rtf</xsl:with-param></xsl:call-template></i>
              </xsl:if>
            </xsl:template>
            <xsl:template name="t"><xsl:param name="v"/><xsl:param name="w"/><xsl:param name="d" select="default"/>
              <xsl:value-of select="$v"/>-<xsl:value-of select="$w"/>-<xsl:value-of select="$d"/>|<xsl:value-of select="format-number(0.64,'####.##')"/>|<xsl:value-of select="format-number(1234.5,'#,##0.00')"/></xsl:template>
            </xsl:transform>"#
        );
        let xml =
            "<d><f id='a'><p ref='1'/></f><f id='b'><p ref='1'/></f><f id='c'><p ref='2'/></f></d>";
        assert_eq!(
            run(&xsl, xml, &[("P", "ext")]),
            "<l><i id=\"a\">2-rtf-|.64|1,234.50</i><i id=\"c\">1-rtf-|.64|1,234.50</i>ext</l>"
        );
    }

    #[test]
    fn computed_elements_attributes_choose_and_text() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:template match="/">
              <xsl:element name="p:list" namespace="urn:p">
                <xsl:element name="e"><xsl:attribute name="a"><xsl:text>x</xsl:text>y</xsl:attribute>
                  <xsl:choose><xsl:when test="d/v &gt; 2">big</xsl:when><xsl:otherwise>small</xsl:otherwise></xsl:choose>
                  <xsl:text>&#10;ETA: </xsl:text><xsl:value-of select="substring(d/t, 9, 2)"/>
                </xsl:element>
              </xsl:element>
            </xsl:template></xsl:transform>"#
        );
        assert_eq!(
            run(&xsl, "<d><v>3</v><t>2024-04-17T22:00:00Z</t></d>", &[]),
            "<p:list><e a=\"xy\">big\nETA: 17</e></p:list>"
        );
    }

    #[test]
    fn comments_between_params_keep_passed_values() {
        let xsl = format!(
            r#"{HEAD}<xsl:template match="/"><xsl:call-template name="t"><xsl:with-param name="a">1</xsl:with-param><xsl:with-param name="b" select="2"/></xsl:call-template></xsl:template>
            <xsl:template name="t"><xsl:param name="a"/> <!-- first --><xsl:param name="b"/><!-- second --><r><xsl:value-of select="$a + $b"/></r></xsl:template></xsl:transform>"#
        );
        assert_eq!(run(&xsl, "<d/>", &[]), "<r>3</r>");
    }

    #[test]
    fn budget_stops_runaway_recursion() {
        let xsl = format!(
            r#"{HEAD}<xsl:template match="/"><xsl:call-template name="r"/></xsl:template>
            <xsl:template name="r"><xsl:call-template name="r"/></xsl:template></xsl:transform>"#
        );
        let sheet = Stylesheet::from_str(&xsl, &std::env::temp_dir(), Limits::default()).unwrap();
        let doc = dom::parse("<d/>", None, None).unwrap();
        match sheet.transform(doc, &[]) {
            Err(e) => assert!(e.0.contains("too deep"), "{e}"),
            Ok(_) => panic!("runaway recursion completed"),
        }
    }

    #[test]
    fn format_number_rounds_half_even() {
        let f = DecimalFormat::default();
        assert_eq!(format_number(2.5, "0", &f).unwrap(), "2");
        assert_eq!(format_number(3.5, "0", &f).unwrap(), "4");
        assert_eq!(format_number(-1.25, "0.0", &f).unwrap(), "-1.2");
        assert_eq!(format_number(0.5, "0%", &f).unwrap(), "50%");
        assert_eq!(format_number(7.0, "000", &f).unwrap(), "007");
        assert_eq!(format_number(-7.0, "0;(0)", &f).unwrap(), "(7)");
        assert_eq!(format_number(f64::NAN, "0", &f).unwrap(), "NaN");
    }
}
