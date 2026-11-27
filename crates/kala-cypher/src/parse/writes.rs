//! Write clauses share the read parser's tokens, expressions, pattern and depth budgets.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

const NARROW_MERGE: &str = "MERGE matches nodes by kala_handle only; put other properties in ON CREATE SET / ON MATCH SET or use MATCH … SET";

impl Parser<'_> {
    pub(super) fn is_kw(&self, word: &str) -> bool {
        matches!(&self.token().kind, Kind::Ident(s,false) if s.eq_ignore_ascii_case(word))
    }

    pub(super) fn updates(
        &mut self,
        mut clauses: Vec<Clause>,
        canon: bool,
        pipeline_tick: Option<Term>,
    ) -> Result<Statement, CypherError> {
        if canon {
            return Err(self.err("CANON is not allowed in a write statement"));
        }
        if clauses.iter().any(|c| matches!(c, Clause::Source(_))) {
            return Err(
                self.err("a procedure or server statement cannot be followed by write clauses")
            );
        }
        if clauses.is_empty()
            && self.is_kw("SET")
            && matches!(self.tokens.get(self.at+1).map(|t| &t.kind),Some(Kind::Ident(s,false)) if s.eq_ignore_ascii_case("CONCURRENCY_MODE"))
            && matches!(
                self.tokens.get(self.at + 2).map(|t| &t.kind),
                Some(Kind::Symbol("="))
            )
        {
            self.bump();
            self.bump();
            self.bump();
            let mode = match self.bump().kind {
                Kind::String(s) if s == "weak" => kala_core::Concurrency::Weak,
                Kind::String(s) if s == "optimistic" => kala_core::Concurrency::Optimistic,
                _ => return Err(self.err("CONCURRENCY_MODE must be 'weak' or 'optimistic'")),
            };
            self.write_end()?;
            return Ok(Statement::Concurrency(mode));
        }
        if clauses.is_empty()
            && self.is_kw("CREATE")
            && matches!(self.tokens.get(self.at + 1).map(|t| &t.kind), Some(Kind::Ident(s, false)) if s.eq_ignore_ascii_case("CONSTRAINT"))
        {
            self.bump();
            self.bump();
            return self.constraint();
        }
        let mut bindings = read_bindings(&clauses);
        let read_names: Vec<_> = bindings.keys().cloned().collect();
        self.write_values = true;
        let writes = self.update_clauses(&mut bindings)?;
        self.write_values = false;
        let explicit_write_tick = if self.kw("AT") {
            self.need_kw("TICK")?;
            Some(self.integer(false)?)
        } else {
            None
        };
        let local_tick = clauses.iter().rev().find_map(|c| {
            if let Clause::Match(m) = c {
                m.tick.clone()
            } else {
                None
            }
        });
        let read_tick = pipeline_tick.or(local_tick);
        for c in &mut clauses {
            if let Clause::Match(m) = c
                && m.tick.is_none()
            {
                m.tick = read_tick.as_ref().or(explicit_write_tick.as_ref()).cloned();
            }
        }
        let tick = explicit_write_tick.or(read_tick);
        if self.is_kw("CANON") {
            return Err(self.err("CANON is not allowed in a write statement"));
        }
        let returns = if self.kw("RETURN") {
            if self.is_kw("DISTINCT") {
                return Err(self.err("RETURN DISTINCT after writes is not supported yet"));
            }
            let items = self.return_items(false)?;
            for i in &items {
                match &i.expr {
                    Expr::Var(n) | Expr::Property {entity:n,..} => {
                        if self.is_bound(n, &bindings)? {
                            return Err(self.err("returning a path after writes is not supported yet"));
                        }
                    }
                    Expr::Param(_) | Expr::Value(_) | Expr::Null => {}
                    _ => return Err(self.err("aggregates and computed expressions in RETURN after writes are not supported yet")),
                }
            }
            items
        } else {
            vec![]
        };
        self.write_end()?;
        let read = Query {
            include_all: false,
            clauses,
            items: read_names
                .into_iter()
                .map(|name| Item {
                    expr: Expr::Var(name.clone()),
                    name,
                })
                .collect(),
            distinct: false,
            order: vec![],
            skip: None,
            limit: None,
        };
        let mut params = vec![];
        for t in &self.tokens {
            if let Kind::Param(p) = &t.kind
                && !params.contains(p)
            {
                params.push(p.clone());
            }
        }
        Ok(Statement::Write(WriteStatement {
            read,
            writes,
            tick,
            returns,
            params,
        }))
    }
    /// `[name] [IF NOT EXISTS] FOR (n:Label) REQUIRE n.prop IS :: TYPE`
    /// (or `IS TYPED TYPE`), after `CREATE CONSTRAINT`. The name is accepted
    /// and ignored; declaring twice is a no-op with or without IF NOT EXISTS.
    fn constraint(&mut self) -> Result<Statement, CypherError> {
        const ONLY: &str = "only property type constraints are supported: CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:Label) REQUIRE n.prop IS :: INTEGER | FLOAT | STRING | BOOLEAN";
        if !self.is_kw("IF") && !self.is_kw("FOR") {
            self.name()?;
        }
        let if_not_exists = self.kw("IF");
        if if_not_exists {
            self.need_kw("NOT")?;
            self.need_kw("EXISTS")?;
        }
        self.need_kw("FOR")?;
        self.need("(")?;
        if matches!(self.token().kind, Kind::Symbol(")")) {
            return Err(self.err(format!(
                "relationship constraints are not supported; {ONLY}"
            )));
        }
        let var = self.name()?;
        if !self.sym(":") {
            return Err(self.err(ONLY));
        }
        let label = self.name()?;
        if !self.sym(")") {
            return Err(self.err(ONLY));
        }
        if matches!(self.token().kind, Kind::Symbol("-" | "<")) {
            return Err(self.err(format!(
                "relationship constraints are not supported; {ONLY}"
            )));
        }
        self.need_kw("REQUIRE")?;
        if matches!(self.token().kind, Kind::Symbol("(")) {
            return Err(self.err(format!(
                "key and composite constraints are not supported; {ONLY}"
            )));
        }
        let subject = self.name()?;
        if subject != var {
            return Err(self.err(format!(
                "Variable `{subject}` not defined (unbound variable)"
            )));
        }
        self.need(".")?;
        let attribute = self.name()?;
        self.need_kw("IS")?;
        if self.kw("UNIQUE") || self.is_kw("NOT") || self.is_kw("NODE") || self.is_kw("KEY") {
            return Err(self.err(format!(
                "uniqueness, existence and key constraints are not supported; {ONLY}"
            )));
        }
        if !self.kw("TYPED") {
            self.need(":")?;
            self.need(":")?;
        }
        let name = self.name()?;
        let value_type = match name.to_ascii_uppercase().as_str() {
            "INTEGER" | "INT" => kala_core::ValueType::I64,
            "FLOAT" => kala_core::ValueType::F64,
            "STRING" => kala_core::ValueType::Str,
            "BOOLEAN" | "BOOL" => kala_core::ValueType::Bool,
            _ => {
                return Err(
                    self.err("a property type constraint takes INTEGER, FLOAT, STRING or BOOLEAN")
                );
            }
        };
        if attribute == "kala_handle" {
            return Err(self.err("kala_handle is built in and cannot be declared"));
        }
        self.write_end()?;
        Ok(Statement::Write(WriteStatement {
            read: Query {
                include_all: false,
                clauses: vec![],
                items: vec![],
                distinct: false,
                order: vec![],
                skip: None,
                limit: None,
            },
            writes: vec![Update::Constraint {
                label,
                attribute,
                value_type,
                if_not_exists,
            }],
            tick: None,
            returns: vec![],
            params: vec![],
        }))
    }
    fn update_clauses(
        &mut self,
        bindings: &mut BTreeMap<String, bool>,
    ) -> Result<Vec<Update>, CypherError> {
        let mut writes = Vec::new();
        loop {
            self.element()?;
            let clause = if self.kw("CREATE") {
                if self.is_kw("CONSTRAINT") {
                    return Err(self.err(
                        "CREATE CONSTRAINT must be a statement of its own (no reading part, no other clause)",
                    ));
                }
                if self.is_kw("INDEX") {
                    return Err(self.err(
                        "CREATE INDEX is not supported: Kala indexes every attribute itself (declare one with CREATE CONSTRAINT … IS :: TYPE or // kala:datalog [:attribute …])",
                    ));
                }
                let mut paths = vec![];
                loop {
                    let path = self.path()?;
                    self.write_path(&path, false, bindings)?;
                    paths.push(path);
                    if !self.sym(",") {
                        break;
                    }
                }
                Update::Create(paths)
            } else if self.kw("MERGE") {
                let path = self.path()?;
                self.write_path(&path, true, bindings)?;
                let (mut created, mut matched) = (None, None);
                while self.kw("ON") {
                    let slot = if self.kw("CREATE") {
                        &mut created
                    } else {
                        self.need_kw("MATCH")?;
                        &mut matched
                    };
                    if slot.is_some() {
                        return Err(self.err("repeated ON CREATE/MATCH SET clause"));
                    }
                    self.need_kw("SET")?;
                    *slot = Some(self.assignments(false, bindings)?);
                }
                Update::Merge {
                    path,
                    created: created.unwrap_or_default(),
                    matched: matched.unwrap_or_default(),
                }
            } else if self.kw("SET") {
                Update::Assign(self.assignments(false, bindings)?)
            } else if self.kw("REMOVE") {
                Update::Assign(self.assignments(true, bindings)?)
            } else if self.is_kw("DELETE") || self.is_kw("DETACH") {
                let detach = self.kw("DETACH");
                self.need_kw("DELETE")?;
                let mut names = vec![];
                loop {
                    self.element()?;
                    let n = self.name()?;
                    if self.is_bound(&n, bindings)? {
                        return Err(self.err("DELETE of a path is not supported yet"));
                    }
                    names.push(n);
                    if !self.sym(",") {
                        break;
                    }
                }
                Update::Delete { names, detach }
            } else {
                break;
            };
            writes.push(clause);
        }
        Ok(writes)
    }
    fn write_end(&mut self) -> Result<(), CypherError> {
        self.sym(";");
        if matches!(self.token().kind, Kind::End) {
            Ok(())
        } else {
            Err(self.unsupported("end of write statement"))
        }
    }
    fn is_bound(&self, n: &str, bindings: &BTreeMap<String, bool>) -> Result<bool, CypherError> {
        bindings
            .get(n)
            .copied()
            .ok_or_else(|| self.err(format!("Variable `{n}` not defined (unbound variable)")))
    }
    fn update_expr(
        &self,
        e: &Expr<String>,
        bindings: &BTreeMap<String, bool>,
    ) -> Result<(), CypherError> {
        match e {
            Expr::Var(n) | Expr::Property { entity: n, .. } => {
                self.is_bound(n, bindings)?;
            }
            Expr::List(xs) => {
                for x in xs {
                    self.update_expr(x, bindings)?;
                }
            }
            Expr::ListIndex(list, index) => {
                self.update_expr(list, bindings)?;
                self.update_expr(index, bindings)?;
            }
            Expr::Value(_) | Expr::Null | Expr::Param(_) => {}
            _ => return Err(self.err("computed write values are not supported yet")),
        }
        Ok(())
    }
    fn assignments(
        &mut self,
        remove: bool,
        bindings: &BTreeMap<String, bool>,
    ) -> Result<Vec<Assignment>, CypherError> {
        let mut items = vec![];
        loop {
            self.element()?;
            let var = self.name()?;
            self.is_bound(&var, bindings)?;
            if self.sym(".") {
                let key = self.name()?;
                let expr = if remove {
                    None
                } else {
                    self.need("=")?;
                    Some(self.item()?)
                };
                if let Some(e) = &expr {
                    self.update_expr(e, bindings)?;
                }
                items.push(Assignment::Property(var, key, expr));
            } else if self.sym(":") {
                loop {
                    self.element()?;
                    items.push(Assignment::Label(var.clone(), self.name()?, !remove));
                    if !self.sym(":") {
                        break;
                    }
                }
            } else if !remove && self.sym("+") {
                self.need("=")?;
                if !matches!(self.token().kind, Kind::Symbol("{")) {
                    return Err(self.err("SET += requires a property map"));
                }
                for (key, e) in self.props()? {
                    self.update_expr(&e, bindings)?;
                    items.push(Assignment::Property(var.clone(), key, Some(e)));
                }
            } else if !remove && self.sym("=") {
                return Err(
                    self.err("SET n = {…} is not supported yet; set the properties you mean")
                );
            } else {
                return Err(self.err("a property or label is required"));
            }
            if !self.sym(",") {
                break;
            }
        }
        Ok(items)
    }
    fn write_path(
        &self,
        path: &Path,
        merge: bool,
        bindings: &mut BTreeMap<String, bool>,
    ) -> Result<(), CypherError> {
        if path.name.is_some()
            || path.edges.iter().any(|e| e.bounds.is_some())
            || (merge && path.edges.len() > 1)
        {
            return Err(self.err("this CREATE/MERGE path is not supported yet"));
        }
        let rel_merge = merge && !path.edges.is_empty();
        if merge && !rel_merge {
            let n = &path.nodes[0];
            if n.labels.len() != 1 || n.props.len() != 1 || n.props[0].0 != "kala_handle" {
                return Err(self.err(NARROW_MERGE));
            }
        }
        for node in &path.nodes {
            let bound = node.name.as_ref().is_some_and(|n| bindings.contains_key(n));
            if rel_merge && (!bound || !node.labels.is_empty() || !node.props.is_empty()) {
                return Err(self.err(
                    "MERGE with unbound or decorated relationship endpoints is not supported yet",
                ));
            }
            if bound && (path.edges.is_empty() || !node.labels.is_empty() || !node.props.is_empty())
            {
                return Err(self.err("CREATE/MERGE cannot redeclare a bound node"));
            }
            if !bound && node.labels.is_empty() {
                return Err(self.err("CREATE needs a node type as its first label"));
            }
            let mut keys = BTreeSet::new();
            for (key, e) in &node.props {
                if !keys.insert(key) {
                    return Err(self.err("duplicate property in a write pattern"));
                }
                self.update_expr(e, bindings)?;
            }
            if let Some(n) = &node.name {
                bindings.insert(n.clone(), false);
            }
        }
        for edge in &path.edges {
            if edge.direction == Orientation::Either {
                return Err(self.err("CREATE/MERGE requires a relationship direction"));
            }
            if merge && !edge.props.is_empty() {
                return Err(self.err(
                    "MERGE relationship properties are not supported; use ON CREATE SET or SET",
                ));
            }
            for (_, e) in &edge.props {
                self.update_expr(e, bindings)?;
            }
            if let Some(n) = &edge.name
                && bindings.insert(n.clone(), false).is_some()
            {
                return Err(self.err("relationship variable already bound"));
            }
        }
        Ok(())
    }
}

/// Visible variable names after MATCH, WITH and UNWIND, with path identities
/// retained so DELETE can reject a path even when no rows match.
fn read_bindings(clauses: &[Clause]) -> BTreeMap<String, bool> {
    let mut bindings = BTreeMap::new();
    for c in clauses {
        match c {
            Clause::Match(m) => {
                for p in &m.paths {
                    bindings.extend(p.name.iter().map(|s| (s.clone(), true)));
                    bindings.extend(
                        p.nodes
                            .iter()
                            .filter_map(|n| n.name.clone().map(|s| (s, false))),
                    );
                    bindings.extend(
                        p.edges
                            .iter()
                            .filter_map(|e| e.name.clone().map(|s| (s, false))),
                    );
                }
            }
            Clause::With(q, _) => {
                let mut names = if q.include_all {
                    bindings.clone()
                } else {
                    BTreeMap::new()
                };
                for i in &q.items {
                    names.insert(
                        i.name.clone(),
                        matches!(&i.expr,Expr::Var(n) if bindings.get(n)==Some(&true)),
                    );
                }
                bindings = names;
            }
            Clause::Unwind(_, n) => {
                bindings.insert(n.clone(), false);
            }
            Clause::Source(_) => unreachable!(),
        }
    }
    bindings
}
