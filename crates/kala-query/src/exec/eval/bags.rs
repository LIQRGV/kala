//! Nullable bag aggregation. Complete bindings retain occurrence slots upstream.
use super::*;
use crate::{BagAggregate, BagFn};
use std::mem::size_of;

#[derive(Default)]
struct Acc {
    count: i64,
    value: Option<Val>,
    values: Vec<Val>,
    seen: HashSet<Val>,
}
impl Exec<'_, '_> {
    pub(super) fn bag_group(
        &mut self,
        rows: BTreeSet<Row>,
        keys: &[usize],
        aggs: &[BagAggregate<usize, Val>],
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut groups: BTreeMap<Vec<Val>, Vec<Acc>> = BTreeMap::new();
        let group_bytes =
            64 + tuple_bytes(keys.len()) + aggs.len().saturating_mul(size_of::<Acc>());
        let mut held = 0usize;
        if keys.is_empty() {
            self.budget.charge_bytes(group_bytes)?;
            held += group_bytes;
            groups.insert(vec![], aggs.iter().map(|_| Acc::default()).collect());
        }
        for row in &rows {
            self.budget.step(1)?;
            let key: Vec<_> = keys
                .iter()
                .map(|s| row[*s].clone().expect("compiled group key"))
                .collect();
            if !groups.contains_key(&key) {
                if groups.len() >= self.budget.limits.max_rows {
                    return Err(QueryError::RowLimit {
                        limit: self.budget.limits.max_rows,
                    });
                }
                self.budget.charge_bytes(group_bytes)?;
                held += group_bytes;
                groups.insert(key.clone(), aggs.iter().map(|_| Acc::default()).collect());
            }
            let accs = groups.get_mut(&key).expect("group inserted");
            for (a, acc) in aggs.iter().zip(accs) {
                self.budget.step(1)?;
                let v = match &a.argument {
                    Some(e) => self.expression(e, row)?,
                    None => Val::Bool(true),
                };
                if v == Val::Null {
                    continue;
                }
                if a.distinct {
                    if acc.seen.contains(&v) {
                        continue;
                    }
                    let bytes = size_of::<Val>() + 64;
                    self.budget.charge_bytes(bytes)?;
                    held += bytes;
                    acc.seen.insert(v.clone());
                }
                acc.count = acc
                    .count
                    .checked_add(1)
                    .ok_or_else(|| QueryError::Overflow("count overflows i64".into()))?;
                match a.function {
                    BagFn::Count => {}
                    BagFn::Collect => {
                        let bytes = size_of::<Val>() * 2;
                        self.budget.charge_bytes(bytes)?;
                        held += bytes;
                        acc.values.push(v);
                    }
                    BagFn::Min | BagFn::Max => {
                        if let Some(old) = &acc.value {
                            if old.kind() != v.kind() {
                                return Err(QueryError::TypeMismatch(
                                    "min/max need values of one type".into(),
                                ));
                            }
                            self.budget.step(
                                ((super::super::val::result_row_bytes(std::slice::from_ref(&v))
                                    .saturating_add(super::super::val::result_row_bytes(
                                        std::slice::from_ref(old),
                                    )))
                                    / 64) as u64,
                            )?;
                            let order = rows::order(&v, old, &mut self.budget.cmp);
                            self.budget.charge_cmp()?;
                            if (a.function == BagFn::Min && order.is_lt())
                                || (a.function == BagFn::Max && order.is_gt())
                            {
                                acc.value = Some(v);
                            }
                        } else {
                            acc.value = Some(v);
                        }
                    }
                    BagFn::Sum | BagFn::Avg => {
                        let number = match v {
                            Val::I64(n) => n as f64,
                            Val::F64(n) => n,
                            _ => {
                                return Err(QueryError::TypeMismatch(
                                    "sum/avg need numbers".into(),
                                ));
                            }
                        };
                        acc.value = Some(if a.function == BagFn::Avg {
                            // Incremental mean avoids overflowing the sum of finite operands.
                            let old = match acc.value {
                                Some(Val::F64(x)) => x,
                                _ => 0.0,
                            };
                            let n = acc.count as f64;
                            Val::F64(old * ((n - 1.0) / n) + number / n)
                        } else {
                            match (acc.value.take(), v) {
                                (None, v) => v,
                                (Some(Val::I64(x)), Val::I64(y)) => {
                                    Val::I64(x.checked_add(y).ok_or_else(|| {
                                        QueryError::Overflow("sum doesn't fit in an i64".into())
                                    })?)
                                }
                                (Some(Val::F64(x)), Val::F64(y)) => Val::F64(x + y),
                                (Some(Val::I64(x)), Val::F64(y)) => Val::F64(x as f64 + y),
                                (Some(Val::F64(x)), Val::I64(y)) => Val::F64(x + y as f64),
                                _ => unreachable!("numeric total"),
                            }
                        });
                        if matches!(acc.value, Some(Val::F64(x)) if x.is_nan()) {
                            return Err(QueryError::Overflow(
                                "aggregate has no numeric value".into(),
                            ));
                        }
                    }
                }
            }
        }
        drop(rows);
        let mut out = BTreeSet::new();
        for (key, accs) in groups {
            let mut row = vec![None; self.width];
            for (s, v) in keys.iter().zip(key) {
                row[*s] = Some(v);
            }
            for (a, acc) in aggs.iter().zip(accs) {
                row[a.output] = Some(match a.function {
                    BagFn::Count => Val::I64(acc.count),
                    BagFn::Collect => {
                        let bytes = 64 + acc.values.len().saturating_mul(size_of::<Val>());
                        self.budget.charge_bytes(bytes)?;
                        self.budget.step(acc.values.len() as u64)?;
                        let (v, new) = self
                            .interner
                            .list(acc.values, self.budget.limits.max_bytes)?;
                        if !new {
                            self.budget.release_bytes(bytes);
                        }
                        v
                    }
                    BagFn::Sum => acc.value.unwrap_or(Val::I64(0)),
                    _ => acc.value.unwrap_or(Val::Null),
                });
            }
            self.budget.emit(&mut out, row)?;
        }
        self.budget.release_bytes(held);
        Ok(out)
    }
}

impl Exec<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bag_project(
        &mut self,
        rows: BTreeSet<Row>,
        vars: &[usize],
        distinct: bool,
        keys: &[(usize, bool)],
        skip: &Option<crate::Term>,
        limit: &Option<crate::Term>,
        ordinal: usize,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let count = |t: &Option<crate::Term>, default| match t {
            Some(crate::Term::Const(kala_core::Value::I64(n))) => *n as usize,
            _ => default,
        };
        let mut seen = HashSet::new();
        let mut input = Vec::new();
        let mut held = 0;
        for row in rows {
            self.budget.step(1)?;
            if distinct {
                let bytes = index_entry_bytes(vars.len());
                self.budget.charge_bytes(bytes)?;
                let key: Vec<_> = vars.iter().map(|s| row[*s].clone()).collect();
                if !seen.insert(key) {
                    self.budget.release_bytes(bytes);
                    continue;
                }
                held += bytes;
            }
            self.budget.charge_bytes(size_of::<Row>())?;
            held += size_of::<Row>();
            input.push(row);
        }
        let order = crate::exec::sort_by(input.len(), &mut self.budget, |a, b, budget| {
            for (s, desc) in keys {
                let a = input[a][*s].as_ref().expect("compiled order key");
                let b = input[b][*s].as_ref().expect("compiled order key");
                budget.step(
                    ((super::super::val::result_row_bytes(std::slice::from_ref(a)).saturating_add(
                        super::super::val::result_row_bytes(std::slice::from_ref(b)),
                    )) / 64) as u64,
                )?;
                let cmp = rows::order(a, b, &mut budget.cmp);
                budget.charge_cmp()?;
                if !cmp.is_eq() {
                    return Ok(if *desc { cmp.reverse() } else { cmp });
                }
            }
            Ok(std::cmp::Ordering::Equal)
        })?;
        let mut out = BTreeSet::new();
        for (n, index) in order
            .iter()
            .skip(count(skip, 0))
            .take(count(limit, usize::MAX))
            .enumerate()
        {
            let mut row = vec![None; self.width];
            for s in vars {
                row[*s] = input[*index][*s].clone();
            }
            row[ordinal] = Some(Val::I64(n as i64));
            self.budget.emit(&mut out, row)?;
        }
        self.budget
            .release_bytes(held + order.len() * size_of::<usize>());
        Ok(out)
    }
}
