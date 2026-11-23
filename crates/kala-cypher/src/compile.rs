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
        used: q
            .matches
            .iter()
            .flat_map(|m| &m.paths)
            .flat_map(|p| {
                p.nodes
                    .iter()
                    .filter_map(|n| n.name.clone())
                    .chain(p.edges.iter().filter_map(|e| e.name.clone()))
            })
            .collect(),
    }
    .run(q)
}
struct Compiler<'a> {
    world: &'a World,
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
                    .ok_or_else(|| self.error(format!("unbound variable {n}")))?
                    .0
                    .clone(),
            ),
            Expr::Value(v) => Expr::Value(v.clone()),
            Expr::Null => Expr::Null,
            Expr::Param(p) => {
                self.param(p, ParamKind::Scalar)?;
                Expr::Param(p.clone())
            }
            Expr::Property { entity, key, .. } => {
                let (var, tick, _) = self
                    .bindings
                    .get(entity)
                    .ok_or_else(|| self.error(format!("unbound variable {entity}")))?;
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
        if let Expr::Param(p) = b {
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
                self.param(p, ParamKind::Value(ty))?;
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
            if let Expr::Param(p) = value {
                let kind = if key == "kala_handle" {
                    ParamKind::Value(ValueType::Str)
                } else {
                    self.world
                        .attribute(key)
                        .map_or(ParamKind::Scalar, |d| ParamKind::Value(d.value_type))
                };
                self.param(p, kind)?;
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
        let default_tick = match q.matches.first() {
            Some(first) => self.clause_tick(first)?,
            None => TickSource::Default,
        };
        self.resolve_canon(q)?;
        for (index, m) in q.matches.iter().enumerate() {
            self.match_clause(index, m)?;
        }
        let returned = self.return_items(q)?;
        let order = self.order_keys(q, &returned)?;
        for t in [&q.skip, &q.limit].into_iter().flatten() {
            if let Term::Param(p) = t {
                self.param(p, ParamKind::Value(ValueType::I64))?;
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
    /// A MATCH clause's own `AT TICK`, or the query default when it has none.
    fn clause_tick(&mut self, m: &Match) -> Result<TickSource, CypherError> {
        Ok(match &m.tick {
            Some(t) => self.time(t, ParamKind::Tick)?,
            None => TickSource::Default,
        })
    }
    /// The query's canon: the `CANON` of whichever MATCH carries one.
    fn resolve_canon(&mut self, q: &Query) -> Result<(), CypherError> {
        for m in &q.matches {
            if let Some(t) = &m.canon {
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
        let mut scope = MatchScope {
            tick,
            edge_count: m.paths.iter().map(|p| p.edges.len()).sum(),
            previous_edges: Vec::new(),
        };
        for path in &m.paths {
            self.path(path, &mut scope)?;
        }
        if let Some(e) = &m.predicate {
            let e = self.expr(e)?;
            self.row(RowAction::Select(e));
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
        for (i, edge) in path.edges.iter().enumerate() {
            self.edge(edge, &vars[i], &vars[i + 1], scope)?;
        }
        self.node_filters(path, &vars, &handles, &scope.tick)
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
    /// One relationship pattern between two bound node variables.
    fn edge(
        &mut self,
        edge: &Edge,
        source: &Var,
        target: &Var,
        scope: &mut MatchScope,
    ) -> Result<(), CypherError> {
        if self.is_simple_edge(edge, scope.edge_count) {
            self.simple_edge(edge, source, target, &scope.tick);
        } else {
            self.general_edge(edge, source, target, scope)?;
        }
        self.bound.extend([source.clone(), target.clone()]);
        Ok(())
    }
    /// Whether an edge compiles to the same plain `Expand` Datalog emits: the
    /// clause's only edge, anonymous, outgoing, without properties, over a
    /// ref attribute known at a canon fixed at compile time.
    fn is_simple_edge(&self, edge: &Edge, edge_count: usize) -> bool {
        let view = self.world.at_canon(match self.canon {
            CanonSource::Const(n) => kala_core::TxId(n),
            _ => self.world.latest_tx(),
        });
        edge_count == 1
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
    ) -> Result<(), CypherError> {
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
            self.row(RowAction::Select(Expr::Compare(
                CmpOp::Ne,
                Box::new(Expr::Var(var.clone())),
                Box::new(Expr::Var(old.clone())),
            )));
        }
        scope.previous_edges.push(var.clone());
        self.property_map(&var, &scope.tick, &edge.props)
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
    /// The ORDER BY keys: a returned item (by alias or by expression) sorts on
    /// its slot; any other expression is bound to a fresh one.
    fn order_keys(
        &mut self,
        q: &Query,
        returned: &Returned,
    ) -> Result<Vec<(Var, bool)>, CypherError> {
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
