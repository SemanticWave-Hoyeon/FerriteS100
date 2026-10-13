//! XPath 1.0 evaluation: values, conversions, comparisons, axes and the
//! core function library.
use crate::dom::{NodeKind, NodeRef};
use crate::xpath::{Axis, BinaryOp, Expr, NodeTest, PathStart, Step, XPathError};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Value {
    /// Distinct nodes in document order.
    Nodes(Vec<NodeRef>),
    Boolean(bool),
    Number(f64),
    String(Rc<str>),
}

impl Value {
    pub fn string(s: impl Into<Rc<str>>) -> Self {
        Value::String(s.into())
    }
    pub fn to_bool(&self) -> bool {
        match self {
            Value::Nodes(n) => !n.is_empty(),
            Value::Boolean(b) => *b,
            Value::Number(n) => *n != 0. && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
        }
    }
    pub fn to_number(&self) -> f64 {
        match self {
            Value::Nodes(_) => string_to_number(&self.to_string_value()),
            Value::Boolean(b) => {
                if *b {
                    1.
                } else {
                    0.
                }
            }
            Value::Number(n) => *n,
            Value::String(s) => string_to_number(s),
        }
    }
    pub fn to_string_value(&self) -> String {
        match self {
            Value::Nodes(n) => n.first().map(NodeRef::string_value).unwrap_or_default(),
            Value::Boolean(b) => if *b { "true" } else { "false" }.into(),
            Value::Number(n) => number_to_string(*n),
            Value::String(s) => s.to_string(),
        }
    }
}

/// XPath 1.0 4.4 `number()` of a string: optional whitespace, optional minus,
/// digits with an optional fraction; anything else is NaN.
pub fn string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    let body = t.strip_prefix('-').unwrap_or(t);
    let valid = !body.is_empty()
        && body.chars().all(|c| c.is_ascii_digit() || c == '.')
        && body.chars().filter(|c| *c == '.').count() <= 1
        && body.chars().any(|c| c.is_ascii_digit());
    if valid {
        t.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// XPath 1.0 4.2 `string()` of a number.
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0. { "Infinity" } else { "-Infinity" }.into()
    } else if n == 0. {
        "0".into()
    } else if n.fract() == 0. && n.abs() < 1e17 {
        format!("{}", n as i64)
    } else {
        // Rust's Display gives the shortest round-trip decimal without exponent.
        format!("{n}")
    }
}

/// Evaluation environment provided by the XSLT processor.
pub trait Environment {
    fn variable(&self, namespace: Option<&str>, local: &str) -> Option<Value>;
    /// Functions beyond the XPath core library (XSLT `current`, `format-number`, ...).
    fn function(
        &self,
        namespace: Option<&str>,
        local: &str,
        args: &[Value],
        ctx: &Context,
    ) -> Option<Result<Value, XPathError>>;
    /// Charged once per evaluated step and predicate; errors abort evaluation.
    fn charge(&self) -> Result<(), XPathError>;
}

pub struct Context<'a> {
    pub node: NodeRef,
    pub position: usize,
    pub size: usize,
    pub env: &'a dyn Environment,
}

impl Context<'_> {
    fn with(&self, node: NodeRef, position: usize, size: usize) -> Context<'_> {
        Context {
            node,
            position,
            size,
            env: self.env,
        }
    }
}

pub fn evaluate(expr: &Expr, ctx: &Context) -> Result<Value, XPathError> {
    match expr {
        Expr::Literal(s) => Ok(Value::String(Rc::clone(s))),
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::Variable { namespace, local } => ctx
            .env
            .variable(namespace.as_deref(), local)
            .ok_or_else(|| XPathError(format!("Undefined variable ${local}"))),
        Expr::Negate(e) => Ok(Value::Number(-evaluate(e, ctx)?.to_number())),
        Expr::Binary(op, a, b) => binary(*op, a, b, ctx),
        Expr::Function {
            namespace,
            local,
            args,
        } => {
            let values = args
                .iter()
                .map(|a| evaluate(a, ctx))
                .collect::<Result<Vec<_>, _>>()?;
            if namespace.is_none() {
                if let Some(result) = core_function(local, &values, ctx) {
                    return result;
                }
            }
            ctx.env
                .function(namespace.as_deref(), local, &values, ctx)
                .unwrap_or_else(|| Err(XPathError(format!("Unknown function {local}()"))))
        }
        Expr::Filter(primary, predicates) => {
            let base = evaluate(primary, ctx)?;
            let Value::Nodes(nodes) = base else {
                return Err(XPathError("Predicate applied to a non-node-set".into()));
            };
            Ok(Value::Nodes(apply_predicates(nodes, predicates, ctx)?))
        }
        Expr::Path(start, steps) => {
            let mut current = match start {
                PathStart::Root => vec![ctx.node.root()],
                PathStart::Context => vec![ctx.node.clone()],
                PathStart::Filter(e) => match evaluate(e, ctx)? {
                    Value::Nodes(n) => n,
                    _ => return Err(XPathError("Path step applied to a non-node-set".into())),
                },
            };
            for step in steps {
                ctx.env.charge()?;
                let mut next = Vec::new();
                for node in &current {
                    next.extend(apply_step(step, node, ctx)?);
                }
                normalize(&mut next);
                current = next;
            }
            Ok(Value::Nodes(current))
        }
    }
}

/// Document order, no duplicates.
pub fn normalize(nodes: &mut Vec<NodeRef>) {
    nodes.sort();
    nodes.dedup();
}

fn apply_step(step: &Step, node: &NodeRef, ctx: &Context) -> Result<Vec<NodeRef>, XPathError> {
    // Axis order (reverse axes are nearest first).
    let candidates: Vec<NodeRef> = match step.axis {
        Axis::Child => node.children().collect(),
        Axis::Attribute => node.attributes().collect(),
        Axis::SelfAxis => vec![node.clone()],
        Axis::Parent => node.parent().into_iter().collect(),
        Axis::Ancestor => node.ancestors(),
        Axis::AncestorOrSelf => {
            let mut v = vec![node.clone()];
            v.extend(node.ancestors());
            v
        }
        Axis::Descendant => node.descendants(),
        Axis::DescendantOrSelf => {
            let mut v = vec![node.clone()];
            v.extend(node.descendants());
            v
        }
        Axis::FollowingSibling => node.following_siblings(),
        Axis::PrecedingSibling => node.preceding_siblings(),
        Axis::Following => following(node),
        Axis::Preceding => preceding(node),
        Axis::Namespace => Vec::new(),
    };
    let principal_attribute = step.axis == Axis::Attribute;
    let matched: Vec<NodeRef> = candidates
        .into_iter()
        .filter(|n| node_test(&step.test, n, principal_attribute))
        .collect();
    apply_predicates(matched, &step.predicates, ctx)
}

fn following(node: &NodeRef) -> Vec<NodeRef> {
    // After the node in document order, excluding descendants (and attributes).
    let anchor = if matches!(node.kind(), NodeKind::Attribute) {
        node.parent().unwrap_or_else(|| node.clone())
    } else {
        node.clone()
    };
    let mut out = Vec::new();
    let mut current = Some(anchor);
    while let Some(n) = current {
        for sibling in n.following_siblings() {
            out.push(sibling.clone());
            out.extend(sibling.descendants());
        }
        current = n.parent();
    }
    out
}

fn preceding(node: &NodeRef) -> Vec<NodeRef> {
    // Before the node in document order, excluding ancestors; nearest first.
    let anchor = if matches!(node.kind(), NodeKind::Attribute) {
        node.parent().unwrap_or_else(|| node.clone())
    } else {
        node.clone()
    };
    let mut out = Vec::new();
    let mut current = Some(anchor);
    while let Some(n) = current {
        for sibling in n.preceding_siblings() {
            let mut sub = vec![sibling.clone()];
            sub.extend(sibling.descendants());
            sub.reverse();
            out.extend(sub);
        }
        current = n.parent();
    }
    out
}

fn node_test(test: &NodeTest, node: &NodeRef, principal_attribute: bool) -> bool {
    let principal = if principal_attribute {
        NodeKind::Attribute
    } else {
        NodeKind::Element
    };
    match test {
        NodeTest::Node => true,
        NodeTest::Text => matches!(node.kind(), NodeKind::Text),
        NodeTest::Comment => matches!(node.kind(), NodeKind::Comment),
        NodeTest::ProcessingInstruction(target) => {
            matches!(node.kind(), NodeKind::ProcessingInstruction)
                && target
                    .as_ref()
                    .is_none_or(|t| node.name().is_some_and(|n| &*n.local == t.as_str()))
        }
        NodeTest::AnyName => *node.kind() == principal,
        NodeTest::NamespaceAny(ns) => {
            *node.kind() == principal
                && node
                    .name()
                    .is_some_and(|n| n.namespace.as_deref() == Some(ns))
        }
        NodeTest::Name { namespace, local } => {
            *node.kind() == principal
                && node.name().is_some_and(|n| {
                    n.local == *local && n.namespace.as_deref() == namespace.as_deref()
                })
        }
    }
}

/// Filters `nodes` (in axis order) by each predicate in turn.
pub fn apply_predicates(
    mut nodes: Vec<NodeRef>,
    predicates: &[Expr],
    ctx: &Context,
) -> Result<Vec<NodeRef>, XPathError> {
    for predicate in predicates {
        ctx.env.charge()?;
        let size = nodes.len();
        let mut kept = Vec::with_capacity(size);
        for (i, node) in nodes.into_iter().enumerate() {
            let inner = ctx.with(node.clone(), i + 1, size);
            let value = evaluate(predicate, &inner)?;
            let keep = match value {
                Value::Number(n) => n == (i + 1) as f64,
                other => other.to_bool(),
            };
            if keep {
                kept.push(node);
            }
        }
        nodes = kept;
    }
    Ok(nodes)
}

fn binary(op: BinaryOp, a: &Expr, b: &Expr, ctx: &Context) -> Result<Value, XPathError> {
    match op {
        BinaryOp::Or => Ok(Value::Boolean(
            evaluate(a, ctx)?.to_bool() || evaluate(b, ctx)?.to_bool(),
        )),
        BinaryOp::And => Ok(Value::Boolean(
            evaluate(a, ctx)?.to_bool() && evaluate(b, ctx)?.to_bool(),
        )),
        BinaryOp::Union => {
            let (Value::Nodes(mut x), Value::Nodes(y)) = (evaluate(a, ctx)?, evaluate(b, ctx)?)
            else {
                return Err(XPathError("Union of non-node-sets".into()));
            };
            x.extend(y);
            normalize(&mut x);
            Ok(Value::Nodes(x))
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let x = evaluate(a, ctx)?.to_number();
            let y = evaluate(b, ctx)?.to_number();
            Ok(Value::Number(match op {
                BinaryOp::Add => x + y,
                BinaryOp::Sub => x - y,
                BinaryOp::Mul => x * y,
                BinaryOp::Div => x / y,
                _ => x % y, // truncating remainder, as in XPath
            }))
        }
        _ => Ok(Value::Boolean(compare(
            op,
            &evaluate(a, ctx)?,
            &evaluate(b, ctx)?,
        ))),
    }
}

/// XPath 1.0 3.4 comparisons.
pub fn compare(op: BinaryOp, x: &Value, y: &Value) -> bool {
    let equality = matches!(op, BinaryOp::Eq | BinaryOp::Ne);
    match (x, y) {
        (Value::Nodes(a), Value::Nodes(b)) => {
            let bs: Vec<String> = b.iter().map(NodeRef::string_value).collect();
            a.iter().any(|n| {
                let s = n.string_value();
                bs.iter()
                    .any(|t| atomic(op, &Value::string(s.as_str()), &Value::string(t.as_str())))
            })
        }
        (Value::Nodes(a), other) | (other, Value::Nodes(a)) => {
            let nodes_left = matches!(x, Value::Nodes(_));
            match other {
                Value::Boolean(_) => {
                    let set = Value::Boolean(!a.is_empty());
                    if nodes_left {
                        atomic(op, &set, other)
                    } else {
                        atomic(op, other, &set)
                    }
                }
                _ => a.iter().any(|n| {
                    let s = n.string_value();
                    let v = if matches!(other, Value::Number(_)) || (!equality) {
                        Value::Number(string_to_number(&s))
                    } else {
                        Value::string(s.as_str())
                    };
                    if nodes_left {
                        atomic(op, &v, other)
                    } else {
                        atomic(op, other, &v)
                    }
                }),
            }
        }
        _ => atomic(op, x, y),
    }
}

fn atomic(op: BinaryOp, x: &Value, y: &Value) -> bool {
    match op {
        BinaryOp::Eq | BinaryOp::Ne => {
            let equal = if matches!(x, Value::Boolean(_)) || matches!(y, Value::Boolean(_)) {
                x.to_bool() == y.to_bool()
            } else if matches!(x, Value::Number(_)) || matches!(y, Value::Number(_)) {
                x.to_number() == y.to_number()
            } else {
                x.to_string_value() == y.to_string_value()
            };
            if op == BinaryOp::Eq {
                equal
            } else {
                !equal
            }
        }
        _ => {
            let (a, b) = (x.to_number(), y.to_number());
            match op {
                BinaryOp::Lt => a < b,
                BinaryOp::Le => a <= b,
                BinaryOp::Gt => a > b,
                _ => a >= b,
            }
        }
    }
}

fn arity(name: &str, args: &[Value], min: usize, max: usize) -> Result<(), XPathError> {
    if args.len() < min || args.len() > max {
        Err(XPathError(format!("Wrong number of arguments to {name}()")))
    } else {
        Ok(())
    }
}

fn string_arg(args: &[Value], i: usize, ctx: &Context) -> String {
    args.get(i)
        .map(Value::to_string_value)
        .unwrap_or_else(|| ctx.node.string_value())
}

/// XPath round(): nearest integer, halves toward positive infinity.
pub fn xpath_round(n: f64) -> f64 {
    if n.is_nan() || n.is_infinite() {
        return n;
    }
    let r = (n + 0.5).floor();
    if r == 0. && n < 0. {
        -0.
    } else {
        r
    }
}

fn core_function(name: &str, args: &[Value], ctx: &Context) -> Option<Result<Value, XPathError>> {
    let result = (|| -> Result<Value, XPathError> {
        Ok(match name {
            "last" => {
                arity(name, args, 0, 0)?;
                Value::Number(ctx.size as f64)
            }
            "position" => {
                arity(name, args, 0, 0)?;
                Value::Number(ctx.position as f64)
            }
            "count" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::Nodes(n) => Value::Number(n.len() as f64),
                    _ => return Err(XPathError("count() requires a node-set".into())),
                }
            }
            "id" => Value::Nodes(Vec::new()),
            "local-name" | "namespace-uri" | "name" => {
                arity(name, args, 0, 1)?;
                let node = match args.first() {
                    Some(Value::Nodes(n)) => n.first().cloned(),
                    Some(_) => return Err(XPathError(format!("{name}() requires a node-set"))),
                    None => Some(ctx.node.clone()),
                };
                let s = node
                    .and_then(|n| n.name().cloned())
                    .map(|q| match name {
                        "local-name" => q.local.to_string(),
                        "namespace-uri" => q.namespace.map(|n| n.to_string()).unwrap_or_default(),
                        _ => q.qualified(),
                    })
                    .unwrap_or_default();
                Value::string(s)
            }
            "string" => {
                arity(name, args, 0, 1)?;
                Value::string(string_arg(args, 0, ctx))
            }
            "concat" => {
                if args.len() < 2 {
                    return Err(XPathError(
                        "concat() requires at least two arguments".into(),
                    ));
                }
                Value::string(args.iter().map(Value::to_string_value).collect::<String>())
            }
            "starts-with" => {
                arity(name, args, 2, 2)?;
                Value::Boolean(
                    args[0]
                        .to_string_value()
                        .starts_with(&args[1].to_string_value()),
                )
            }
            "contains" => {
                arity(name, args, 2, 2)?;
                Value::Boolean(
                    args[0]
                        .to_string_value()
                        .contains(&args[1].to_string_value()),
                )
            }
            "substring-before" => {
                arity(name, args, 2, 2)?;
                let (s, t) = (args[0].to_string_value(), args[1].to_string_value());
                Value::string(s.find(&t).map(|i| s[..i].to_string()).unwrap_or_default())
            }
            "substring-after" => {
                arity(name, args, 2, 2)?;
                let (s, t) = (args[0].to_string_value(), args[1].to_string_value());
                Value::string(
                    s.find(&t)
                        .map(|i| s[i + t.len()..].to_string())
                        .unwrap_or_default(),
                )
            }
            "substring" => {
                arity(name, args, 2, 3)?;
                let s: Vec<char> = args[0].to_string_value().chars().collect();
                let start = xpath_round(args[1].to_number());
                let end = match args.get(2) {
                    Some(len) => start + xpath_round(len.to_number()),
                    None => f64::INFINITY,
                };
                // Characters at 1-based position p with start <= p < end.
                let out: String = s
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        let p = (*i + 1) as f64;
                        p >= start && p < end
                    })
                    .map(|(_, c)| *c)
                    .collect();
                Value::string(out)
            }
            "string-length" => {
                arity(name, args, 0, 1)?;
                Value::Number(string_arg(args, 0, ctx).chars().count() as f64)
            }
            "normalize-space" => {
                arity(name, args, 0, 1)?;
                let s = string_arg(args, 0, ctx);
                Value::string(
                    s.split([' ', '\t', '\n', '\r'])
                        .filter(|w| !w.is_empty())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
            "translate" => {
                arity(name, args, 3, 3)?;
                let from: Vec<char> = args[1].to_string_value().chars().collect();
                let to: Vec<char> = args[2].to_string_value().chars().collect();
                let out: String = args[0]
                    .to_string_value()
                    .chars()
                    .filter_map(|c| match from.iter().position(|f| *f == c) {
                        Some(i) => to.get(i).copied(),
                        None => Some(c),
                    })
                    .collect();
                Value::string(out)
            }
            "boolean" => {
                arity(name, args, 1, 1)?;
                Value::Boolean(args[0].to_bool())
            }
            "not" => {
                arity(name, args, 1, 1)?;
                Value::Boolean(!args[0].to_bool())
            }
            "true" => Value::Boolean(true),
            "false" => Value::Boolean(false),
            "lang" => {
                arity(name, args, 1, 1)?;
                let wanted = args[0].to_string_value().to_ascii_lowercase();
                let mut current = Some(ctx.node.clone());
                let mut found = None;
                while let Some(n) = current {
                    if let Some(v) = n.attributes().find(|a| {
                        a.name().is_some_and(|q| {
                            &*q.local == "lang"
                                && q.namespace.as_deref()
                                    == Some("http://www.w3.org/XML/1998/namespace")
                        })
                    }) {
                        found = Some(v.node().value.to_ascii_lowercase());
                        break;
                    }
                    current = n.parent();
                }
                Value::Boolean(found.is_some_and(|l| {
                    l == wanted || l.strip_prefix(&wanted).is_some_and(|r| r.starts_with('-'))
                }))
            }
            "number" => {
                arity(name, args, 0, 1)?;
                Value::Number(match args.first() {
                    Some(v) => v.to_number(),
                    None => string_to_number(&ctx.node.string_value()),
                })
            }
            "sum" => {
                arity(name, args, 1, 1)?;
                match &args[0] {
                    Value::Nodes(n) => {
                        Value::Number(n.iter().map(|x| string_to_number(&x.string_value())).sum())
                    }
                    _ => return Err(XPathError("sum() requires a node-set".into())),
                }
            }
            "floor" => {
                arity(name, args, 1, 1)?;
                Value::Number(args[0].to_number().floor())
            }
            "ceiling" => {
                arity(name, args, 1, 1)?;
                Value::Number(args[0].to_number().ceil())
            }
            "round" => {
                arity(name, args, 1, 1)?;
                Value::Number(xpath_round(args[0].to_number()))
            }
            _ => return Err(XPathError(String::new())),
        })
    })();
    match result {
        Err(XPathError(m)) if m.is_empty() => None,
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom;
    use crate::xpath::{parse, NamespaceResolver};

    struct Env;
    impl Environment for Env {
        fn variable(&self, _: Option<&str>, local: &str) -> Option<Value> {
            (local == "v").then(|| Value::Number(2.))
        }
        fn function(
            &self,
            _: Option<&str>,
            _: &str,
            _: &[Value],
            _: &Context,
        ) -> Option<Result<Value, XPathError>> {
            None
        }
        fn charge(&self) -> Result<(), XPathError> {
            Ok(())
        }
    }
    struct NoNs;
    impl NamespaceResolver for NoNs {
        fn resolve(&self, _: &str) -> Option<Rc<str>> {
            None
        }
    }
    fn eval(xml: &str, expr: &str) -> String {
        let doc = dom::parse(xml, None, None).unwrap();
        let ctx = Context {
            node: NodeRef::root_of(doc),
            position: 1,
            size: 1,
            env: &Env,
        };
        let e = parse(expr, &NoNs).unwrap();
        match evaluate(&e, &ctx).unwrap() {
            Value::Nodes(n) => n
                .iter()
                .map(|x| x.string_value())
                .collect::<Vec<_>>()
                .join("|"),
            v => v.to_string_value(),
        }
    }
    #[test]
    fn core_semantics() {
        let xml = "<r><a x='1'>p</a><a x='2'>q</a><b>3</b><b>4</b></r>";
        assert_eq!(eval(xml, "count(r/a)"), "2");
        assert_eq!(eval(xml, "r/a[2]"), "q");
        assert_eq!(eval(xml, "r/a[@x=2]"), "q");
        assert_eq!(eval(xml, "r/b = 4"), "true");
        assert_eq!(eval(xml, "r/b != 4"), "true");
        assert_eq!(eval(xml, "r/b > 3.5"), "true");
        assert_eq!(eval(xml, "r/a/@x = r/b"), "false");
        assert_eq!(eval(xml, "sum(r/b) div 2"), "3.5");
        assert_eq!(eval(xml, "1 div 0"), "Infinity");
        assert_eq!(eval(xml, "0 div 0 = 0 div 0"), "false");
        assert_eq!(eval(xml, "number('1e3')"), "NaN");
        assert_eq!(eval(xml, "-5 mod 3"), "-2");
        assert_eq!(eval(xml, "round(-0.5)"), "0");
        assert_eq!(eval(xml, "round(2.5)"), "3");
        assert_eq!(eval(xml, "substring('12345', 1.5, 2.6)"), "234");
        assert_eq!(eval(xml, "substring('12345', 0, 3)"), "12");
        assert_eq!(eval(xml, "substring('12345', 0 div 0, 3)"), "");
        assert_eq!(eval(xml, "substring('2024-04-17T22:00:00Z', 9, 2)"), "17");
        assert_eq!(eval(xml, "translate('bar','abc','ABC')"), "BAr");
        assert_eq!(eval(xml, "normalize-space('  a  b ')"), "a b");
        assert_eq!(eval(xml, "concat('a', 1, true())"), "a1true");
        assert_eq!(eval(xml, "$v * 3"), "6");
        assert_eq!(eval(xml, "r/a[last()]"), "q");
        assert_eq!(eval(xml, "r/b[2]/preceding-sibling::*[1]"), "3");
        assert_eq!(eval(xml, "r/b[2]/preceding::a[1]"), "q");
        assert_eq!(eval(xml, "(r/b[2]/preceding::a)[1]"), "p");
        assert_eq!(eval(xml, "r/a[1]/following::*"), "q|3|4");
        assert_eq!(eval(xml, "//a | //b[1]"), "p|q|3");
        assert_eq!(eval(xml, "name(r/*[3])"), "b");
        assert_eq!(eval(xml, "string(0.1 + 0.2 = 0.3)"), "false");
        assert_eq!(eval(xml, "1.50"), "1.5");
    }
}
