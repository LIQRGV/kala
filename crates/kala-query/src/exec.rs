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
//! Limits are enforced as errors, never by truncating: every index read and
//! every row produced by any operator costs one step, and no operator may
//! hold more than `max_rows` rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::rc::Rc;

use kala_core::{
    Direction, EdgeId, EntityId, Error as CoreError, Tick, TxId, Value, ValueType, View, World,
};

use crate::ir::{Access, AttrRef, CanonSource, CmpOp, Op, Plan, Term, TickSource, Var};

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

/// Resource limits for one query. Exceeding either is an error
/// ([`QueryError::RowLimit`], [`QueryError::StepBudget`]), never a
/// truncated answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most rows any single operator (intermediate or final) may produce.
    pub max_rows: usize,
    /// The most steps the query may take: one per index read and one per
    /// row produced by any operator.
    pub max_steps: u64,
}

impl Default for Limits {
    /// 100 000 rows, 10 000 000 steps.
    fn default() -> Limits {
        Limits {
            max_rows: 100_000,
            max_steps: 10_000_000,
        }
    }
}

/// A query's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultSet {
    /// The result columns: the plan's `find` variables, in order.
    pub columns: Vec<Var>,
    /// The rows, one value per column, sorted ascending ([`Value`]'s order,
    /// lexicographic) with no duplicates.
    pub rows: Vec<Vec<Value>>,
    /// The resolved default tick (what [`TickSource::Default`] read at).
    pub tick: Tick,
    /// The canon the query read at.
    pub canon: TxId,
}

/// Why a query couldn't run.
#[derive(Debug)]
pub enum QueryError {
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
    /// A kala-core read failed (unknown attribute, unknown node type, ...).
    Core(CoreError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

/// Resolves `plan.canon` against `world` (a canon past the latest is the
/// latest) and runs the plan there with [`execute`].
pub fn run(
    world: &World,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
) -> Result<ResultSet, QueryError> {
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
    execute(&world.at_canon(canon.min(latest)), plan, params, limits)
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
    execute_on(view.world(), view, plan, params, limits)
}

fn execute_on(
    world: &World,
    view: &View,
    plan: &Plan,
    params: &Params,
    limits: &Limits,
) -> Result<ResultSet, QueryError> {
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
    let default_tick = match &plan.default_tick {
        TickSource::Default => view.latest_tick(),
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
    let (root, bound) = compile(&plan.root, world, params, default_tick, &mut slots)?;
    let mut find = Vec::with_capacity(plan.find.len());
    for v in &plan.find {
        match slots.get(v) {
            Some(s) if bound.contains(&s) => find.push(s),
            _ => {
                return Err(QueryError::Plan(format!(
                    "find variable ?{} is not bound by the plan",
                    v.0
                )));
            }
        }
    }
    let mut ex = Exec {
        world,
        view,
        width: slots.len(),
        budget: Budget {
            steps: 0,
            limits: *limits,
        },
        scans: HashMap::new(),
        type_counts: HashMap::new(),
    };
    let rows = ex.eval(&root)?;
    let mut out = BTreeSet::new();
    for row in rows {
        let r: Vec<Value> = find
            .iter()
            .map(|&s| {
                row[s]
                    .clone()
                    .expect("invariant: compile checked every find var is bound")
            })
            .collect();
        out.insert(r);
    }
    Ok(ResultSet {
        columns: plan.find.clone(),
        rows: out.into_iter().collect(),
        tick: default_tick,
        canon: view.canon(),
    })
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

/// `(entity, value)` pairs read from an index.
type Pairs = Vec<(EntityId, Value)>;

/// A row: one optional value per slot. Every row of one operator binds the
/// same slots (checked at compile time), so `None` means "not bound here".
type Row = Vec<Option<Value>>;

/// A pattern position after compile.
#[derive(Clone, Debug)]
enum Pos {
    /// A constant (a resolved param included), already type-checked.
    Const(Value),
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
}

/// One item of a plan's post-order walk.
enum Visit<'p> {
    /// `op`'s inputs are done (for a `Union`: its end).
    Op(&'p Op),
    /// A `Union` starts.
    UnionBegin,
    /// One `Union` input is done.
    UnionAdd,
}

/// `root`'s operators in post-order, children left to right, walked with an
/// explicit stack.
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
            | Op::Project { input, .. } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(input));
            }
            Op::Join { left, right } => {
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
        }
    }
    out
}

/// The number of operators on `root`'s longest root-to-leaf path, computed
/// without recursion.
fn plan_depth(root: &Op) -> usize {
    let mut deepest = 0;
    let mut todo = vec![(root, 1usize)];
    while let Some((op, d)) = todo.pop() {
        deepest = deepest.max(d);
        match op {
            Op::Unit => {}
            Op::NodeScan { input, .. }
            | Op::NodeSeek { input, .. }
            | Op::Expand { input, .. }
            | Op::Filter { input, .. }
            | Op::Project { input, .. } => todo.push((input, d + 1)),
            Op::Join { left, right } => {
                todo.push((left, d + 1));
                todo.push((right, d + 1));
            }
            Op::Union { inputs } => todo.extend(inputs.iter().map(|i| (i, d + 1))),
        }
    }
    deepest
}

/// Compiles `root` into a flat program, returning it and the slots its
/// rows bind. Errors come in the order a recursive walk (inputs first, left
/// to right) would find them.
fn compile(
    root: &Op,
    world: &World,
    params: &Params,
    default_tick: Tick,
    slots: &mut Slots,
) -> Result<(Vec<Step>, BTreeSet<usize>), QueryError> {
    let walk = post_order(root);
    let mut prog = Vec::with_capacity(walk.len());
    // The slots bound by each pending operator's output.
    let mut stack: Vec<BTreeSet<usize>> = Vec::new();
    // Per open `Union`: the slots its first input binds.
    let mut unions: Vec<Option<BTreeSet<usize>>> = Vec::new();
    for v in walk {
        let step = match v {
            Visit::UnionBegin => {
                unions.push(None);
                Step::UnionBegin
            }
            Visit::UnionAdd => {
                let b = pop(&mut stack);
                let first = unions
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
            Visit::Op(op) => {
                let (step, bound) = compile_op(
                    op,
                    world,
                    params,
                    default_tick,
                    slots,
                    &mut stack,
                    &mut unions,
                )?;
                stack.push(bound);
                step
            }
        };
        prog.push(step);
    }
    let bound = pop(&mut stack);
    debug_assert!(stack.is_empty() && unions.is_empty());
    Ok((prog, bound))
}

/// Pops one operator's bound slots.
fn pop<T>(stack: &mut Vec<T>) -> T {
    stack
        .pop()
        .expect("invariant: post-order puts every input before its operator")
}

/// Compiles one operator whose inputs are compiled (their bound slots on top
/// of `stack`, which this pops), returning its step and its bound slots.
fn compile_op(
    op: &Op,
    world: &World,
    params: &Params,
    default_tick: Tick,
    slots: &mut Slots,
    stack: &mut Vec<BTreeSet<usize>>,
    unions: &mut Vec<Option<BTreeSet<usize>>>,
) -> Result<(Step, BTreeSet<usize>), QueryError> {
    Ok(match op {
        Op::Unit => (Step::Unit, BTreeSet::new()),
        Op::NodeScan { var, node_type, .. } => {
            let mut bound = pop(stack);
            let slot = slots.slot(var);
            let was = !bound.insert(slot);
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
                            ":handle can only be read via handle, not {access:?}"
                        )));
                    }
                    (None, ValueType::Str)
                }
                AttrRef::Named(name) => {
                    if *access == Access::Handle {
                        return Err(QueryError::Plan(format!(
                            "handle access is only for :handle, not :{name}"
                        )));
                    }
                    let a = resolve_attr(world, name)?;
                    let t = a.value_type;
                    (Some(a), t)
                }
            };
            let e = entity_pos(entity, params, slots, &bound, "entity")?;
            let v = value_pos(value, value_type, params, slots, &bound)?;
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
            let left = operand_pos(&pred.left, params, slots, &bound)?;
            let right = operand_pos(&pred.right, params, slots, &bound)?;
            if let (Pos::Const(l), Pos::Const(r)) = (&left, &right) {
                // Constant comparisons are checked up front, like constants
                // elsewhere.
                compare(pred.op, l, r)?;
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
            let first = unions
                .pop()
                .expect("invariant: a Union's end follows its UnionBegin");
            (Step::UnionEnd, first.unwrap_or_default())
        }
    })
}

/// Adds the variables an `Expand` binds to `bound`.
fn bind_free(bound: &mut BTreeSet<usize>, e: &Pos, v: &Pos) {
    for p in [e, v] {
        if let Pos::Free(s) = p {
            bound.insert(*s);
        }
    }
}

fn resolve_attr(world: &World, name: &str) -> Result<Attr, QueryError> {
    if let Some(def) = world.attribute(name) {
        return Ok(Attr {
            name: name.to_owned(),
            value_type: def.value_type,
            inverse: false,
            indexed: def.indexed,
        });
    }
    match world
        .attributes()
        .find(|d| d.inverse.as_deref() == Some(name))
    {
        Some(def) => Ok(Attr {
            name: name.to_owned(),
            value_type: ValueType::Ref,
            inverse: true,
            indexed: def.indexed,
        }),
        None => Err(QueryError::Core(CoreError::UnknownAttribute(
            name.to_owned(),
        ))),
    }
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
            "handle access is only for :handle, not :{name}"
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
        Term::Const(c @ Value::Ref(_)) => Pos::Const(c.clone()),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "{what} position needs an entity, got {}",
                type_name(c.value_type())
            )));
        }
        Term::Param(name) => match param(params, name)? {
            c @ Value::Ref(_) => Pos::Const(c.clone()),
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
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(c) if c.value_type() == ty => Pos::Const(c.clone()),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "value position needs {}, got {}",
                type_name(ty),
                type_name(c.value_type())
            )));
        }
        Term::Param(name) => match param(params, name)? {
            c if c.value_type() == ty => Pos::Const(c.clone()),
            _ => {
                return Err(QueryError::ParamType {
                    name: name.clone(),
                    expected: type_name(ty).into(),
                });
            }
        },
    })
}

/// A filter operand: a constant, a param or a bound variable.
fn operand_pos(
    t: &Term,
    params: &Params,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    match t {
        Term::Const(c) => Ok(Pos::Const(c.clone())),
        Term::Param(name) => Ok(Pos::Const(param(params, name)?.clone())),
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

/// `l op r`. Equality across types is just false; ordering needs one type.
fn compare(op: CmpOp, l: &Value, r: &Value) -> Result<bool, QueryError> {
    if !matches!(op, CmpOp::Eq | CmpOp::Ne) && l.value_type() != r.value_type() {
        return Err(QueryError::TypeMismatch(format!(
            "can't order {} against {}",
            type_name(l.value_type()),
            type_name(r.value_type())
        )));
    }
    Ok(match op {
        CmpOp::Eq => l == r,
        CmpOp::Ne => l != r,
        CmpOp::Lt => l < r,
        CmpOp::Le => l <= r,
        CmpOp::Gt => l > r,
        CmpOp::Ge => l >= r,
    })
}

// ---------------------------------------------------------------- eval

struct Budget {
    steps: u64,
    limits: Limits,
}

impl Budget {
    fn step(&mut self, n: u64) -> Result<(), QueryError> {
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.limits.max_steps {
            return Err(QueryError::StepBudget {
                limit: self.limits.max_steps,
            });
        }
        Ok(())
    }

    /// Adds `row` to an operator's output set, charging one step.
    fn emit(&mut self, out: &mut BTreeSet<Row>, row: Row) -> Result<(), QueryError> {
        self.step(1)?;
        out.insert(row);
        if out.len() > self.limits.max_rows {
            return Err(QueryError::RowLimit {
                limit: self.limits.max_rows,
            });
        }
        Ok(())
    }
}

struct Exec<'a, 'w> {
    world: &'a World,
    view: &'a View<'w>,
    width: usize,
    budget: Budget,
    /// `attr_scan` results by attribute name, then tick, read once per query.
    /// Only scans at constant, parameter or default ticks are kept here; a
    /// per-row tick's scan lives only while its rows expand (see
    /// `Step::Expand`). Keyed by name first so a lookup needn't allocate.
    scans: HashMap<String, HashMap<Tick, Rc<Pairs>>>,
    /// Entities of each node type defined at the canon (a prefix of the
    /// type's sequence numbers, so a count is enough).
    type_counts: HashMap<u16, u64>,
}

impl Exec<'_, '_> {
    /// Runs a compiled program (see [`Step`]) and returns its root's rows.
    fn eval(&mut self, prog: &[Step]) -> Result<BTreeSet<Row>, QueryError> {
        // The row sets of operators whose consumer hasn't run yet (for an
        // open union: its accumulated output).
        let mut stack: Vec<BTreeSet<Row>> = Vec::new();
        for step in prog {
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
                    stack.push(out);
                }
                Step::UnionBegin => stack.push(BTreeSet::new()),
                Step::UnionAdd => {
                    let rows = pop(&mut stack);
                    let acc = stack
                        .last_mut()
                        .expect("invariant: UnionAdd follows its UnionBegin");
                    for row in rows {
                        self.budget.emit(acc, row)?;
                    }
                }
                // The accumulated union is already on top.
                Step::UnionEnd => {}
                unary => {
                    let rows = pop(&mut stack);
                    let out = self.eval_unary(unary, rows)?;
                    stack.push(out);
                }
            }
        }
        let out = pop(&mut stack);
        debug_assert!(stack.is_empty());
        Ok(out)
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
        let mut by_key: HashMap<Vec<&Value>, Vec<&Row>> = HashMap::new();
        for row in r {
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
                let all: Vec<EntityId> = self.view.entities_of_type(node_type)?.collect();
                for row in rows {
                    if *bound {
                        if let Some(Value::Ref(e)) = &row[*slot]
                            && all.binary_search(e).is_ok()
                        {
                            self.budget.emit(&mut out, row)?;
                        }
                        continue;
                    }
                    for &e in &all {
                        let mut r = row.clone();
                        r[*slot] = Some(Value::Ref(e));
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
                        Some(Value::Ref(e)) => *e,
                        Some(other) => return Err(not_an_entity(other)),
                        None => unreachable!("invariant: compile checked NodeSeek's entity"),
                    };
                    if !self.defined_at_canon(e)? {
                        continue;
                    }
                    if *bound {
                        if row[*slot] != Some(Value::Ref(e)) {
                            continue;
                        }
                    } else {
                        row[*slot] = Some(Value::Ref(e));
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
                        let all = self.scan_once(&attr.name, t)?;
                        for row in rows {
                            let pairs =
                                self.expand_row(&row, entity, attr, value, t, *access, Some(&all))?;
                            for (e, v) in pairs {
                                if let Some(r) = bind(&row, entity, value, Value::Ref(e), v) {
                                    self.budget.emit(&mut out, r)?;
                                }
                            }
                        }
                        // `all` is dropped here, before the next tick's scan.
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
                            if let Some(r) = bind(&row, entity, value, Value::Ref(e), v) {
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
                        (Some(Value::Ref(e)), _) => {
                            let e = *e;
                            if !self.defined_at_canon(e)? {
                                None
                            } else {
                                self.world
                                    .handle_of(e)
                                    .map(|h| (e, Value::Str(h.to_owned())))
                            }
                        }
                        (Some(other), _) => return Err(not_an_entity(other)),
                        (None, Some(Value::Str(h))) => match self.world.entity_by_handle(h) {
                            Some(e) if self.defined_at_canon(e)? => {
                                Some((e, Value::Str(h.clone())))
                            }
                            _ => None,
                        },
                        (None, Some(other)) => {
                            return Err(QueryError::TypeMismatch(format!(
                                "a handle must be a string, got {}",
                                type_name(other.value_type())
                            )));
                        }
                        (None, None) => unreachable!("invariant: compile checked handle access"),
                    };
                    if let Some((e, v)) = pair
                        && let Some(r) = bind(&row, entity, value, Value::Ref(e), v)
                    {
                        self.budget.emit(&mut out, r)?;
                    }
                }
            }
            Step::Filter { op, left, right } => {
                for row in rows {
                    let (Some(l), Some(r)) = (resolve(left, &row), resolve(right, &row)) else {
                        unreachable!("invariant: compile checked filter operands are bound")
                    };
                    if compare(*op, l, r)? {
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
            Step::Unit | Step::Join { .. } | Step::UnionBegin | Step::UnionAdd | Step::UnionEnd => {
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
            Some(Value::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        let v = match resolve(value, row) {
            Some(v) if v.value_type() != attr.value_type => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    type_name(v.value_type())
                )));
            }
            // NaN is never stored, so it matches nothing, whichever access.
            Some(Value::F64(x)) if x.is_nan() => return Ok(Vec::new()),
            other => other,
        };
        let name = attr.name.as_str();
        let pairs = match access {
            Access::E => {
                let e = e.expect("invariant: compile checked E has the entity bound");
                self.budget.step(1)?;
                if let (Some(v), false) = (v, attr.inverse) {
                    let holds = absent_as_empty(self.view.as_of_value(e, name, v, t))?;
                    if holds == Some(true) {
                        vec![(e, v.clone())]
                    } else {
                        Vec::new()
                    }
                } else if attr.inverse {
                    let ns = self.view.neighbors(e, name, Direction::Out, t);
                    absent_as_empty(ns)?
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(x, _)| (e, Value::Ref(x)))
                        .collect()
                } else {
                    absent_as_empty(self.view.as_of(e, name, t))?
                        .unwrap_or_default()
                        .into_iter()
                        .map(|x| (e, x))
                        .collect()
                }
            }
            Access::R | Access::V => {
                let v = v.expect("invariant: compile checked R/V have the value bound");
                self.budget.step(1)?;
                match v {
                    Value::Ref(y) => {
                        let ns = self.view.neighbors(*y, name, Direction::In, t);
                        absent_as_empty(ns)?
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(x, _)| (x, v.clone()))
                            .collect()
                    }
                    _ => self
                        .view
                        .lookup(name, v, t)?
                        .into_iter()
                        .map(|x| (x, v.clone()))
                        .collect(),
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
            Access::Handle => unreachable!("invariant: compile routes :handle to Step::Handle"),
        };
        self.budget.step(pairs.len() as u64)?;
        Ok(pairs)
    }

    /// `View::attr_scan(attr, t)`, read once per query and cached for the
    /// rest of it (for constant, parameter and default ticks: one scan each).
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
    fn scan_once(&mut self, attr: &str, t: Tick) -> Result<Rc<Pairs>, QueryError> {
        if let Some(s) = self.scans.get(attr).and_then(|m| m.get(&t)) {
            return Ok(Rc::clone(s));
        }
        self.read_scan(attr, t)
    }

    /// Reads `View::attr_scan(attr, t)`, charging one step plus one per pair.
    fn read_scan(&mut self, attr: &str, t: Tick) -> Result<Rc<Pairs>, QueryError> {
        self.budget.step(1)?;
        let pairs = Rc::new(self.view.attr_scan(attr, t)?);
        self.budget.step(pairs.len() as u64)?;
        Ok(pairs)
    }

    /// Whether `id` (an entity or an edge) is defined at the view's canon.
    fn defined_at_canon(&mut self, id: EntityId) -> Result<bool, QueryError> {
        if id.is_edge() {
            return Ok(id
                .as_edge()
                .is_some_and(|edge: EdgeId| self.view.edge(edge).is_some()));
        }
        let ty = id.node_type();
        let count = match self.type_counts.get(&ty.0) {
            Some(&n) => n,
            None => {
                let Some(name) = self.world.node_types().nth(usize::from(ty.0)) else {
                    return Ok(false);
                };
                self.budget.step(1)?;
                let n = self.view.entities_of_type(name)?.count() as u64;
                self.type_counts.insert(ty.0, n);
                n
            }
        };
        // A canon's entities of a type are a prefix of its sequences.
        Ok(id.seq() >= 1 && id.seq() <= count)
    }
}

/// The tick in slot `s` of `row` (a bound tick variable).
fn tick_of(row: &Row, s: usize) -> Result<Tick, QueryError> {
    match &row[s] {
        Some(Value::I64(n)) => Ok(Tick(*n)),
        Some(other) => Err(QueryError::TypeMismatch(format!(
            "a tick must be i64, got {}",
            type_name(other.value_type())
        ))),
        None => unreachable!("invariant: compile checked the tick var"),
    }
}

/// A position's value in `row`: constants and bound vars; `None` for free
/// vars and `_`.
fn resolve<'r>(p: &'r Pos, row: &'r Row) -> Option<&'r Value> {
    match p {
        Pos::Const(v) => Some(v),
        Pos::Bound(s) => row[*s].as_ref(),
        Pos::Free(_) | Pos::Any => None,
    }
}

/// `row` extended with `entity = e` and `value = v`, or `None` if a bound
/// position (or the same free variable twice) disagrees.
fn bind(row: &Row, entity: &Pos, value: &Pos, e: Value, v: Value) -> Option<Row> {
    let mut r = row.clone();
    for (p, x) in [(entity, e), (value, v)] {
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

fn key_of<'r>(row: &'r Row, shared: &[usize]) -> Vec<&'r Value> {
    shared
        .iter()
        .map(|&s| {
            row[s]
                .as_ref()
                .expect("invariant: shared join slots are bound on both sides")
        })
        .collect()
}

fn not_an_entity(v: &Value) -> QueryError {
    QueryError::TypeMismatch(format!(
        "entity position needs an entity, got {}",
        type_name(v.value_type())
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
