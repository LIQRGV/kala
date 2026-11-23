//! The executor: runs a [`Plan`] against a [`View`] (a pinned canon) through
//! kala-core's public reads only.
//!
//! Every operator produces a **set** of rows; the final rows are sorted
//! ascending by [`Value`]'s order and duplicate-free. The executor is
//! language-neutral: bag semantics, NULLs and the like are a front-end's
//! business, decided before a plan reaches here.
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
//! Inside the executor, rows hold [`Val`]s: a [`Value`], an op or an
//! interval set (the three kinds of [`Datum`]), whose strings and interval
//! sets are shared (`Rc`) and interned once per query with a dense id
//! (hash-consing: equal contents, one id): a long string read once and
//! joined into many rows is stored once, not once per row, and every
//! row-set operation (set inserts, joins, anti/semi-joins, `Rel` indexes,
//! grouping, relations) compares and hashes it by id, in O(1) whatever its
//! length. Row sets are therefore kept in id order, not [`Datum`] order;
//! what needs real value order (`<`-style filters, `min`/`max`, and the final
//! [`ResultSet`], sorted once at the end) compares contents and charges that
//! work to the step budget: about one step per 64 bytes and per 4 intervals
//! compared. Only the final [`ResultSet`] holds plain [`Datum`]s.
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
//! A `Fixpoint` compiles inline into the flat program: `FixBegin`, each
//! rule's steps followed by `RuleEnd` (which folds the rule's rows into its
//! relation as positional tuples, a set), `FixLoop`, the body's steps, then
//! `FixEnd` (the relations are dropped; the body's rows are the
//! `Fixpoint`'s). A `Rel` resolves at compile time to the nearest enclosing
//! `Fixpoint` defining its name; inside that `Fixpoint`'s own rules it is a
//! recursive read, numbered.
//!
//! Recursion is evaluated semi-naively, with a program counter and no
//! native recursion. Round 0 runs every rule once with recursive reads
//! empty. Each later round runs, for every recursive read of every rule
//! (a *variant*: the rule's contiguous run of steps), the rule with that
//! read seeing only the previous round's new tuples (the delta) and every
//! other read the whole relation; `FixLoop` keeps what is new, and stops
//! once no delta a variant reads is non-empty. The result equals naive
//! iteration (set semantics), each derivation costing work about once.
//! Relations of enclosing `Fixpoint`s are fixed and read in full. All limits
//! hold across rounds: rows (each relation, with its new tuples, and each
//! operator's output), steps and bytes (relations and deltas held).

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use kala_core::{
    Direction, EdgeId, EntityId, Error as CoreError, Interval, IntervalSet, MeteredView,
    Op as FactOp, Tick, TxId, Value, ValueType, View, WorkMeter, World,
};

use kala_core::Precision;

use crate::datum::{Datum, op_rank};
use crate::ir::{
    Access, AggFn, AttrRef, CanonSource, CmpOp, Func, Op, OpTerm, Plan, Term, TickSource, Var,
};

/// The most operators a plan may nest on one root-to-leaf path (the root
/// and the `Unit` leaf included). Deeper plans are refused with
/// [`QueryError::Plan`] before anything runs.
///
/// The executor itself doesn't recurse, so this isn't what keeps it off the
/// native stack; it is a sanity bound for plans no front-end should build
/// (Datalog's 256-pattern limit gives a few hundred), and it keeps such
/// plans away from code that does recurse over [`Op`] (its derived `Clone`,
/// `Debug` and `PartialEq`).
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
    /// The rows, one datum per column, sorted ascending ([`Datum`]'s order,
    /// lexicographic) with no duplicates.
    pub rows: Vec<Vec<Datum>>,
    /// The resolved default tick (what [`TickSource::Default`] read at).
    pub tick: Tick,
    /// The canon the query read at.
    pub canon: TxId,
}

/// Why a query couldn't run.
#[derive(Debug)]
pub enum QueryError {
    /// The caller cancelled this run; no partial answer is returned.
    Cancelled,
    /// A parameter the plan uses or declares has no value.
    MissingParam(String),
    /// A parameter has the wrong type for where it is used.
    ParamType {
        /// The parameter's name.
        name: String,
        /// What was expected (`i64`, `ref`, `non-negative i64`, ...).
        expected: String,
    },
    /// An operator reads at a tick variable that nothing before it binds.
    TickNotBound(String),
    /// A constant or a bound value has the wrong type for where it is used.
    TypeMismatch(String),
    /// The plan is malformed: an infeasible access, an unbound variable, a
    /// union of inputs binding different variables, ...
    Plan(String),
    /// An operator produced more than `limit` rows.
    RowLimit {
        /// The row limit that was exceeded.
        limit: usize,
    },
    /// The query took more than `limit` steps.
    StepBudget {
        /// The step budget that was exceeded.
        limit: u64,
    },
    /// The query needed more than `limit` bytes at once.
    MemoryBudget {
        /// The byte budget that was exceeded.
        limit: usize,
    },
    /// Arithmetic left its type's range: an i64 `sum` aggregate that
    /// doesn't fit in an i64.
    Overflow(String),
    /// A kala-core read failed (unknown attribute, unknown node type, ...).
    Core(CoreError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Cancelled => write!(f, "cancelled"),
            QueryError::MissingParam(n) => write!(f, "missing parameter ${n}"),
            QueryError::ParamType { name, expected } => {
                write!(f, "parameter ${name} must be {expected}")
            }
            QueryError::TickNotBound(v) => {
                write!(f, "tick variable ?{v} is not bound where it is used")
            }
            QueryError::TypeMismatch(m) => write!(f, "type mismatch: {m}"),
            QueryError::Plan(m) => write!(f, "invalid plan: {m}"),
            QueryError::RowLimit { limit } => {
                write!(f, "query produced more than {limit} rows")
            }
            QueryError::StepBudget { limit } => {
                write!(f, "query took more than {limit} steps")
            }
            QueryError::MemoryBudget { limit } => {
                write!(f, "query needed more than {limit} bytes of memory")
            }
            QueryError::Overflow(m) => write!(f, "arithmetic overflow: {m}"),
            QueryError::Core(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            QueryError::Core(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CoreError> for QueryError {
    fn from(e: CoreError) -> QueryError {
        QueryError::Core(e)
    }
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
    params: &Params,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(ResultSet, Usage), QueryError> {
    check_cancel(cancel)?;
    let latest = world.latest_tx();
    let canon = match &plan.canon {
        CanonSource::Latest => latest,
        CanonSource::Const(n) => TxId(*n),
        CanonSource::Param(name) => match params.get(name) {
            None => return Err(QueryError::MissingParam(name.clone())),
            Some(Value::I64(n)) if *n >= 0 => TxId(n.unsigned_abs()),
            Some(_) => {
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
    params: &Params,
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
    params: &Params,
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
    let prog = compile(
        &plan.root,
        world,
        view.canon(),
        params,
        default_tick,
        &mut slots,
        &mut interner,
    )?;
    let mut find = Vec::with_capacity(plan.find.len());
    for v in &plan.find {
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
    let mut ex = Exec {
        world,
        view,
        meter,
        width,
        budget: Budget {
            cancel,
            peak_bytes: 0,
            steps: 0,
            bytes: 0,
            row_bytes: row_bytes(width),
            cmp: CmpWork::default(),
            memo: Memo::default(),
            limits: *limits,
        },
        interner,
        scans: HashMap::new(),
        history_scans: HashMap::new(),
        during_scans: HashMap::new(),
        checked_types: HashSet::new(),
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
    let mut out = BTreeSet::new();
    for row in &rows {
        let r: Vec<Val> = find
            .iter()
            .map(|&s| {
                row[s]
                    .clone()
                    .expect("invariant: compile checked every find var is bound")
            })
            .collect();
        out.insert(r);
    }
    // The answer is charged at its real size (its strings are copies),
    // while the root's rows are still held, before it is built.
    let result_bytes = out.iter().map(|r| result_row_bytes(r)).sum();
    ex.budget.charge_bytes(result_bytes)?;
    drop(rows);
    // `out` is duplicate-free but in the executor's id order: sort it once
    // into the answer's order, charging long comparisons.
    let out: Vec<Vec<Val>> = out.into_iter().collect();
    let order = sort_real(&out, &mut ex.budget)?;
    let rows = order
        .into_iter()
        .map(|i| out[i].iter().map(Val::to_datum).collect())
        .collect();
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

fn tick_param(params: &Params, name: &str) -> Result<Tick, QueryError> {
    match params.get(name) {
        None => Err(QueryError::MissingParam(name.to_owned())),
        Some(Value::I64(n)) => Ok(Tick(*n)),
        Some(_) => Err(QueryError::ParamType {
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

// ---------------------------------------------------------------- compile

/// Variable name -> row slot.
#[derive(Default)]
struct Slots(BTreeMap<Var, usize>);

impl Slots {
    fn get(&self, v: &Var) -> Option<usize> {
        self.0.get(v).copied()
    }
    fn slot(&mut self, v: &Var) -> usize {
        let n = self.0.len();
        *self.0.entry(v.clone()).or_insert(n)
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// The kind of a [`Val`]: a value of one type, an op or an interval set.
/// Ordered as [`Datum`] orders its kinds (values by type in [`ValueType`]
/// order, which is the encoding's tag order; then ops; then interval sets).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ValKind {
    Value(ValueType),
    Op,
    Intervals,
    Precision,
}

impl ValKind {
    /// The kind's name in error messages.
    fn name(self) -> &'static str {
        match self {
            ValKind::Value(t) => type_name(t),
            ValKind::Op => "op",
            ValKind::Precision => "precision",
            ValKind::Intervals => "interval set",
        }
    }
}

/// A per-query dense id of an interned string or interval set: equal
/// contents share one id (hash-consing, see [`Interner`]), so rows compare
/// and hash such values in O(1), whatever their length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Id(u32);

/// A value inside the executor: a [`Datum`] whose string or interval set
/// is shared and interned, so copying a row copies a pointer, not the text.
///
/// Compared and hashed in O(1): a string or interval set by its [`Id`],
/// which is content equality (every one is interned by the query's
/// [`Interner`]). So row sets deduplicate exactly as the answer will, but
/// their internal *order* is not [`Datum`]'s: whatever needs real value
/// order (`<`-style filters, `min`/`max`, the final sort) uses
/// [`cmp_real`], which charges long comparisons to the step budget.
#[derive(Clone, Debug)]
enum Val {
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(Id, Rc<str>),
    Ref(EntityId),
    /// A change point's op (bound by a history read).
    Op(FactOp),
    Precision(Precision),
    /// An interval set (built by `During` or `Apply`).
    Intervals(Id, Rc<IntervalSet>),
}

impl Val {
    /// The plain value (a string is copied), if this is a value.
    fn to_value(&self) -> Option<Value> {
        Some(match self {
            Val::Bool(b) => Value::Bool(*b),
            Val::I64(n) => Value::I64(*n),
            Val::F64(x) => Value::F64(*x),
            Val::Str(_, s) => Value::Str(s.to_string()),
            Val::Ref(e) => Value::Ref(*e),
            Val::Op(_) | Val::Precision(_) | Val::Intervals(..) => return None,
        })
    }

    /// The answer's datum (a string or interval set is copied).
    fn to_datum(&self) -> Datum {
        match self {
            Val::Op(op) => Datum::Op(*op),
            Val::Precision(p) => Datum::Precision(*p),
            Val::Intervals(_, s) => Datum::Intervals(IntervalSet::clone(s)),
            v => Datum::Value(v.to_value().expect("invariant: every other Val is a value")),
        }
    }

    fn kind(&self) -> ValKind {
        match self {
            Val::Bool(_) => ValKind::Value(ValueType::Bool),
            Val::I64(_) => ValKind::Value(ValueType::I64),
            Val::F64(_) => ValKind::Value(ValueType::F64),
            Val::Str(..) => ValKind::Value(ValueType::Str),
            Val::Ref(_) => ValKind::Value(ValueType::Ref),
            Val::Op(_) => ValKind::Op,
            Val::Precision(_) => ValKind::Precision,
            Val::Intervals(..) => ValKind::Intervals,
        }
    }
}

impl Ord for Val {
    /// The executor's internal order: by kind ([`ValKind`]), then within the
    /// kind; floats as [`Value`] orders them (NaN included); strings and
    /// interval sets by [`Id`] (O(1), not their contents' order).
    fn cmp(&self, other: &Val) -> Ordering {
        match (self, other) {
            (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
            (Val::I64(a), Val::I64(b)) => a.cmp(b),
            (Val::F64(a), Val::F64(b)) => Value::F64(*a).cmp(&Value::F64(*b)),
            (Val::Str(a, _), Val::Str(b, _)) => a.cmp(b),
            (Val::Ref(a), Val::Ref(b)) => a.cmp(b),
            (Val::Precision(a), Val::Precision(b)) => (*a as u8).cmp(&(*b as u8)),
            (Val::Op(a), Val::Op(b)) => op_rank(*a).cmp(&op_rank(*b)),
            (Val::Intervals(a, _), Val::Intervals(b, _)) => a.cmp(b),
            _ => self.kind().cmp(&other.kind()),
        }
    }
}

impl PartialOrd for Val {
    fn partial_cmp(&self, other: &Val) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Val {
    fn eq(&self, other: &Val) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Val {}

impl Hash for Val {
    /// Hashes what `cmp` compares, so equal values hash alike.
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Val::Str(id, _) => (ValueType::Str, id).hash(state),
            Val::F64(x) => Value::F64(*x).hash(state),
            Val::Bool(b) => (ValueType::Bool, b).hash(state),
            Val::I64(n) => (ValueType::I64, n).hash(state),
            Val::Ref(e) => (ValueType::Ref, e.raw()).hash(state),
            Val::Op(op) => (ValKind::Op, op_rank(*op)).hash(state),
            Val::Precision(p) => (ValKind::Precision, *p as u8).hash(state),
            Val::Intervals(id, _) => (ValKind::Intervals, id).hash(state),
        }
    }
}

/// Bytes of string compared per step charged by [`cmp_real`].
const CMP_BYTES_PER_STEP: u64 = 64;

/// Intervals compared per step charged by [`cmp_real`].
const CMP_INTERVALS_PER_STEP: u64 = 4;

/// Comparison work done in real value order ([`cmp_real`]) and not yet
/// charged as steps: the lengths of the prefixes compared.
#[derive(Default)]
struct CmpWork {
    bytes: u64,
    intervals: u64,
}

impl CmpWork {
    /// The whole steps of work done so far, removed (the remainder stays,
    /// so the charge is deterministic however the work is split).
    fn take_steps(&mut self) -> u64 {
        let steps = self.bytes / CMP_BYTES_PER_STEP + self.intervals / CMP_INTERVALS_PER_STEP;
        self.bytes %= CMP_BYTES_PER_STEP;
        self.intervals %= CMP_INTERVALS_PER_STEP;
        steps
    }
}

/// How many leading elements `a` and `b` share, compared in chunks of
/// `chunk` (slice equality, so long equal runs are cheap even unoptimized).
fn common_prefix<T: PartialEq>(a: &[T], b: &[T], chunk: usize) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + chunk <= n && a[i..i + chunk] == b[i..i + chunk] {
        i += chunk;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// [`Datum`]'s order (the answer's): by kind, then within the kind, strings
/// by their bytes and interval sets as [`IntervalSet`] orders them. Values
/// that are one interned value (same [`Id`]) are equal at once; otherwise
/// the prefix compared is added to `work`.
fn cmp_real(a: &Val, b: &Val, work: &mut CmpWork) -> Ordering {
    match (a, b) {
        (Val::Str(x, s), Val::Str(y, t)) => {
            if x == y || Rc::ptr_eq(s, t) {
                return Ordering::Equal;
            }
            let (s, t) = (s.as_bytes(), t.as_bytes());
            let k = common_prefix(s, t, CMP_BYTES_PER_STEP as usize);
            work.bytes = work.bytes.saturating_add(k as u64 + 1);
            s[k..].cmp(&t[k..])
        }
        (Val::Intervals(x, s), Val::Intervals(y, t)) => {
            if x == y || Rc::ptr_eq(s, t) {
                return Ordering::Equal;
            }
            let (s, t) = (s.intervals(), t.intervals());
            let k = common_prefix(s, t, CMP_INTERVALS_PER_STEP as usize);
            work.intervals = work.intervals.saturating_add(k as u64 + 1);
            // Past the shared prefix: the first differing interval decides,
            // compared as `IntervalSet` does; a prefix sorts first.
            let key = |i: &Interval| (i.from, i.until.is_none(), i.until);
            s[k..].iter().map(key).cmp(t[k..].iter().map(key))
        }
        _ => a.cmp(b),
    }
}

/// [`cmp_real`] over two rows of equal length, lexicographically.
fn cmp_real_rows(a: &[Val], b: &[Val], work: &mut CmpWork) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = cmp_real(x, y, work);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Every string and interval set the query holds, each once with its dense
/// [`Id`] (hash-consing): a value is hashed once, when it enters the table;
/// from then on rows share it and compare it by id.
#[derive(Default)]
struct Interner {
    /// Each string, its id, and whether it is charged to the byte budget
    /// (plan constants aren't: the front-end bounds the plan's size; a
    /// string read from the world is charged once, on first read).
    strings: HashMap<Rc<str>, (Id, bool)>,
    /// Each interval set and its id.
    sets: HashMap<Rc<IntervalSet>, Id>,
}

impl Interner {
    /// The next id for a table of `len` entries. Ids run out only past
    /// `u32::MAX` distinct values, beyond any sane byte budget, but limits
    /// are caller-chosen, so running out is an error, never a panic.
    fn next_id(len: usize) -> Result<Id, QueryError> {
        u32::try_from(len).map(Id).map_err(|_| {
            QueryError::Overflow("more than 2^32 distinct values in one query".to_owned())
        })
    }

    /// The interned copy of `s`, and whether the caller must charge it to
    /// the byte budget now: `charge` asks for that, and it is true only the
    /// first time a charged string enters (a constant's text included).
    fn string(&mut self, s: &str, charge: bool) -> Result<(Val, bool), QueryError> {
        if let Some((shared, &(id, charged))) = self.strings.get_key_value(s) {
            let v = Val::Str(id, Rc::clone(shared));
            let newly = charge && !charged;
            if newly && let Some(e) = self.strings.get_mut(s) {
                e.1 = true;
            }
            return Ok((v, newly));
        }
        let id = Interner::next_id(self.strings.len())?;
        let shared: Rc<str> = Rc::from(s);
        self.strings.insert(Rc::clone(&shared), (id, charge));
        Ok((Val::Str(id, shared), charge))
    }

    /// `set` interned: an equal set already held is shared.
    fn set(&mut self, set: IntervalSet) -> Result<Val, QueryError> {
        if let Some((shared, id)) = self.sets.get_key_value(&set) {
            return Ok(Val::Intervals(*id, Rc::clone(shared)));
        }
        let id = Interner::next_id(self.sets.len())?;
        let shared = Rc::new(set);
        self.sets.insert(Rc::clone(&shared), id);
        Ok(Val::Intervals(id, shared))
    }

    /// A plan constant (or a resolved param) as a value, its string
    /// interned but not charged.
    fn constant(&mut self, v: &Value) -> Result<Val, QueryError> {
        Ok(match v {
            Value::Bool(b) => Val::Bool(*b),
            Value::I64(n) => Val::I64(*n),
            Value::F64(x) => Val::F64(*x),
            Value::Str(s) => self.string(s, false)?.0,
            Value::Ref(e) => Val::Ref(*e),
        })
    }
}

/// `(entity, value)` pairs read from an index.
type Pairs = Vec<(EntityId, Val)>;

/// Change points `(entity, value, tick, op)` read from an index, sorted by
/// entity (then as [`View::history`] orders one entity's).
type Points = Vec<(EntityId, Val, Tick, FactOp, Precision)>;

/// Charged per change point of a held history scan.
const POINT_BYTES: usize = size_of::<(EntityId, Val, Tick, FactOp, Precision)>();

/// The interval sets of an attribute's history scan, one per `(entity,
/// value)` that ever held, sorted by entity (then value).
type Sets = Vec<(EntityId, Val, Val)>;

/// Charged per `(entity, value, set)` entry of a held grouped scan, beyond
/// the set itself.
const SET_ENTRY_BYTES: usize = size_of::<(EntityId, Val, Val)>();

/// Charged per interval set created, beyond its intervals: the `Rc` header
/// and the set's own `Vec`.
const SET_OVERHEAD_BYTES: usize = 2 * size_of::<usize>() + size_of::<IntervalSet>();

/// What an interval set costs while held: [`SET_OVERHEAD_BYTES`] plus its
/// intervals.
fn set_bytes(s: &IntervalSet) -> usize {
    s.len()
        .saturating_mul(size_of::<Interval>())
        .saturating_add(SET_OVERHEAD_BYTES)
}

/// A row: one optional value per slot. Every row of one operator binds the
/// same slots (checked at compile time), so `None` means "not bound here".
type Row = Vec<Option<Val>>;

/// Bookkeeping charged per row beyond its slots: the B-tree set's share of
/// node space and pointers. A fixed estimate, so the budget is
/// deterministic.
const ROW_OVERHEAD_BYTES: usize = 16;

/// Charged per distinct string the query holds, beyond its text: the
/// `Rc` header and the intern table's slot.
const STRING_OVERHEAD_BYTES: usize = 32;

/// Charged per `(entity, value)` pair of a held scan.
const PAIR_BYTES: usize = size_of::<(EntityId, Val)>();

/// Conservative hash entry charge: key slots use the same fixed cost as
/// row sets, plus a row-reference slot and the bucket's vector/header.
fn index_entry_bytes(keys: usize) -> usize {
    row_bytes(keys)
        .saturating_add(size_of::<Vec<&Row>>())
        .saturating_add(size_of::<&Row>())
}

/// What one tuple of an `arity`-column relation costs while it is held.
fn tuple_bytes(arity: usize) -> usize {
    size_of::<Vec<Val>>() + arity * size_of::<Val>() + ROW_OVERHEAD_BYTES
}

/// What one row of `width` slots costs in an intermediate set.
fn row_bytes(width: usize) -> usize {
    size_of::<Row>() + width * size_of::<Option<Val>>() + ROW_OVERHEAD_BYTES
}

/// What one row of the final [`ResultSet`] costs: a `Vec` of [`Datum`]s
/// whose strings and interval sets are copies.
fn result_row_bytes(row: &[Val]) -> usize {
    let heap: usize = row
        .iter()
        .map(|v| match v {
            Val::Str(_, s) => s.len(),
            Val::Intervals(_, s) => s.len().saturating_mul(size_of::<Interval>()),
            _ => 0,
        })
        .sum();
    size_of::<Vec<Datum>>() + row.len() * size_of::<Datum>() + heap
}

/// A pattern position after compile.
#[derive(Clone, Debug)]
enum Pos {
    /// A constant (a resolved param included), already type-checked.
    Const(Val),
    /// A variable bound by the input: a constraint, type-checked per row.
    Bound(usize),
    /// A variable the read binds.
    Free(usize),
    /// `_`.
    Any,
}

impl Pos {
    fn is_bound(&self) -> bool {
        matches!(self, Pos::Const(_) | Pos::Bound(_))
    }
}

/// A compiled tick source.
#[derive(Clone, Copy, Debug)]
enum TickPos {
    Const(Tick),
    Var(usize),
}

/// A resolved named attribute.
#[derive(Clone, Debug)]
struct Attr {
    /// The name as written (an inverse name stays an inverse name).
    name: String,
    /// The value type of the name's values (`Ref` for an inverse name).
    value_type: ValueType,
    /// Whether `name` is an inverse name (`child_of`).
    inverse: bool,
    /// Whether values of the base attribute are in the `V` index.
    indexed: bool,
    /// The base attribute's name (`parent_of` for `child_of`; `name` itself
    /// for a forward name).
    base: String,
}

/// A compiled step. A plan compiles to a flat program of steps in post-order
/// (every operator's inputs before the operator), run as a stack machine:
/// leaves push a row set, unary steps replace the top one, `Join` merges the
/// top two. A `Union` is `UnionBegin`, then each input followed by
/// `UnionAdd`, then `UnionEnd`, so each input is checked and folded in
/// before the next one runs, exactly as a recursive walk would.
///
/// Flat on purpose: neither compile nor eval recurses, so the native stack
/// they use doesn't grow with plan depth (any front-end may build a long
/// left-deep chain).
#[derive(Debug)]
enum Step {
    Unit,
    NodeScan {
        slot: usize,
        bound: bool,
        node_type: String,
    },
    NodeSeek {
        slot: usize,
        bound: bool,
        entity: Pos,
    },
    Expand {
        entity: Pos,
        attr: Attr,
        value: Pos,
        tick: TickPos,
        access: Access,
    },
    Handle {
        entity: Pos,
        value: Pos,
    },
    /// The change points of `(entity, attr, value)` at every tick, binding
    /// the free positions among the four.
    History {
        entity: Pos,
        attr: Attr,
        value: Pos,
        tick: Pos,
        op: Pos,
        precision: Pos,
    },
    /// The interval set where `(entity, attr, value)` holds, over every
    /// tick, bound to `out` (a constraint if bound); free entity and value
    /// positions bind one row per pair that ever held.
    During {
        entity: Pos,
        attr: Attr,
        value: Pos,
        out: Pos,
    },
    /// `func(args)`: bound to `out` (a constraint if bound), or with no
    /// `out` a filter keeping the rows where it is true.
    Apply {
        func: Func,
        args: Vec<Pos>,
        out: Option<Pos>,
    },
    Filter {
        op: CmpOp,
        left: Pos,
        right: Pos,
    },
    Join {
        shared: Vec<usize>,
    },
    Project {
        keep: Vec<usize>,
    },
    UnionBegin,
    UnionAdd,
    UnionEnd,
    /// A `Not`'s body starts: its input's rows (on top) become the seed
    /// every `Unit` of the body yields (see [`Step::Seed`]).
    NotBegin,
    /// A `Unit` inside a `Not` body: a copy of the innermost open `Not`'s
    /// input rows, so the body runs from every input row at once.
    Seed,
    /// A `Not` ends: keep the input rows (its seed) whose values in `key`
    /// (every slot the input binds) match no row of the body (on top).
    NotEnd {
        key: Vec<usize>,
    },
    /// Anti-join: the left rows (below the top) matching no right row (on
    /// top) on the `shared` slots.
    Difference {
        shared: Vec<usize>,
    },
    /// Semi-join: the left rows (below the top) matching some right row (on
    /// top) on the `shared` slots, each once.
    Intersect {
        shared: Vec<usize>,
    },
    /// A `Fixpoint` starts: its relations (by id) start empty, and its
    /// first round (every rule once, its own relations read as empty)
    /// begins.
    FixBegin {
        rels: Vec<usize>,
    },
    /// One rule of relation `rel` is done: fold its rows (the top row set)
    /// into the relation's tuples new this round, as tuples of the slots
    /// `cols`. In a later round, go on to the next variant to run.
    RuleEnd {
        rel: usize,
        cols: Vec<usize>,
    },
    /// A `Fixpoint`'s rules are done for this round: its new tuples join
    /// its relations and become the delta. If some delta that a variant
    /// reads is non-empty, run another round (jump to the first such
    /// variant); else go on to its body.
    FixLoop {
        rels: Vec<usize>,
        variants: Vec<Variant>,
    },
    /// A `Fixpoint` ends: its relations are dropped; its body's rows (on
    /// top) are its rows.
    FixEnd {
        rels: Vec<usize>,
    },
    /// Group the input rows by the `keys` slots and bind each of `aggs`
    /// per group: one row per group holding only `keys` and the outputs.
    Aggregate {
        keys: Vec<usize>,
        aggs: Vec<AggStep>,
    },
    /// Join the input rows with relation `rel`'s tuples, matched to `args`
    /// by position. `occ` numbers a recursive read (a rule reading a
    /// relation of its own `Fixpoint`), which reads nothing in the first
    /// round and, in later rounds, the delta when its variant runs and the
    /// whole relation otherwise.
    Rel {
        rel: usize,
        args: Vec<Pos>,
        occ: Option<usize>,
    },
}

/// One compiled aggregate: `func` over the values in slot `var` (named
/// `name`, for messages), bound to slot `out`.
#[derive(Debug)]
struct AggStep {
    func: AggFn,
    var: usize,
    name: String,
    out: usize,
}

/// One semi-naive variant of a recursive rule: the rule's steps (from
/// `start` to its `RuleEnd`) run with recursive read `occ` (of relation
/// `rel`) reading only the previous round's delta.
#[derive(Clone, Copy, Debug)]
struct Variant {
    start: usize,
    occ: usize,
    rel: usize,
}

/// One item of a plan's walk, in execution order.
enum Visit<'p> {
    /// `op`'s inputs are done (for a `Union`: its end; for a `Fixpoint`:
    /// its body is done).
    Op(&'p Op),
    /// A `Union` starts.
    UnionBegin,
    /// One `Union` input is done.
    UnionAdd,
    /// A `Not`'s input is done; its body starts.
    NotBody,
    /// A `Fixpoint` starts (before its rules).
    FixBegin(&'p Op),
    /// Rule number `rule` (0-based) of relation number `rel` of the
    /// innermost open `Fixpoint` is done.
    RuleEnd { rel: usize, rule: usize },
    /// The innermost open `Fixpoint`'s rules are done; its body starts.
    FixBody,
}

/// `root`'s operators in post-order, children left to right (a
/// `Fixpoint`'s rules, in order, before its body), walked with an explicit
/// stack.
fn post_order(root: &Op) -> Vec<Visit<'_>> {
    enum Todo<'p> {
        Enter(&'p Op),
        Leave(Visit<'p>),
    }
    let mut out = Vec::new();
    let mut todo = vec![Todo::Enter(root)];
    while let Some(t) = todo.pop() {
        let op = match t {
            Todo::Leave(v) => {
                out.push(v);
                continue;
            }
            Todo::Enter(op) => op,
        };
        // Pushed in reverse: the stack pops them in execution order.
        match op {
            Op::Unit => out.push(Visit::Op(op)),
            Op::NodeScan { input, .. }
            | Op::NodeSeek { input, .. }
            | Op::Expand { input, .. }
            | Op::Filter { input, .. }
            | Op::Project { input, .. }
            | Op::History { input, .. }
            | Op::During { input, .. }
            | Op::Apply { input, .. }
            | Op::Aggregate { input, .. }
            | Op::Rel { input, .. } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(input));
            }
            // The input, then the body seeded with the input's rows.
            Op::Not { input, body } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(body));
                todo.push(Todo::Leave(Visit::NotBody));
                todo.push(Todo::Enter(input));
            }
            Op::Join { left, right }
            | Op::Difference { left, right }
            | Op::Intersect { left, right } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(right));
                todo.push(Todo::Enter(left));
            }
            Op::Union { inputs } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                for i in inputs.iter().rev() {
                    todo.push(Todo::Leave(Visit::UnionAdd));
                    todo.push(Todo::Enter(i));
                }
                todo.push(Todo::Leave(Visit::UnionBegin));
            }
            Op::Fixpoint { rels, body } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(body));
                todo.push(Todo::Leave(Visit::FixBody));
                for (ri, rel) in rels.iter().enumerate().rev() {
                    for (k, rule) in rel.rules.iter().enumerate().rev() {
                        todo.push(Todo::Leave(Visit::RuleEnd { rel: ri, rule: k }));
                        todo.push(Todo::Enter(rule));
                    }
                }
                todo.push(Todo::Leave(Visit::FixBegin(op)));
            }
        }
    }
    out
}

/// The number of operators on `root`'s longest root-to-leaf path, computed
/// without recursion. Every child counts: a `Not`'s body, a `Difference`'s
/// or `Intersect`'s right side, and a `Fixpoint`'s rules (one level below
/// the `Fixpoint`: a relation's definition is not an operator) and body.
fn plan_depth(root: &Op) -> usize {
    let mut deepest = 0;
    let mut todo = vec![(root, 1usize)];
    let mut kids = Vec::new();
    while let Some((op, d)) = todo.pop() {
        deepest = deepest.max(d);
        kids.clear();
        op.children(&mut kids);
        todo.extend(kids.iter().map(|&k| (k, d + 1)));
    }
    deepest
}

/// A compiled plan: the flat program, the slots its root's rows bind, and
/// the columns (slots) of every relation, indexed by relation id.
struct Program {
    steps: Vec<Step>,
    bound: BTreeSet<usize>,
    rels: Vec<Vec<usize>>,
}

/// Compiles `root` into a flat program. Errors come in the order a
/// recursive walk (inputs first, left to right) would find them.
fn compile(
    root: &Op,
    world: &World,
    canon: TxId,
    params: &Params,
    default_tick: Tick,
    slots: &mut Slots,
    interner: &mut Interner,
) -> Result<Program, QueryError> {
    let walk = post_order(root);
    let mut c = Compiler {
        world,
        canon,
        params,
        default_tick,
        slots,
        interner,
        stack: Vec::new(),
        unions: Vec::new(),
        fixes: Vec::new(),
        seeds: Vec::new(),
        rels: Vec::new(),
        pc: 0,
        occs: 0,
        kinds: HashMap::new(),
    };
    let mut steps = Vec::with_capacity(walk.len());
    for v in walk {
        c.pc = steps.len();
        if let Some(step) = c.visit(v)? {
            steps.push(step);
        }
    }
    let bound = pop(&mut c.stack);
    debug_assert!(
        c.stack.is_empty() && c.unions.is_empty() && c.fixes.is_empty() && c.seeds.is_empty()
    );
    Ok(Program {
        steps,
        bound,
        rels: c.rels.into_iter().map(|r| r.cols).collect(),
    })
}

/// Pops one operator's bound slots (or row set).
fn pop<T>(stack: &mut Vec<T>) -> T {
    stack
        .pop()
        .expect("invariant: post-order puts every input before its operator")
}

/// A relation known to the compiler.
struct RelMeta {
    name: String,
    /// The slots of its columns, in order.
    cols: Vec<usize>,
    vars: Vec<Var>,
}

/// An open `Fixpoint` during compile.
struct FixScope {
    /// Its relations' ids, in definition order.
    rels: Vec<usize>,
    /// Whether its rules (rather than its body) are being compiled: a `Rel`
    /// of one of its relations here is recursion.
    in_rules: bool,
    /// The program counter of the first step of the rule being compiled.
    rule_start: usize,
    /// The recursive reads (relation, occurrence) of that rule so far.
    pending: Vec<(usize, usize)>,
    /// The variants of the rules compiled so far, in program order.
    variants: Vec<Variant>,
}

/// Compile state: the slots bound by each pending operator's output, the
/// open `Union`s and `Fixpoint`s, and every relation seen so far.
struct Compiler<'c> {
    world: &'c World,
    canon: TxId,
    params: &'c Params,
    default_tick: Tick,
    slots: &'c mut Slots,
    /// Interns the plan's string constants (uncharged).
    interner: &'c mut Interner,
    /// The slots bound by each pending operator's output.
    stack: Vec<BTreeSet<usize>>,
    /// Per open `Union`: the slots its first input binds.
    unions: Vec<Option<BTreeSet<usize>>>,
    /// The open `Fixpoint`s, innermost last.
    fixes: Vec<FixScope>,
    /// Per open `Not` body, innermost last: the slots its input binds,
    /// which every `Unit` inside it binds too.
    seeds: Vec<BTreeSet<usize>>,
    /// Every relation, by id.
    rels: Vec<RelMeta>,
    /// The program counter of the step being compiled.
    pc: usize,
    /// Recursive reads numbered so far.
    occs: usize,
    /// The kind of value each slot is bound to wherever the plan binds it,
    /// when that is known statically (`None`: bound to different kinds, or
    /// by a `Rel`, whose columns' kinds aren't tracked). Only used to refuse
    /// an `Apply` whose argument can never be of the kind it needs.
    kinds: HashMap<usize, Option<ValKind>>,
}

/// Records that the plan binds `pos` (if it is free) to values of `kind`.
fn note_kind(kinds: &mut HashMap<usize, Option<ValKind>>, pos: &Pos, kind: Option<ValKind>) {
    if let Pos::Free(s) = pos {
        let k = kinds.entry(*s).or_insert(kind);
        if *k != kind {
            *k = None;
        }
    }
}

impl Compiler<'_> {
    /// Compiles one walk item into its step, if it has one.
    fn visit(&mut self, v: Visit<'_>) -> Result<Option<Step>, QueryError> {
        Ok(Some(match v {
            Visit::UnionBegin => {
                self.unions.push(None);
                Step::UnionBegin
            }
            Visit::UnionAdd => {
                let b = pop(&mut self.stack);
                let first = self
                    .unions
                    .last_mut()
                    .expect("invariant: UnionAdd follows its UnionBegin");
                match first {
                    Some(prev) if *prev != b => {
                        return Err(QueryError::Plan(
                            "Union inputs must bind the same variables".into(),
                        ));
                    }
                    Some(_) => {}
                    None => *first = Some(b),
                }
                Step::UnionAdd
            }
            Visit::NotBody => {
                let input = self
                    .stack
                    .last()
                    .expect("invariant: a Not's input is compiled before its body")
                    .clone();
                self.seeds.push(input);
                Step::NotBegin
            }
            Visit::FixBegin(op) => self.fix_begin(op)?,
            Visit::RuleEnd { rel, rule } => {
                let bound = pop(&mut self.stack);
                let fix = self
                    .fixes
                    .last()
                    .expect("invariant: RuleEnd is inside its Fixpoint");
                let id = fix.rels[rel];
                let meta = &self.rels[id];
                let pc = self.pc;
                for (s, v) in meta.cols.iter().zip(&meta.vars) {
                    if !bound.contains(s) {
                        return Err(QueryError::Plan(format!(
                            "Fixpoint relation `{}`: rule {} doesn't bind ?{}",
                            meta.name,
                            rule + 1,
                            v.0
                        )));
                    }
                }
                let cols = meta.cols.clone();
                let fix = self
                    .fixes
                    .last_mut()
                    .expect("invariant: RuleEnd is inside its Fixpoint");
                let start = fix.rule_start;
                fix.variants
                    .extend(
                        fix.pending
                            .drain(..)
                            .map(|(rel, occ)| Variant { start, occ, rel }),
                    );
                fix.rule_start = pc + 1;
                Step::RuleEnd { rel: id, cols }
            }
            Visit::FixBody => {
                let fix = self
                    .fixes
                    .last_mut()
                    .expect("invariant: FixBody is inside its Fixpoint");
                fix.in_rules = false;
                Step::FixLoop {
                    rels: fix.rels.clone(),
                    variants: std::mem::take(&mut fix.variants),
                }
            }
            Visit::Op(op) => {
                let (step, bound) = self.op(op)?;
                self.stack.push(bound);
                step
            }
        }))
    }

    /// A `Fixpoint` starts: checks its relations and gives each an id.
    fn fix_begin(&mut self, op: &Op) -> Result<Step, QueryError> {
        let Op::Fixpoint { rels, .. } = op else {
            unreachable!("invariant: FixBegin is only visited for a Fixpoint")
        };
        if rels.is_empty() {
            return Err(QueryError::Plan("Fixpoint defines no relations".into()));
        }
        // Its rules' `Unit`s would be seeded with the Not's rows, which has
        // no meaning for a relation; front-ends put Fixpoints outermost.
        if !self.seeds.is_empty() {
            return Err(QueryError::Plan(
                "a Fixpoint can't be inside a Not body".into(),
            ));
        }
        let mut ids = Vec::with_capacity(rels.len());
        for (i, r) in rels.iter().enumerate() {
            if rels[..i].iter().any(|o| o.name == r.name) {
                return Err(QueryError::Plan(format!(
                    "Fixpoint defines relation `{}` twice",
                    r.name
                )));
            }
            let mut cols = Vec::with_capacity(r.vars.len());
            for v in &r.vars {
                let s = self.slots.slot(v);
                if cols.contains(&s) {
                    return Err(QueryError::Plan(format!(
                        "Fixpoint relation `{}` names column ?{} twice",
                        r.name, v.0
                    )));
                }
                cols.push(s);
            }
            ids.push(self.rels.len());
            self.rels.push(RelMeta {
                name: r.name.clone(),
                cols,
                vars: r.vars.clone(),
            });
        }
        self.fixes.push(FixScope {
            rels: ids.clone(),
            in_rules: true,
            rule_start: self.pc + 1,
            pending: Vec::new(),
            variants: Vec::new(),
        });
        Ok(Step::FixBegin { rels: ids })
    }

    /// The relation a `Rel` named `name` reads: the nearest enclosing
    /// `Fixpoint`'s relation of that name, and its occurrence number if
    /// the read is recursive (inside that `Fixpoint`'s rules).
    ///
    /// A recursive read must be in the rules of the innermost open
    /// `Fixpoint`: semi-naive evaluation substitutes the delta for one read
    /// at a time, which is only sound where the read is joined into the
    /// rule's rows, not under another `Fixpoint` (whose result isn't linear
    /// in what it reads).
    fn resolve_rel(&mut self, name: &str) -> Result<(usize, Option<usize>), QueryError> {
        let innermost = self.fixes.len().wrapping_sub(1);
        for (fi, fix) in self.fixes.iter().enumerate().rev() {
            if let Some(&id) = fix.rels.iter().find(|&&id| self.rels[id].name == name) {
                if !fix.in_rules {
                    return Ok((id, None));
                }
                if fi != innermost {
                    return Err(QueryError::Plan(format!(
                        "recursive relation; Rel `{name}` is read inside a nested Fixpoint in \
                         the rules of the Fixpoint defining it"
                    )));
                }
                // Negation must read only lower strata: a relation still
                // growing can't be negated (no Fixpoint is inside a Not
                // body, so any open Not body is in this rule).
                if !self.seeds.is_empty() {
                    return Err(QueryError::Plan(format!(
                        "recursive relation; Rel `{name}` is read inside a Not body in the \
                         rules of the Fixpoint defining it (negation must read lower strata)"
                    )));
                }
                let occ = self.occs;
                self.occs += 1;
                self.fixes[fi].pending.push((id, occ));
                return Ok((id, Some(occ)));
            }
        }
        Err(QueryError::Plan(format!(
            "Rel `{name}`: no enclosing Fixpoint defines it"
        )))
    }

    /// Compiles one operator whose inputs are compiled (their bound slots on
    /// top of the stack, which this pops), returning its step and its bound
    /// slots.
    fn op(&mut self, op: &Op) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let (world, params, default_tick) = (self.world, self.params, self.default_tick);
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let stack = &mut self.stack;
        Ok(match op {
            Op::Unit => match self.seeds.last() {
                Some(seed) => (Step::Seed, seed.clone()),
                None => (Step::Unit, BTreeSet::new()),
            },
            Op::Not { .. } => {
                let body = pop(stack);
                let input = pop(stack);
                self.seeds.pop();
                // The body's rows are matched back to the input rows they
                // came from by every input slot, so it must keep them all.
                if let Some(s) = input.difference(&body).next() {
                    let name = slots
                        .0
                        .iter()
                        .find(|&(_, &v)| v == *s)
                        .map_or("?", |(v, _)| v.0.as_str());
                    return Err(QueryError::Plan(format!(
                        "a Not body must keep every variable of its input, but it drops ?{name}"
                    )));
                }
                let key = input.iter().copied().collect();
                (Step::NotEnd { key }, input)
            }
            Op::Difference { .. } | Op::Intersect { .. } => {
                let rb = pop(stack);
                let lb = pop(stack);
                let shared: Vec<usize> = lb.intersection(&rb).copied().collect();
                let step = if matches!(op, Op::Difference { .. }) {
                    Step::Difference { shared }
                } else {
                    Step::Intersect { shared }
                };
                (step, lb)
            }
            Op::NodeScan { var, node_type, .. } => {
                let mut bound = pop(stack);
                let slot = slots.slot(var);
                let was = !bound.insert(slot);
                note_kind(
                    &mut self.kinds,
                    &Pos::Free(slot),
                    Some(ValKind::Value(ValueType::Ref)),
                );
                let step = Step::NodeScan {
                    slot,
                    bound: was,
                    node_type: node_type.clone(),
                };
                (step, bound)
            }
            Op::NodeSeek { var, entity, .. } => {
                let mut bound = pop(stack);
                let entity = entity_pos(entity, params, slots, &bound, "NodeSeek")?;
                if !entity.is_bound() {
                    return Err(QueryError::Plan(
                        "NodeSeek needs a constant, a param or a bound variable".into(),
                    ));
                }
                let slot = slots.slot(var);
                let was = !bound.insert(slot);
                note_kind(
                    &mut self.kinds,
                    &Pos::Free(slot),
                    Some(ValKind::Value(ValueType::Ref)),
                );
                let step = Step::NodeSeek {
                    slot,
                    bound: was,
                    entity,
                };
                (step, bound)
            }
            Op::Expand {
                entity,
                attr,
                value,
                tick,
                access,
                ..
            } => {
                let mut bound = pop(stack);
                let (attr, value_type) = match attr {
                    AttrRef::Handle => {
                        if *access != Access::Handle {
                            return Err(QueryError::Plan(format!(
                                ":kala_handle can only be read via handle, not {access:?}"
                            )));
                        }
                        (None, ValueType::Str)
                    }
                    AttrRef::Named(name) => {
                        if *access == Access::Handle {
                            return Err(QueryError::Plan(format!(
                                "handle access is only for :kala_handle, not :{name}"
                            )));
                        }
                        let a = resolve_attr(world, self.canon, name)?;
                        let t = a.value_type;
                        (Some(a), t)
                    }
                };
                let e = entity_pos(entity, params, slots, &bound, "entity")?;
                let v = value_pos(value, value_type, params, interner, slots, &bound)?;
                note_kind(&mut self.kinds, &e, Some(ValKind::Value(ValueType::Ref)));
                note_kind(&mut self.kinds, &v, Some(ValKind::Value(value_type)));
                match attr {
                    None => {
                        if !e.is_bound() && !v.is_bound() {
                            return Err(QueryError::Plan(
                                "handle access needs the entity or the handle bound".into(),
                            ));
                        }
                        bind_free(&mut bound, &e, &v);
                        (
                            Step::Handle {
                                entity: e,
                                value: v,
                            },
                            bound,
                        )
                    }
                    Some(attr) => {
                        check_access(*access, &attr, &e, &v)?;
                        let tick = match tick {
                            TickSource::Default => TickPos::Const(default_tick),
                            TickSource::Const(t) => TickPos::Const(*t),
                            TickSource::Param(name) => TickPos::Const(tick_param(params, name)?),
                            TickSource::Var(var) => match slots.get(var) {
                                Some(s) if bound.contains(&s) => TickPos::Var(s),
                                _ => return Err(QueryError::TickNotBound(var.0.clone())),
                            },
                        };
                        bind_free(&mut bound, &e, &v);
                        let step = Step::Expand {
                            entity: e,
                            attr,
                            value: v,
                            tick,
                            access: *access,
                        };
                        (step, bound)
                    }
                }
            }
            Op::Filter { pred, .. } => {
                let bound = pop(stack);
                let left = operand_pos(&pred.left, params, interner, slots, &bound)?;
                let right = operand_pos(&pred.right, params, interner, slots, &bound)?;
                if let (Pos::Const(l), Pos::Const(r)) = (&left, &right) {
                    // Constant comparisons are checked up front, like constants
                    // elsewhere (the plan bounds their size: no charge).
                    compare(pred.op, l, r, &mut CmpWork::default())?;
                }
                let step = Step::Filter {
                    op: pred.op,
                    left,
                    right,
                };
                (step, bound)
            }
            Op::Join { .. } => {
                let rb = pop(stack);
                let lb = pop(stack);
                let shared: Vec<usize> = lb.intersection(&rb).copied().collect();
                let bound = lb.union(&rb).copied().collect();
                (Step::Join { shared }, bound)
            }
            Op::Project { vars, .. } => {
                let bound = pop(stack);
                let mut keep = BTreeSet::new();
                for v in vars {
                    match slots.get(v) {
                        Some(s) if bound.contains(&s) => {
                            keep.insert(s);
                        }
                        _ => {
                            return Err(QueryError::Plan(format!(
                                "Project of ?{}, which its input doesn't bind",
                                v.0
                            )));
                        }
                    }
                }
                let step = Step::Project {
                    keep: keep.iter().copied().collect(),
                };
                (step, keep)
            }
            // The end of a union: its inputs were checked by `UnionAdd`.
            Op::Union { .. } => {
                let first = self
                    .unions
                    .pop()
                    .expect("invariant: a Union's end follows its UnionBegin");
                (Step::UnionEnd, first.unwrap_or_default())
            }
            // The end of a `Fixpoint`: its rows are its body's.
            Op::Fixpoint { .. } => {
                let bound = pop(stack);
                let fix = self
                    .fixes
                    .pop()
                    .expect("invariant: a Fixpoint's end follows its FixBegin");
                (Step::FixEnd { rels: fix.rels }, bound)
            }
            Op::Rel { name, args, .. } => {
                let mut bound = pop(stack);
                let (id, occ) = self.resolve_rel(name)?;
                let meta = &self.rels[id];
                if args.len() != meta.cols.len() {
                    return Err(QueryError::Plan(format!(
                        "Rel `{name}` has {} arguments, but its relation has {} columns",
                        args.len(),
                        meta.cols.len()
                    )));
                }
                let slots = &mut *self.slots;
                let interner = &mut *self.interner;
                let mut pos = Vec::with_capacity(args.len());
                for a in args {
                    pos.push(match a {
                        Term::Var(v) => var_pos(v, slots, &bound),
                        Term::Any => Pos::Any,
                        Term::Const(c) => Pos::Const(interner.constant(c)?),
                        Term::Op(o) => Pos::Const(Val::Op(*o)),
                        Term::Precision(p) => Pos::Const(Val::Precision(*p)),
                        Term::Param(p) => Pos::Const(interner.constant(param(params, p)?)?),
                    });
                }
                for p in &pos {
                    if let Pos::Free(s) = p {
                        bound.insert(*s);
                    }
                    note_kind(&mut self.kinds, p, None);
                }
                (
                    Step::Rel {
                        rel: id,
                        args: pos,
                        occ,
                    },
                    bound,
                )
            }
            Op::History {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
                ..
            } => {
                let mut bound = pop(stack);
                let attr = resolve_attr(world, self.canon, attr)?;
                let e = entity_pos(entity, params, slots, &bound, "entity")?;
                let v = value_pos(value, attr.value_type, params, interner, slots, &bound)?;
                let t = tick_term_pos(tick, params, slots, &bound)?;
                let o = match op {
                    OpTerm::Any => Pos::Any,
                    OpTerm::Const(o) => Pos::Const(Val::Op(*o)),
                    OpTerm::Var(var) => var_pos(var, slots, &bound),
                };
                let prec = match precision {
                    Term::Any => Pos::Any,
                    Term::Var(v) => var_pos(v, slots, &bound),
                    Term::Precision(p) => Pos::Const(Val::Precision(*p)),
                    _ => {
                        return Err(QueryError::TypeMismatch(
                            "History precision needs a precision, variable or wildcard".into(),
                        ));
                    }
                };
                for (p, k) in [
                    (&prec, ValKind::Precision),
                    (&e, ValKind::Value(ValueType::Ref)),
                    (&v, ValKind::Value(attr.value_type)),
                    (&t, ValKind::Value(ValueType::I64)),
                    (&o, ValKind::Op),
                ] {
                    if let Pos::Free(s) = p {
                        bound.insert(*s);
                    }
                    note_kind(&mut self.kinds, p, Some(k));
                }
                let step = Step::History {
                    entity: e,
                    attr,
                    value: v,
                    tick: t,
                    op: o,
                    precision: prec,
                };
                (step, bound)
            }
            Op::During {
                entity,
                attr,
                value,
                out,
                ..
            } => {
                let mut bound = pop(stack);
                let attr = resolve_attr(world, self.canon, attr)?;
                let e = entity_pos(entity, params, slots, &bound, "entity")?;
                let v = value_pos(value, attr.value_type, params, interner, slots, &bound)?;
                let o = var_pos(out, slots, &bound);
                for (p, k) in [
                    (&e, ValKind::Value(ValueType::Ref)),
                    (&v, ValKind::Value(attr.value_type)),
                    (&o, ValKind::Intervals),
                ] {
                    if let Pos::Free(s) = p {
                        bound.insert(*s);
                    }
                    note_kind(&mut self.kinds, p, Some(k));
                }
                let step = Step::During {
                    entity: e,
                    attr,
                    value: v,
                    out: o,
                };
                (step, bound)
            }
            Op::Apply {
                func, args, out, ..
            } => {
                let mut bound = pop(stack);
                let (want, has_out) = func_signature(*func);
                if args.len() != want.len() || out.is_some() != has_out {
                    return Err(QueryError::Plan(format!(
                        "Apply {} takes {} argument{} and {}, got {} argument{} and {}",
                        func.name(),
                        want.len(),
                        if want.len() == 1 { "" } else { "s" },
                        if has_out { "an output" } else { "no output" },
                        args.len(),
                        if args.len() == 1 { "" } else { "s" },
                        if out.is_some() {
                            "an output"
                        } else {
                            "no output"
                        },
                    )));
                }
                let mut pos = Vec::with_capacity(args.len());
                for (a, &kind) in args.iter().zip(want) {
                    let p = apply_arg_pos(*func, a, params, interner, slots, &bound)?;
                    let got = match &p {
                        Pos::Const(c) => Some(c.kind()),
                        Pos::Bound(s) => self.kinds.get(s).copied().flatten(),
                        Pos::Free(_) | Pos::Any => None,
                    };
                    if let Some(got) = got
                        && got != kind
                    {
                        if let Term::Param(name) = a {
                            return Err(QueryError::ParamType {
                                name: name.clone(),
                                expected: kind.name().into(),
                            });
                        }
                        let msg = format!(
                            "Apply {} needs {} arguments, but {} is {}",
                            func.name(),
                            kind.name(),
                            term_name(a),
                            got.name()
                        );
                        return Err(if matches!(p, Pos::Const(_)) {
                            QueryError::TypeMismatch(msg)
                        } else {
                            QueryError::Plan(msg)
                        });
                    }
                    pos.push(p);
                }
                let out = out.as_ref().map(|v| var_pos(v, slots, &bound));
                if let Some(o) = &out {
                    if let Pos::Free(s) = o {
                        bound.insert(*s);
                    }
                    note_kind(&mut self.kinds, o, Some(ValKind::Intervals));
                }
                let step = Step::Apply {
                    func: *func,
                    args: pos,
                    out,
                };
                (step, bound)
            }
            Op::Aggregate { group, aggs, .. } => {
                let input = pop(stack);
                // Semi-naive evaluation reruns a rule on deltas and unions
                // what it derives, which is only sound for monotone rules:
                // an aggregate over part of a relation isn't.
                if self.fixes.iter().any(|f| f.in_rules) {
                    return Err(QueryError::Plan(
                        "an Aggregate can't be inside a Fixpoint's rules".into(),
                    ));
                }
                let mut keys = Vec::with_capacity(group.len());
                for v in group {
                    let s = input_slot(slots, &input, v, "group variable")?;
                    if !keys.contains(&s) {
                        keys.push(s);
                    }
                }
                let mut bound: BTreeSet<usize> = keys.iter().copied().collect();
                let mut steps = Vec::with_capacity(aggs.len());
                for a in aggs {
                    let var = input_slot(slots, &input, &a.var, "aggregated variable")?;
                    let kind = self.kinds.get(&var).copied().flatten();
                    if let Some(k) = kind
                        && !agg_takes(a.func, k)
                    {
                        return Err(QueryError::TypeMismatch(format!(
                            "{} of ?{} needs {}, but ?{} is {}",
                            a.func.name(),
                            a.var.0,
                            agg_wants(a.func),
                            a.var.0,
                            k.name()
                        )));
                    }
                    // An output is a new variable: a variable of the input
                    // (a group variable included) or an earlier output would
                    // leave it unclear which value the row holds.
                    let out = slots.slot(&a.out);
                    if input.contains(&out) || !bound.insert(out) {
                        return Err(QueryError::Plan(format!(
                            "Aggregate output ?{} must be a new variable, but its input or \
                             another aggregate binds it",
                            a.out.0
                        )));
                    }
                    let out_kind = match a.func {
                        AggFn::Count | AggFn::CountDistinct => Some(ValKind::Value(ValueType::I64)),
                        AggFn::Min | AggFn::Max | AggFn::Sum => kind,
                    };
                    note_kind(&mut self.kinds, &Pos::Free(out), out_kind);
                    steps.push(AggStep {
                        func: a.func,
                        var,
                        name: a.var.0.clone(),
                        out,
                    });
                }
                (Step::Aggregate { keys, aggs: steps }, bound)
            }
        })
    }
}

/// The slot of `v`, which `input` must bind (a plan error otherwise).
fn input_slot(
    slots: &Slots,
    input: &BTreeSet<usize>,
    v: &Var,
    what: &str,
) -> Result<usize, QueryError> {
    match slots.get(v) {
        Some(s) if input.contains(&s) => Ok(s),
        _ => Err(QueryError::Plan(format!(
            "Aggregate {what} ?{} is not bound by its input",
            v.0
        ))),
    }
}

/// Whether `func` takes values of `kind`: counts take anything, `min` and
/// `max` ordered scalars (i64, f64, string), `sum` numbers.
fn agg_takes(func: AggFn, kind: ValKind) -> bool {
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
fn agg_wants(func: AggFn) -> &'static str {
    match func {
        AggFn::Count | AggFn::CountDistinct => "any value",
        AggFn::Min | AggFn::Max => "i64, f64 or string values",
        AggFn::Sum => "i64 or f64 values",
    }
}

/// Adds the variables an `Expand` binds to `bound`.
fn bind_free(bound: &mut BTreeSet<usize>, e: &Pos, v: &Pos) {
    for p in [e, v] {
        if let Pos::Free(s) = p {
            bound.insert(*s);
        }
    }
}

fn resolve_attr(world: &World, canon: TxId, name: &str) -> Result<Attr, QueryError> {
    let (def, inverse) = world.at_canon(canon).resolve_attribute(name)?;
    Ok(Attr {
        name: name.into(),
        value_type: def.value_type,
        inverse,
        indexed: def.indexed,
        base: def.name.clone(),
    })
}

/// Whether `access` can serve a read with these positions bound.
fn check_access(access: Access, attr: &Attr, e: &Pos, v: &Pos) -> Result<(), QueryError> {
    let name = &attr.name;
    match access {
        Access::E if !e.is_bound() => Err(QueryError::Plan(format!(
            "E access on :{name} needs the entity bound"
        ))),
        Access::R if attr.value_type != ValueType::Ref => Err(QueryError::Plan(format!(
            "R access needs a ref attribute; :{name} isn't one"
        ))),
        Access::R if !v.is_bound() => Err(QueryError::Plan(format!(
            "R access on :{name} needs the value bound"
        ))),
        Access::V if !v.is_bound() => Err(QueryError::Plan(format!(
            "V access on :{name} needs the value bound"
        ))),
        Access::V if attr.value_type != ValueType::Ref && !attr.indexed => Err(QueryError::Plan(
            format!("V access needs an indexed attribute; :{name} is unindexed"),
        )),
        Access::Handle => Err(QueryError::Plan(format!(
            "handle access is only for :kala_handle, not :{name}"
        ))),
        _ => Ok(()),
    }
}

/// A variable term: bound by the input (a constraint) or bound here.
fn var_pos(v: &Var, slots: &mut Slots, bound: &BTreeSet<usize>) -> Pos {
    let s = slots.slot(v);
    if bound.contains(&s) {
        Pos::Bound(s)
    } else {
        Pos::Free(s)
    }
}

fn param<'p>(params: &'p Params, name: &str) -> Result<&'p Value, QueryError> {
    params
        .get(name)
        .ok_or_else(|| QueryError::MissingParam(name.to_owned()))
}

/// An entity position: constants and params must be refs.
fn entity_pos(
    t: &Term,
    params: &Params,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
    what: &str,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(Value::Ref(e)) => Pos::Const(Val::Ref(*e)),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "{what} position needs an entity, got {}",
                type_name(c.value_type())
            )));
        }
        Term::Op(_) | Term::Precision(_) => {
            return Err(QueryError::TypeMismatch(format!(
                "{what} position needs an entity, got an op"
            )));
        }
        Term::Param(name) => match param(params, name)? {
            Value::Ref(e) => Pos::Const(Val::Ref(*e)),
            _ => {
                return Err(QueryError::ParamType {
                    name: name.clone(),
                    expected: "ref".into(),
                });
            }
        },
    })
}

/// A value position of an attribute of type `ty`.
fn value_pos(
    t: &Term,
    ty: ValueType,
    params: &Params,
    interner: &mut Interner,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(c) if c.value_type() == ty => Pos::Const(interner.constant(c)?),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "value position needs {}, got {}",
                type_name(ty),
                type_name(c.value_type())
            )));
        }
        Term::Op(_) | Term::Precision(_) => {
            return Err(QueryError::TypeMismatch(format!(
                "value position needs {}, got an op",
                type_name(ty)
            )));
        }
        Term::Param(name) => match param(params, name)? {
            c if c.value_type() == ty => Pos::Const(interner.constant(c)?),
            _ => {
                return Err(QueryError::ParamType {
                    name: name.clone(),
                    expected: type_name(ty).into(),
                });
            }
        },
    })
}

/// A `History` tick position: an `i64` constant or param, a variable or `_`.
fn tick_term_pos(
    t: &Term,
    params: &Params,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(Value::I64(n)) => Pos::Const(Val::I64(*n)),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "History tick position needs i64, got {}",
                type_name(c.value_type())
            )));
        }
        Term::Op(_) | Term::Precision(_) => {
            return Err(QueryError::TypeMismatch(
                "History tick position needs i64, got an op".into(),
            ));
        }
        Term::Param(name) => Pos::Const(Val::I64(tick_param(params, name)?.0)),
    })
}

/// A filter operand: a constant, a param or a bound variable.
fn operand_pos(
    t: &Term,
    params: &Params,
    interner: &mut Interner,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    match t {
        Term::Const(c) => Ok(Pos::Const(interner.constant(c)?)),
        Term::Op(o) => Ok(Pos::Const(Val::Op(*o))),
        Term::Precision(p) => Ok(Pos::Const(Val::Precision(*p))),
        Term::Param(name) => Ok(Pos::Const(interner.constant(param(params, name)?)?)),
        Term::Var(v) => match slots.get(v) {
            Some(s) if bound.contains(&s) => Ok(Pos::Bound(s)),
            _ => Err(QueryError::Plan(format!(
                "Filter on ?{}, which its input doesn't bind",
                v.0
            ))),
        },
        Term::Any => Err(QueryError::Plan("Filter operand can't be _".into())),
    }
}

/// The kinds of `func`'s arguments, in order, and whether it binds an
/// output (else it is a filter).
fn func_signature(func: Func) -> (&'static [ValKind], bool) {
    const SETS: &[ValKind] = &[ValKind::Intervals, ValKind::Intervals];
    match func {
        Func::Intersect | Func::Union | Func::Difference => (SETS, true),
        Func::NotEmpty => (&[ValKind::Intervals], false),
        Func::Contains => (&[ValKind::Intervals, ValKind::Value(ValueType::I64)], false),
    }
}

/// An `Apply` argument: a constant, a param or a bound variable.
fn apply_arg_pos(
    func: Func,
    t: &Term,
    params: &Params,
    interner: &mut Interner,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    match t {
        Term::Var(v) => match slots.get(v) {
            Some(s) if bound.contains(&s) => Ok(Pos::Bound(s)),
            _ => Err(QueryError::Plan(format!(
                "Apply {} on ?{}, which its input doesn't bind",
                func.name(),
                v.0
            ))),
        },
        Term::Any => Err(QueryError::Plan(format!(
            "Apply {} argument can't be _",
            func.name()
        ))),
        other => operand_pos(other, params, interner, slots, bound),
    }
}

/// How a term is named in an error message.
fn term_name(t: &Term) -> String {
    match t {
        Term::Var(v) => format!("?{}", v.0),
        Term::Param(p) => format!("${p}"),
        Term::Const(_) | Term::Op(_) | Term::Precision(_) => "a constant".into(),
        Term::Any => "_".into(),
    }
}

/// `l op r`. Equality across types is just false; ordering needs one type
/// and is real value order ([`cmp_real`], its work added to `work`).
fn compare(op: CmpOp, l: &Val, r: &Val, work: &mut CmpWork) -> Result<bool, QueryError> {
    if !matches!(op, CmpOp::Eq | CmpOp::Ne) && l.kind() != r.kind() {
        return Err(QueryError::TypeMismatch(format!(
            "can't order {} against {}",
            l.kind().name(),
            r.kind().name()
        )));
    }
    Ok(match op {
        CmpOp::Eq => l == r,
        CmpOp::Ne => l != r,
        CmpOp::Lt => cmp_real(l, r, work).is_lt(),
        CmpOp::Le => cmp_real(l, r, work).is_le(),
        CmpOp::Gt => cmp_real(l, r, work).is_gt(),
        CmpOp::Ge => cmp_real(l, r, work).is_ge(),
    })
}

// ---------------------------------------------------------------- aggregates

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

fn check_cancel(cancel: &AtomicBool) -> Result<(), QueryError> {
    if cancel.load(AtomicOrdering::Relaxed) {
        Err(QueryError::Cancelled)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------- eval

struct Budget<'a> {
    cancel: &'a AtomicBool,
    peak_bytes: usize,
    steps: u64,
    /// Bytes held now: live row sets, held scans, interned strings.
    bytes: usize,
    /// What one row of this plan's width costs ([`row_bytes`]).
    row_bytes: usize,
    /// Real-order comparison work not yet charged as steps.
    cmp: CmpWork,
    /// The per-row read memo, its bytes included in `bytes`. Held here so a
    /// charge that would exceed the budget can drop it first.
    memo: Memo,
    limits: Limits,
}

impl Budget<'_> {
    /// Charges `n` more bytes held. If that exceeds the budget, the memo
    /// (a cache) is dropped first; only then is it an error.
    fn charge_bytes(&mut self, n: usize) -> Result<(), QueryError> {
        check_cancel(self.cancel)?;
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > self.limits.max_bytes && self.memo.bytes > 0 {
            self.evict_memo();
        }
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        if self.bytes > self.limits.max_bytes {
            return Err(QueryError::MemoryBudget {
                limit: self.limits.max_bytes,
            });
        }
        Ok(())
    }

    /// Drops the whole memo and releases its bytes. Deterministic: whether
    /// it happens depends only on byte counts.
    fn evict_memo(&mut self) {
        self.release_bytes(self.memo.bytes);
        self.memo = Memo::default();
    }

    /// Charges `n` bytes to the memo, dropping the memo first if they don't
    /// fit; false (nothing charged) if they don't fit even then, and the
    /// caller doesn't keep the entry.
    fn charge_memo(&mut self, n: usize) -> bool {
        if self.bytes.saturating_add(n) > self.limits.max_bytes {
            self.evict_memo();
            if self.bytes.saturating_add(n) > self.limits.max_bytes {
                return false;
            }
        }
        self.bytes += n;
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        self.memo.bytes += n;
        true
    }

    /// The memo of `attr`, with `n` more bytes charged for the entry the
    /// caller is about to insert, or `None` (nothing charged) if it doesn't
    /// fit in the byte budget.
    fn memo_slot(&mut self, attr: &str, n: usize) -> Option<&mut AttrMemo> {
        if !self.memo.attrs.contains_key(attr) {
            if !self.charge_memo(MEMO_ENTRY_BYTES.saturating_add(attr.len())) {
                return None;
            }
            self.memo.attrs.insert(attr.to_owned(), AttrMemo::default());
        }
        if !self.charge_memo(n) {
            return None;
        }
        if !self.memo.attrs.contains_key(attr) {
            // Charging `n` dropped the memo, the new slot included.
            self.release_bytes(n);
            self.memo.bytes -= n;
            return None;
        }
        self.memo.attrs.get_mut(attr)
    }

    /// The memo of `attr`, if any of its reads are memoized.
    fn memo(&self, attr: &str) -> Option<&AttrMemo> {
        self.memo.attrs.get(attr)
    }

    /// `n` bytes are no longer held.
    fn release_bytes(&mut self, n: usize) {
        self.bytes = self.bytes.saturating_sub(n);
    }

    /// A row set of `rows` rows is dropped.
    fn release_rows(&mut self, rows: usize) {
        self.release_bytes(rows.saturating_mul(self.row_bytes));
    }

    /// Charges the real-order comparison work done since the last charge
    /// (about one step per 64 bytes, per 4 intervals compared).
    fn charge_cmp(&mut self) -> Result<(), QueryError> {
        check_cancel(self.cancel)?;
        let n = self.cmp.take_steps();
        if n == 0 {
            return Ok(());
        }
        self.step(n)
    }

    fn step(&mut self, n: u64) -> Result<(), QueryError> {
        check_cancel(self.cancel)?;
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.limits.max_steps {
            return Err(QueryError::StepBudget {
                limit: self.limits.max_steps,
            });
        }
        Ok(())
    }

    /// Adds `row` to an operator's output set, charging one step, and its
    /// bytes if it is new to the set.
    fn emit(&mut self, out: &mut BTreeSet<Row>, row: Row) -> Result<(), QueryError> {
        self.step(1)?;
        if out.insert(row) {
            self.charge_bytes(self.row_bytes)?;
        }
        if out.len() > self.limits.max_rows {
            return Err(QueryError::RowLimit {
                limit: self.limits.max_rows,
            });
        }
        Ok(())
    }
}

/// Per-row kala-core reads already made by this query, so a read repeated
/// for many rows (the same entity's long string, once per input row) is made,
/// copied and interned once. Every read here depends only on its key and the
/// query's canon, which is fixed, so entries stay valid for the whole query
/// (every `Fixpoint` round included). Charged to the byte budget while held
/// and dropped whole when the budget needs the room ([`Budget::evict_memo`]),
/// so it never makes a query fail.
#[derive(Default)]
struct Memo {
    /// Keyed by attribute name as written, so a lookup needn't allocate.
    attrs: HashMap<String, AttrMemo>,
    /// Bytes charged for everything in `attrs`.
    bytes: usize,
}

/// One attribute name's memoized reads (see [`Memo`]).
#[derive(Default)]
struct AttrMemo {
    /// `View::as_of(entity, attr, tick)`, values interned.
    as_of: HashMap<(EntityId, Tick), Rc<[Val]>>,
    /// `View::as_of_value(entity, attr, value, tick)`.
    as_of_value: HashMap<(EntityId, Val, Tick), bool>,
    /// `View::lookup(attr, value, tick)`.
    lookup: HashMap<(Val, Tick), Rc<[EntityId]>>,
    /// `View::history(entity, attr)` as the entity's points, interned.
    history: HashMap<EntityId, Rc<Points>>,
    /// `View::history(entity, attr)` grouped into `(value, set)`, empty
    /// sets left out.
    during: HashMap<EntityId, Rc<[(Val, Val)]>>,
    /// `View::holds_during(entity, attr, value)` as a held set, if not empty.
    holds_during: HashMap<(EntityId, Val), Option<Val>>,
}

/// Charged per memo entry (and per attribute's memo) beyond its contents:
/// the hash table's slot and the `Rc` header.
const MEMO_ENTRY_BYTES: usize = 64;

struct Exec<'a, 'w> {
    world: &'a World,
    /// The query's view, metered by `meter`.
    view: &'a MeteredView<'w>,
    /// Index keys examined by `view`'s reads not yet charged as steps.
    meter: &'a WorkMeter<'a>,
    width: usize,
    budget: Budget<'a>,
    /// Every string and interval set the query holds, each once with its
    /// id: rows share these and compare them by id.
    interner: Interner,
    /// `attr_scan` results by attribute name, then tick, read once per query.
    /// Only scans at constant, parameter or default ticks are kept here; a
    /// per-row tick's scan lives only while its rows expand (see
    /// `Step::Expand`). Keyed by name first so a lookup needn't allocate.
    scans: HashMap<String, HashMap<Tick, Rc<Pairs>>>,
    /// `history_scan` results by attribute name, read once per query and
    /// charged to the byte budget while held (until the query ends).
    history_scans: HashMap<String, Rc<Points>>,
    /// The cached `history_scan`s grouped into interval sets by `(entity,
    /// value)`, by attribute name, built once per query and charged while
    /// held (until the query ends).
    during_scans: HashMap<String, Rc<Sets>>,
    /// Types whose visibility lookup has paid its historical one-step
    /// charge. Individual IDs can be checked in constant time.
    checked_types: HashSet<u16>,
    /// Every relation of the plan, by id (empty outside its `Fixpoint`).
    rels: Vec<Relation>,
}

/// Which tuples of a relation a `Rel` reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Read {
    All,
    Delta,
    Nothing,
}

/// A `Fixpoint` being evaluated (see [`Exec::eval`]).
struct Frame {
    /// The program counter of its `FixLoop` (known once the first round
    /// reaches it).
    loop_pc: usize,
    /// The round: 0 runs every rule once; later rounds run variants.
    round: u64,
    /// In a later round, the index of the variant running.
    variant: usize,
    /// The recursive read that reads the delta in the running variant.
    delta_occ: Option<usize>,
}

/// A relation's tuples while its `Fixpoint` runs: sets, keyed by position.
struct Relation {
    /// Every tuple derived so far (in the body: the whole relation).
    tuples: BTreeSet<Vec<Val>>,
    /// The tuples the previous round added (a subset of `tuples`).
    delta: BTreeSet<Vec<Val>>,
    /// The tuples this round derived that aren't in `tuples` yet.
    new: BTreeSet<Vec<Val>>,
    /// What one tuple costs ([`tuple_bytes`]).
    tuple_bytes: usize,
}

impl Exec<'_, '_> {
    /// Bound this read by the steps still available. Updating the allowance
    /// does not change successful reads' historical key/step accounting.
    fn read_view(&self) -> MeteredView<'_> {
        self.meter.set_max_keys(
            self.budget
                .limits
                .max_steps
                .saturating_sub(self.budget.steps),
        );
        *self.view
    }

    /// Charges the index keys the view's reads examined since the last
    /// charge as steps (one per key). Called after every kala-core read.
    fn charge_index_work(&mut self) -> Result<(), QueryError> {
        self.meter.check()?;
        self.budget.step(self.meter.take_keys())
    }

    /// Charges the up-front part of a memo-missing kala-core read that takes
    /// the bound value `v`: about one step per [`CMP_BYTES_PER_STEP`] bytes
    /// of a string (it is copied for the call), before the copy is made, so
    /// a long value can't be copied many times before the budget trips.
    /// Returns that per-copy charge for [`Exec::charge_bound_read`].
    fn charge_bound_value(&mut self, v: &Val) -> Result<u64, QueryError> {
        let per_key = match v {
            Val::Str(_, s) => s.len() as u64 / CMP_BYTES_PER_STEP,
            _ => 0,
        };
        self.budget.step(per_key)?;
        Ok(per_key)
    }

    /// Like [`Exec::charge_index_work`], for a read that took a bound value
    /// charged `per_key` up front ([`Exec::charge_bound_value`]): every index
    /// key it examined carries (and compares) the whole value, so each costs
    /// one step plus `per_key`.
    fn charge_bound_read(&mut self, per_key: u64) -> Result<(), QueryError> {
        self.meter.check()?;
        let keys = self.meter.take_keys();
        self.budget
            .step(keys.saturating_add(keys.saturating_mul(per_key)))
    }

    /// `v` as an executor value, its string interned: a string the query
    /// already holds is shared; a new one is charged once (text plus
    /// [`STRING_OVERHEAD_BYTES`]).
    ///
    /// Only for values a kala-core read returned: a string the query already
    /// holds still cost that read a copy and this a re-hash, so it is
    /// charged about one step per [`CMP_BYTES_PER_STEP`] bytes (per-row
    /// reads are memoized, so this is paid per distinct read, not per row).
    fn intern(&mut self, v: Value) -> Result<Val, QueryError> {
        Ok(match v {
            Value::Str(s) => {
                let (v, charge) = self.interner.string(&s, true)?;
                if charge {
                    self.budget
                        .charge_bytes(s.len().saturating_add(STRING_OVERHEAD_BYTES))?;
                } else {
                    self.budget.step(s.len() as u64 / CMP_BYTES_PER_STEP)?;
                }
                v
            }
            Value::Bool(b) => Val::Bool(b),
            Value::I64(n) => Val::I64(n),
            Value::F64(x) => Val::F64(x),
            Value::Ref(e) => Val::Ref(e),
        })
    }

    /// The query's shared copy of `s` ([`Exec::intern`]), hashed once
    /// here; a string new to the budget (a plan constant's text included)
    /// is charged once.
    fn intern_str(&mut self, s: &str) -> Result<Val, QueryError> {
        let (v, charge) = self.interner.string(s, true)?;
        if charge {
            self.budget
                .charge_bytes(s.len().saturating_add(STRING_OVERHEAD_BYTES))?;
        }
        Ok(v)
    }

    /// Runs a compiled program (see [`Step`]) and returns its root's rows.
    /// Every row set is charged to the byte budget while it is held (as its
    /// rows are emitted) and released once its consumer has run.
    ///
    /// The program is walked with a program counter rather than an
    /// iterator, so a step may move it (semi-naive evaluation will jump
    /// back to re-run a `Fixpoint`'s rules).
    fn eval(&mut self, prog: &[Step]) -> Result<BTreeSet<Row>, QueryError> {
        // The row sets of operators whose consumer hasn't run yet (for an
        // open union: its accumulated output).
        let mut stack: Vec<BTreeSet<Row>> = Vec::new();
        // The open `Fixpoint`s, innermost last.
        let mut frames: Vec<Frame> = Vec::new();
        // The input rows of the open `Not`s, innermost last (still charged).
        let mut seeds: Vec<BTreeSet<Row>> = Vec::new();
        let mut pc = 0;
        while let Some(step) = prog.get(pc) {
            pc += 1;
            match step {
                Step::Unit => {
                    let mut out = BTreeSet::new();
                    self.budget.emit(&mut out, vec![None; self.width])?;
                    stack.push(out);
                }
                Step::Join { shared } => {
                    let r = pop(&mut stack);
                    let l = pop(&mut stack);
                    let out = self.join(&l, &r, shared)?;
                    self.budget.release_rows(l.len() + r.len());
                    stack.push(out);
                }
                Step::UnionBegin => stack.push(BTreeSet::new()),
                Step::UnionAdd => {
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    let acc = stack
                        .last_mut()
                        .expect("invariant: UnionAdd follows its UnionBegin");
                    for row in rows {
                        self.budget.emit(acc, row)?;
                    }
                    self.budget.release_rows(n);
                }
                // The accumulated union is already on top.
                Step::UnionEnd => {}
                Step::NotBegin => seeds.push(pop(&mut stack)),
                Step::Seed => {
                    let seed = seeds
                        .last()
                        .expect("invariant: a Seed runs inside its Not body");
                    let mut out = BTreeSet::new();
                    for row in seed {
                        self.budget.emit(&mut out, row.clone())?;
                    }
                    stack.push(out);
                }
                Step::NotEnd { key } => {
                    let body = pop(&mut stack);
                    let input = seeds
                        .pop()
                        .expect("invariant: a NotEnd closes its NotBegin");
                    let out = self.anti_semi(&input, &body, key, false)?;
                    self.budget.release_rows(input.len() + body.len());
                    stack.push(out);
                }
                Step::Difference { shared } | Step::Intersect { shared } => {
                    let r = pop(&mut stack);
                    let l = pop(&mut stack);
                    let keep_matched = matches!(step, Step::Intersect { .. });
                    let out = self.anti_semi(&l, &r, shared, keep_matched)?;
                    self.budget.release_rows(l.len() + r.len());
                    stack.push(out);
                }
                // A Fixpoint evaluated again starts from empty relations.
                Step::FixBegin { rels } => {
                    for &id in rels {
                        self.clear_rel(id);
                    }
                    frames.push(Frame {
                        loop_pc: 0,
                        round: 0,
                        variant: 0,
                        delta_occ: None,
                    });
                }
                Step::RuleEnd { rel, cols } => {
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    self.add_tuples(*rel, cols, &rows)?;
                    self.budget.release_rows(n);
                    let f = frames
                        .last_mut()
                        .expect("invariant: a RuleEnd runs inside its Fixpoint");
                    if f.round > 0 {
                        // A variant is done: the next one, or the round's end.
                        let Step::FixLoop { variants, .. } = &prog[f.loop_pc] else {
                            unreachable!("invariant: loop_pc is its Fixpoint's FixLoop")
                        };
                        pc = match self.next_variant(variants, f.variant + 1) {
                            Some(i) => {
                                f.variant = i;
                                f.delta_occ = Some(variants[i].occ);
                                variants[i].start
                            }
                            None => f.loop_pc,
                        };
                    }
                }
                Step::FixLoop { rels, variants } => {
                    let f = frames
                        .last_mut()
                        .expect("invariant: a FixLoop runs inside its Fixpoint");
                    f.loop_pc = pc - 1;
                    for &id in rels {
                        self.merge_new(id, !variants.is_empty())?;
                    }
                    f.round += 1;
                    // Only variants reading a non-empty delta can derive
                    // anything new; with none, the relations are complete.
                    match self.next_variant(variants, 0) {
                        Some(i) => {
                            f.variant = i;
                            f.delta_occ = Some(variants[i].occ);
                            pc = variants[i].start;
                        }
                        None => {
                            for &id in rels {
                                self.clear_delta(id);
                            }
                            frames.pop();
                        }
                    }
                }
                Step::Rel { rel, args, occ } => {
                    let read = match occ {
                        None => Read::All,
                        Some(o) => {
                            let f = frames
                                .last()
                                .expect("invariant: a recursive Rel runs inside its Fixpoint");
                            if f.round == 0 {
                                Read::Nothing
                            } else if f.delta_occ == Some(*o) {
                                Read::Delta
                            } else {
                                Read::All
                            }
                        }
                    };
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    let mut out = BTreeSet::new();
                    self.rel(*rel, args, read, rows, &mut out)?;
                    self.budget.release_rows(n);
                    stack.push(out);
                }
                // The body's rows stay on top as the Fixpoint's.
                Step::FixEnd { rels } => {
                    for &id in rels {
                        self.clear_rel(id);
                    }
                }
                unary => {
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    let out = self.eval_unary(unary, rows)?;
                    self.budget.release_rows(n);
                    stack.push(out);
                }
            }
        }
        let out = pop(&mut stack);
        debug_assert!(stack.is_empty() && seeds.is_empty());
        Ok(out)
    }

    /// Groups `rows` (a set: one row per distinct binding of every input
    /// slot) by the `keys` slots and computes `aggs` per group. One step
    /// per input row and per output row; each group and each distinct value
    /// a `count-distinct` holds is charged while held, and the groups count
    /// toward `max_rows`. No input rows, no groups: no output rows.
    fn aggregate(
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

    /// The rows of `l` that match some row of `r` on the `shared` slots
    /// (bound on both sides) if `keep_matched`, else those matching none:
    /// a semi-join or an anti-join. With no shared slot, every row matches
    /// any row, so the answer is all of `l` or nothing. One step per probe.
    fn anti_semi(
        &mut self,
        l: &BTreeSet<Row>,
        r: &BTreeSet<Row>,
        shared: &[usize],
        keep_matched: bool,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut keys = HashSet::new();
        let entry_bytes = index_entry_bytes(shared.len());
        let mut held = 0usize;
        for row in r {
            // Charge before allocating even the temporary key. Duplicate
            // keys release their temporary charge immediately.
            self.budget.charge_bytes(entry_bytes)?;
            if keys.insert(key_of(row, shared)) {
                held = held.saturating_add(entry_bytes);
            } else {
                self.budget.release_bytes(entry_bytes);
            }
        }
        let mut out = BTreeSet::new();
        for row in l {
            self.budget.step(1)?;
            if keys.contains(&key_of(row, shared)) == keep_matched {
                self.budget.emit(&mut out, row.clone())?;
            }
        }
        drop(keys);
        self.budget.release_bytes(held);
        Ok(out)
    }

    /// Empties relation `id`, releasing its tuples' bytes.
    fn clear_rel(&mut self, id: usize) {
        let r = &mut self.rels[id];
        let held = r.tuples.len() + r.delta.len() + r.new.len();
        r.tuples.clear();
        r.delta.clear();
        r.new.clear();
        self.budget
            .release_bytes(held.saturating_mul(r.tuple_bytes));
    }

    /// Drops relation `id`'s delta, releasing its bytes.
    fn clear_delta(&mut self, id: usize) {
        let r = &mut self.rels[id];
        let bytes = r.delta.len().saturating_mul(r.tuple_bytes);
        r.delta.clear();
        self.budget.release_bytes(bytes);
    }

    /// The end of a round for relation `id`: its new tuples join it. If
    /// `keep_delta`, they also become its delta (a copy, charged while
    /// held), replacing the previous one.
    fn merge_new(&mut self, id: usize, keep_delta: bool) -> Result<(), QueryError> {
        self.clear_delta(id);
        let r = &mut self.rels[id];
        let new = std::mem::take(&mut r.new);
        if keep_delta {
            let copies = new.len().saturating_mul(r.tuple_bytes);
            r.tuples.extend(new.iter().cloned());
            r.delta = new;
            self.budget.charge_bytes(copies)?;
        } else {
            r.tuples.extend(new);
        }
        Ok(())
    }

    /// The first variant from `from` on whose recursive read has a
    /// non-empty delta to read.
    fn next_variant(&self, variants: &[Variant], from: usize) -> Option<usize> {
        (from..variants.len()).find(|&i| !self.rels[variants[i].rel].delta.is_empty())
    }

    /// Folds one rule's rows into relation `id`'s tuples new this round as
    /// tuples of the slots `cols` (a tuple the relation already has is not
    /// new): one step per row, new tuples charged while held, and the
    /// relation (with this round's new tuples) bounded by `max_rows` like
    /// any union.
    fn add_tuples(
        &mut self,
        id: usize,
        cols: &[usize],
        rows: &BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let rel = &mut self.rels[id];
        for row in rows {
            self.budget.step(1)?;
            let t: Vec<Val> = cols
                .iter()
                .map(|&c| {
                    row[c]
                        .clone()
                        .expect("invariant: compile checked every rule binds its columns")
                })
                .collect();
            if !rel.tuples.contains(&t) && rel.new.insert(t) {
                self.budget.charge_bytes(rel.tuple_bytes)?;
            }
            if rel.tuples.len() + rel.new.len() > self.budget.limits.max_rows {
                return Err(QueryError::RowLimit {
                    limit: self.budget.limits.max_rows,
                });
            }
        }
        Ok(())
    }

    /// Joins `rows` with relation `id`'s tuples, matched to `args` by
    /// position: constants and bound variables must equal the tuple's
    /// value (looked up in a hash index on those positions, built once for
    /// all rows), free variables bind it (a variable repeated must take
    /// one value), `_` matches anything.
    fn rel(
        &mut self,
        id: usize,
        args: &[Pos],
        read: Read,
        rows: BTreeSet<Row>,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let nothing = BTreeSet::new();
        let tuples = match read {
            Read::All => &self.rels[id].tuples,
            Read::Delta => &self.rels[id].delta,
            Read::Nothing => &nothing,
        };
        let keyed: Vec<usize> = (0..args.len()).filter(|&i| args[i].is_bound()).collect();
        // Reading the relation (or indexing it) examines every tuple once.
        self.budget.step(tuples.len() as u64)?;
        let mut index: HashMap<Vec<&Val>, Vec<&Vec<Val>>> = HashMap::new();
        let entry_bytes = if keyed.is_empty() {
            size_of::<&Vec<Val>>()
        } else {
            index_entry_bytes(keyed.len())
        };
        let mut index_bytes = 0usize;
        let mut all = Vec::new();
        for t in tuples {
            self.budget.charge_bytes(entry_bytes)?;
            index_bytes = index_bytes.saturating_add(entry_bytes);
            if keyed.is_empty() {
                all.push(t);
            } else {
                let k: Vec<&Val> = keyed.iter().map(|&i| &t[i]).collect();
                index.entry(k).or_default().push(t);
            }
        }
        for row in rows {
            self.budget.step(1)?;
            let found: &[&Vec<Val>] = if keyed.is_empty() {
                &all
            } else {
                let k: Vec<&Val> = keyed
                    .iter()
                    .map(|&i| {
                        resolve(&args[i], &row)
                            .expect("invariant: compile checked Rel's bound arguments")
                    })
                    .collect();
                index.get(&k).map_or(&[], Vec::as_slice)
            };
            'tuples: for t in found {
                self.budget.step(1)?;
                let mut r = row.clone();
                for (p, x) in args.iter().zip(t.iter()) {
                    if let Pos::Free(s) = p {
                        match &r[*s] {
                            Some(prev) if prev != x => continue 'tuples,
                            Some(_) => {}
                            None => r[*s] = Some(x.clone()),
                        }
                    }
                }
                self.budget.emit(out, r)?;
            }
        }
        drop(index);
        drop(all);
        self.budget.release_bytes(index_bytes);
        Ok(())
    }

    /// Hash join on the shared slots (all of them bound on both sides); no
    /// shared slots is a Cartesian product.
    fn join(
        &mut self,
        l: &BTreeSet<Row>,
        r: &BTreeSet<Row>,
        shared: &[usize],
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        let mut by_key: HashMap<Vec<&Val>, Vec<&Row>> = HashMap::new();
        let entry_bytes = index_entry_bytes(shared.len());
        let mut held = 0usize;
        for row in r {
            self.budget.charge_bytes(entry_bytes)?;
            held = held.saturating_add(entry_bytes);
            by_key.entry(key_of(row, shared)).or_default().push(row);
        }
        for lrow in l {
            let Some(matches) = by_key.get(&key_of(lrow, shared)) else {
                continue;
            };
            for rrow in matches {
                let merged: Row = lrow
                    .iter()
                    .zip(rrow.iter())
                    .map(|(a, b)| a.clone().or_else(|| b.clone()))
                    .collect();
                self.budget.emit(&mut out, merged)?;
            }
        }
        drop(by_key);
        self.budget.release_bytes(held);
        Ok(out)
    }

    /// Runs a one-input step on its input's rows.
    fn eval_unary(
        &mut self,
        step: &Step,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        match step {
            Step::NodeScan {
                slot,
                bound,
                node_type,
            } => {
                self.budget.step(1)?;
                let all: Vec<EntityId> = self
                    .read_view()
                    .entities_of_type(node_type)?
                    .collect::<Result<_, _>>()?;
                self.charge_index_work()?;
                for row in rows {
                    if *bound {
                        if let Some(Val::Ref(e)) = &row[*slot]
                            && all.binary_search(e).is_ok()
                        {
                            self.budget.emit(&mut out, row)?;
                        }
                        continue;
                    }
                    for &e in &all {
                        let mut r = row.clone();
                        r[*slot] = Some(Val::Ref(e));
                        self.budget.emit(&mut out, r)?;
                    }
                }
            }
            Step::NodeSeek {
                slot,
                bound,
                entity,
            } => {
                for mut row in rows {
                    let e = match resolve(entity, &row) {
                        Some(Val::Ref(e)) => *e,
                        Some(other) => return Err(not_an_entity(other)),
                        None => unreachable!("invariant: compile checked NodeSeek's entity"),
                    };
                    if !self.defined_at_canon(e)? {
                        continue;
                    }
                    if *bound {
                        if row[*slot] != Some(Val::Ref(e)) {
                            continue;
                        }
                    } else {
                        row[*slot] = Some(Val::Ref(e));
                    }
                    self.budget.emit(&mut out, row)?;
                }
            }
            Step::Expand {
                entity,
                attr,
                value,
                tick,
                access,
            } => match (access, tick) {
                // A scan at a per-row tick: group the rows by tick and hold
                // one tick's scan at a time, so a query over many distinct
                // ticks never keeps every scan (long unindexed strings
                // included) alive at once. Still one scan per distinct tick.
                (Access::Scan, TickPos::Var(s)) => {
                    let mut by_tick: BTreeMap<Tick, Vec<Row>> = BTreeMap::new();
                    for row in rows {
                        let t = tick_of(&row, *s)?;
                        by_tick.entry(t).or_default().push(row);
                    }
                    for (t, rows) in by_tick {
                        let (all, fresh) = self.scan_once(&attr.name, t)?;
                        for row in rows {
                            let pairs =
                                self.expand_row(&row, entity, attr, value, t, *access, Some(&all))?;
                            for (e, v) in pairs {
                                if let Some(r) = bind(&row, entity, value, Val::Ref(e), v) {
                                    self.budget.emit(&mut out, r)?;
                                }
                            }
                        }
                        // `all` is dropped here, before the next tick's scan.
                        if fresh {
                            self.budget.release_bytes(all.len() * PAIR_BYTES);
                        }
                    }
                }
                _ => {
                    for row in rows {
                        let t = match tick {
                            TickPos::Const(t) => *t,
                            TickPos::Var(s) => tick_of(&row, *s)?,
                        };
                        let pairs = self.expand_row(&row, entity, attr, value, t, *access, None)?;
                        for (e, v) in pairs {
                            if let Some(r) = bind(&row, entity, value, Val::Ref(e), v) {
                                self.budget.emit(&mut out, r)?;
                            }
                        }
                    }
                }
            },
            Step::Handle { entity, value } => {
                for row in rows {
                    self.budget.step(1)?;
                    let pair = match (resolve(entity, &row), resolve(value, &row)) {
                        (Some(Val::Ref(e)), _) => {
                            let e = *e;
                            if !self.defined_at_canon(e)? {
                                None
                            } else {
                                match self.world.handle_of(e) {
                                    Some(h) => Some((e, self.intern_str(h)?)),
                                    None => None,
                                }
                            }
                        }
                        (Some(other), _) => return Err(not_an_entity(other)),
                        (None, Some(v @ Val::Str(_, h))) => match self.world.entity_by_handle(h) {
                            Some(e) if self.defined_at_canon(e)? => Some((e, v.clone())),
                            _ => None,
                        },
                        (None, Some(other)) => {
                            return Err(QueryError::TypeMismatch(format!(
                                "a handle must be a string, got {}",
                                other.kind().name()
                            )));
                        }
                        (None, None) => unreachable!("invariant: compile checked handle access"),
                    };
                    if let Some((e, v)) = pair
                        && let Some(r) = bind(&row, entity, value, Val::Ref(e), v)
                    {
                        self.budget.emit(&mut out, r)?;
                    }
                }
            }
            Step::History {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => {
                for row in rows {
                    self.history_row(&row, entity, attr, value, tick, op, precision, &mut out)?;
                }
            }
            Step::During {
                entity,
                attr,
                value,
                out: set,
            } => {
                for row in rows {
                    self.during_row(&row, entity, attr, value, set, &mut out)?;
                }
            }
            Step::Apply {
                func,
                args,
                out: res,
            } => {
                for row in rows {
                    if let Some(r) = self.apply_row(row, *func, args, res.as_ref())? {
                        self.budget.emit(&mut out, r)?;
                    }
                }
            }
            Step::Filter { op, left, right } => {
                for row in rows {
                    let (Some(l), Some(r)) = (resolve(left, &row), resolve(right, &row)) else {
                        unreachable!("invariant: compile checked filter operands are bound")
                    };
                    let keep = compare(*op, l, r, &mut self.budget.cmp)?;
                    self.budget.charge_cmp()?;
                    if keep {
                        self.budget.emit(&mut out, row)?;
                    }
                }
            }
            Step::Project { keep } => {
                for row in rows {
                    let mut r = vec![None; self.width];
                    for &s in keep {
                        r[s] = row[s].clone();
                    }
                    self.budget.emit(&mut out, r)?;
                }
            }
            Step::Aggregate { keys, aggs } => return self.aggregate(rows, keys, aggs),
            Step::Unit
            | Step::Join { .. }
            | Step::UnionBegin
            | Step::UnionAdd
            | Step::UnionEnd
            | Step::NotBegin
            | Step::Seed
            | Step::NotEnd { .. }
            | Step::Difference { .. }
            | Step::Intersect { .. }
            | Step::FixBegin { .. }
            | Step::RuleEnd { .. }
            | Step::FixLoop { .. }
            | Step::Rel { .. }
            | Step::FixEnd { .. } => {
                unreachable!("invariant: eval runs only one-input steps through eval_unary")
            }
        }
        Ok(out)
    }

    /// The `(entity, value)` pairs of `attr` holding at `t` that `access`
    /// reads for `row`, already narrowed by the bound side the access needs
    /// (any other bound side is checked by [`bind`]). For [`Access::Scan`],
    /// `scanned` is the scan of `attr` at `t` if the caller already holds it;
    /// otherwise the per-query cache ([`Exec::scan`]) is used.
    #[allow(clippy::too_many_arguments)]
    fn expand_row(
        &mut self,
        row: &Row,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        t: Tick,
        access: Access,
        scanned: Option<&Pairs>,
    ) -> Result<Pairs, QueryError> {
        let e = match resolve(entity, row) {
            Some(Val::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        let v = match resolve(value, row) {
            Some(v) if v.kind() != ValKind::Value(attr.value_type) => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    v.kind().name()
                )));
            }
            // NaN is never stored, so it matches nothing, whichever access.
            Some(Val::F64(x)) if x.is_nan() => return Ok(Vec::new()),
            other => other,
        };
        let name = attr.name.as_str();
        let pairs = match access {
            Access::E => {
                let e = e.expect("invariant: compile checked E has the entity bound");
                self.budget.step(1)?;
                if let (Some(v), false) = (v, attr.inverse) {
                    let key = (e, v.clone(), t);
                    let memo = self.budget.memo(name);
                    let holds = match memo.and_then(|m| m.as_of_value.get(&key)) {
                        Some(&h) => h,
                        None => {
                            // kala-core takes an owned value: a string is
                            // copied here, once per memo key, and charged by
                            // its length (a per-row tick misses every time).
                            let per_key = self.charge_bound_value(v)?;
                            let r = self.read_view().as_of_value(e, name, &value_of(v), t);
                            self.charge_bound_read(per_key)?;
                            let h = absent_as_empty(r)? == Some(true);
                            let n = MEMO_ENTRY_BYTES + size_of_val(&key);
                            if let Some(m) = self.budget.memo_slot(name, n) {
                                m.as_of_value.insert(key, h);
                            }
                            h
                        }
                    };
                    if holds {
                        vec![(e, v.clone())]
                    } else {
                        Vec::new()
                    }
                } else if attr.inverse {
                    let ns = self.read_view().neighbors(e, name, Direction::Out, t);
                    self.charge_index_work()?;
                    absent_as_empty(ns)?
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(x, _)| (e, Val::Ref(x)))
                        .collect()
                } else {
                    let memo = self.budget.memo(name);
                    let vals = match memo.and_then(|m| m.as_of.get(&(e, t))) {
                        Some(vals) => Rc::clone(vals),
                        None => {
                            let vs = self.read_view().as_of(e, name, t);
                            self.charge_index_work()?;
                            let mut vals = Vec::new();
                            for x in absent_as_empty(vs)?.unwrap_or_default() {
                                vals.push(self.intern(x)?);
                            }
                            let vals: Rc<[Val]> = vals.into();
                            let n = MEMO_ENTRY_BYTES + vals.len() * size_of::<Val>();
                            if let Some(m) = self.budget.memo_slot(name, n) {
                                m.as_of.insert((e, t), Rc::clone(&vals));
                            }
                            vals
                        }
                    };
                    vals.iter().map(|x| (e, x.clone())).collect()
                }
            }
            Access::R | Access::V => {
                let v = v.expect("invariant: compile checked R/V have the value bound");
                self.budget.step(1)?;
                match v {
                    Val::Ref(y) => {
                        let ns = self.read_view().neighbors(*y, name, Direction::In, t);
                        self.charge_index_work()?;
                        absent_as_empty(ns)?
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(x, _)| (x, v.clone()))
                            .collect()
                    }
                    _ => {
                        let key = (v.clone(), t);
                        let memo = self.budget.memo(name);
                        let found = match memo.and_then(|m| m.lookup.get(&key)) {
                            Some(found) => Rc::clone(found),
                            None => {
                                // Copies a string value, once per memo key,
                                // charged by its length.
                                let per_key = self.charge_bound_value(v)?;
                                let found = self.read_view().lookup(name, &value_of(v), t);
                                self.charge_bound_read(per_key)?;
                                let found: Rc<[EntityId]> = found?.into();
                                let n = MEMO_ENTRY_BYTES
                                    + size_of_val(&key)
                                    + found.len() * size_of::<EntityId>();
                                if let Some(m) = self.budget.memo_slot(name, n) {
                                    m.lookup.insert(key, Rc::clone(&found));
                                }
                                found
                            }
                        };
                        found.iter().map(|&x| (x, v.clone())).collect()
                    }
                }
            }
            Access::Scan => {
                let cached;
                let all: &Pairs = match scanned {
                    Some(p) => p,
                    None => {
                        cached = self.scan(name, t)?;
                        &cached
                    }
                };
                let range = match e {
                    // Sorted by (entity, value): the entity's pairs are one run.
                    Some(e) => {
                        let lo = all.partition_point(|(x, _)| *x < e);
                        let hi = all.partition_point(|(x, _)| *x <= e);
                        lo..hi
                    }
                    None => 0..all.len(),
                };
                self.budget.step(range.len() as u64)?;
                all[range]
                    .iter()
                    .filter(|(_, x)| v.is_none_or(|v| x == v))
                    .cloned()
                    .collect()
            }
            Access::Handle => {
                unreachable!("invariant: compile routes :kala_handle to Step::Handle")
            }
        };
        self.budget.step(pairs.len() as u64)?;
        Ok(pairs)
    }

    /// Emits into `out` `row` extended with each change point of `attr`
    /// matching the row's bound positions. With the entity bound (and a
    /// forward name) the points are one [`View::history`] read; otherwise
    /// they come from the query's cached [`View::history_scan`] (narrowed to
    /// the entity's run if it is bound, for an inverse name). One step per
    /// point examined, plus the index work.
    #[allow(clippy::too_many_arguments)]
    fn history_row(
        &mut self,
        row: &Row,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        tick: &Pos,
        op: &Pos,
        precision: &Pos,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let e = match resolve(entity, row) {
            Some(Val::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        match resolve(value, row) {
            Some(v) if v.kind() != ValKind::Value(attr.value_type) => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    v.kind().name()
                )));
            }
            // NaN is never stored, so it matches nothing.
            Some(Val::F64(x)) if x.is_nan() => return Ok(()),
            _ => {}
        }
        if let Some(t) = resolve(tick, row)
            && !matches!(t, Val::I64(_))
        {
            return Err(QueryError::TypeMismatch(format!(
                "a tick must be i64, got {}",
                t.kind().name()
            )));
        }
        let (points, range) = match e {
            Some(e) if !attr.inverse => {
                self.budget.step(1)?;
                let points = self.history_of(e, &attr.name)?;
                let n = points.len();
                (points, 0..n)
            }
            _ => {
                let all = self.history_scan(&attr.name)?;
                let range = match e {
                    // Sorted by entity: the entity's points are one run.
                    Some(e) => {
                        let lo = all.partition_point(|(x, ..)| *x < e);
                        let hi = all.partition_point(|(x, ..)| *x <= e);
                        lo..hi
                    }
                    None => 0..all.len(),
                };
                (all, range)
            }
        };
        self.budget.step(range.len() as u64)?;
        for (x, v, t, o, p) in &points[range] {
            let binds = [
                (entity, Val::Ref(*x)),
                (value, v.clone()),
                (tick, Val::I64(t.0)),
                (op, Val::Op(*o)),
                (precision, Val::Precision(*p)),
            ];
            if let Some(r) = bind_all(row, binds) {
                self.budget.emit(out, r)?;
            }
        }
        Ok(())
    }

    /// Emits into `out` `row` extended with the interval set where each
    /// matching `(entity, attr, value)` holds, over every tick, at the
    /// view's canon (only pairs that ever held: never an empty set). With
    /// the entity and value bound, one [`View::holds_during`] read; with
    /// only a forward name's entity bound, one [`View::history`] read
    /// grouped by value; otherwise the query's grouped history scan
    /// ([`Exec::during_scan`]), narrowed to the entity's run if it is
    /// bound. One step per read, set or change point examined, plus the
    /// index work; each set read is charged by size.
    fn during_row(
        &mut self,
        row: &Row,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        set: &Pos,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let e = match resolve(entity, row) {
            Some(Val::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        let v = match resolve(value, row) {
            Some(v) if v.kind() != ValKind::Value(attr.value_type) => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    v.kind().name()
                )));
            }
            // NaN is never stored, so it matches nothing.
            Some(Val::F64(x)) if x.is_nan() => return Ok(()),
            other => other,
        };
        match (e, v) {
            (Some(e), Some(v)) => {
                self.budget.step(1)?;
                let name = attr.name.as_str();
                let key = (e, v.clone());
                let memo = self.budget.memo(name);
                let found = match memo.and_then(|m| m.holds_during.get(&key)) {
                    Some(found) => found.clone(),
                    None => {
                        // kala-core takes an owned value: a string is copied
                        // here, once per memo key, and charged by its length
                        // (entities sharing one value miss every time).
                        let per_key = self.charge_bound_value(v)?;
                        let read = match (attr.inverse, v) {
                            // `[e child_of v]` is `[v parent_of e]`.
                            (true, Val::Ref(source)) => {
                                self.read_view()
                                    .holds_during(*source, &attr.base, &Value::Ref(e))
                            }
                            (true, _) => {
                                unreachable!("invariant: an inverse name's values are refs")
                            }
                            (false, v) => self.read_view().holds_during(e, name, &value_of(v)),
                        };
                        self.charge_bound_read(per_key)?;
                        let found = absent_as_empty(read)?.unwrap_or_default();
                        self.budget.step(found.len() as u64)?;
                        let found = if found.is_empty() {
                            None
                        } else {
                            Some(self.hold_set(found)?)
                        };
                        let n = MEMO_ENTRY_BYTES + size_of_val(&key) + size_of::<Option<Val>>();
                        if let Some(m) = self.budget.memo_slot(name, n) {
                            m.holds_during.insert(key, found.clone());
                        }
                        found
                    }
                };
                if let Some(found) = found
                    && let Some(r) = bind_all(row, [(set, found)])
                {
                    self.budget.emit(out, r)?;
                }
            }
            (Some(e), None) if !attr.inverse => {
                self.budget.step(1)?;
                let name = attr.name.as_str();
                let memo = self.budget.memo(name);
                let sets = match memo.and_then(|m| m.during.get(&e)) {
                    Some(sets) => Rc::clone(sets),
                    None => {
                        let points = self.history_of(e, name)?;
                        self.budget.step(points.len() as u64)?;
                        let mut by_value: BTreeMap<&Val, Vec<(Tick, FactOp)>> = BTreeMap::new();
                        for (_, x, t, o, _) in points.iter() {
                            by_value.entry(x).or_default().push((*t, *o));
                        }
                        let mut sets = Vec::new();
                        for (x, changes) in by_value {
                            let found = IntervalSet::from_changes(&changes);
                            if found.is_empty() {
                                continue;
                            }
                            sets.push((x.clone(), self.hold_set(found)?));
                        }
                        let sets: Rc<[(Val, Val)]> = sets.into();
                        let n = MEMO_ENTRY_BYTES + sets.len() * size_of::<(Val, Val)>();
                        if let Some(m) = self.budget.memo_slot(name, n) {
                            m.during.insert(e, Rc::clone(&sets));
                        }
                        sets
                    }
                };
                for (x, found) in sets.iter() {
                    if let Some(r) = bind_all(row, [(value, x.clone()), (set, found.clone())]) {
                        self.budget.emit(out, r)?;
                    }
                }
            }
            _ => {
                let all = self.during_scan(&attr.name)?;
                let range = match e {
                    // Sorted by entity: the entity's sets are one run.
                    Some(e) => {
                        let lo = all.partition_point(|(x, ..)| *x < e);
                        let hi = all.partition_point(|(x, ..)| *x <= e);
                        lo..hi
                    }
                    None => 0..all.len(),
                };
                self.budget.step(range.len() as u64)?;
                for (x, v, found) in &all[range] {
                    let binds = [
                        (entity, Val::Ref(*x)),
                        (value, v.clone()),
                        (set, found.clone()),
                    ];
                    if let Some(r) = bind_all(row, binds) {
                        self.budget.emit(out, r)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `row` with `func(args)` bound to `res` (`None` if a bound `res`
    /// differs), or for a filter (no `res`) `row` itself if it is true. One
    /// step, plus one per interval the set operations sweep; a result set is
    /// charged by size.
    fn apply_row(
        &mut self,
        row: Row,
        func: Func,
        args: &[Pos],
        res: Option<&Pos>,
    ) -> Result<Option<Row>, QueryError> {
        let arg = |i: usize| {
            resolve(&args[i], &row).expect("invariant: compile checked Apply's arguments are bound")
        };
        match func {
            Func::Intersect | Func::Union | Func::Difference => {
                let (a, b) = (as_set(func, arg(0))?, as_set(func, arg(1))?);
                let swept = a.len().saturating_add(b.len()) as u64;
                self.budget.step(swept.saturating_add(1))?;
                let r = match func {
                    Func::Intersect => a.intersect(b),
                    Func::Union => a.union(b),
                    _ => a.difference(b),
                };
                let r = self.hold_set(r)?;
                let res = res.expect("invariant: compile checked set functions have an output");
                Ok(bind_all(&row, [(res, r)]))
            }
            Func::NotEmpty => {
                self.budget.step(1)?;
                let keep = !as_set(func, arg(0))?.is_empty();
                Ok(keep.then_some(row))
            }
            Func::Contains => {
                self.budget.step(1)?;
                let t = match arg(1) {
                    Val::I64(n) => Tick(*n),
                    other => {
                        return Err(QueryError::TypeMismatch(format!(
                            "Apply contains needs an i64 tick, got {}",
                            other.kind().name()
                        )));
                    }
                };
                let keep = as_set(func, arg(0))?.contains(t);
                Ok(keep.then_some(row))
            }
        }
    }

    /// `View::history(e, attr)` as `e`'s change points (values interned),
    /// memoized per query by `(e, attr)`: a repeat costs nothing here (the
    /// caller charges the row's read step).
    fn history_of(&mut self, e: EntityId, attr: &str) -> Result<Rc<Points>, QueryError> {
        if let Some(points) = self.budget.memo(attr).and_then(|m| m.history.get(&e)) {
            return Ok(Rc::clone(points));
        }
        let read = self.read_view().history(e, attr);
        self.charge_index_work()?;
        let mut points = Vec::new();
        for c in absent_as_empty(read)?.unwrap_or_default() {
            points.push((e, self.intern(c.value)?, c.tick, c.op, c.precision));
        }
        let points = Rc::new(points);
        let n = MEMO_ENTRY_BYTES + points.len() * POINT_BYTES;
        if let Some(m) = self.budget.memo_slot(attr, n) {
            m.history.insert(e, Rc::clone(&points));
        }
        Ok(points)
    }

    /// An interval set the query now holds, as a value: charged by size
    /// until the query ends (rows may share it, so it is never released
    /// early), and interned (hashed once here), so an equal set already
    /// held is shared. Every set created is charged, shared or not.
    fn hold_set(&mut self, s: IntervalSet) -> Result<Val, QueryError> {
        self.budget.charge_bytes(set_bytes(&s))?;
        self.interner.set(s)
    }

    /// The query's history scan of `attr` ([`Exec::history_scan`]) grouped
    /// into one interval set per `(entity, value)` that ever held, built
    /// once per query and cached: one step per point grouped, and each set
    /// and entry charged while held.
    fn during_scan(&mut self, attr: &str) -> Result<Rc<Sets>, QueryError> {
        if let Some(s) = self.during_scans.get(attr) {
            return Ok(Rc::clone(s));
        }
        let points = self.history_scan(attr)?;
        self.budget.step(points.len() as u64)?;
        let mut sets = Vec::new();
        let mut i = 0;
        while i < points.len() {
            let e = points[i].0;
            // Sorted by entity: the entity's points are one run.
            let j = i + points[i..].partition_point(|(x, ..)| *x == e);
            let mut by_value: BTreeMap<&Val, Vec<(Tick, FactOp)>> = BTreeMap::new();
            for (_, v, t, o, _) in &points[i..j] {
                by_value.entry(v).or_default().push((*t, *o));
            }
            for (v, changes) in by_value {
                let found = IntervalSet::from_changes(&changes);
                if found.is_empty() {
                    continue;
                }
                self.budget.charge_bytes(SET_ENTRY_BYTES)?;
                let found = self.hold_set(found)?;
                sets.push((e, v.clone(), found));
            }
            i = j;
        }
        let sets = Rc::new(sets);
        self.during_scans.insert(attr.to_owned(), Rc::clone(&sets));
        Ok(sets)
    }

    /// `View::history_scan(attr)`, read once per query and cached for the
    /// rest of it: one step plus one per point plus the index work, and the
    /// points' bytes (strings interned) held until the query ends.
    fn history_scan(&mut self, attr: &str) -> Result<Rc<Points>, QueryError> {
        if let Some(p) = self.history_scans.get(attr) {
            return Ok(Rc::clone(p));
        }
        self.budget.step(1)?;
        let read = self.read_view().history_scan(attr);
        self.charge_index_work()?;
        let read = read?;
        self.budget.step(read.len() as u64)?;
        self.budget
            .charge_bytes(read.len().saturating_mul(POINT_BYTES))?;
        let mut points = Vec::with_capacity(read.len());
        for (e, c) in read {
            points.push((e, self.intern(c.value)?, c.tick, c.op, c.precision));
        }
        let points = Rc::new(points);
        self.history_scans
            .insert(attr.to_owned(), Rc::clone(&points));
        Ok(points)
    }

    /// `View::attr_scan(attr, t)`, read once per query and cached for the
    /// rest of it (for constant, parameter and default ticks: one scan each).
    /// A cached scan stays charged to the byte budget until the query ends.
    fn scan(&mut self, attr: &str, t: Tick) -> Result<Rc<Pairs>, QueryError> {
        if let Some(s) = self.scans.get(attr).and_then(|m| m.get(&t)) {
            return Ok(Rc::clone(s));
        }
        let pairs = self.read_scan(attr, t)?;
        self.scans
            .entry(attr.to_owned())
            .or_default()
            .insert(t, Rc::clone(&pairs));
        Ok(pairs)
    }

    /// `View::attr_scan(attr, t)` for a per-row tick: reuses a cached scan
    /// if one exists but doesn't cache a new one, so the caller decides how
    /// long it lives (one distinct tick at a time, see `Step::Expand`).
    /// Returns whether the scan was freshly read: the caller releases a
    /// fresh scan's bytes when it drops it.
    fn scan_once(&mut self, attr: &str, t: Tick) -> Result<(Rc<Pairs>, bool), QueryError> {
        if let Some(s) = self.scans.get(attr).and_then(|m| m.get(&t)) {
            return Ok((Rc::clone(s), false));
        }
        Ok((self.read_scan(attr, t)?, true))
    }

    /// Reads `View::attr_scan(attr, t)`, charging one step plus one per
    /// pair plus the index work, and the pairs' bytes (strings interned).
    fn read_scan(&mut self, attr: &str, t: Tick) -> Result<Rc<Pairs>, QueryError> {
        self.budget.step(1)?;
        let read = self.read_view().attr_scan(attr, t);
        self.charge_index_work()?;
        let read = read?;
        self.budget.step(read.len() as u64)?;
        self.budget
            .charge_bytes(read.len().saturating_mul(PAIR_BYTES))?;
        let mut pairs = Vec::with_capacity(read.len());
        for (e, v) in read {
            pairs.push((e, self.intern(v)?));
        }
        Ok(Rc::new(pairs))
    }

    /// Whether `id` (an entity or an edge) is defined at the view's canon.
    fn defined_at_canon(&mut self, id: EntityId) -> Result<bool, QueryError> {
        if id.is_edge() {
            return Ok(id
                .as_edge()
                .is_some_and(|edge: EdgeId| self.read_view().edge(edge).is_some()));
        }
        let ty = id.node_type();
        if !self.checked_types.contains(&ty.0) {
            if self.world.node_types().nth(usize::from(ty.0)).is_none() {
                return Ok(false);
            }
            self.budget.step(1)?;
            self.checked_types.insert(ty.0);
        }
        Ok(self.read_view().is_defined(id))
    }
}

/// The tick in slot `s` of `row` (a bound tick variable).
fn tick_of(row: &Row, s: usize) -> Result<Tick, QueryError> {
    match &row[s] {
        Some(Val::I64(n)) => Ok(Tick(*n)),
        Some(other) => Err(QueryError::TypeMismatch(format!(
            "a tick must be i64, got {}",
            other.kind().name()
        ))),
        None => unreachable!("invariant: compile checked the tick var"),
    }
}

/// A position's value in `row`: constants and bound vars; `None` for free
/// vars and `_`.
fn resolve<'r>(p: &'r Pos, row: &'r Row) -> Option<&'r Val> {
    match p {
        Pos::Const(v) => Some(v),
        Pos::Bound(s) => row[*s].as_ref(),
        Pos::Free(_) | Pos::Any => None,
    }
}

/// `row` extended with `entity = e` and `value = v`, or `None` if a bound
/// position (or the same free variable twice) disagrees.
fn bind(row: &Row, entity: &Pos, value: &Pos, e: Val, v: Val) -> Option<Row> {
    bind_all(row, [(entity, e), (value, v)])
}

/// `row` extended with each position bound to its value, or `None` if a
/// bound position (or a free variable met twice) disagrees.
fn bind_all<const N: usize>(row: &Row, binds: [(&Pos, Val); N]) -> Option<Row> {
    let mut r = row.clone();
    for (p, x) in binds {
        match p {
            Pos::Any => {}
            Pos::Const(c) => {
                if *c != x {
                    return None;
                }
            }
            Pos::Bound(s) => {
                if r[*s].as_ref() != Some(&x) {
                    return None;
                }
            }
            // The first occurrence binds, a second one (same var in both
            // positions) must agree.
            Pos::Free(s) => match &r[*s] {
                Some(prev) if *prev != x => return None,
                Some(_) => {}
                None => r[*s] = Some(x),
            },
        }
    }
    Some(r)
}

fn key_of<'r>(row: &'r Row, shared: &[usize]) -> Vec<&'r Val> {
    shared
        .iter()
        .map(|&s| {
            row[s]
                .as_ref()
                .expect("invariant: shared join slots are bound on both sides")
        })
        .collect()
}

/// A bound value already checked against its attribute's value type.
fn value_of(v: &Val) -> Value {
    v.to_value()
        .expect("invariant: expand_row checked the value against the attribute's type")
}

/// `v` as an interval set, for an argument of `func`.
fn as_set(func: Func, v: &Val) -> Result<&IntervalSet, QueryError> {
    match v {
        Val::Intervals(_, s) => Ok(s),
        other => Err(QueryError::TypeMismatch(format!(
            "Apply {} needs interval sets, got {}",
            func.name(),
            other.kind().name()
        ))),
    }
}

fn not_an_entity(v: &Val) -> QueryError {
    QueryError::TypeMismatch(format!(
        "entity position needs an entity, got {}",
        v.kind().name()
    ))
}

/// A read of an entity undefined (at this canon) finds nothing, like a
/// `NodeSeek` of it, rather than failing.
fn absent_as_empty<T>(r: Result<T, CoreError>) -> Result<Option<T>, QueryError> {
    match r {
        Ok(x) => Ok(Some(x)),
        Err(CoreError::UnknownEntity(_)) => Ok(None),
        Err(e) => Err(QueryError::Core(e)),
    }
}

#[cfg(test)]
mod scan_meter_tests {
    use super::*;
    use kala_core::{AttrDef, Cardinality, Kind, Schema, WorkInterruptReason};
    use proptest::prelude::*;

    fn world(values: &[i64]) -> World {
        let mut world = World::in_memory(Schema::new().node_type("N").attribute(AttrDef::new(
            "score",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        )))
        .unwrap();
        let mut tx = world.begin();
        for &value in values {
            let entity = tx.create_entity("N", None).unwrap();
            tx.assert(entity, "score", Value::I64(value), Some(Tick(-1)))
                .unwrap();
        }
        tx.commit().unwrap();
        world
    }

    fn scan_plan() -> Plan {
        Plan {
            find: vec![Var("e".into()), Var("v".into())],
            params: Vec::new(),
            default_tick: TickSource::Const(Tick(0)),
            canon: CanonSource::Latest,
            root: Op::Expand {
                input: Box::new(Op::Unit),
                entity: Term::Var(Var("e".into())),
                attr: AttrRef::Named("score".into()),
                value: Term::Var(Var("v".into())),
                tick: TickSource::Default,
                access: Access::Scan,
            },
        }
    }

    #[test]
    fn one_scan_is_interrupted_before_finishing_its_key_walk() {
        let world = world(&(0..10_000).collect::<Vec<_>>());
        let limits = Limits {
            max_steps: 100,
            ..Limits::default()
        };
        let cancel = AtomicBool::new(false);
        let meter = WorkMeter::bounded(limits.max_steps, Some(&cancel));
        let result = execute_inner(
            &world,
            &world.at_canon(world.latest_tx()),
            &scan_plan(),
            &Params::new(),
            &limits,
            &cancel,
            &meter,
        );
        assert!(
            matches!(
                result,
                Err(QueryError::Core(CoreError::WorkInterrupted {
                    reason: WorkInterruptReason::Limit
                }))
            ),
            "scan must stop inside kala-core; got {result:?}"
        );
        assert!(
            (1..=101).contains(&meter.keys()),
            "scan examined {} keys",
            meter.keys()
        );
    }

    #[test]
    fn cancellation_interrupts_one_scan_before_all_keys_are_read() {
        let world = world(&(0..100_000).collect::<Vec<_>>());
        let cancel = AtomicBool::new(false);
        let meter = WorkMeter::bounded(u64::MAX, Some(&cancel));
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                execute_inner(
                    &world,
                    &world.at_canon(world.latest_tx()),
                    &scan_plan(),
                    &Params::new(),
                    &Limits::default(),
                    &cancel,
                    &meter,
                )
            });
            while meter.keys() < 100 && !reader.is_finished() {
                std::thread::yield_now();
            }
            cancel.store(true, AtomicOrdering::Relaxed);
            let result = reader.join().unwrap();
            assert!(
                matches!(
                    result,
                    Err(QueryError::Core(CoreError::WorkInterrupted {
                        reason: WorkInterruptReason::Stopped
                    }))
                ),
                "cancellation must land in the core scan, got {result:?}"
            );
            assert!(
                (100..100_000).contains(&meter.keys()),
                "scan completed {} keys",
                meter.keys()
            );
        });
    }

    #[test]
    fn repeated_empty_scans_share_the_query_id_budget() {
        // Every scan crosses 5,001 IDs, but nearly all prefix seeks miss:
        // only the first ID has facts, under a different attribute. Ten
        // per-row ticks must not each receive a fresh 10,000-ID allowance.
        let schema = Schema::new()
            .node_type("N")
            .attribute(AttrDef::new(
                "score",
                ValueType::I64,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(AttrDef::new(
                "tick",
                ValueType::I64,
                Kind::State,
                Cardinality::Many,
            ));
        let mut world = World::in_memory(schema).unwrap();
        let mut tx = world.begin();
        let seed = tx.create_entity("N", None).unwrap();
        for tick in 0..10 {
            tx.assert(seed, "tick", Value::I64(tick), Some(Tick(0)))
                .unwrap();
        }
        for _ in 0..5_000 {
            tx.create_entity("N", None).unwrap();
        }
        tx.commit().unwrap();
        let tick_var = Var("t".into());
        let mut plan = scan_plan();
        if let Op::Expand { input, tick, .. } = &mut plan.root {
            **input = Op::Expand {
                input: Box::new(Op::Unit),
                entity: Term::Const(Value::Ref(seed)),
                attr: AttrRef::Named("tick".into()),
                value: Term::Var(tick_var.clone()),
                tick: TickSource::Const(Tick(0)),
                access: Access::E,
            };
            *tick = TickSource::Var(tick_var);
        }
        let limits = Limits {
            max_steps: 10_000,
            ..Limits::default()
        };
        let cancel = AtomicBool::new(false);
        let meter = WorkMeter::bounded(limits.max_steps, Some(&cancel));
        let result = execute_inner(
            &world,
            &world.at_canon(world.latest_tx()),
            &plan,
            &Params::new(),
            &limits,
            &cancel,
            &meter,
        );
        assert!(
            matches!(
                result,
                Err(QueryError::Core(CoreError::WorkInterrupted {
                    reason: WorkInterruptReason::Limit
                })) | Err(QueryError::StepBudget { .. })
            ),
            "repeated empty scans escaped the query ID budget: {result:?}"
        );
    }

    proptest! {
        #[test]
        fn bounded_and_unbounded_execution_have_identical_rows_and_steps(
            values in prop::collection::vec(-10i64..11, 0..40),
            ticks in prop::sample::select(vec![i64::MIN, -2, -1, 0, i64::MAX]),
            main_index in any::<bool>(),
            default_tick in any::<bool>(),
        ) {
            let mut world = world(&values);
            let ids: Vec<_> = world.entities_of_type("N").unwrap().collect();
            let mut tx = world.begin();
            for (&id, &value) in ids.iter().zip(&values) {
                if value % 2 == 0 {
                    tx.retract(id, "score", Value::I64(value), Some(Tick(1))).unwrap();
                }
                if value < 0 {
                    tx.assert(id, "score", Value::I64(value), Some(Tick(i64::MAX))).unwrap();
                }
            }
            tx.commit().unwrap();
            let canon = world.latest_tx();
            if main_index { world.create_entity("N", None).unwrap(); }
            let view = world.at_canon(canon);
            let mut plan = scan_plan();
            plan.default_tick = if default_tick { TickSource::Default } else { TickSource::Const(Tick(ticks)) };
            let cancel = AtomicBool::new(false);
            let limits = Limits::default();
            let unbounded = WorkMeter::new();
            let bounded = WorkMeter::bounded(limits.max_steps, Some(&cancel));
            let (want, want_usage) = execute_inner(&world, &view, &plan, &Params::new(), &limits, &cancel, &unbounded).unwrap();
            let (got, got_usage) = execute_inner(&world, &view, &plan, &Params::new(), &limits, &cancel, &bounded).unwrap();
            prop_assert_eq!(got, want);
            prop_assert_eq!(got_usage.steps, want_usage.steps);
        }
    }
}
