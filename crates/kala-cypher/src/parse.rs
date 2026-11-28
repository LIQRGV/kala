//! Recursive-descent parser with checked depth and element budgets.
use crate::{
    CypherError, ErrorKind, MAX_DEPTH, MAX_PATTERNS,
    ast::*,
    lex::{self, Kind, Token},
};
use kala_core::Value;
use kala_query::{BagFn, CmpOp, Expr, Orientation, Term, TickSource};
pub(crate) fn parse(text: &str) -> Result<Statement, CypherError> {
    let mut p = Parser {
        text,
        tokens: lex::lex(text)?,
        at: 0,
        elements: 0,
        copied: 0,
        write_values: false,
    };
    p.query()
}
struct Parser<'a> {
    text: &'a str,
    tokens: Vec<Token>,
    at: usize,
    elements: usize,
    /// Nodes plus string bytes duplicated by comparison chains (see
    /// [`Parser::comparison`]), bounded by [`MAX_CHAIN_COPY`].
    copied: usize,
    /// Inside an update, a row binding may be a parallel list indexed by an UNWIND ordinal.
    write_values: bool,
}
/// How much a query's comparison chains may duplicate in total: a middle
/// operand is copied into two comparisons, and nested chains would otherwise
/// double the copy per level. Real middles are a property or a parameter.
const MAX_CHAIN_COPY: usize = crate::MAX_QUERY_BYTES;
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
            Kind::Symbol(s) => self.err(format!("{s} is not supported yet (expected {expected})")),
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
    /// `MATCH … [MATCH …] RETURN … [ORDER BY …] [SKIP n] [LIMIT n] [;]`.
    fn query(&mut self) -> Result<Statement, CypherError> {
        let mut clauses = Vec::new();
        let mut canon_seen = false;
        loop {
            self.element()?;
            if self.kw("MATCH") {
                clauses.push(Clause::Match(self.match_clause(&mut canon_seen, false)?));
            } else if self.kw("OPTIONAL") {
                self.need_kw("MATCH")?;
                clauses.push(Clause::Match(self.match_clause(&mut canon_seen, true)?));
            } else if self.kw("CALL") {
                clauses.push(Clause::Source(self.source(false, &mut canon_seen)?));
            } else if self.kw("SHOW") {
                clauses.push(Clause::Source(self.source(true, &mut canon_seen)?));
            } else if self.kw("UNWIND") {
                let e = self.item()?;
                self.need_kw("AS")?;
                clauses.push(Clause::Unwind(e, self.name()?));
            } else if self.kw("WITH") {
                let q = self.projection(true)?;
                let predicate = if self.kw("WHERE") {
                    Some(self.expression(0, 0)?)
                } else {
                    None
                };
                clauses.push(Clause::With(Box::new(q), predicate));
            } else {
                break;
            }
        }
        let pipeline_tick = if !clauses.is_empty() && self.kw("AT") {
            self.need_kw("TICK")?;
            Some(self.integer(false)?)
        } else {
            None
        };
        if self.is_kw("CREATE")
            || self.is_kw("MERGE")
            || self.is_kw("SET")
            || self.is_kw("REMOVE")
            || self.is_kw("DELETE")
            || self.is_kw("DETACH")
        {
            return self.updates(clauses, canon_seen, pipeline_tick);
        }
        if pipeline_tick.is_some() {
            return Err(
                self.err("AT TICK at the end of a reading pipeline requires a write clause")
            );
        }
        if clauses.is_empty() {
            return Err(self.unsupported("MATCH, OPTIONAL MATCH, WITH, UNWIND or a procedure"));
        }
        let mut q = if self.kw("RETURN") {
            self.projection(false)?
        } else if let Some(Clause::Source(source)) = clauses.last() {
            Query {
                include_all: false,
                clauses: vec![],
                items: source
                    .yields
                    .iter()
                    .map(|(_, n)| Item {
                        name: n.clone(),
                        expr: Expr::Var(n.clone()),
                    })
                    .collect(),
                distinct: false,
                order: vec![],
                skip: None,
                limit: None,
            }
        } else {
            return Err(self.unsupported("RETURN"));
        };
        q.clauses = clauses;
        self.sym(";");
        if !matches!(self.token().kind, Kind::End) {
            return Err(self.unsupported("end of query"));
        }
        Ok(Statement::Read(q))
    }
    fn source(&mut self, show: bool, canon_seen: &mut bool) -> Result<Source, CypherError> {
        use kala_query::{DatabaseSelection, RowSource, ServerRequest};
        if !show && matches!(self.token().kind, Kind::Symbol("{")) {
            return Err(self.err("CALL subqueries are not supported yet"));
        }
        let source = if show {
            let selection = if self.kw("DATABASES") {
                DatabaseSelection::All
            } else if self.kw("DEFAULT") {
                self.need_kw("DATABASE")?;
                DatabaseSelection::Default
            } else if self.kw("HOME") {
                self.need_kw("DATABASE")?;
                DatabaseSelection::Home
            } else if self.kw("DATABASE") {
                let name = match self.bump().kind {
                    Kind::Ident(n, _) | Kind::String(n) => n,
                    _ => return Err(self.err("database name expected")),
                };
                DatabaseSelection::Named(name)
            } else {
                return Err(self.unsupported("DATABASES"));
            };
            RowSource::Server(ServerRequest::Databases(selection))
        } else {
            let mut name = self.name()?;
            while self.sym(".") {
                name.push('.');
                name.push_str(&self.name()?);
            }
            self.need("(")?;
            if !self.sym(")") {
                return Err(self.err("procedure arguments are not supported yet"));
            }
            match name.as_str() {
                "db.labels" => RowSource::Labels,
                "db.relationshipTypes" => RowSource::RelationshipTypes,
                "db.propertyKeys" => RowSource::PropertyKeys,
                "dbms.components" => RowSource::Server(ServerRequest::Components),
                _ => {
                    return Err(CypherError::new(
                        self.text,
                        self.token().start,
                        ErrorKind::Procedure,
                        format!(
                            "procedure {} was not found",
                            name.chars().take(80).collect::<String>()
                        ),
                    ));
                }
            }
        };
        let tick = if self.kw("AT") {
            self.need_kw("TICK")?;
            Some(self.integer(false)?)
        } else {
            None
        };
        let canon = if self.kw("CANON") {
            if tick.is_none() || *canon_seen {
                return Err(self.err("CANON requires AT TICK and may appear only once"));
            }
            *canon_seen = true;
            Some(self.integer(true)?)
        } else {
            None
        };
        let yields = if self.kw("YIELD") {
            let mut items: Vec<(String, String)> = Vec::new();
            loop {
                self.element()?;
                let name = self.name()?;
                let alias = if self.kw("AS") {
                    self.name()?
                } else {
                    name.clone()
                };
                if items.iter().any(|(_, n)| n == &alias) {
                    return Err(self.err("duplicate YIELD column"));
                }
                items.push((name, alias));
                if !self.sym(",") {
                    break;
                }
            }
            items
        } else {
            source
                .columns()
                .iter()
                .map(|n| (n.to_string(), n.to_string()))
                .collect()
        };
        let predicate = if self.kw("WHERE") {
            Some(self.expression(0, 0)?)
        } else {
            None
        };
        Ok(Source {
            source,
            yields,
            predicate,
            tick,
            canon,
        })
    }
    fn projection(&mut self, with: bool) -> Result<Query, CypherError> {
        let distinct = self.kw("DISTINCT");
        let include_all = with && self.sym("*");
        let items = if include_all {
            if self.sym(",") {
                self.return_items(with)?
            } else {
                vec![]
            }
        } else {
            self.return_items(with)?
        };
        let order = self.order_by()?;
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
        Ok(Query {
            include_all,
            clauses: vec![],
            items,
            distinct,
            order,
            skip,
            limit,
        })
    }
    /// One MATCH clause after its keyword: paths, then `AT TICK`, `CANON` and
    /// `WHERE`. `canon_seen` records that some clause already set the canon.
    fn match_clause(
        &mut self,
        canon_seen: &mut bool,
        optional: bool,
    ) -> Result<Match, CypherError> {
        let mut paths = vec![self.path()?];
        while self.sym(",") {
            paths.push(self.path()?);
        }
        self.unique_edge_names(&paths)?;
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
            if *canon_seen {
                return Err(self.err("CANON may appear only once"));
            }
            *canon_seen = true;
            Some(self.integer(true)?)
        } else {
            None
        };
        let predicate = if self.kw("WHERE") {
            Some(self.expression(0, 0)?)
        } else {
            None
        };
        Ok(Match {
            optional,
            paths,
            tick,
            canon,
            predicate,
        })
    }
    /// Refuse a relationship variable that names two relationships in one clause.
    fn unique_edge_names(&self, paths: &[Path]) -> Result<(), CypherError> {
        // Cypher binds one relationship per variable within a clause; a later
        // clause may reuse it to mean the same edge.
        let mut edge_names = std::collections::BTreeSet::new();
        for name in paths
            .iter()
            .flat_map(|p| &p.edges)
            .filter_map(|e| e.name.as_ref())
        {
            if !edge_names.insert(name) {
                return Err(self.err(format!(
                    "relationship variable `{name}` is used for more than one relationship in one MATCH"
                )));
            }
        }
        Ok(())
    }
    /// The comma-separated RETURN items, each with a distinct column name.
    fn return_items(&mut self, with: bool) -> Result<Vec<Item>, CypherError> {
        let mut items = Vec::new();
        loop {
            self.element()?;
            let start = self.token().start;
            let expr = self.item()?;
            let end = self.tokens[self.at.saturating_sub(1)].end;
            let name = if self.kw("AS") {
                self.name()?
            } else {
                if with && !matches!(expr, Expr::Var(_)) {
                    return Err(self.err("WITH expressions require an alias"));
                }
                match &expr {
                    Expr::Var(v) => v.clone(),
                    Expr::Property { entity, key, .. } => format!("{entity}.{key}"),
                    _ => self.text[start..end].into(),
                }
            };
            if items.iter().any(|i: &Item| i.name == name) {
                return Err(CypherError::new(
                    self.text,
                    start,
                    ErrorKind::Syntax,
                    "Multiple result columns with the same name are not supported",
                ));
            }
            items.push(Item { expr, name });
            if !self.sym(",") {
                break;
            }
        }
        Ok(items)
    }
    /// An optional `ORDER BY` list: each key with whether it sorts descending.
    fn order_by(&mut self) -> Result<Vec<(Expr<String>, bool)>, CypherError> {
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
        Ok(order)
    }
    fn path(&mut self) -> Result<Path, CypherError> {
        let path_name = if matches!(self.token().kind, Kind::Ident(..)) {
            let n = self.name()?;
            self.need("=")?;
            Some(n)
        } else {
            None
        };
        let mut nodes = vec![self.node()?];
        let mut edges = Vec::new();
        while matches!(self.token().kind, Kind::Symbol("-") | Kind::Symbol("<-")) {
            self.element()?;
            let incoming = self.sym("<-");
            if !incoming {
                self.need("-")?;
            }
            self.need("[")?;
            let name = if matches!(self.token().kind, Kind::Ident(..)) {
                Some(self.name()?)
            } else {
                None
            };
            let kind = if self.sym(":") {
                self.name()?
            } else {
                String::new()
            };
            if matches!(self.token().kind, Kind::Symbol("|")) {
                return Err(self.err("multiple relationship types are not supported yet"));
            }
            let bounds = if self.sym("*") {
                Some(self.bounds()?)
            } else {
                None
            };
            if kind.is_empty() && bounds != Some((0, 0)) {
                return Err(self.err("exactly one relationship type is required"));
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
                bounds,
                name,
                kind,
                direction,
                props,
            });
            nodes.push(self.node()?);
        }
        Ok(Path {
            name: path_name,
            nodes,
            edges,
        })
    }
    fn bounds(&mut self) -> Result<(usize, usize), CypherError> {
        let message = "variable-length relationships need an upper bound of at most 10";
        let number = |p: &mut Self| -> Result<Option<usize>, CypherError> {
            if let Kind::Number(n) = &p.token().kind {
                let n = n.parse::<usize>().map_err(|_| p.err(message))?;
                p.bump();
                Ok(Some(n))
            } else {
                Ok(None)
            }
        };
        let first = number(self)?;
        let (min, max) = if self.sym("..") {
            (
                first.unwrap_or(1),
                number(self)?.ok_or_else(|| self.err(message))?,
            )
        } else {
            let n = first.ok_or_else(|| self.err(message))?;
            (n, n)
        };
        if max > kala_query::MAX_VAR_LENGTH {
            return Err(self.err(message));
        }
        if min > max {
            return Err(self.err("variable-length minimum exceeds maximum"));
        }
        Ok((min, max))
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
        self.item_depth(0)
    }
    fn item_depth(&mut self, depth: usize) -> Result<Expr<String>, CypherError> {
        if depth >= MAX_DEPTH {
            return Err(self.err("expression nesting depth limit exceeded"));
        }
        self.element()?;
        if self.sym("[") {
            let mut xs = Vec::new();
            if !self.sym("]") {
                loop {
                    xs.push(self.item_depth(depth + 1)?);
                    if self.sym("]") {
                        break;
                    }
                    self.need(",")?;
                }
            }
            return Ok(Expr::List(xs));
        }
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
                if self.sym("(") {
                    let name = s.to_ascii_lowercase();
                    let row_fn = match name.as_str() {
                        "length" => Some(kala_query::RowFn::Length),
                        "nodes" => Some(kala_query::RowFn::Nodes),
                        "relationships" => Some(kala_query::RowFn::Relationships),
                        "size" => Some(kala_query::RowFn::Size),
                        _ => None,
                    };
                    if let Some(f) = row_fn {
                        let e = self.item_depth(depth + 1)?;
                        self.need(")")?;
                        return Ok(Expr::Function(f, Box::new(e)));
                    }
                    let function = match name.as_str() {
                        "count" => BagFn::Count,
                        "collect" => BagFn::Collect,
                        "min" => BagFn::Min,
                        "max" => BagFn::Max,
                        "sum" => BagFn::Sum,
                        "avg" => BagFn::Avg,
                        _ => {
                            return Err(
                                self.err(format!("{s} functions/aggregates are not supported yet"))
                            );
                        }
                    };
                    let distinct = self.kw("DISTINCT");
                    let argument = if self.sym("*") {
                        if function != BagFn::Count || distinct {
                            return Err(self.err("only count(*) accepts *"));
                        }
                        None
                    } else {
                        Some(Box::new(self.item_depth(depth + 1)?))
                    };
                    self.need(")")?;
                    if argument.as_deref().is_some_and(has_aggregate) {
                        return Err(self.err("nested aggregates are not supported"));
                    }
                    return Ok(Expr::Aggregate {
                        function,
                        distinct,
                        argument,
                    });
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
        if (matches!(e, Expr::Param(_) | Expr::Property { .. })
            || self.write_values && matches!(e, Expr::Var(_)))
            && self.sym("[")
        {
            let index = self.item_depth(depth + 1)?;
            self.need("]")?;
            if !matches!(
                index,
                Expr::Var(_) | Expr::Param(_) | Expr::Value(_) | Expr::Null | Expr::Property { .. }
            ) {
                return Err(self.err("list offset must be an integer, variable or parameter"));
            }
            return Ok(Expr::ListIndex(Box::new(e), Box::new(index)));
        }
        Ok(e)
    }
    /// Boolean connectives (`OR` < `XOR` < `AND` < `NOT`) by precedence climbing.
    fn expression(&mut self, min: u8, depth: usize) -> Result<Expr<String>, CypherError> {
        if depth >= MAX_DEPTH {
            return Err(self.err("expression nesting depth limit exceeded"));
        }
        self.element()?;
        let mut left = if self.kw("NOT") {
            Expr::Not(Box::new(self.expression(4, depth + 1)?))
        } else {
            self.comparison(depth)?
        };
        loop {
            let precedence = match &self.token().kind {
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("OR") => 1,
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("XOR") => 2,
                Kind::Ident(s, false) if s.eq_ignore_ascii_case("AND") => 3,
                _ => break,
            };
            if precedence < min {
                break;
            }
            self.bump();
            let right = Box::new(self.expression(precedence + 1, depth + 1)?);
            let a = Box::new(left);
            left = match precedence {
                1 => Expr::Or(a, right),
                2 => Expr::Xor(a, right),
                _ => Expr::And(a, right),
            };
            self.check_depth(&left)?;
        }
        Ok(left)
    }
    fn comparison_op(&self) -> Option<CmpOp> {
        match &self.token().kind {
            Kind::Symbol("=") => Some(CmpOp::Eq),
            Kind::Symbol("<>") => Some(CmpOp::Ne),
            Kind::Symbol("<") => Some(CmpOp::Lt),
            Kind::Symbol("<=") => Some(CmpOp::Le),
            Kind::Symbol(">") => Some(CmpOp::Gt),
            Kind::Symbol(">=") => Some(CmpOp::Ge),
            _ => None,
        }
    }
    /// A comparison chain: `a < b <= c` means `a < b AND b <= c` (openCypher).
    /// A middle operand is copied into both comparisons. Each copy is charged
    /// (nodes plus string bytes) before it is made, against one budget for the
    /// whole query, so nested chains can't double their copies per level: the
    /// parse output stays linear in the input.
    fn comparison(&mut self, depth: usize) -> Result<Expr<String>, CypherError> {
        let mut left = self.postfix(depth)?;
        let Some(mut op) = self.comparison_op() else {
            return Ok(left);
        };
        self.bump();
        let mut right = self.postfix(depth)?;
        let mut chain: Option<Expr<String>> = None;
        loop {
            let Some(next) = self.comparison_op() else {
                let compare = Expr::Compare(op, Box::new(left), Box::new(right));
                let chain = match chain {
                    None => compare,
                    Some(c) => Expr::And(Box::new(c), Box::new(compare)),
                };
                self.check_depth(&chain)?;
                return Ok(chain);
            };
            self.bump();
            self.element()?;
            self.charge_copy(&right)?;
            let compare = Expr::Compare(op, Box::new(left), Box::new(right.clone()));
            let joined = match chain {
                None => compare,
                Some(c) => Expr::And(Box::new(c), Box::new(compare)),
            };
            self.check_depth(&joined)?;
            chain = Some(joined);
            left = right;
            op = next;
            right = self.postfix(depth)?;
        }
    }
    /// Charges a copy of `e` to the query's chain-copy budget, walking it
    /// iteratively (it is already depth-checked, but this stays stack-free).
    fn charge_copy(&mut self, e: &Expr<String>) -> Result<(), CypherError> {
        let mut stack = vec![e];
        while let Some(e) = stack.pop() {
            let bytes = match e {
                Expr::Var(v) => v.len(),
                Expr::Value(Value::Str(s)) => s.len(),
                Expr::Param(p) | Expr::RowParameter(p) => p.len(),
                Expr::RowField {
                    row,
                    parameter,
                    key,
                } => row.len() + parameter.len() + key.len(),
                Expr::Property { entity, key, .. } => entity.len() + key.len(),
                Expr::Value(_) | Expr::Null => 0,
                Expr::Path { start, segments } => {
                    start.len() + segments.iter().map(String::len).sum::<usize>()
                }
                Expr::List(xs) => {
                    stack.extend(xs);
                    0
                }
                Expr::Aggregate { argument, .. } => {
                    stack.extend(argument.as_deref());
                    0
                }
                Expr::Not(e) | Expr::IsNull(e) | Expr::Function(_, e) => {
                    stack.push(e);
                    0
                }
                Expr::ListIndex(a, b)
                | Expr::Disjoint(a, b)
                | Expr::Compare(_, a, b)
                | Expr::And(a, b)
                | Expr::Or(a, b)
                | Expr::Xor(a, b) => {
                    stack.push(a);
                    stack.push(b);
                    0
                }
            };
            self.copied = self.copied.saturating_add(1 + bytes);
            if self.copied > MAX_CHAIN_COPY {
                return Err(self.err("comparison chain operands are too large to repeat"));
            }
        }
        Ok(())
    }
    /// An operand with its `IS [NOT] NULL` suffixes, which bind tighter than comparisons.
    fn postfix(&mut self, depth: usize) -> Result<Expr<String>, CypherError> {
        let mut e = if self.sym("(") {
            let e = self.expression(0, depth + 1)?;
            self.need(")")?;
            e
        } else {
            self.item()?
        };
        while self.kw("IS") {
            self.element()?;
            let not = self.kw("NOT");
            self.need_kw("NULL")?;
            e = Expr::IsNull(Box::new(e));
            if not {
                e = Expr::Not(Box::new(e));
            }
            self.check_depth(&e)?;
        }
        Ok(e)
    }
    fn check_depth(&self, expr: &Expr<String>) -> Result<(), CypherError> {
        let mut stack = vec![(expr, 1)];
        while let Some((e, d)) = stack.pop() {
            if d >= MAX_DEPTH {
                return Err(self.err("expression nesting depth limit exceeded"));
            }
            match e {
                Expr::Not(e) | Expr::IsNull(e) | Expr::Function(_, e) => stack.push((e, d + 1)),
                Expr::List(xs) => stack.extend(xs.iter().map(|e| (e, d + 1))),
                Expr::Aggregate { argument, .. } => {
                    stack.extend(argument.as_deref().map(|e| (e, d + 1)))
                }
                Expr::ListIndex(a, b)
                | Expr::Disjoint(a, b)
                | Expr::Compare(_, a, b)
                | Expr::And(a, b)
                | Expr::Or(a, b)
                | Expr::Xor(a, b) => {
                    stack.push((a, d + 1));
                    stack.push((b, d + 1));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(crate) fn has_aggregate(e: &Expr<String>) -> bool {
    match e {
        Expr::Aggregate { .. } => true,
        Expr::List(xs) => xs.iter().any(has_aggregate),
        Expr::Not(x) | Expr::IsNull(x) | Expr::Function(_, x) => has_aggregate(x),
        Expr::ListIndex(a, b)
        | Expr::Compare(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b) => has_aggregate(a) || has_aggregate(b),
        _ => false,
    }
}

mod writes;
