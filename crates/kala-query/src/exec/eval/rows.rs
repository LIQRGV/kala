//! Nullable reads and expressions, using the same interner, meter and budgets.
use super::*;
use crate::exec::val::{cmp_real, result_row_bytes};
use crate::{Expr, Orientation, RowAction, TickSource};
use kala_core::{Cardinality, Direction, ValueType};

fn tick(t: &TickSource) -> Tick {
    match t {
        TickSource::Const(t) => *t,
        _ => unreachable!("row ticks are resolved by compile"),
    }
}
fn boolean(v: Val) -> Result<Option<bool>, QueryError> {
    match v {
        Val::Null => Ok(None),
        Val::Bool(b) => Ok(Some(b)),
        _ => Err(QueryError::TypeMismatch(
            "boolean expression expected".into(),
        )),
    }
}
fn bool_value(v: Option<bool>) -> Val {
    v.map_or(Val::Null, Val::Bool)
}
fn put(row: &mut Row, slot: usize, val: Val) -> bool {
    match &row[slot] {
        Some(old) => *old == val,
        None => {
            row[slot] = Some(val);
            true
        }
    }
}
/// Neo4j orderability for the supported value types: nodes, relationships,
/// lists, strings, booleans, numbers, NULL. Integers and floats compare numerically.
pub(in crate::exec) fn order(
    a: &Val,
    b: &Val,
    work: &mut crate::exec::val::CmpWork,
) -> std::cmp::Ordering {
    let rank = |v: &Val| match v {
        Val::Ref(e) => {
            if e.is_edge() {
                1
            } else {
                0
            }
        }
        Val::List(..) => 2,
        Val::Str(..) => 3,
        Val::Bool(_) => 4,
        Val::I64(_) | Val::F64(_) => 5,
        Val::Null => 7,
        _ => 6,
    };
    match (a, b) {
        (Val::I64(i), Val::F64(f)) => int_float(*i, *f),
        (Val::F64(f), Val::I64(i)) => int_float(*i, *f).reverse(),
        (Val::List(_, xs), Val::List(_, ys)) => {
            for (x, y) in xs.iter().zip(ys.iter()) {
                let o = order(x, y, work);
                if !o.is_eq() {
                    return o;
                }
            }
            xs.len().cmp(&ys.len())
        }
        _ if rank(a) != rank(b) => rank(a).cmp(&rank(b)),
        _ => cmp_real(a, b, work),
    }
}
fn int_float(i: i64, f: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    if f >= 9223372036854775808.0 {
        return Less;
    }
    if f < -9223372036854775808.0 {
        return Greater;
    }
    let integral = f as i64;
    i.cmp(&integral).then_with(|| (i as f64).total_cmp(&f))
}
impl Exec<'_, '_> {
    pub(super) fn eval_row(
        &mut self,
        action: &RowAction<usize, Val>,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        for row in rows {
            self.budget.step(1)?;
            match action {
                RowAction::Bind { var, expr } => {
                    let value = self.expression(expr, &row)?;
                    let mut row = row;
                    row[*var] = Some(value);
                    self.budget.emit(&mut out, row)?;
                }
                RowAction::Select(expr) => {
                    if boolean(self.expression(expr, &row)?)? == Some(true) {
                        self.budget.emit(&mut out, row)?;
                    }
                }
                RowAction::Nodes {
                    var,
                    labels,
                    tick: t,
                } => {
                    if let Some(Val::Ref(e)) = row[*var].as_ref() {
                        if self.node_labels(*e, labels, tick(t))? {
                            self.budget.emit(&mut out, row)?;
                        }
                    } else if row[*var].is_none() {
                        self.read_view();
                        let view = *self.view;
                        let schema = self.world.at_canon(view.canon());
                        for (name, _, _) in schema.node_type_definitions() {
                            self.budget.step(1)?;
                            for e in view.entities_of_type(name)? {
                                let e = e?;
                                self.charge_index_work()?;
                                if self.node_labels(e, labels, tick(t))? {
                                    let mut r = row.clone();
                                    r[*var] = Some(Val::Ref(e));
                                    self.budget.emit(&mut out, r)?;
                                }
                            }
                        }
                    }
                }
                RowAction::Edge {
                    source,
                    target,
                    edge,
                    attr,
                    direction,
                    tick: t,
                } => {
                    let schema = self.world.at_canon(self.view.canon());
                    let Ok((def, _)) = schema.resolve_attribute(attr) else {
                        continue;
                    };
                    if def.value_type != ValueType::Ref {
                        continue;
                    }
                    // The shared scan already implements inverse names and bidirectional attributes.
                    let (pairs, fresh) = self.scan_once(attr, tick(t))?;
                    for (a, b) in pairs.iter() {
                        self.budget.step(1)?;
                        let Val::Ref(b) = b else { continue };
                        let Some(id) = self.view.edge_id(*a, attr, *b) else {
                            continue;
                        };
                        let (a, b) = if *direction == Orientation::In {
                            (*b, *a)
                        } else {
                            (*a, *b)
                        };
                        let reverse = *direction == Orientation::Either
                            && def.direction == Some(kala_core::RefDirection::Directed);
                        for (a, b) in std::iter::once((a, b))
                            .chain(reverse.then_some((b, a)).filter(|(x, y)| x != y))
                        {
                            let mut r = row.clone();
                            if put(&mut r, *source, Val::Ref(a))
                                && put(&mut r, *target, Val::Ref(b))
                                && put(&mut r, *edge, Val::Ref(id.into()))
                            {
                                self.budget.emit(&mut out, r)?;
                            }
                        }
                    }
                    if fresh {
                        self.budget
                            .release_bytes(pairs.len().saturating_mul(PAIR_BYTES));
                    }
                }
            }
        }
        Ok(out)
    }
    fn node_labels(
        &mut self,
        id: EntityId,
        labels: &[String],
        t: Tick,
    ) -> Result<bool, QueryError> {
        if id.is_edge() || !self.view.is_defined(id) {
            return Ok(false);
        }
        if self.node_types.is_none() && !labels.is_empty() {
            let mut types = HashSet::new();
            for (name, _, _) in self
                .world
                .at_canon(self.view.canon())
                .node_type_definitions()
            {
                self.budget.step(1)?;
                self.budget.charge_bytes(64 + name.len())?;
                types.insert(name.to_owned());
            }
            self.node_types = Some(types);
        }
        let typed = self.world.format_id(id);
        let node_type = typed.rsplit_once('#').map(|(s, _)| s).unwrap_or("");
        for label in labels {
            self.budget.step(1)?;
            if node_type == label {
                continue;
            }
            if self
                .node_types
                .as_ref()
                .is_some_and(|types| types.contains(label))
            {
                return Ok(false);
            }
            let value = self.property(id, "label", t)?;
            let matches = match value {
                Val::Str(_, s) => s.as_ref() == label,
                Val::List(_, xs) => xs
                    .iter()
                    .any(|v| matches!(v,Val::Str(_,s) if s.as_ref() == label)),
                _ => false,
            };
            if !matches {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn property(&mut self, id: EntityId, key: &str, t: Tick) -> Result<Val, QueryError> {
        self.budget.step(1)?;
        if let Some(value) = self.budget.memo(key).and_then(|m| m.property.get(&(id, t))) {
            return Ok(value.clone());
        }
        if !self.view.is_defined(id) {
            return Ok(Val::Null);
        }
        if key == "kala_handle" {
            return match self.world.handle_of(id) {
                Some(s) => self.intern_str(s),
                None => Ok(Val::Null),
            };
        }
        let schema = self.world.at_canon(self.view.canon());
        let Ok((def, inverse)) = schema.resolve_attribute(key) else {
            return Ok(Val::Null);
        };
        let many = if inverse {
            def.reverse_cardinality == Some(Cardinality::Many)
        } else {
            def.cardinality == Cardinality::Many
        };
        let mut values = Vec::new();
        if def.value_type == ValueType::Ref {
            let found = self.read_view().neighbors(id, key, Direction::Out, t)?;
            self.charge_index_work()?;
            self.budget.charge_bytes(found.len().saturating_mul(64))?;
            values.extend(found.into_iter().map(|(e, _)| Val::Ref(e)));
        } else {
            // Visit scalars one at a time, charging before retaining their copies.
            self.read_view();
            let view = *self.view;
            let mut failure = None;
            let read = view.visit_non_ref_values(id, key, t, |v| {
                let result = self
                    .budget
                    .step(1)
                    .and_then(|_| self.intern(v))
                    .and_then(|v| {
                        self.budget.charge_bytes(64)?;
                        values.push(v);
                        Ok(())
                    });
                if let Err(e) = result {
                    failure = Some(e);
                    return Err(CoreError::WorkInterrupted {
                        reason: kala_core::WorkInterruptReason::Limit,
                    });
                }
                Ok(())
            });
            if let Some(e) = failure {
                return Err(e);
            }
            read?;
            self.charge_index_work()?;
        }
        if values.is_empty() {
            return Ok(Val::Null);
        }
        // All transient buffers and sort indices are charged only while alive.
        let n = values.len();
        let order = crate::exec::sort_values(&values, &mut self.budget)?;
        let list_bytes = 64 + n.saturating_mul(std::mem::size_of::<Val>());
        self.budget.charge_bytes(list_bytes)?;
        let sorted: Vec<_> = order.into_iter().map(|i| values[i].clone()).collect();
        drop(values);
        self.budget
            .release_bytes(n.saturating_mul(64 + std::mem::size_of::<usize>()));
        let value = if many || sorted.len() > 1 {
            self.budget.step(sorted.len() as u64)?;
            let (value, new) = self.interner.list(sorted)?;
            if !new {
                self.budget.release_bytes(list_bytes);
            }
            value
        } else {
            self.budget.release_bytes(list_bytes);
            sorted[0].clone()
        };
        if let Some(memo) = self.budget.memo_slot(
            key,
            crate::exec::budget::MEMO_ENTRY_BYTES + std::mem::size_of::<Val>(),
        ) {
            memo.property.insert((id, t), value.clone());
        }
        Ok(value)
    }
    fn expression(&mut self, e: &Expr<usize, Val>, row: &Row) -> Result<Val, QueryError> {
        self.budget.step(1)?;
        Ok(match e {
            Expr::Var(s) => row[*s]
                .clone()
                .ok_or_else(|| QueryError::Plan("unbound expression".into()))?,
            Expr::Value(v) => v.clone(),
            Expr::Null => Val::Null,
            Expr::List(values) => {
                self.budget
                    .charge_bytes(64 + values.len().saturating_mul(std::mem::size_of::<Val>()))?;
                let values = values
                    .iter()
                    .map(|e| self.expression(e, row))
                    .collect::<Result<Vec<_>, _>>()?;
                let bytes = 64 + values.len().saturating_mul(std::mem::size_of::<Val>());
                let (v, new) = self.interner.list(values)?;
                if !new {
                    self.budget.release_bytes(bytes);
                }
                v
            }
            Expr::Param(_) => unreachable!("parameters resolved by compile"),
            Expr::Property {
                entity,
                key,
                tick: t,
            } => match row[*entity].as_ref() {
                Some(Val::Ref(id)) => self.property(*id, key, tick(t))?,
                Some(Val::Null) => Val::Null,
                _ => {
                    return Err(QueryError::TypeMismatch(
                        "property access needs an entity".into(),
                    ));
                }
            },
            Expr::IsNull(e) => Val::Bool(matches!(self.expression(e, row)?, Val::Null)),
            Expr::Not(e) => bool_value(boolean(self.expression(e, row)?)?.map(|b| !b)),
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
                let a = boolean(self.expression(a, row)?)?;
                let b = boolean(self.expression(b, row)?)?;
                bool_value(match e {
                    Expr::And(..) => match (a, b) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    },
                    Expr::Or(..) => match (a, b) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    },
                    _ => a.zip(b).map(|(a, b)| a ^ b),
                })
            }
            Expr::Compare(op, a, b) => {
                let a = self.expression(a, row)?;
                let b = self.expression(b, row)?;
                if matches!(a, Val::Null) || matches!(b, Val::Null) {
                    Val::Null
                } else {
                    self.budget.step(
                        ((result_row_bytes(std::slice::from_ref(&a))
                            + result_row_bytes(std::slice::from_ref(&b)))
                            / 64) as u64,
                    )?;
                    if matches!(op, CmpOp::Eq | CmpOp::Ne) {
                        let equal = equal(&a, &b, &mut self.budget)?;
                        return Ok(bool_value(
                            equal.map(|v| if *op == CmpOp::Ne { !v } else { v }),
                        ));
                    }
                    let o = order(&a, &b, &mut self.budget.cmp);
                    self.budget.charge_cmp()?;
                    Val::Bool(match op {
                        CmpOp::Eq => o.is_eq(),
                        CmpOp::Ne => !o.is_eq(),
                        CmpOp::Lt => o.is_lt(),
                        CmpOp::Le => o.is_le(),
                        CmpOp::Gt => o.is_gt(),
                        CmpOp::Ge => o.is_ge(),
                    })
                }
            }
        })
    }
}

/// A known differing list element decides false even if another element is NULL.
fn equal(
    a: &Val,
    b: &Val,
    budget: &mut crate::exec::budget::Budget,
) -> Result<Option<bool>, QueryError> {
    budget.step(1)?;
    match (a, b) {
        (Val::Null, _) | (_, Val::Null) => Ok(None),
        (Val::List(_, a), Val::List(_, b)) => {
            if a.len() != b.len() {
                return Ok(Some(false));
            }
            let mut result = Some(true);
            for (a, b) in a.iter().zip(b.iter()) {
                match equal(a, b, budget)? {
                    Some(false) => return Ok(Some(false)),
                    None => result = None,
                    _ => {}
                }
            }
            Ok(result)
        }
        _ => {
            let eq = order(a, b, &mut budget.cmp).is_eq();
            budget.charge_cmp()?;
            Ok(Some(eq))
        }
    }
}
