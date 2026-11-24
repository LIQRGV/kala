//! Bind names and per-clause ticks, then emit the shared logical operators.
use crate::{CypherError, ErrorKind, ast::*};
use kala_core::{Tick, Value, ValueType, World};
use kala_query::{
    Access, AttrRef, CanonSource, CmpOp, Expr, Op, Orientation, OutputSpec, ParamKind, Plan,
    Prepared, RowAction, Term, TickSource, Var,
};
use std::collections::{BTreeMap, BTreeSet};
pub(crate) fn compile(world: &World, text: &str, q: &Query) -> Result<Prepared, CypherError> {
    Compiler {
        world,
        text,
        root: Op::Unit,
        bindings: BTreeMap::new(),
        params: Vec::new(),
        serial: 0,
        canon: CanonSource::Latest,
        bound: BTreeSet::new(),
        used: BTreeSet::new(),
        ordinal: None,
        nullable: false,
        unwind_params: BTreeMap::new(),
        list_params: BTreeSet::new(),
    }
    .run(q)
}
struct Compiler<'a> {
    world: &'a World,
    ordinal: Option<Var>,
    nullable: bool,
    unwind_params: BTreeMap<String, String>,
    list_params: BTreeSet<String>,
    text: &'a str,
    root: Op,
    bindings: BTreeMap<String, (Var, TickSource, bool)>,
    params: Vec<(String, ParamKind)>,
    serial: usize,
    used: BTreeSet<String>,
    canon: CanonSource,
    /// Node variables some earlier operator already binds.
    bound: BTreeSet<Var>,
}
impl Compiler<'_> {
    fn error(&self, s: impl Into<String>) -> CypherError {
        CypherError::new(self.text, 0, ErrorKind::Syntax, s)
    }
    fn fresh(&mut self) -> Var {
        loop {
            self.serial += 1;
            let name = format!("\0{}", self.serial);
            if self.used.insert(name.clone()) {
                return Var(name);
            }
        }
    }
    fn row(&mut self, action: RowAction) {
        self.root = Op::Row {
            input: Box::new(std::mem::replace(&mut self.root, Op::Unit)),
            action,
        };
    }
    /// Bind `expr` to a fresh variable and return it.
    fn bind_fresh(&mut self, expr: Expr) -> Var {
        let var = self.fresh();
        self.row(RowAction::Bind {
            var: var.clone(),
            expr,
        });
        var
    }
    /// Append an index expansion to the pipeline.
    fn expand(
        &mut self,
        entity: Term,
        attr: AttrRef,
        value: Term,
        tick: TickSource,
        access: Access,
    ) {
        self.root = Op::Expand {
            input: Box::new(std::mem::replace(&mut self.root, Op::Unit)),
            entity,
            attr,
            value,
            tick,
            access,
        };
    }
    fn param(&mut self, p: &str, kind: ParamKind) -> Result<(), CypherError> {
        if let Some((_, old)) = self.params.iter_mut().find(|(n, _)| n == p) {
            if *old == ParamKind::Scalar {
                *old = kind;
            } else if kind != ParamKind::Scalar && *old != kind {
                let integer = |k: &ParamKind| {
                    matches!(
                        k,
                        ParamKind::Tick | ParamKind::Canon | ParamKind::Value(ValueType::I64)
                    )
                };
                if !(integer(old) && integer(&kind)) {
                    return Err(CypherError::new(
                        self.text,
                        0,
                        ErrorKind::Type,
                        "incompatible parameter uses",
                    ));
                }
            }
        } else {
            self.params.push((p.into(), kind));
        }
        Ok(())
    }
    fn time(&mut self, t: &Term, kind: ParamKind) -> Result<TickSource, CypherError> {
        match t {
            Term::Const(Value::I64(n)) => Ok(TickSource::Const(Tick(*n))),
            Term::Param(p) => {
                self.param(p, kind)?;
                Ok(TickSource::Param(p.clone()))
            }
            _ => Err(self.error("integer time expected")),
        }
    }
    fn bind_name(
        &mut self,
        name: &Option<String>,
        tick: &TickSource,
        edge: bool,
    ) -> Result<Var, CypherError> {
        if let Some(name) = name {
            if let Some((v, _, is_edge)) = self.bindings.get(name) {
                if *is_edge != edge {
                    return Err(self.error("a variable cannot be both a node and a relationship"));
                }
                return Ok(v.clone());
            }
            let v = Var(name.clone());
            self.bindings
                .insert(name.clone(), (v.clone(), tick.clone(), edge));
            Ok(v)
        } else {
            Ok(self.fresh())
        }
    }
    fn expr(&mut self, e: &Expr<String>) -> Result<Expr, CypherError> {
        Ok(match e {
            Expr::Var(n) => Expr::Var(
                self.bindings
                    .get(n)
                    .ok_or_else(|| {
                        self.error(format!("Variable `{n}` not defined (unbound variable)"))
                    })?
                    .0
                    .clone(),
            ),
            Expr::Value(v) => Expr::Value(v.clone()),
            Expr::Null => Expr::Null,
            Expr::Function(f, e) => Expr::Function(*f, Box::new(self.expr(e)?)),
            Expr::Disjoint(a, b) => {
                Expr::Disjoint(Box::new(self.expr(a)?), Box::new(self.expr(b)?))
            }
            Expr::Path { .. } => {
                return Err(self.error("path expressions must originate in a pattern"));
            }
            Expr::List(xs) => {
                Expr::List(xs.iter().map(|e| self.expr(e)).collect::<Result<_, _>>()?)
            }
            Expr::Aggregate { .. } => {
                return Err(self.error("aggregates are only allowed as projection items"));
            }
            Expr::Param(p) => {
                self.param(p, ParamKind::Scalar)?;
                Expr::Param(p.clone())
            }
            Expr::Property { entity, key, .. } => {
                let (var, tick, _) = self.bindings.get(entity).ok_or_else(|| {
                    self.error(format!(
                        "Variable `{entity}` not defined (unbound variable)"
                    ))
                })?;
                Expr::Property {
                    entity: var.clone(),
                    key: key.clone(),
                    tick: tick.clone(),
                }
            }
            Expr::Compare(op, a, b) => {
                self.infer_comparison(a, b)?;
                self.infer_comparison(b, a)?;
                Expr::Compare(*op, Box::new(self.expr(a)?), Box::new(self.expr(b)?))
            }
            Expr::And(a, b) => Expr::And(Box::new(self.expr(a)?), Box::new(self.expr(b)?)),
            Expr::Or(a, b) => Expr::Or(Box::new(self.expr(a)?), Box::new(self.expr(b)?)),
            Expr::Xor(a, b) => Expr::Xor(Box::new(self.expr(a)?), Box::new(self.expr(b)?)),
            Expr::Not(e) => Expr::Not(Box::new(self.expr(e)?)),
            Expr::IsNull(e) => Expr::IsNull(Box::new(self.expr(e)?)),
        })
    }
    fn infer_comparison(&mut self, a: &Expr<String>, b: &Expr<String>) -> Result<(), CypherError> {
        let parameter = match b {
            Expr::Param(p) => Some(p.clone()),
            Expr::Var(v) => self.unwind_params.get(v).cloned(),
            _ => None,
        };
        if let Some(p) = parameter {
            let ty = match a {
                Expr::Property { key, .. } => {
                    if key == "kala_handle" {
                        Some(ValueType::Str)
                    } else {
                        self.world.attribute(key).map(|d| d.value_type)
                    }
                }
                Expr::Value(v) => Some(v.value_type()),
                _ => None,
            };
            if let Some(ty) = ty {
                self.param(&p, ParamKind::Value(ty))?;
            }
        }
        Ok(())
    }
    fn property_map(
        &mut self,
        var: &Var,
        tick: &TickSource,
        props: &[(String, Expr<String>)],
    ) -> Result<(), CypherError> {
        for (key, value) in props {
            let parameter = match value {
                Expr::Param(p) => Some(p.clone()),
                Expr::Var(v) => self.unwind_params.get(v).cloned(),
                _ => None,
            };
            if let Some(p) = parameter {
                let kind = if key == "kala_handle" {
                    ParamKind::Value(ValueType::Str)
                } else {
                    self.world
                        .attribute(key)
                        .map_or(ParamKind::Scalar, |d| ParamKind::Value(d.value_type))
                };
                self.param(&p, kind)?;
            }
            let rhs = self.expr(value)?;
            self.row(RowAction::Select(Expr::Compare(
                CmpOp::Eq,
                Box::new(Expr::Property {
                    entity: var.clone(),
                    key: key.clone(),
                    tick: tick.clone(),
                }),
                Box::new(rhs),
            )));
        }
        Ok(())
    }
    /// Compile the query clause by clause, then wrap the row pipeline in its
    /// output operator.
    fn run(mut self, q: &Query) -> Result<Prepared, CypherError> {
        // Reserve every authored name, including quoted NUL-prefixed ones.
        self.reserve_names(q);
        if q.clauses.iter().any(|c| {
            matches!(
                c,
                Clause::With(..) | Clause::Unwind(..) | Clause::Source(..)
            )
        }) {
            let ordinal = self.bind_fresh(Expr::Value(Value::I64(0)));
            self.ordinal = Some(ordinal);
        }
        let first_tick = q
            .clauses
            .iter()
            .find_map(|c| match c {
                Clause::Match(m) => Some(m.tick.as_ref()),
                Clause::Source(s) => Some(s.tick.as_ref()),
                _ => None,
            })
            .flatten();
        let default_tick = if let Some(t) = first_tick {
            self.time(t, ParamKind::Tick)?
        } else {
            TickSource::Default
        };
        self.resolve_canon(q)?;
        let mut match_index = 0;
        for clause in &q.clauses {
            match clause {
                Clause::Match(m) => {
                    self.match_clause(match_index, m)?;
                    match_index += 1;
                }
                Clause::With(q, predicate) => self.with_clause(q, predicate.as_ref())?,
                Clause::Source(s) => {
                    self.source_clause(s)?;
                    match_index += 1;
                }
                Clause::Unwind(e, name) => {
                    if self.bindings.contains_key(name) {
                        return Err(self.error("UNWIND variable already declared"));
                    }
                    let expr = self.expr(e)?;
                    let var = self.fresh();
                    self.row(RowAction::Unwind {
                        expr,
                        var: var.clone(),
                        ordinal: self.ordinal.clone().expect("pipeline ordinal"),
                    });
                    self.bindings
                        .insert(name.clone(), (var, TickSource::Default, false));
                    let parameter = match e {
                        Expr::Param(p) => Some(p.clone()),
                        Expr::Var(v) => self.unwind_params.get(v).cloned(),
                        _ => None,
                    };
                    if let Some(p) = parameter {
                        self.list_params.insert(p.clone());
                        self.unwind_params.insert(name.clone(), p);
                    }
                }
            }
        }
        let returned = self.return_items(q)?;
        let order = self.order_keys(q, &returned)?;
        for t in [&q.skip, &q.limit].into_iter().flatten() {
            if let Term::Param(p) = t {
                self.param(p, ParamKind::Value(ValueType::I64))?;
            }
        }
        for (name, kind) in self.params.clone() {
            if let ParamKind::Value(expected) = kind
                && self.list_params.contains(&name)
            {
                self.row(RowAction::CheckParameter { name, expected });
            }
        }
        self.output(q, returned.find, order);
        // Keep execution slots separate from column labels; aliases can shadow input names.
        let columns = q.items.iter().map(|i| Var(i.name.clone())).collect();
        let plan = Plan {
            find: columns,
            params: self.params.iter().map(|(n, _)| n.clone()).collect(),
            default_tick,
            canon: self.canon,
            root: self.root,
        };
        Ok(Prepared {
            plan,
            params: self.params,
        })
    }
    fn reserve_names(&mut self, q: &Query) {
        for clause in &q.clauses {
            match clause {
                Clause::Match(m) => {
                    for p in &m.paths {
                        self.used.extend(p.name.clone());
                        self.used
                            .extend(p.nodes.iter().filter_map(|n| n.name.clone()));
                        self.used
                            .extend(p.edges.iter().filter_map(|n| n.name.clone()));
                    }
                }
                Clause::With(q, _) => self.reserve_names(q),
                Clause::Source(s) => self.used.extend(s.yields.iter().map(|(_, n)| n.clone())),
                Clause::Unwind(_, n) => {
                    self.used.insert(n.clone());
                }
            }
        }
        self.used.extend(q.items.iter().map(|i| i.name.clone()));
    }
    fn source_clause(&mut self, source: &Source) -> Result<(), CypherError> {
        let columns: Vec<_> = source
            .source
            .columns()
            .iter()
            .map(|_| self.fresh())
            .collect();
        let tick = if let Some(t) = &source.tick {
            self.time(t, ParamKind::Tick)?
        } else {
            TickSource::Default
        };
        self.row(RowAction::Source {
            source: source.source.clone(),
            columns: columns.clone(),
            tick: tick.clone(),
            ordinal: self.ordinal.clone().expect("source ordinal"),
        });
        for (name, alias) in &source.yields {
            let i = source
                .source
                .columns()
                .iter()
                .position(|n| *n == name)
                .ok_or_else(|| self.error(format!("unknown YIELD column {name}")))?;
            if self.bindings.contains_key(alias) {
                return Err(self.error("YIELD variable already declared"));
            }
            self.bindings
                .insert(alias.clone(), (columns[i].clone(), tick.clone(), false));
        }
        if let Some(e) = &source.predicate {
            let e = self.expr(e)?;
            self.row(RowAction::Select(e));
        }
        Ok(())
    }
    fn with_clause(
        &mut self,
        projection: &Query,
        predicate: Option<&Expr<String>>,
    ) -> Result<(), CypherError> {
        let mut expanded;
        let q = if projection.include_all {
            expanded = projection.clone();
            expanded.items = self
                .bindings
                .keys()
                .map(|n| Item {
                    name: n.clone(),
                    expr: Expr::Var(n.clone()),
                })
                .collect();
            for item in &projection.items {
                if expanded.items.iter().any(|i| i.name == item.name) {
                    return Err(
                        self.error("Multiple result columns with the same name are not supported")
                    );
                }
                expanded.items.push(item.clone());
            }
            &expanded
        } else {
            projection
        };
        let returned = self.return_items(q)?;
        let order = self.order_keys(q, &returned)?;
        for t in [&q.skip, &q.limit].into_iter().flatten() {
            if let Term::Param(p) = t {
                self.param(p, ParamKind::Value(ValueType::I64))?;
            }
        }
        let mut bindings = BTreeMap::new();
        let mut unwind_params = BTreeMap::new();
        for (item, var) in q.items.iter().zip(&returned.find) {
            if let Expr::Param(p) = &item.expr {
                unwind_params.insert(item.name.clone(), p.clone());
            }
            let (tick, edge) = if let Expr::Var(n) = &item.expr {
                if let Some(p) = self.unwind_params.get(n) {
                    unwind_params.insert(item.name.clone(), p.clone());
                }
                self.bindings
                    .get(n)
                    .map(|(_, t, e)| (t.clone(), *e))
                    .unwrap_or((TickSource::Default, false))
            } else {
                (TickSource::Default, false)
            };
            bindings.insert(item.name.clone(), (var.clone(), tick, edge));
        }
        self.row(RowAction::Project {
            vars: returned.find.clone(),
            distinct: q.distinct,
            order,
            skip: q.skip.clone(),
            limit: q.limit.clone(),
            ordinal: self.ordinal.clone().expect("WITH ordinal"),
        });
        self.bindings = bindings;
        self.unwind_params = unwind_params;
        self.bound = returned.find.into_iter().collect();
        if let Some(e) = predicate {
            let e = self.expr(e)?;
            self.row(RowAction::Select(e));
        }
        Ok(())
    }
    /// A MATCH clause's own `AT TICK`, or the query default when it has none.
    fn clause_tick(&mut self, m: &Match) -> Result<TickSource, CypherError> {
        Ok(match &m.tick {
            Some(t) => self.time(t, ParamKind::Tick)?,
            None => TickSource::Default,
        })
    }
    /// The query's canon: the `CANON` of whichever MATCH carries one.
    fn resolve_canon(&mut self, q: &Query) -> Result<(), CypherError> {
        for clause in &q.clauses {
            let canon = match clause {
                Clause::Match(m) => m.canon.as_ref(),
                Clause::Source(s) => s.canon.as_ref(),
                _ => None,
            };
            if let Some(t) = canon {
                self.canon = match t {
                    Term::Const(Value::I64(n)) => CanonSource::Const(*n as u64),
                    Term::Param(p) => {
                        self.param(p, ParamKind::Canon)?;
                        CanonSource::Param(p.clone())
                    }
                    _ => return Err(self.error("integer canon expected")),
                };
            }
        }
        Ok(())
    }
    /// One MATCH clause: its paths, then its WHERE. The first clause's tick
    /// is the query default, so its operators read at `Default`.
    fn match_clause(&mut self, index: usize, m: &Match) -> Result<(), CypherError> {
        let tick = if index == 0 {
            TickSource::Default
        } else {
            self.clause_tick(m)?
        };
        self.nullable |= m.optional;
        let input = if m.optional {
            Some(Box::new(std::mem::replace(&mut self.root, Op::Unit)))
        } else {
            None
        };
        let mut scope = MatchScope {
            tick,
            edge_count: if m
                .paths
                .iter()
                .any(|p| p.name.is_some() || p.edges.iter().any(|e| e.bounds.is_some()))
            {
                2
            } else {
                m.paths.iter().map(|p| p.edges.len()).sum()
            },
            previous_edges: Vec::new(),
        };
        for path in &m.paths {
            self.path(path, &mut scope)?;
        }
        if let Some(e) = &m.predicate {
            let e = self.expr(e)?;
            self.row(RowAction::Select(e));
        }
        if let Some(input) = input {
            self.root = Op::Optional {
                input,
                body: Box::new(std::mem::replace(&mut self.root, Op::Unit)),
            };
        }
        Ok(())
    }
    /// One path: bind its node variables, seek nodes named by handle, expand
    /// each edge, then scan and filter the nodes.
    fn path(&mut self, path: &Path, scope: &mut MatchScope) -> Result<(), CypherError> {
        let vars = path
            .nodes
            .iter()
            .map(|n| self.bind_name(&n.name, &scope.tick, false))
            .collect::<Result<Vec<_>, _>>()?;
        let handles = self.handle_lookups(path, &vars)?;
        let mut segments = Vec::new();
        for (i, edge) in path.edges.iter().enumerate() {
            if let Some((min, max)) = edge.bounds {
                if !self.bound.contains(&vars[i]) {
                    self.row(RowAction::Nodes {
                        var: vars[i].clone(),
                        labels: path.nodes[i].labels.clone(),
                        tick: scope.tick.clone(),
                    });
                    self.bound.insert(vars[i].clone());
                }
                let history = self.fresh();
                let properties = edge
                    .props
                    .iter()
                    .map(|(k, e)| Ok((k.clone(), self.expr(e)?)))
                    .collect::<Result<_, CypherError>>()?;
                self.row(RowAction::Trail {
                    source: vars[i].clone(),
                    target: vars[i + 1].clone(),
                    path: history.clone(),
                    attr: edge.kind.clone(),
                    direction: edge.direction,
                    tick: scope.tick.clone(),
                    min,
                    max,
                    excluded: scope.previous_edges.clone(),
                    properties,
                });
                let rels = self.bind_name(&edge.name, &scope.tick, true)?;
                let expr = Expr::Function(
                    kala_query::RowFn::Relationships,
                    Box::new(Expr::Var(history.clone())),
                );
                if self.bound.contains(&rels) {
                    self.row(RowAction::Select(Expr::Compare(
                        CmpOp::Eq,
                        Box::new(Expr::Var(rels.clone())),
                        Box::new(expr),
                    )));
                } else {
                    self.row(RowAction::Bind {
                        var: rels.clone(),
                        expr,
                    });
                }
                self.bound.extend([vars[i + 1].clone(), rels.clone()]);
                scope.previous_edges.push(rels);
                segments.push(history);
            } else if self.is_simple_edge(edge, scope.edge_count) {
                self.simple_edge(edge, &vars[i], &vars[i + 1], &scope.tick);
                self.bound.extend([vars[i].clone(), vars[i + 1].clone()]);
            } else {
                let r = self.general_edge(edge, &vars[i], &vars[i + 1], scope)?;
                self.bound
                    .extend([vars[i].clone(), vars[i + 1].clone(), r.clone()]);
                segments.push(r);
            }
        }
        self.node_filters(path, &vars, &handles, &scope.tick)?;
        if let Some(name) = &path.name {
            if self.bindings.contains_key(name) {
                return Err(self.error("path variable already declared"));
            }
            let var = self.bind_fresh(Expr::Path {
                start: vars[0].clone(),
                segments,
            });
            self.bindings
                .insert(name.clone(), (var, scope.tick.clone(), false));
        }
        Ok(())
    }
    /// Turn each `{kala_handle: …}` property into a handle seek, and return
    /// the `(node position, property index)` pairs it consumed.
    fn handle_lookups(
        &mut self,
        path: &Path,
        vars: &[Var],
    ) -> Result<BTreeSet<(usize, usize)>, CypherError> {
        // Handle lookup is selective, timeless and identical to Datalog's access.
        // Keyed by node position, not variable: a variable repeated in the path
        // keeps every other occurrence's property filters.
        let mut handles = BTreeSet::new();
        for (position, (n, v)) in path.nodes.iter().zip(vars).enumerate() {
            if self.nullable && self.bound.contains(v) {
                continue;
            }
            for (i, (key, value)) in n.props.iter().enumerate() {
                if key != "kala_handle" {
                    continue;
                }
                let value = match value {
                    Expr::Value(v @ Value::Str(_)) => Term::Const(v.clone()),
                    Expr::Param(p) => {
                        self.param(p, ParamKind::Value(ValueType::Str))?;
                        Term::Param(p.clone())
                    }
                    // Other values stay ordinary (never-equal) property filters.
                    _ => continue,
                };
                self.expand(
                    Term::Var(v.clone()),
                    AttrRef::Handle,
                    value,
                    TickSource::Default,
                    Access::Handle,
                );
                self.bound.insert(v.clone());
                handles.insert((position, i));
            }
        }
        Ok(handles)
    }
    /// Whether an edge compiles to the same plain `Expand` Datalog emits: the
    /// clause's only edge, anonymous, outgoing, without properties, over a
    /// ref attribute known at a canon fixed at compile time.
    fn is_simple_edge(&self, edge: &Edge, edge_count: usize) -> bool {
        let view = self.world.at_canon(match self.canon {
            CanonSource::Const(n) => kala_core::TxId(n),
            _ => self.world.latest_tx(),
        });
        !self.nullable
            && edge_count == 1
            && edge.name.is_none()
            && edge.props.is_empty()
            && edge.direction == Orientation::Out
            && !matches!(self.canon, CanonSource::Param(_))
            && view
                .resolve_attribute(&edge.kind)
                .is_ok_and(|(d, _)| d.value_type == ValueType::Ref)
    }
    /// The fast path: an index expansion from whichever end is already bound.
    fn simple_edge(&mut self, edge: &Edge, source: &Var, target: &Var, tick: &TickSource) {
        let access = if self.bound.contains(source) {
            Access::E
        } else if self.bound.contains(target) {
            Access::R
        } else {
            Access::Scan
        };
        self.expand(
            Term::Var(source.clone()),
            AttrRef::Named(edge.kind.clone()),
            Term::Var(target.clone()),
            tick.clone(),
            access,
        );
    }
    /// The general path: bind the relationship itself, keep it distinct from
    /// the clause's earlier relationships, then filter on its properties.
    fn general_edge(
        &mut self,
        edge: &Edge,
        source: &Var,
        target: &Var,
        scope: &mut MatchScope,
    ) -> Result<Var, CypherError> {
        let var = self.bind_name(&edge.name, &scope.tick, true)?;
        self.row(RowAction::Edge {
            source: source.clone(),
            target: target.clone(),
            edge: var.clone(),
            attr: edge.kind.clone(),
            direction: edge.direction,
            tick: scope.tick.clone(),
        });
        for old in &scope.previous_edges {
            self.row(RowAction::Select(Expr::Disjoint(
                Box::new(Expr::Var(var.clone())),
                Box::new(Expr::Var(old.clone())),
            )));
        }
        scope.previous_edges.push(var.clone());
        self.property_map(&var, &scope.tick, &edge.props)?;
        Ok(var)
    }
    /// Scan the path's nodes that are still unbound or carry labels, and
    /// filter every node on the properties no handle seek consumed.
    fn node_filters(
        &mut self,
        path: &Path,
        vars: &[Var],
        handles: &BTreeSet<(usize, usize)>,
        tick: &TickSource,
    ) -> Result<(), CypherError> {
        for (position, (node, var)) in path.nodes.iter().zip(vars).enumerate() {
            if !self.bound.contains(var) || !node.labels.is_empty() {
                self.row(RowAction::Nodes {
                    var: var.clone(),
                    labels: node.labels.clone(),
                    tick: tick.clone(),
                });
                self.bound.insert(var.clone());
            }
            let props: Vec<_> = node
                .props
                .iter()
                .enumerate()
                .filter(|(i, _)| !handles.contains(&(position, *i)))
                .map(|(_, p)| p.clone())
                .collect();
            self.property_map(var, tick, &props)?;
        }
        Ok(())
    }
    /// Give every RETURN item an execution slot: a returned variable keeps its
    /// own unless an alias renames it; anything else is bound to a fresh one.
    fn return_items(&mut self, q: &Query) -> Result<Returned, CypherError> {
        if q.items.iter().any(|i| crate::parse::has_aggregate(&i.expr)) {
            return self.aggregate_items(q);
        }
        let mut returned = Returned::default();
        for item in &q.items {
            let expr = self.expr(&item.expr)?;
            let v = match &expr {
                Expr::Var(v) if v.0 == item.name => v.clone(),
                _ => self.bind_fresh(expr),
            };
            returned.aliases.insert(item.name.clone(), v.clone());
            returned.items.push((item.expr.clone(), v.clone()));
            returned.find.push(v);
        }
        Ok(returned)
    }
    fn aggregate_items(&mut self, q: &Query) -> Result<Returned, CypherError> {
        let mut returned = Returned::default();
        let mut keys = Vec::new();
        let mut aggregates = Vec::new();
        for item in &q.items {
            let v = if let Expr::Aggregate {
                function,
                distinct,
                argument,
            } = &item.expr
            {
                let argument = argument.as_deref().map(|e| self.expr(e)).transpose()?;
                let output = self.fresh();
                aggregates.push(kala_query::BagAggregate {
                    output: output.clone(),
                    function: *function,
                    distinct: *distinct,
                    argument,
                });
                output
            } else {
                let expr = self.expr(&item.expr)?;
                let v = match expr {
                    Expr::Var(v) => v,
                    e => self.bind_fresh(e),
                };
                keys.push(v.clone());
                v
            };
            returned.aliases.insert(item.name.clone(), v.clone());
            returned.items.push((item.expr.clone(), v.clone()));
            returned.find.push(v);
        }
        self.row(RowAction::Group { keys, aggregates });
        Ok(returned)
    }
    /// The ORDER BY keys: a returned item (by alias or by expression) sorts on
    /// its slot; any other expression is bound to a fresh one.
    fn order_keys(
        &mut self,
        q: &Query,
        returned: &Returned,
    ) -> Result<Vec<(Var, bool)>, CypherError> {
        let original = self.bindings.clone();
        // ORDER BY sees projected aliases. After grouping it can only read
        // retained keys/aggregate outputs, not pre-group bindings.
        if q.items.iter().any(|i| crate::parse::has_aggregate(&i.expr)) {
            self.bindings
                .retain(|_, (v, _, _)| returned.find.contains(v));
        }
        for (item, var) in q.items.iter().zip(&returned.find) {
            let (tick, edge) = match &item.expr {
                Expr::Var(name) => original
                    .get(name)
                    .map(|(_, tick, edge)| (tick.clone(), *edge))
                    .unwrap_or((TickSource::Default, false)),
                _ => (TickSource::Default, false),
            };
            self.bindings
                .insert(item.name.clone(), (var.clone(), tick, edge));
        }
        let mut order = Vec::new();
        for (e, desc) in &q.order {
            let known = if let Expr::Var(v) = e {
                returned.aliases.get(v).cloned()
            } else {
                None
            }
            .or_else(|| {
                returned
                    .items
                    .iter()
                    .find(|(x, _)| x == e)
                    .map(|(_, v)| v.clone())
            });
            let var = if let Some(v) = known {
                v
            } else {
                if q.distinct {
                    return Err(self.error("ORDER BY with DISTINCT must reference a returned item"));
                }
                let expr = self.expr(e)?;
                self.bind_fresh(expr)
            };
            order.push((var, *desc));
        }
        self.bindings = original;
        Ok(order)
    }
    /// Wrap the pipeline in its result operator. A plain `RETURN DISTINCT` of
    /// variables is the same `Project` Datalog emits.
    fn output(&mut self, q: &Query, find: Vec<Var>, order: Vec<(Var, bool)>) {
        let plain_names = q.items.iter().zip(&find).all(|(i, v)| i.name == v.0);
        let input = Box::new(std::mem::replace(&mut self.root, Op::Unit));
        self.root = if q.distinct
            && order.is_empty()
            && q.skip.is_none()
            && q.limit.is_none()
            && plain_names
        {
            Op::Project { input, vars: find }
        } else {
            Op::Output {
                input,
                spec: OutputSpec {
                    vars: find,
                    distinct: q.distinct,
                    order,
                    skip: q.skip.clone(),
                    limit: q.limit.clone(),
                },
            }
        };
    }
}
/// What one MATCH clause's paths share.
struct MatchScope {
    /// The tick the clause's operators read at.
    tick: TickSource,
    /// How many relationship patterns the clause has, over all its paths.
    edge_count: usize,
    /// The clause's relationship variables so far: Cypher never matches one
    /// relationship twice within a clause.
    previous_edges: Vec<Var>,
}
/// The RETURN items' execution slots.
#[derive(Default)]
struct Returned {
    /// One slot per item, in column order.
    find: Vec<Var>,
    /// Column name to slot.
    aliases: BTreeMap<String, Var>,
    /// Each item's source expression and slot.
    items: Vec<(Expr<String>, Var)>,
}
