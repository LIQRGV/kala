//! `Aggregate`: grouping rows and folding each group's values.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

use kala_core::ValueType;

use crate::ir::AggFn;

use super::QueryError;
use super::eval::Exec;
use super::program::AggStep;
use super::val::{CmpWork, ROW_OVERHEAD_BYTES, Row, Val, ValKind, cmp_real};

/// Charged per distinct value a `count-distinct` group holds.
const DISTINCT_BYTES: usize = size_of::<Val>() + ROW_OVERHEAD_BYTES;

/// One group's running state for one aggregate.
enum Acc {
    /// `count`: bindings seen.
    Count(i64),
    /// `count-distinct`: the distinct values seen.
    Distinct(BTreeSet<Val>),
    /// `sum`: nothing yet, or the running i64 or f64 total.
    Sum(Option<Val>),
    /// `min` / `max`: the extreme value seen so far.
    Min(Option<Val>),
    Max(Option<Val>),
}

impl Acc {
    fn new(func: AggFn) -> Acc {
        match func {
            AggFn::Count => Acc::Count(0),
            AggFn::CountDistinct => Acc::Distinct(BTreeSet::new()),
            AggFn::Sum => Acc::Sum(None),
            AggFn::Min => Acc::Min(None),
            AggFn::Max => Acc::Max(None),
        }
    }

    /// Folds in one binding's value `v` of `a`'s variable (`min`/`max` in
    /// real value order, their comparison work added to `work`). Returns
    /// whether a new distinct value is now held (to be charged).
    fn add(&mut self, a: &AggStep, v: Val, work: &mut CmpWork) -> Result<bool, QueryError> {
        if !agg_takes(a.func, v.kind()) {
            return Err(QueryError::TypeMismatch(format!(
                "{} of ?{} needs {}, got {}",
                a.func.name(),
                a.name,
                agg_wants(a.func),
                v.kind().name()
            )));
        }
        match self {
            Acc::Count(n) => *n = n.saturating_add(1),
            Acc::Distinct(seen) => return Ok(seen.insert(v)),
            Acc::Sum(total) => {
                *total = Some(match (total.take(), v) {
                    (None, v) => v,
                    (Some(Val::I64(x)), Val::I64(y)) => {
                        Val::I64(x.checked_add(y).ok_or_else(|| {
                            QueryError::Overflow(format!(
                                "sum of ?{} doesn't fit in an i64",
                                a.name
                            ))
                        })?)
                    }
                    // IEEE addition: a total may reach ±inf; that is the
                    // answer. NaN (+inf plus -inf) is no value at all.
                    (Some(Val::F64(x)), Val::F64(y)) => {
                        let t = x + y;
                        if t.is_nan() {
                            return Err(QueryError::Overflow(format!(
                                "sum of ?{} adds +inf and -inf, which has no value",
                                a.name
                            )));
                        }
                        Val::F64(t)
                    }
                    (Some(t), v) => {
                        return Err(QueryError::TypeMismatch(format!(
                            "sum of ?{} mixes {} and {} values",
                            a.name,
                            t.kind().name(),
                            v.kind().name()
                        )));
                    }
                });
            }
            Acc::Min(m) => {
                if m.as_ref().is_none_or(|m| cmp_real(&v, m, work).is_lt()) {
                    *m = Some(v);
                }
            }
            Acc::Max(m) => {
                if m.as_ref().is_none_or(|m| cmp_real(&v, m, work).is_gt()) {
                    *m = Some(v);
                }
            }
        }
        Ok(false)
    }

    /// The group's result. Every group has at least one binding, so every
    /// state holds a value.
    fn finish(self) -> Val {
        let count = |n: usize| Val::I64(i64::try_from(n).unwrap_or(i64::MAX));
        match self {
            Acc::Count(n) => Val::I64(n),
            Acc::Distinct(seen) => count(seen.len()),
            Acc::Sum(v) | Acc::Min(v) | Acc::Max(v) => {
                v.expect("invariant: a group exists only once a binding was folded in")
            }
        }
    }
}

/// Whether `func` takes values of `kind`: counts take anything, `min` and
/// `max` ordered scalars (i64, f64, string), `sum` numbers.
pub(super) fn agg_takes(func: AggFn, kind: ValKind) -> bool {
    match func {
        AggFn::Count | AggFn::CountDistinct => true,
        AggFn::Min | AggFn::Max => matches!(
            kind,
            ValKind::Value(ValueType::I64 | ValueType::F64 | ValueType::Str)
        ),
        AggFn::Sum => matches!(kind, ValKind::Value(ValueType::I64 | ValueType::F64)),
    }
}

/// What `func` takes, for error messages.
pub(super) fn agg_wants(func: AggFn) -> &'static str {
    match func {
        AggFn::Count | AggFn::CountDistinct => "any value",
        AggFn::Min | AggFn::Max => "i64, f64 or string values",
        AggFn::Sum => "i64 or f64 values",
    }
}

impl Exec<'_, '_> {
    /// Groups `rows` (a set: one row per distinct binding of every input
    /// slot) by the `keys` slots and computes `aggs` per group. One step
    /// per input row and per output row; each group and each distinct value
    /// a `count-distinct` holds is charged while held, and the groups count
    /// toward `max_rows`. No input rows, no groups: no output rows.
    pub(super) fn aggregate(
        &mut self,
        rows: BTreeSet<Row>,
        keys: &[usize],
        aggs: &[AggStep],
    ) -> Result<BTreeSet<Row>, QueryError> {
        let bound = |row: &Row, s: usize| -> Val {
            row[s]
                .clone()
                .expect("invariant: compile checked the input binds every aggregate slot")
        };
        let group_bytes = self
            .budget
            .row_bytes
            .saturating_add(aggs.len().saturating_mul(size_of::<Acc>()));
        let mut groups: BTreeMap<Vec<Val>, Vec<Acc>> = BTreeMap::new();
        let mut held = 0usize;
        for row in &rows {
            self.budget.step(1)?;
            let key: Vec<Val> = keys.iter().map(|&s| bound(row, s)).collect();
            let full = groups.len() >= self.budget.limits.max_rows;
            let accs = match groups.entry(key) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(_) if full => {
                    return Err(QueryError::RowLimit {
                        limit: self.budget.limits.max_rows,
                    });
                }
                Entry::Vacant(e) => {
                    self.budget.charge_bytes(group_bytes)?;
                    held = held.saturating_add(group_bytes);
                    e.insert(aggs.iter().map(|a| Acc::new(a.func)).collect())
                }
            };
            for (acc, a) in accs.iter_mut().zip(aggs) {
                let new = acc.add(a, bound(row, a.var), &mut self.budget.cmp)?;
                self.budget.charge_cmp()?;
                if new {
                    self.budget.charge_bytes(DISTINCT_BYTES)?;
                    held = held.saturating_add(DISTINCT_BYTES);
                }
            }
        }
        // The input's rows are released by the caller, as for every step.
        drop(rows);
        let mut out = BTreeSet::new();
        for (key, accs) in groups {
            let mut r = vec![None; self.width];
            for (&s, v) in keys.iter().zip(key) {
                r[s] = Some(v);
            }
            for (acc, a) in accs.into_iter().zip(aggs) {
                r[a.out] = Some(acc.finish());
            }
            self.budget.emit(&mut out, r)?;
        }
        self.budget.release_bytes(held);
        Ok(out)
    }
}
