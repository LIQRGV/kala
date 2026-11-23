//! The executor: runs a [`Plan`] against a [`View`] (a pinned canon) through
//! kala-core's public reads only.
//!
//! Relational operators retain a set of complete bindings. The opt-in `Output`
//! operator projects bags or sets, orders and slices; `Row` evaluates nullable
//! expressions and optional reads. Without `Output`, final rows keep the
//! original sorted, duplicate-free semantics. Front-ends select these explicit
//! language-neutral operators.
//!
//! Execution is two passes. `compile` checks the plan statically (attribute
//! names, which variables each operator binds, access feasibility, params)
//! and resolves params to constants; `eval` then reads the indexes. Errors
//! therefore don't depend on the data, except for the types of values bound
//! to variables, which are checked per row.
//!
//! Neither pass recurses: a plan is walked with an explicit stack and run as
//! a flat program, so a deep plan (a long left-deep chain of `Expand`s, from
//! any front-end) costs heap, not native stack. Plans deeper than
//! [`MAX_PLAN_DEPTH`] are refused up front with [`QueryError::Plan`].
//!
//! Limits are enforced as errors, never by truncating: every index key a
//! read examines and every row produced by any operator costs one step, no
//! operator may hold more than `max_rows` rows, and the row sets, scans and
//! strings alive at once may not take more than `max_bytes`.
//!
//! `Fixpoint` and `Rel` run, recursion included, as do `Not`,
//! `Difference`, `Intersect` and `History` (change points at every tick, at
//! the view's canon: one [`View::history`] read per row with the entity
//! bound, else one [`View::history_scan`] per attribute, cached for the
//! query). `During` binds the interval set where a fact holds (at the
//! view's canon, over every tick): one [`View::holds_during`] read per row
//! with the entity and value bound, one [`View::history`] read with only a
//! (forward) entity bound, else the cached `history_scan` grouped by
//! `(entity, value)`, once per query. `Apply` runs the interval-set
//! functions ([`Func`]); every set it or `During` creates is charged to the
//! byte budget by size. `Aggregate` groups its input's rows (a set: one per
//! distinct binding of every variable the input binds) by its group
//! variables and binds one row per group: `count` (bindings), `count-distinct`
//! (values), `sum` (over the bindings; i64 checked, [`QueryError::Overflow`],
//! or IEEE f64, where ±inf is a result and NaN an overflow), `min`/`max`
//! (i64, f64 or string); no input, no rows. It may not appear inside a
//! `Fixpoint`'s rules, which must stay monotone.
//!
//! How rows hold values (shared, interned strings and interval sets) is
//! described in `exec/val.rs`; how a `Fixpoint` compiles and is evaluated
//! semi-naively, in `exec/eval/fixpoint.rs`.
//!
//! [`Func`]: crate::ir::Func

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::AtomicBool;

use kala_core::{Error as CoreError, Tick, TxId, Value, ValueType, View, WorkMeter, World};

use crate::ParameterInput;
use crate::datum::Datum;
use crate::ir::{CanonSource, Plan, TickSource, Var};

use budget::{Budget, Memo, check_cancel};
use compile::{compile, plan_depth};
use eval::{Exec, Relation};
use program::Slots;
use val::{CmpWork, Interner, Val, cmp_real_rows, result_row_bytes, row_bytes, tuple_bytes};

mod agg;
mod budget;
mod compile;
mod error;
mod eval;
mod program;
mod val;

pub use error::QueryError;

/// The most operators a plan may nest on one root-to-leaf path (the root
/// and the `Unit` leaf included). Deeper plans are refused with
/// [`QueryError::Plan`] before anything runs.
///
/// The executor itself doesn't recurse, so this isn't what keeps it off the
/// native stack; it is a sanity bound for plans no front-end should build
/// (Datalog's 256-pattern limit gives a few hundred), and it keeps such
/// plans away from code that does recurse over [`Op`] (its derived `Clone`,
/// `Debug` and `PartialEq`).
///
/// [`Op`]: crate::ir::Op
pub const MAX_PLAN_DEPTH: usize = 4096;

/// Query parameters by name (without `?`/`$`). Tick params are
/// [`Value::I64`], canon params non-negative [`Value::I64`].
pub type Params = BTreeMap<String, Value>;

/// Resource limits for one query. Exceeding any is an error
/// ([`QueryError::RowLimit`], [`QueryError::StepBudget`],
/// [`QueryError::MemoryBudget`]), never a truncated answer. All three are
/// deterministic: they count work and sizes, never time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most rows any single operator (intermediate or final) may produce.
    pub max_rows: usize,
    /// The most steps the query may take: one per index key examined by a
    /// kala-core read, one per index read and one per row produced by any
    /// operator, plus about one per 64 bytes (or 4 intervals) compared in
    /// value order (ordering filters, `min`/`max`, sorting the answer).
    pub max_steps: u64,
    /// The most bytes the query may hold at once: its materialized row sets
    /// (a fixed cost per row slot), its scans, the strings it read (each
    /// distinct string once, however many rows share it) and the final
    /// [`ResultSet`] at its real size.
    pub max_bytes: usize,
}

impl Default for Limits {
    /// 100 000 rows, 10 000 000 steps, 256 MiB.
    fn default() -> Limits {
        Limits {
            max_rows: 100_000,
            max_steps: 10_000_000,
            max_bytes: 256 * 1024 * 1024,
        }
    }
}

/// A query's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultSet {
    /// The result columns: the plan's `find` variables, in order.
    pub columns: Vec<Var>,
    /// The rows, one datum per column. Set plans sort and deduplicate; an
    /// `Output` root preserves its requested order and multiplicity.
    pub rows: Vec<Vec<Datum>>,
    /// The resolved default tick (what [`TickSource::Default`] read at).
    pub tick: Tick,
    /// The canon the query read at.
    pub canon: TxId,
}

/// Runs a plan with a caller-owned cancellation flag.
pub fn run_with_cancel(
    world: &World,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<ResultSet, QueryError> {
    run_tracked(world, plan, params, limits, cancel).map(|(rows, _)| rows)
}

/// Measured resource usage of a successful run. A batch can deduct these
/// values from its remaining budget without changing [`Limits`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Steps charged by the executor, including index reads.
    pub steps: u64,
    /// Peak charged live memory, including the final answer.
    pub peak_bytes: usize,
}

/// Runs a plan and reports the resources it consumed. Batches conservatively
/// sum each run's peak memory, even though runs execute sequentially.
pub fn run_with_usage(
    world: &World,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
) -> Result<(ResultSet, Usage), QueryError> {
    run_tracked(world, plan, params, limits, &AtomicBool::new(false))
}

/// Resolves `plan.canon` against `world` (a canon past the latest is the
/// latest) and runs the plan there with [`execute`].
pub fn run(
    world: &World,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
) -> Result<ResultSet, QueryError> {
    run_with_cancel(world, plan, params, limits, &AtomicBool::new(false))
}

fn run_tracked(
    world: &World,
    plan: &Plan,
    params: &dyn ParameterInput,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(ResultSet, Usage), QueryError> {
    check_cancel(cancel)?;
    let latest = world.latest_tx();
    let canon = match &plan.canon {
        CanonSource::Latest => latest,
        CanonSource::Const(n) => TxId(*n),
        CanonSource::Param(name) => match params.scalar(name)? {
            Value::I64(n) if *n >= 0 => TxId(n.unsigned_abs()),
            _ => {
                return Err(QueryError::ParamType {
                    name: name.clone(),
                    expected: "non-negative i64".into(),
                });
            }
        },
    };
    execute_on(
        world,
        &world.at_canon(canon.min(latest)),
        plan,
        params,
        limits,
        cancel,
    )
}

/// Runs `plan` on `view`. `plan.canon` is ignored: the view is already
/// pinned. The default tick is resolved once ([`View::latest_tick`] at most
/// once).
pub fn execute(
    view: &View,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
) -> Result<ResultSet, QueryError> {
    execute_on(
        view.world(),
        view,
        plan,
        params,
        limits,
        &AtomicBool::new(false),
    )
    .map(|(rows, _)| rows)
}

fn execute_on(
    world: &World,
    view: &View,
    plan: &Plan,
    params: &dyn ParameterInput,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(ResultSet, Usage), QueryError> {
    let meter = WorkMeter::bounded(limits.max_steps, Some(cancel));
    execute_inner(world, view, plan, params, limits, cancel, &meter).map_err(|error| match error {
        QueryError::Core(CoreError::WorkInterrupted { reason }) => match reason {
            kala_core::WorkInterruptReason::Limit => QueryError::StepBudget {
                limit: limits.max_steps,
            },
            kala_core::WorkInterruptReason::Stopped => QueryError::Cancelled,
        },
        other => other,
    })
}

fn execute_inner(
    world: &World,
    view: &View,
    plan: &Plan,
    params: &dyn ParameterInput,
    limits: &Limits,
    cancel: &AtomicBool,
    meter: &WorkMeter,
) -> Result<(ResultSet, Usage), QueryError> {
    check_cancel(cancel)?;
    let depth = plan_depth(&plan.root);
    if depth > MAX_PLAN_DEPTH {
        return Err(QueryError::Plan(format!(
            "plan too deep: {depth} nested operators, at most {MAX_PLAN_DEPTH}"
        )));
    }
    for name in &plan.params {
        if !params.contains_key(name) {
            return Err(QueryError::MissingParam(name.clone()));
        }
    }
    // Every read below goes through the metered view, so its index work is
    // charged to the step budget (after each kala-core call).
    let view = &view.metered(meter);
    let default_tick = match &plan.default_tick {
        TickSource::Default => view.latest_tick()?,
        TickSource::Const(t) => *t,
        TickSource::Param(name) => tick_param(params, name)?,
        TickSource::Var(v) => {
            return Err(QueryError::Plan(format!(
                "the default tick can't be a variable (?{})",
                v.0
            )));
        }
    };
    let mut slots = Slots::default();
    // Plan constants are interned first, uncharged; the rest of the query's
    // strings and interval sets join the same table.
    let mut interner = Interner::default();
    let (root, output) = match &plan.root {
        crate::Op::Output { input, spec } => (input.as_ref(), Some(spec)),
        root => (root, None),
    };
    let mut budget = Budget {
        cancel,
        peak_bytes: 0,
        steps: 0,
        bytes: 0,
        row_bytes: 0,
        cmp: CmpWork::default(),
        memo: Memo::default(),
        limits: *limits,
    };
    let prog = compile(
        root,
        world,
        view.canon(),
        params,
        default_tick,
        &mut slots,
        &mut interner,
        &mut budget,
    )?;
    let mut find = Vec::with_capacity(plan.find.len());
    for v in output.map_or(&plan.find, |spec| &spec.vars) {
        match slots.get(v) {
            Some(s) if prog.bound.contains(&s) => find.push(s),
            _ => {
                return Err(QueryError::Plan(format!(
                    "find variable ?{} is not bound by the plan (its root is {})",
                    v.0,
                    plan.root.name()
                )));
            }
        }
    }
    let width = slots.len();
    budget.row_bytes = row_bytes(width);
    let mut ex = Exec {
        world,
        view,
        meter,
        width,
        budget,
        interner,
        scans: HashMap::new(),
        history_scans: HashMap::new(),
        during_scans: HashMap::new(),
        checked_types: HashSet::new(),
        node_types: None,
        rels: prog
            .rels
            .iter()
            .map(|cols| Relation {
                tuples: BTreeSet::new(),
                delta: BTreeSet::new(),
                new: BTreeSet::new(),
                tuple_bytes: tuple_bytes(cols.len()),
            })
            .collect(),
    };
    // The default tick's read (a walk of the E index at an older canon).
    ex.charge_index_work()?;
    let rows = ex.eval(&prog.steps)?;
    let rows = if let Some(spec) = output {
        output_rows(&rows, &find, spec, &slots, params, &mut ex.budget)?
    } else {
        let mut out = BTreeSet::new();
        for row in &rows {
            let r: Vec<Val> = find
                .iter()
                .map(|&s| row[s].clone().expect("checked find binding"))
                .collect();
            out.insert(r);
        }
        let result_bytes = out.iter().map(|r| result_row_bytes(r)).sum();
        ex.budget.charge_bytes(result_bytes)?;
        let out: Vec<Vec<Val>> = out.into_iter().collect();
        let order = sort_real(&out, &mut ex.budget)?;
        order
            .into_iter()
            .map(|i| out[i].iter().map(Val::to_datum).collect())
            .collect()
    };
    check_cancel(cancel)?;
    Ok((
        ResultSet {
            columns: plan.find.clone(),
            rows,
            tick: default_tick,
            canon: view.canon(),
        },
        Usage {
            steps: ex.budget.steps,
            peak_bytes: ex.budget.peak_bytes,
        },
    ))
}

/// The order of `rows` (their indices) ascending by [`cmp_real_rows`]: a
/// stable bottom-up merge sort whose comparisons are charged to `budget` as
/// they happen, so a sort of many long values stops at the step budget
/// instead of finishing first. Deterministic: the comparisons made depend
/// only on the rows.
fn sort_real(rows: &[Vec<Val>], budget: &mut Budget) -> Result<Vec<usize>, QueryError> {
    let n = rows.len();
    let mut a: Vec<usize> = (0..n).collect();
    let mut b = vec![0; n];
    let mut width = 1;
    while width < n {
        let mut lo = 0;
        while lo < n {
            let mid = lo.saturating_add(width).min(n);
            let hi = lo.saturating_add(width.saturating_mul(2)).min(n);
            let (mut i, mut j, mut k) = (lo, mid, lo);
            while i < mid && j < hi {
                let ord = cmp_real_rows(&rows[a[i]], &rows[a[j]], &mut budget.cmp);
                budget.charge_cmp()?;
                if ord.is_le() {
                    b[k] = a[i];
                    i += 1;
                } else {
                    b[k] = a[j];
                    j += 1;
                }
                k += 1;
            }
            b[k..k + (mid - i)].copy_from_slice(&a[i..mid]);
            k += mid - i;
            b[k..hi].copy_from_slice(&a[j..hi]);
            lo = hi;
        }
        std::mem::swap(&mut a, &mut b);
        width = width.saturating_mul(2);
    }
    Ok(a)
}

fn tick_param(params: &dyn ParameterInput, name: &str) -> Result<Tick, QueryError> {
    match params.scalar(name)? {
        Value::I64(n) => Ok(Tick(*n)),
        _ => Err(QueryError::ParamType {
            name: name.to_owned(),
            expected: "i64".into(),
        }),
    }
}

fn type_name(t: ValueType) -> &'static str {
    match t {
        ValueType::Bool => "bool",
        ValueType::I64 => "i64",
        ValueType::F64 => "f64",
        ValueType::Str => "string",
        ValueType::Ref => "ref",
    }
}

#[cfg(test)]
mod scan_meter_tests;

fn count_term(
    term: Option<&crate::Term>,
    params: &dyn ParameterInput,
    default: usize,
) -> Result<usize, QueryError> {
    let Some(term) = term else { return Ok(default) };
    let value = match term {
        crate::Term::Const(v) => v,
        crate::Term::Param(p) => params.scalar(p)?,
        _ => {
            return Err(QueryError::Plan(
                "pagination needs an integer literal or parameter".into(),
            ));
        }
    };
    match value {
        Value::I64(n) if *n >= 0 => Ok(usize::try_from(*n).unwrap_or(usize::MAX)),
        _ => Err(QueryError::TypeMismatch(
            "pagination needs a non-negative integer".into(),
        )),
    }
}
fn output_rows(
    rows: &BTreeSet<val::Row>,
    find: &[usize],
    spec: &crate::OutputSpec,
    slots: &Slots,
    params: &dyn ParameterInput,
    budget: &mut Budget,
) -> Result<Vec<Vec<Datum>>, QueryError> {
    let skip = count_term(spec.skip.as_ref(), params, 0)?;
    let limit = count_term(spec.limit.as_ref(), params, usize::MAX)?;
    let keys = spec
        .order
        .iter()
        .map(|(v, desc)| {
            slots
                .get(v)
                .map(|s| (s, *desc))
                .ok_or_else(|| QueryError::Plan("unbound order key".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = HashSet::new();
    let key_bytes = val::index_entry_bytes(find.len());
    let mut seen_bytes = 0usize;
    let mut out = Vec::new();
    let mut orders = Vec::new();
    for row in rows {
        budget.step(1)?;
        budget.charge_bytes(tuple_bytes(find.len()))?;
        let value: Vec<_> = find
            .iter()
            .map(|&s| row[s].clone().expect("checked find binding"))
            .collect();
        if spec.distinct {
            // The hash key and projected output retain separate tuples.
            budget.charge_bytes(key_bytes)?;
            if !seen.insert(value.clone()) {
                budget.release_bytes(key_bytes.saturating_add(tuple_bytes(find.len())));
                continue;
            }
            seen_bytes = seen_bytes.saturating_add(key_bytes);
        }
        budget.charge_bytes(result_row_bytes(&value).saturating_add(tuple_bytes(keys.len())))?;
        orders.push(
            keys.iter()
                .map(|&(s, _)| {
                    row[s]
                        .clone()
                        .ok_or_else(|| QueryError::Plan("unbound order key".into()))
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        out.push(value);
    }
    drop(seen);
    budget.release_bytes(seen_bytes);
    let order = sort_by(out.len(), budget, |a, b, budget| {
        for (k, (_, desc)) in keys.iter().enumerate() {
            // Charge list traversal as well as string comparison prefixes.
            budget.step(
                (result_row_bytes(std::slice::from_ref(&orders[a][k]))
                    .saturating_add(result_row_bytes(std::slice::from_ref(&orders[b][k])))
                    / 64) as u64,
            )?;
            let cmp = eval::order(&orders[a][k], &orders[b][k], &mut budget.cmp);
            budget.charge_cmp()?;
            if !cmp.is_eq() {
                return Ok(if *desc { cmp.reverse() } else { cmp });
            }
        }
        Ok(std::cmp::Ordering::Equal)
    })?;
    Ok(order
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|i| out[i].iter().map(Val::to_datum).collect())
        .collect())
}
fn sort_values(values: &[Val], budget: &mut Budget) -> Result<Vec<usize>, QueryError> {
    sort_by(values.len(), budget, |a, b, budget| {
        let cmp = val::cmp_real(&values[a], &values[b], &mut budget.cmp);
        budget.charge_cmp()?;
        Ok(cmp)
    })
}
/// Fallible stable merge sort: temporary indices are charged before allocation,
/// and every comparison checks cancellation and the work budget.
fn sort_by(
    n: usize,
    budget: &mut Budget,
    mut compare: impl FnMut(usize, usize, &mut Budget) -> Result<std::cmp::Ordering, QueryError>,
) -> Result<Vec<usize>, QueryError> {
    budget.charge_bytes(n.saturating_mul(2 * std::mem::size_of::<usize>()))?;
    let mut a: Vec<_> = (0..n).collect();
    let mut b = vec![0; n];
    let mut width: usize = 1;
    while width < n {
        let mut lo = 0;
        while lo < n {
            let mid = lo.saturating_add(width).min(n);
            let hi = mid.saturating_add(width).min(n);
            let (mut i, mut j, mut k) = (lo, mid, lo);
            while i < mid && j < hi {
                budget.step(1)?;
                if compare(a[i], a[j], budget)?.is_le() {
                    b[k] = a[i];
                    i += 1;
                } else {
                    b[k] = a[j];
                    j += 1;
                }
                k += 1;
            }
            b[k..k + mid - i].copy_from_slice(&a[i..mid]);
            k += mid - i;
            b[k..hi].copy_from_slice(&a[j..hi]);
            lo = hi;
        }
        std::mem::swap(&mut a, &mut b);
        width = width.saturating_mul(2);
    }
    budget.release_bytes(n.saturating_mul(std::mem::size_of::<usize>()));
    Ok(a)
}

/// Run with nullable/list parameters, leaving the original scalar API unchanged.
pub fn run_with_values(
    world: &World,
    plan: &Plan,
    params: &crate::QueryParams,
    limits: &Limits,
) -> Result<ResultSet, QueryError> {
    run_with_values_and_cancel(world, plan, params, limits, &AtomicBool::new(false))
}
/// Run nullable/list parameters with caller-owned cancellation.
pub fn run_with_values_and_cancel(
    world: &World,
    plan: &Plan,
    params: &crate::QueryParams,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<ResultSet, QueryError> {
    run_tracked(world, plan, params, limits, cancel).map(|(rows, _)| rows)
}
