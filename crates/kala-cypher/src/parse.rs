//! Recursive-descent parser with checked depth and element budgets.
use crate::{
    CypherError, ErrorKind, MAX_DEPTH, MAX_PATTERNS,
    ast::*,
    lex::{self, Kind, Token},
};
use kala_core::Value;
use kala_query::{CmpOp, Expr, Orientation, Term, TickSource};
pub(crate) fn parse(text: &str) -> Result<Query, CypherError> {
    let mut p = Parser {
        text,
        tokens: lex::lex(text)?,
        at: 0,
        elements: 0,
    };
    p.query()
}
struct Parser<'a> {
    text: &'a str,
    tokens: Vec<Token>,
    at: usize,
    elements: usize,
}
impl Parser<'_> {
    fn token(&self) -> &Token {
        &self.tokens[self.at]
    }
    fn err(&self, message: impl Into<String>) -> CypherError {
        CypherError::new(self.text, self.token().start, ErrorKind::Syntax, message)
    }
    fn bump(&mut self) -> Token {
        let t = self.tokens[self.at].clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        t
    }
    fn kw(&mut self, s: &str) -> bool {
        if matches!(&self.token().kind,Kind::Ident(x,false) if x.eq_ignore_ascii_case(s)) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn sym(&mut self, s: &str) -> bool {
        if matches!(&self.token().kind,Kind::Symbol(x) if *x==s) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn need_kw(&mut self, s: &str) -> Result<(), CypherError> {
        if self.kw(s) {
            Ok(())
        } else {
            Err(self.unsupported(s))
        }
    }
    fn need(&mut self, s: &str) -> Result<(), CypherError> {
        if self.sym(s) {
            Ok(())
        } else {
            Err(self.err(format!("expected {s}")))
        }
    }
    fn unsupported(&self, expected: &str) -> CypherError {
        match &self.token().kind {
            Kind::Ident(s, _) => {
                self.err(format!("{s} is not supported yet (expected {expected})"))
            }
            Kind::Symbol("*") => {
                self.err("variable-length relationships and RETURN * are not supported yet")
            }
            _ => self.err(format!(
                "construct is not supported yet; expected {expected}"
            )),
        }
    }
    fn name(&mut self) -> Result<String, CypherError> {
        match self.bump().kind {
            Kind::Ident(s, _) => Ok(s),
            _ => Err(self.err("identifier expected")),
        }
    }
    fn element(&mut self) -> Result<(), CypherError> {
        self.elements += 1;
        if self.elements > MAX_PATTERNS {
            Err(self.err("pattern/element limit exceeded"))
        } else {
            Ok(())
        }
    }
    fn query(&mut self) -> Result<Query, CypherError> {
        let mut matches = Vec::new();
        let mut canon_seen = false;
        self.need_kw("MATCH")?;
        loop {
            let mut paths = vec![self.path()?];
            while self.sym(",") {
                paths.push(self.path()?);
            }
            let tick = if self.kw("AT") {
                self.need_kw("TICK")?;
                Some(self.integer(false)?)
            } else {
                None
            };
            let canon = if self.kw("CANON") {
                if tick.is_none() {
                    return Err(self.err("CANON requires AT TICK"));
                }
                if canon_seen {
                    return Err(self.err("CANON may appear only once"));
                }
                canon_seen = true;
                Some(self.integer(true)?)
            } else {
                None
            };
            let predicate = if self.kw("WHERE") {
                Some(self.expression(0, 0)?)
            } else {
                None
            };
            matches.push(Match {
                paths,
                tick,
                canon,
                predicate,
            });
            if !self.kw("MATCH") {
                break;
            }
        }
        self.need_kw("RETURN")?;
        let distinct = self.kw("DISTINCT");
        let mut items = Vec::new();
        loop {
            self.element()?;
            let start = self.token().start;
            let expr = self.item()?;
            let end = self.tokens[self.at.saturating_sub(1)].end;
            let name = if self.kw("AS") {
                self.name()?
            } else {
                match &expr {
                    Expr::Var(v) => v.clone(),
                    Expr::Property { entity, key, .. } => format!("{entity}.{key}"),
                    _ => self.text[start..end].into(),
                }
            };
            items.push(Item { expr, name });
            if !self.sym(",") {
                break;
            }
        }
        let mut order = Vec::new();
        if self.kw("ORDER") {
            self.need_kw("BY")?;
            loop {
                self.element()?;
                let e = self.item()?;
                let desc = self.kw("DESC") || self.kw("DESCENDING");
                if !desc {
                    self.kw("ASC");
                    self.kw("ASCENDING");
                }
                order.push((e, desc));
                if !self.sym(",") {
                    break;
                }
            }
        }
        let skip = if self.kw("SKIP") {
            Some(self.integer(true)?)
        } else {
            None
        };
        let limit = if self.kw("LIMIT") {
            Some(self.integer(true)?)
        } else {
            None
        };
        self.sym(";");
        if !matches!(self.token().kind, Kind::End) {
            return Err(self.unsupported("end of query"));
        }
        Ok(Query {
            matches,
            items,
            distinct,
            order,
            skip,
            limit,
        })
    }
    fn path(&mut self) -> Result<Path, CypherError> {
        let mut nodes = vec![self.node()?];
        let mut edges = Vec::new();
        while matches!(self.token().kind, Kind::Symbol("-") | Kind::Symbol("<-")) {
            self.element()?;
            let incoming = self.sym("<-");
            if !incoming {
                self.need("-")?;
            }
            self.need("[")?;
            if matches!(self.token().kind, Kind::Symbol("*")) {
                return Err(self.err("variable-length relationships are not supported yet"));
            }
            let name = if matches!(self.token().kind, Kind::Ident(..)) {
                Some(self.name()?)
            } else {
                None
            };
            if !self.sym(":") {
                return Err(self.err("exactly one relationship type is required"));
            }
            let kind = self.name()?;
            if matches!(self.token().kind, Kind::Symbol("|")) {
                return Err(self.err("multiple relationship types are not supported yet"));
            }
            if matches!(self.token().kind, Kind::Symbol("*")) {
                return Err(self.err("variable-length relationships are not supported yet"));
            }
            let props = self.props()?;
            self.need("]")?;
            let direction = if incoming {
                self.need("-")?;
                Orientation::In
            } else if self.sym("->") {
                Orientation::Out
            } else {
                self.need("-")?;
                Orientation::Either
            };
            edges.push(Edge {
                name,
                kind,
                direction,
                props,
            });
            nodes.push(self.node()?);
        }
        Ok(Path { nodes, edges })
    }
    fn node(&mut self) -> Result<Node, CypherError> {
        self.element()?;
        self.need("(")?;
        let name = if matches!(self.token().kind, Kind::Ident(..)) {
            Some(self.name()?)
        } else {
            None
        };
        let mut labels = Vec::new();
        while self.sym(":") {
            self.element()?;
            labels.push(self.name()?);
        }
        let props = self.props()?;
        self.need(")")?;
        Ok(Node {
            name,
            labels,
            props,
        })
    }
    fn props(&mut self) -> Result<Vec<(String, Expr<String>)>, CypherError> {
        let mut props = Vec::new();
        if self.sym("{") && !self.sym("}") {
            loop {
                self.element()?;
                let key = self.name()?;
                self.need(":")?;
                let expr = self.item()?;
                props.push((key, expr));
                if self.sym("}") {
                    break;
                }
                self.need(",")?;
            }
        }
        Ok(props)
    }
    fn integer(&mut self, nonnegative: bool) -> Result<Term, CypherError> {
        if let Kind::Param(p) = &self.token().kind {
            let p = p.clone();
            self.bump();
            return Ok(Term::Param(p));
        }
        let negative = self.sym("-");
        self.sym("+");
        let t = self.bump();
        let Kind::Number(n) = t.kind else {
            return Err(self.err("integer literal or parameter expected"));
        };
        let n = if negative { format!("-{n}") } else { n };
        let n = n
            .parse::<i64>()
            .map_err(|_| self.err("integer literal is out of range or not an integer"))?;
        if nonnegative && n < 0 {
            return Err(self.err("non-negative integer required"));
        }
        Ok(Term::Const(Value::I64(n)))
    }
    fn item(&mut self) -> Result<Expr<String>, CypherError> {
        let negative = self.sym("-");
        let positive = if !negative { self.sym("+") } else { false };
        let t = self.bump();
        let e = match t.kind {
            Kind::Number(n) => {
                let n = if negative { format!("-{n}") } else { n };
                if n.contains(['.', 'e', 'E']) {
                    let f = n
                        .parse::<f64>()
                        .map_err(|_| self.err("invalid float literal"))?;
                    if !f.is_finite() {
                        return Err(self.err("non-finite float literal"));
                    }
                    Expr::Value(Value::F64(f))
                } else {
                    Expr::Value(Value::I64(
                        n.parse()
                            .map_err(|_| self.err("integer literal out of range"))?,
                    ))
                }
            }
            _ if negative || positive => return Err(self.err("sign requires a numeric literal")),
            Kind::String(s) => Expr::Value(Value::Str(s)),
            Kind::Param(p) => Expr::Param(p),
            Kind::Ident(s, false) if s.eq_ignore_ascii_case("null") => Expr::Null,
            Kind::Ident(s, false) if s.eq_ignore_ascii_case("true") => {
                Expr::Value(Value::Bool(true))
            }
            Kind::Ident(s, false) if s.eq_ignore_ascii_case("false") => {
                Expr::Value(Value::Bool(false))
            }
            Kind::Ident(s, _) => {
                if matches!(self.token().kind, Kind::Symbol("(")) {
                    return Err(self.err(format!("{s} functions/aggregates are not supported yet")));
                }
                if self.sym(".") {
                    Expr::Property {
                        entity: s,
                        key: self.name()?,
                        tick: TickSource::Default,
                    }
                } else {
                    Expr::Var(s)
                }
            }
            _ => return Err(self.unsupported("variable, property, literal or parameter")),
        };
        Ok(e)
    }
    fn expression(&mut self, min: u8, depth: usize) -> Result<Expr<String>, CypherError> {
        if depth >= MAX_DEPTH {
            return Err(self.err("expression nesting depth limit exceeded"));
        }
        self.element()?;
        let mut left = if self.kw("NOT") {
            Expr::Not(Box::new(self.expression(4, depth + 1)?))
        } else if self.sym("(") {
            let e = self.expression(0, depth + 1)?;
            self.need(")")?;
            e
        } else {
            self.item()?
        };
        loop {
            let (precedence, op) = match &self.token().kind {
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("OR") => (1, "OR"),
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("XOR") => (2, "XOR"),
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("AND") => (3, "AND"),
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("IS") => (5, "IS"),
                Kind::Symbol(s @ ("=" | "<>" | "<" | "<=" | ">" | ">=")) => (5, *s),
                _ => break,
            };
            if precedence < min {
                break;
            }
            self.bump();
            if op == "IS" {
                let not = self.kw("NOT");
                self.need_kw("NULL")?;
                left = Expr::IsNull(Box::new(left));
                if not {
                    left = Expr::Not(Box::new(left));
                }
            } else {
                let right = Box::new(self.expression(precedence + 1, depth + 1)?);
                let a = Box::new(left);
                left = match op {
                    "AND" => Expr::And(a, right),
                    "OR" => Expr::Or(a, right),
                    "XOR" => Expr::Xor(a, right),
                    _ => Expr::Compare(
                        match op {
                            "=" => CmpOp::Eq,
                            "<>" => CmpOp::Ne,
                            "<" => CmpOp::Lt,
                            "<=" => CmpOp::Le,
                            ">" => CmpOp::Gt,
                            _ => CmpOp::Ge,
                        },
                        a,
                        right,
                    ),
                };
            }
            self.check_depth(&left)?;
        }
        Ok(left)
    }
    fn check_depth(&self, expr: &Expr<String>) -> Result<(), CypherError> {
        let mut stack = vec![(expr, 1)];
        while let Some((e, d)) = stack.pop() {
            if d >= MAX_DEPTH {
                return Err(self.err("expression nesting depth limit exceeded"));
            }
            match e {
                Expr::Not(e) | Expr::IsNull(e) => stack.push((e, d + 1)),
                Expr::Compare(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
                    stack.push((a, d + 1));
                    stack.push((b, d + 1));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
