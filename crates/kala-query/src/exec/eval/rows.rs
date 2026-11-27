//! Nullable reads and expressions, using the same interner, meter and budgets.
use super::*;
use crate::exec::val::{cmp_real, result_row_bytes};
use crate::{Expr, Orientation, RowAction, TickSource};
use kala_core::{Cardinality, Direction, ValueType};

pub(super) fn tick(t: &TickSource) -> Tick {
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
pub(super) fn put(row: &mut Row, slot: usize, val: Val) -> bool {
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
        Val::Path(..) => 6,
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
        (Val::List(ix, xs), Val::List(iy, ys)) => {
            if ix == iy {
                return std::cmp::Ordering::Equal;
            }
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
/// The ordering `<`, `<=`, `>` and `>=` use: `None` (NULL) unless both sides
/// are mutually comparable, i.e. both numbers (integers and floats compare
/// numerically), both strings, both booleans, or both lists of such values,
/// compared element-wise (a NULL or incomparable element pair is NULL). Nodes
/// and relationships aren't comparable. [`order`] (the cross-type ranking)
/// stays for `ORDER BY`.
fn compare(a: &Val, b: &Val, work: &mut crate::exec::val::CmpWork) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Val::List(_, xs), Val::List(_, ys)) => {
            for (x, y) in xs.iter().zip(ys.iter()) {
                // Lists hold scalars; a nested list is not compared.
                let o = compare_scalar(x, y, work)?;
                if !o.is_eq() {
                    return Some(o);
                }
            }
            Some(xs.len().cmp(&ys.len()))
        }
        _ => compare_scalar(a, b, work),
    }
}
fn compare_scalar(
    a: &Val,
    b: &Val,
    work: &mut crate::exec::val::CmpWork,
) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Val::I64(_) | Val::F64(_), Val::I64(_) | Val::F64(_))
        | (Val::Str(..), Val::Str(..))
        | (Val::Bool(_), Val::Bool(_)) => Some(order(a, b, work)),
        _ => None,
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
    i.cmp(&integral).then_with(|| {
        if f == 0.0 {
            std::cmp::Ordering::Equal
        } else {
            (i as f64).total_cmp(&f)
        }
    })
}
/// One relationship pattern read at one tick: the row slots of its two
/// ends and of the edge, in `attr`'s named orientation (`Out`: `x` is the
/// source, `In`: `x` is the target; `Either`: both, for a directed
/// attribute).
#[derive(Clone, Copy)]
struct EdgePattern<'s> {
    x: usize,
    y: usize,
    edge: usize,
    attr: &'s str,
    direction: Orientation,
    t: Tick,
}

impl Exec<'_, '_> {
    pub(super) fn eval_row(
        &mut self,
        action: &RowAction<usize, Val>,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        if let RowAction::Project {
            vars,
            distinct,
            order,
            skip,
            limit,
            ordinal,
        } = action
        {
            return self.bag_project(rows, vars, *distinct, order, skip, limit, *ordinal);
        }
        if let RowAction::Group { keys, aggregates } = action {
            return self.bag_group(rows, keys, aggregates);
        }
        let mut out = BTreeSet::new();
        for row in rows {
            self.budget.step(1)?;
            match action {
                RowAction::Source {
                    source,
                    columns,
                    tick,
                    ordinal,
                } => {
                    self.source_rows(source, columns, self::tick(tick), *ordinal, row, &mut out)?
                }
                RowAction::Values {
                    rows,
                    columns,
                    ordinal,
                } => {
                    let value = self.expression(rows, &row)?;
                    let Val::List(_, input) = value else {
                        return Err(QueryError::TypeMismatch(
                            "supplied rows must be a list".into(),
                        ));
                    };
                    for values in input.iter() {
                        let Val::List(_, values) = values else {
                            return Err(QueryError::TypeMismatch(
                                "supplied row must be a list".into(),
                            ));
                        };
                        if values.len() != columns.len() {
                            return Err(QueryError::TypeMismatch(
                                "supplied row width does not match columns".into(),
                            ));
                        }
                        let mut r = row.clone();
                        for (s, v) in columns.iter().zip(values.iter()) {
                            r[*s] = Some(v.clone());
                        }
                        r[*ordinal] = Some(Val::I64(out.len() as i64));
                        self.budget.emit(&mut out, r)?;
                    }
                }
                RowAction::CheckParameter { .. } => {
                    self.budget.emit(&mut out, row)?;
                }
                RowAction::Trail { .. } => self.trail(action, row, &mut out)?,
                RowAction::Group { .. } | RowAction::Project { .. } => {
                    unreachable!("whole-input operation handled above")
                }
                RowAction::Unwind { expr, var, ordinal } => match self.expression(expr, &row)? {
                    Val::Null => {}
                    Val::List(_, values) => {
                        for value in values.iter() {
                            self.budget.step(1)?;
                            let mut r = row.clone();
                            r[*var] = Some(value.clone());
                            r[*ordinal] = Some(Val::I64(out.len() as i64));
                            self.budget.emit(&mut out, r)?;
                        }
                    }
                    _ => {
                        return Err(QueryError::TypeMismatch(
                            "UNWIND needs a list or NULL".into(),
                        ));
                    }
                },
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
                RowAction::NodeByHandle { var, handle } => {
                    // The loop's step above pays for this row's one lookup.
                    let Some(Val::Str(_, h)) = row[*handle].as_ref() else {
                        continue;
                    };
                    let Some(e) = self.world.entity_by_handle(h) else {
                        continue;
                    };
                    if self.defined_at_canon(e)? {
                        let mut row = row;
                        if put(&mut row, *var, Val::Ref(e)) {
                            self.budget.emit(&mut out, row)?;
                        }
                    }
                }
                RowAction::Nodes {
                    var,
                    labels,
                    tick: t,
                } => {
                    if let Some(bound) = row[*var].as_ref() {
                        match bound {
                            Val::Ref(e) => {
                                if self.node_labels(*e, labels, tick(t))? {
                                    self.budget.emit(&mut out, row)?;
                                }
                            }
                            // A NULL node matches nothing.
                            Val::Null => {}
                            other => {
                                return Err(QueryError::TypeMismatch(format!(
                                    "a node pattern needs a node, got {}",
                                    other.kind().name()
                                )));
                            }
                        }
                    } else {
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
                    let pattern = EdgePattern {
                        x: *source,
                        y: *target,
                        edge: *edge,
                        attr,
                        direction: *direction,
                        t: tick(t),
                    };
                    self.edge_row(row, pattern, &mut out)?
                }
            }
        }
        Ok(out)
    }
    /// Emits `row` extended with each relationship of the pattern's `attr`
    /// holding at `t` that fits its bound ends (see [`EdgePattern`]). With
    /// an end bound this is one `neighbors` seek (which reads inverse names
    /// and both sides of a bidirectional pair, and returns the stored edge's
    /// ID); with neither bound, the query's cached scan of `attr` at `t`,
    /// read once per tick.
    fn edge_row(
        &mut self,
        row: Row,
        pattern: EdgePattern<'_>,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let EdgePattern {
            x,
            y,
            edge,
            attr,
            direction,
            t,
        } = pattern;
        let schema = self.world.at_canon(self.view.canon());
        let Ok((def, inverse)) = schema.resolve_attribute(attr) else {
            return Ok(());
        };
        if def.value_type != ValueType::Ref {
            return Ok(());
        }
        let end = |v: &Option<Val>| match v {
            None => Ok(None),
            Some(Val::Ref(e)) => Ok(Some(*e)),
            // A non-entity end (NULL, a scalar) matches no relationship.
            Some(_) => Err(()),
        };
        let (Ok(bx), Ok(by)) = (end(&row[x]), end(&row[y])) else {
            return Ok(());
        };
        // Seek from the bound end; from `y` the read runs the other way.
        let (from, slot, dir) = match (bx, by) {
            (Some(e), _) => (e, y, direction),
            (None, Some(e)) => (
                e,
                x,
                match direction {
                    Orientation::Out => Orientation::In,
                    Orientation::In => Orientation::Out,
                    Orientation::Either => Orientation::Either,
                },
            ),
            (None, None) => {
                return self.edge_scan(row, pattern, inverse, def, out);
            }
        };
        // `Both` on a directed attribute reads `Out` and `In`; a self loop is
        // one (partner, edge) pair after kala-core's dedup, so one row. A
        // bidirectional attribute reads both sides whatever the direction.
        let dir = match dir {
            Orientation::Out => Direction::Out,
            Orientation::In => Direction::In,
            Orientation::Either => Direction::Both,
        };
        self.budget.step(1)?;
        let found = self.read_view().neighbors(from, attr, dir, t);
        self.charge_index_work()?;
        let found = absent_as_empty(found)?.unwrap_or_default();
        self.budget.step(found.len() as u64)?;
        for (other, id) in found {
            let mut r = row.clone();
            if put(&mut r, slot, Val::Ref(other)) && put(&mut r, edge, Val::Ref(id.into())) {
                self.budget.emit(out, r)?;
            }
        }
        Ok(())
    }
    /// [`Exec::edge_row`] with neither end bound: every pair of the cached
    /// scan, one step each. The edge ID is looked up under the base
    /// attribute in stored order: `(c child_of p)` is the edge `(p parent_of
    /// c)`, and kala-core finds a bidirectional pair in either order.
    fn edge_scan(
        &mut self,
        row: Row,
        pattern: EdgePattern<'_>,
        inverse: bool,
        def: &kala_core::AttrDef,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let EdgePattern {
            x,
            y,
            edge,
            attr,
            direction,
            t,
        } = pattern;
        // `View::edge_id` resolves names in the latest name table, so the
        // base is named there too (the canon's definition may predate a rename).
        let base = if inverse {
            let latest = self.world.at_canon(self.world.latest_tx());
            match latest.resolve_attribute(attr) {
                Ok((d, true)) => Some(d.name.clone()),
                _ => return Ok(()),
            }
        } else {
            None
        };
        let reverse = direction == Orientation::Either
            && def.direction == Some(kala_core::RefDirection::Directed);
        let pairs = self.scan(attr, t)?;
        for (a, b) in pairs.iter() {
            self.budget.step(1)?;
            let Val::Ref(b) = b else { continue };
            let id = match &base {
                Some(base) => self.view.edge_id(*b, base, *a),
                None => self.view.edge_id(*a, attr, *b),
            };
            let Some(id) = id else { continue };
            let (a, b) = if direction == Orientation::In {
                (*b, *a)
            } else {
                (*a, *b)
            };
            for (a, b) in
                std::iter::once((a, b)).chain(reverse.then_some((b, a)).filter(|(p, q)| p != q))
            {
                let mut r = row.clone();
                if put(&mut r, x, Val::Ref(a))
                    && put(&mut r, y, Val::Ref(b))
                    && put(&mut r, edge, Val::Ref(id.into()))
                {
                    self.budget.emit(out, r)?;
                }
            }
        }
        Ok(())
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
            // A many-valued label must pay for every comparison, even when
            // its property value came from the memo.
            self.budget.step(label.len() as u64 / 64)?;
            let wanted = self.intern_str(label)?;
            let matches = match value {
                Val::List(_, xs) => {
                    let mut found = false;
                    for value in xs.iter() {
                        self.budget.step(1)?;
                        let same = cmp_real(value, &wanted, &mut self.budget.cmp).is_eq();
                        self.budget.charge_cmp()?;
                        if same {
                            found = true;
                            break;
                        }
                    }
                    found
                }
                value => {
                    let same = cmp_real(&value, &wanted, &mut self.budget.cmp).is_eq();
                    self.budget.charge_cmp()?;
                    same
                }
            };
            if !matches {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub(super) fn property(&mut self, id: EntityId, key: &str, t: Tick) -> Result<Val, QueryError> {
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
            let (value, new) = self.interner.list(sorted, self.budget.limits.max_bytes)?;
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
    pub(super) fn expression(
        &mut self,
        e: &Expr<usize, Val>,
        row: &Row,
    ) -> Result<Val, QueryError> {
        self.budget.step(1)?;
        Ok(match e {
            Expr::Var(s) => row[*s]
                .clone()
                .ok_or_else(|| QueryError::Plan("unbound expression".into()))?,
            Expr::Value(v) => v.clone(),
            Expr::Null => Val::Null,
            Expr::Function(f, e) => {
                let v = self.expression(e, row)?;
                self.row_function(*f, v)?
            }
            Expr::ListIndex(list, index) => {
                match (self.expression(list, row)?, self.expression(index, row)?) {
                    (Val::Null, _) | (_, Val::Null) => Val::Null,
                    (Val::List(_, values), Val::I64(i)) => crate::rows::list_element(&values, i)
                        .cloned()
                        .unwrap_or(Val::Null),
                    _ => {
                        return Err(QueryError::TypeMismatch(
                            "list indexing requires a list and an integer offset".into(),
                        ));
                    }
                }
            }
            Expr::Disjoint(a, b) => {
                let a = self.expression(a, row)?;
                let b = self.expression(b, row)?;
                let a = self.edge_ids(&a)?;
                let b = self.edge_ids(&b)?;
                self.budget.step(a.len().saturating_mul(b.len()) as u64)?;
                let disjoint = !a.iter().any(|e| b.contains(e));
                self.budget
                    .release_bytes((a.len() + b.len()).saturating_mul(8));
                Val::Bool(disjoint)
            }
            Expr::Path { start, segments } => self.make_path(row, *start, segments)?,
            Expr::List(xs) => {
                let bytes = 64 + xs.len().saturating_mul(std::mem::size_of::<Val>());
                self.budget.charge_bytes(bytes)?;
                let values = xs
                    .iter()
                    .map(|e| self.expression(e, row))
                    .collect::<Result<_, _>>()?;
                let (value, new) = self.interner.list(values, self.budget.limits.max_bytes)?;
                if !new {
                    self.budget.release_bytes(bytes);
                }
                value
            }
            Expr::Aggregate { .. } => unreachable!("aggregate lowered before execution"),
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
                    let o = compare(&a, &b, &mut self.budget.cmp);
                    self.budget.charge_cmp()?;
                    bool_value(o.map(|o| match op {
                        CmpOp::Lt => o.is_lt(),
                        CmpOp::Le => o.is_le(),
                        CmpOp::Gt => o.is_gt(),
                        CmpOp::Ge => o.is_ge(),
                        CmpOp::Eq | CmpOp::Ne => unreachable!("equality returned above"),
                    }))
                }
            }
        })
    }
}

/// A known differing list element decides false even if another element is NULL.
pub(super) fn equal(
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
