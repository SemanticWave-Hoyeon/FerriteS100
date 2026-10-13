//! XPath 1.0 expressions: lexer (with the 3.7 disambiguation rules),
//! recursive-descent parser and abstract syntax tree.
use std::fmt;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    SelfAxis,
}
impl Axis {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "ancestor" => Self::Ancestor,
            "ancestor-or-self" => Self::AncestorOrSelf,
            "attribute" => Self::Attribute,
            "child" => Self::Child,
            "descendant" => Self::Descendant,
            "descendant-or-self" => Self::DescendantOrSelf,
            "following" => Self::Following,
            "following-sibling" => Self::FollowingSibling,
            "namespace" => Self::Namespace,
            "parent" => Self::Parent,
            "preceding" => Self::Preceding,
            "preceding-sibling" => Self::PrecedingSibling,
            "self" => Self::SelfAxis,
            _ => return None,
        })
    }
    pub fn is_reverse(&self) -> bool {
        matches!(
            self,
            Self::Ancestor
                | Self::AncestorOrSelf
                | Self::Preceding
                | Self::PrecedingSibling
                | Self::Parent
        )
    }
}

/// Name test with its prefix already resolved to a namespace URI.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeTest {
    Name {
        namespace: Option<Rc<str>>,
        local: Rc<str>,
    },
    AnyName,
    NamespaceAny(Rc<str>),
    Node,
    Text,
    Comment,
    ProcessingInstruction(Option<String>),
}

#[derive(Debug, Clone)]
pub struct Step {
    pub axis: Axis,
    pub test: NodeTest,
    pub predicates: Vec<Expr>,
}

#[derive(Debug, Clone)]
pub enum PathStart {
    /// Absolute: from the root of the context node's tree.
    Root,
    /// Relative to the context node.
    Context,
    /// Filter expression followed by steps.
    Filter(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Union,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    Path(PathStart, Vec<Step>),
    Filter(Box<Expr>, Vec<Expr>),
    Literal(Rc<str>),
    Number(f64),
    Variable {
        namespace: Option<Rc<str>>,
        local: Rc<str>,
    },
    Function {
        namespace: Option<Rc<str>>,
        local: Rc<str>,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct XPathError(pub String);
impl fmt::Display for XPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for XPathError {}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DotDot,
    At,
    Comma,
    ColonColon,
    Slash,
    DoubleSlash,
    Pipe,
    Plus,
    Minus,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Operator names (and, or, mod, div) and the multiply operator `*`.
    OperatorName(&'static str),
    Multiply,
    /// NameTest: prefix and local (`*` for any).
    NameTest(Option<String>, String),
    NodeType(String),
    FunctionName(Option<String>, String),
    AxisName(String),
    Literal(String),
    Number(f64),
    Variable(Option<String>, String),
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\u{B7}')
}

fn lex(input: &str) -> Result<Vec<Token>, XPathError> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut tokens: Vec<Token> = Vec::new();
    // 3.7: a preceding token that is not @ :: ( [ , or an operator makes
    // `*` the multiply operator and an NCName an operator name.
    let operator_context = |tokens: &Vec<Token>| -> bool {
        match tokens.last() {
            None => false,
            Some(t) => !matches!(
                t,
                Token::At
                    | Token::ColonColon
                    | Token::LParen
                    | Token::LBracket
                    | Token::Comma
                    | Token::OperatorName(_)
                    | Token::Multiply
                    | Token::Slash
                    | Token::DoubleSlash
                    | Token::Pipe
                    | Token::Plus
                    | Token::Minus
                    | Token::Eq
                    | Token::Ne
                    | Token::Lt
                    | Token::Le
                    | Token::Gt
                    | Token::Ge
            ),
        }
    };
    let read_ncname = |chars: &[char], mut i: usize| -> (String, usize) {
        let start = i;
        while i < chars.len() && is_name_char(chars[i]) {
            i += 1;
        }
        (chars[start..i].iter().collect(), i)
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).copied();
        match c {
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            '@' => {
                tokens.push(Token::At);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '|' => {
                tokens.push(Token::Pipe);
                i += 1;
            }
            '+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Eq);
                i += 1;
            }
            '!' if next == Some('=') => {
                tokens.push(Token::Ne);
                i += 2;
            }
            '<' if next == Some('=') => {
                tokens.push(Token::Le);
                i += 2;
            }
            '<' => {
                tokens.push(Token::Lt);
                i += 1;
            }
            '>' if next == Some('=') => {
                tokens.push(Token::Ge);
                i += 2;
            }
            '>' => {
                tokens.push(Token::Gt);
                i += 1;
            }
            '/' if next == Some('/') => {
                tokens.push(Token::DoubleSlash);
                i += 2;
            }
            '/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            ':' if next == Some(':') => {
                tokens.push(Token::ColonColon);
                i += 2;
            }
            '.' if next == Some('.') => {
                tokens.push(Token::DotDot);
                i += 2;
            }
            '.' if next.is_some_and(|n| n.is_ascii_digit()) => {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::Number(
                    s.parse()
                        .map_err(|_| XPathError(format!("Bad number {s}")))?,
                ));
            }
            '.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            '"' | '\'' => {
                let quote = c;
                let start = i + 1;
                let end = chars[start..]
                    .iter()
                    .position(|x| *x == quote)
                    .ok_or_else(|| XPathError("Unterminated string literal".into()))?;
                tokens.push(Token::Literal(chars[start..start + end].iter().collect()));
                i = start + end + 1;
            }
            '$' => {
                let (first, j) = read_ncname(&chars, i + 1);
                if first.is_empty() {
                    return Err(XPathError("Empty variable name".into()));
                }
                if chars.get(j) == Some(&':') && chars.get(j + 1).is_some_and(|c| is_name_start(*c))
                {
                    let (local, k) = read_ncname(&chars, j + 1);
                    tokens.push(Token::Variable(Some(first), local));
                    i = k;
                } else {
                    tokens.push(Token::Variable(None, first));
                    i = j;
                }
            }
            '*' => {
                if operator_context(&tokens) {
                    tokens.push(Token::Multiply);
                } else {
                    tokens.push(Token::NameTest(None, "*".into()));
                }
                i += 1;
            }
            d if d.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '.' {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::Number(
                    s.parse()
                        .map_err(|_| XPathError(format!("Bad number {s}")))?,
                ));
            }
            n if is_name_start(n) => {
                let (first, j) = read_ncname(&chars, i);
                if operator_context(&tokens) {
                    let op = match first.as_str() {
                        "and" => "and",
                        "or" => "or",
                        "mod" => "mod",
                        "div" => "div",
                        _ => return Err(XPathError(format!("Expected operator, found {first}"))),
                    };
                    tokens.push(Token::OperatorName(op));
                    i = j;
                    continue;
                }
                // QName or prefix:*
                let (prefix, local, mut k) =
                    if chars.get(j) == Some(&':') && chars.get(j + 1) != Some(&':') {
                        if chars.get(j + 1) == Some(&'*') {
                            (Some(first.clone()), "*".to_string(), j + 2)
                        } else {
                            let (local, k) = read_ncname(&chars, j + 1);
                            if local.is_empty() {
                                return Err(XPathError(format!("Bad QName after {first}:")));
                            }
                            (Some(first.clone()), local, k)
                        }
                    } else {
                        (None, first.clone(), j)
                    };
                // Look ahead past whitespace.
                let mut look = k;
                while look < chars.len() && chars[look].is_whitespace() {
                    look += 1;
                }
                let following = chars.get(look).copied();
                let following2 = chars.get(look + 1).copied();
                if prefix.is_none() && following == Some(':') && following2 == Some(':') {
                    tokens.push(Token::AxisName(local));
                    k = look + 2;
                    tokens.push(Token::ColonColon);
                    i = k;
                    continue;
                }
                if following == Some('(') && local != "*" {
                    if prefix.is_none()
                        && matches!(
                            local.as_str(),
                            "comment" | "text" | "processing-instruction" | "node"
                        )
                    {
                        tokens.push(Token::NodeType(local));
                    } else {
                        tokens.push(Token::FunctionName(prefix, local));
                    }
                } else {
                    tokens.push(Token::NameTest(prefix, local));
                }
                i = k;
            }
            other => {
                return Err(XPathError(format!(
                    "Unexpected character '{other}' in XPath"
                )))
            }
        }
    }
    Ok(tokens)
}

/// Resolves prefixes of names in an expression (from the stylesheet element).
pub trait NamespaceResolver {
    fn resolve(&self, prefix: &str) -> Option<Rc<str>>;
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    resolver: &'a dyn NamespaceResolver,
}

pub fn parse(input: &str, resolver: &dyn NamespaceResolver) -> Result<Expr, XPathError> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        resolver,
    };
    let expr = parser.expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(XPathError(format!(
            "Unexpected token {:?} in '{input}'",
            parser.tokens[parser.pos]
        )));
    }
    Ok(expr)
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }
    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }
    fn expect(&mut self, t: Token) -> Result<(), XPathError> {
        match self.advance() {
            Some(x) if x == t => Ok(()),
            other => Err(XPathError(format!("Expected {t:?}, found {other:?}"))),
        }
    }
    fn resolve(&self, prefix: &Option<String>) -> Result<Option<Rc<str>>, XPathError> {
        match prefix {
            None => Ok(None),
            Some(p) => self
                .resolver
                .resolve(p)
                .map(Some)
                .ok_or_else(|| XPathError(format!("Undeclared namespace prefix '{p}'"))),
        }
    }
    fn expr(&mut self) -> Result<Expr, XPathError> {
        self.or_expr()
    }
    fn or_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.and_expr()?;
        while self.peek() == Some(&Token::OperatorName("or")) {
            self.advance();
            let right = self.and_expr()?;
            left = Expr::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn and_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.equality()?;
        while self.peek() == Some(&Token::OperatorName("and")) {
            self.advance();
            let right = self.equality()?;
            left = Expr::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn equality(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.relational()?;
        loop {
            let op = match self.peek() {
                Some(Token::Eq) => BinaryOp::Eq,
                Some(Token::Ne) => BinaryOp::Ne,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.relational()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }
    fn relational(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Lt) => BinaryOp::Lt,
                Some(Token::Le) => BinaryOp::Le,
                Some(Token::Gt) => BinaryOp::Gt,
                Some(Token::Ge) => BinaryOp::Ge,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.additive()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }
    fn additive(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinaryOp::Add,
                Some(Token::Minus) => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.multiplicative()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }
    fn multiplicative(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Multiply) => BinaryOp::Mul,
                Some(Token::OperatorName("div")) => BinaryOp::Div,
                Some(Token::OperatorName("mod")) => BinaryOp::Mod,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }
    fn unary(&mut self) -> Result<Expr, XPathError> {
        if self.peek() == Some(&Token::Minus) {
            self.advance();
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        self.union()
    }
    fn union(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.path_expr()?;
        while self.peek() == Some(&Token::Pipe) {
            self.advance();
            let right = self.path_expr()?;
            left = Expr::Binary(BinaryOp::Union, Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn starts_primary(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Variable(..))
                | Some(Token::LParen)
                | Some(Token::Literal(_))
                | Some(Token::Number(_))
                | Some(Token::FunctionName(..))
        )
    }
    fn path_expr(&mut self) -> Result<Expr, XPathError> {
        if self.starts_primary() {
            let primary = self.primary()?;
            let mut predicates = Vec::new();
            while self.peek() == Some(&Token::LBracket) {
                predicates.push(self.predicate()?);
            }
            let filter = if predicates.is_empty() {
                primary
            } else {
                Expr::Filter(Box::new(primary), predicates)
            };
            let mut steps = Vec::new();
            match self.peek() {
                Some(Token::Slash) => {
                    self.advance();
                    self.relative_path(&mut steps)?;
                }
                Some(Token::DoubleSlash) => {
                    self.advance();
                    steps.push(descendant_or_self());
                    self.relative_path(&mut steps)?;
                }
                _ => return Ok(filter),
            }
            return Ok(Expr::Path(PathStart::Filter(Box::new(filter)), steps));
        }
        let mut steps = Vec::new();
        match self.peek() {
            Some(Token::Slash) => {
                self.advance();
                if self.starts_step() {
                    self.relative_path(&mut steps)?;
                }
                Ok(Expr::Path(PathStart::Root, steps))
            }
            Some(Token::DoubleSlash) => {
                self.advance();
                steps.push(descendant_or_self());
                self.relative_path(&mut steps)?;
                Ok(Expr::Path(PathStart::Root, steps))
            }
            _ => {
                self.relative_path(&mut steps)?;
                Ok(Expr::Path(PathStart::Context, steps))
            }
        }
    }
    fn starts_step(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Dot)
                | Some(Token::DotDot)
                | Some(Token::At)
                | Some(Token::AxisName(_))
                | Some(Token::NameTest(..))
                | Some(Token::NodeType(_))
        )
    }
    fn relative_path(&mut self, steps: &mut Vec<Step>) -> Result<(), XPathError> {
        steps.push(self.step()?);
        loop {
            match self.peek() {
                Some(Token::Slash) => {
                    self.advance();
                    steps.push(self.step()?);
                }
                Some(Token::DoubleSlash) => {
                    self.advance();
                    steps.push(descendant_or_self());
                    steps.push(self.step()?);
                }
                _ => return Ok(()),
            }
        }
    }
    fn step(&mut self) -> Result<Step, XPathError> {
        match self.peek() {
            Some(Token::Dot) => {
                self.advance();
                return Ok(Step {
                    axis: Axis::SelfAxis,
                    test: NodeTest::Node,
                    predicates: Vec::new(),
                });
            }
            Some(Token::DotDot) => {
                self.advance();
                return Ok(Step {
                    axis: Axis::Parent,
                    test: NodeTest::Node,
                    predicates: Vec::new(),
                });
            }
            _ => {}
        }
        let axis = match self.peek() {
            Some(Token::At) => {
                self.advance();
                Axis::Attribute
            }
            Some(Token::AxisName(name)) => {
                let axis = Axis::from_name(name)
                    .ok_or_else(|| XPathError(format!("Unknown axis {name}")))?;
                self.advance();
                self.expect(Token::ColonColon)?;
                axis
            }
            _ => Axis::Child,
        };
        let test = match self.advance() {
            Some(Token::NameTest(prefix, local)) => {
                if local == "*" {
                    match prefix {
                        None => NodeTest::AnyName,
                        Some(_) => {
                            NodeTest::NamespaceAny(self.resolve(&prefix)?.expect("prefix given"))
                        }
                    }
                } else {
                    NodeTest::Name {
                        namespace: self.resolve(&prefix)?,
                        local: local.into(),
                    }
                }
            }
            Some(Token::NodeType(kind)) => {
                self.expect(Token::LParen)?;
                let test = match kind.as_str() {
                    "node" => NodeTest::Node,
                    "text" => NodeTest::Text,
                    "comment" => NodeTest::Comment,
                    _ => {
                        let target = match self.peek() {
                            Some(Token::Literal(l)) => {
                                let l = l.clone();
                                self.advance();
                                Some(l)
                            }
                            _ => None,
                        };
                        NodeTest::ProcessingInstruction(target)
                    }
                };
                self.expect(Token::RParen)?;
                test
            }
            other => return Err(XPathError(format!("Expected node test, found {other:?}"))),
        };
        let mut predicates = Vec::new();
        while self.peek() == Some(&Token::LBracket) {
            predicates.push(self.predicate()?);
        }
        Ok(Step {
            axis,
            test,
            predicates,
        })
    }
    fn predicate(&mut self) -> Result<Expr, XPathError> {
        self.expect(Token::LBracket)?;
        let e = self.expr()?;
        self.expect(Token::RBracket)?;
        Ok(e)
    }
    fn primary(&mut self) -> Result<Expr, XPathError> {
        match self.advance() {
            Some(Token::Variable(prefix, local)) => Ok(Expr::Variable {
                namespace: self.resolve(&prefix)?,
                local: local.into(),
            }),
            Some(Token::LParen) => {
                let e = self.expr()?;
                self.expect(Token::RParen)?;
                Ok(e)
            }
            Some(Token::Literal(s)) => Ok(Expr::Literal(s.into())),
            Some(Token::Number(n)) => Ok(Expr::Number(n)),
            Some(Token::FunctionName(prefix, local)) => {
                self.expect(Token::LParen)?;
                let mut args = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    args.push(self.expr()?);
                    while self.peek() == Some(&Token::Comma) {
                        self.advance();
                        args.push(self.expr()?);
                    }
                }
                self.expect(Token::RParen)?;
                Ok(Expr::Function {
                    namespace: self.resolve(&prefix)?,
                    local: local.into(),
                    args,
                })
            }
            other => Err(XPathError(format!(
                "Expected primary expression, found {other:?}"
            ))),
        }
    }
}

fn descendant_or_self() -> Step {
    Step {
        axis: Axis::DescendantOrSelf,
        test: NodeTest::Node,
        predicates: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoNs;
    impl NamespaceResolver for NoNs {
        fn resolve(&self, prefix: &str) -> Option<Rc<str>> {
            (prefix == "p").then(|| "urn:p".into())
        }
    }
    #[test]
    fn disambiguates_star_and_operator_names() {
        assert!(matches!(
            parse("2 * 3", &NoNs).unwrap(),
            Expr::Binary(BinaryOp::Mul, ..)
        ));
        assert!(matches!(
            parse("*", &NoNs).unwrap(),
            Expr::Path(PathStart::Context, _)
        ));
        assert!(matches!(
            parse("div div div", &NoNs).unwrap(),
            Expr::Binary(BinaryOp::Div, ..)
        ));
        assert!(matches!(
            parse("a and b or c", &NoNs).unwrap(),
            Expr::Binary(BinaryOp::Or, ..)
        ));
        parse(
            "count(../NavwarnPart[Point/@ref = current()/Point/@ref])",
            &NoNs,
        )
        .unwrap();
        parse("NavwarnPart[@primitive='Point' and restriction=1]", &NoNs).unwrap();
        parse(
            "../NavwarnPart[Point/@ref = current()/Point/@ref][1]/@id = @id",
            &NoNs,
        )
        .unwrap();
        parse("p:x/child::text() | //@a[. != 'b'] - -1", &NoNs).unwrap();
        assert!(parse("q:x", &NoNs).is_err());
        assert!(parse("a[", &NoNs).is_err());
    }
}
