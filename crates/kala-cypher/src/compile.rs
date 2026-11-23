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
    fn run(mut self, q: &Query) -> Result<Prepared, CypherError> {
        let default_tick = q
            .matches
            .first()
            .and_then(|m| m.tick.as_ref())
            .map(|t| self.time(t, ParamKind::Tick))
            .transpose()?
            .unwrap_or(TickSource::Default);
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
        let mut bound = BTreeSet::new();
        for (index, m) in q.matches.iter().enumerate() {
            let tick = if index == 0 {
                TickSource::Default
            } else {
                m.tick
                    .as_ref()
                    .map(|t| self.time(t, ParamKind::Tick))
                    .transpose()?
                    .unwrap_or(TickSource::Default)
            };
            let count = m.paths.iter().map(|p| p.edges.len()).sum::<usize>();
            let mut previous_edges: Vec<Var> = Vec::new();
            for path in &m.paths {
                let vars = path
                    .nodes
                    .iter()
                    .map(|n| self.bind_name(&n.name, &tick, false))
                    .collect::<Result<Vec<_>, _>>()?;
                // Handle lookup is selective, timeless and identical to Datalog's access.
                let mut handles = BTreeSet::new();
                for (n, v) in path.nodes.iter().zip(&vars) {
                    for (i, (key, value)) in n.props.iter().enumerate() {
                        if key == "kala_handle" && matches!(value, Expr::Value(Value::Str(_))) {
                            let value = match value {
                                Expr::Value(v) => Term::Const(v.clone()),
                                Expr::Param(p) => {
                                    self.param(p, ParamKind::Value(ValueType::Str))?;
                                    Term::Param(p.clone())
                                }
                                _ => unreachable!(),
                            };
                            self.root = Op::Expand {
                                input: Box::new(self.root),
                                entity: Term::Var(v.clone()),
                                attr: AttrRef::Handle,
                                value,
                                tick: TickSource::Default,
                                access: Access::Handle,
                            };
                            bound.insert(v.clone());
                            handles.insert((v.clone(), i));
                        }
                    }
                }
                for (i, edge) in path.edges.iter().enumerate() {
                    let source = &vars[i];
                    let target = &vars[i + 1];
                    let view = self.world.at_canon(match self.canon {
                        CanonSource::Const(n) => kala_core::TxId(n),
                        _ => self.world.latest_tx(),
                    });
                    let simple = count == 1
                        && edge.name.is_none()
                        && edge.props.is_empty()
                        && edge.direction == Orientation::Out
                        && !matches!(self.canon, CanonSource::Param(_))
                        && view
                            .resolve_attribute(&edge.kind)
                            .is_ok_and(|(d, _)| d.value_type == ValueType::Ref);
                    if simple {
                        let access = if bound.contains(source) {
                            Access::E
                        } else if bound.contains(target) {
                            Access::R
                        } else {
                            Access::Scan
                        };
                        self.root = Op::Expand {
                            input: Box::new(self.root),
                            entity: Term::Var(source.clone()),
                            attr: AttrRef::Named(edge.kind.clone()),
                            value: Term::Var(target.clone()),
                            tick: tick.clone(),
                            access,
                        };
                    } else {
                        let var = self.bind_name(&edge.name, &tick, true)?;
                        self.row(RowAction::Edge {
                            source: source.clone(),
                            target: target.clone(),
                            edge: var.clone(),
                            attr: edge.kind.clone(),
                            direction: edge.direction,
                            tick: tick.clone(),
                        });
                        for old in &previous_edges {
                            self.row(RowAction::Select(Expr::Compare(
                                CmpOp::Ne,
                                Box::new(Expr::Var(var.clone())),
                                Box::new(Expr::Var(old.clone())),
                            )));
                        }
                        previous_edges.push(var.clone());
                        self.property_map(&var, &tick, &edge.props)?;
                    }
                    bound.extend([source.clone(), target.clone()]);
                }
                for (node, var) in path.nodes.iter().zip(&vars) {
                    if !bound.contains(var) || !node.labels.is_empty() {
                        self.row(RowAction::Nodes {
                            var: var.clone(),
                            labels: node.labels.clone(),
                            tick: tick.clone(),
                        });
                        bound.insert(var.clone());
                    }
                    let props: Vec<_> = node
                        .props
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !handles.contains(&(var.clone(), *i)))
                        .map(|(_, p)| p.clone())
                        .collect();
                    self.property_map(var, &tick, &props)?;
                }
            }
            if let Some(e) = &m.predicate {
                let e = self.expr(e)?;
                self.row(RowAction::Select(e));
            }
        }
        let mut find = Vec::new();
        let mut aliases = BTreeMap::new();
        let mut returned = Vec::new();
        for item in &q.items {
            let expr = self.expr(&item.expr)?;
            let v = if let Expr::Var(v) = &expr {
                if v.0 == item.name {
                    v.clone()
                } else {
                    let alias = self.fresh();
                    self.row(RowAction::Bind {
                        var: alias.clone(),
                        expr: expr.clone(),
                    });
                    alias
                }
            } else {
                let v = self.fresh();
                self.row(RowAction::Bind {
                    var: v.clone(),
                    expr: expr.clone(),
                });
                v
            };
            aliases.insert(item.name.clone(), v.clone());
            returned.push((item.expr.clone(), v.clone()));
            find.push(v);
        }
        let mut order = Vec::new();
        for (e, desc) in &q.order {
            let known = if let Expr::Var(v) = e {
                aliases.get(v).cloned()
            } else {
                None
            }
            .or_else(|| {
                returned
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
                let v = self.fresh();
                self.row(RowAction::Bind {
                    var: v.clone(),
                    expr,
                });
                v
            };
            order.push((var, *desc));
        }
        for t in [&q.skip, &q.limit].into_iter().flatten() {
            if let Term::Param(p) = t {
                self.param(p, ParamKind::Value(ValueType::I64))?;
            }
        }
        let plain_names = q.items.iter().zip(&find).all(|(i, v)| i.name == v.0);
        if q.distinct && order.is_empty() && q.skip.is_none() && q.limit.is_none() && plain_names {
            self.root = Op::Project {
                input: Box::new(self.root),
                vars: find.clone(),
            };
        } else {
            self.root = Op::Output {
                input: Box::new(self.root),
                spec: OutputSpec {
                    vars: find.clone(),
                    distinct: q.distinct,
                    order,
                    skip: q.skip.clone(),
                    limit: q.limit.clone(),
                },
            };
        }
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
}
