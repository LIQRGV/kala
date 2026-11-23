//! The compiled form of a plan: a flat program of steps, run as a stack
//! machine.

use std::collections::{BTreeMap, BTreeSet};

use kala_core::{Tick, ValueType};

use crate::ir::{Access, AggFn, CmpOp, Func, Var};

use super::val::Val;

/// Variable name -> row slot.
#[derive(Default)]
pub(super) struct Slots(pub(super) BTreeMap<Var, usize>);

impl Slots {
    pub(super) fn get(&self, v: &Var) -> Option<usize> {
        self.0.get(v).copied()
    }
    pub(super) fn slot(&mut self, v: &Var) -> usize {
        let n = self.0.len();
        *self.0.entry(v.clone()).or_insert(n)
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

/// A pattern position after compile.
#[derive(Clone, Debug)]
pub(super) enum Pos {
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
    pub(super) fn is_bound(&self) -> bool {
        matches!(self, Pos::Const(_) | Pos::Bound(_))
    }
}

/// A compiled tick source.
#[derive(Clone, Copy, Debug)]
pub(super) enum TickPos {
    Const(Tick),
    Var(usize),
}

/// A resolved named attribute.
#[derive(Clone, Debug)]
pub(super) struct Attr {
    /// The name as written (an inverse name stays an inverse name).
    pub(super) name: String,
    /// The value type of the name's values (`Ref` for an inverse name).
    pub(super) value_type: ValueType,
    /// Whether `name` is an inverse name (`child_of`).
    pub(super) inverse: bool,
    /// Whether values of the base attribute are in the `V` index.
    pub(super) indexed: bool,
    /// The base attribute's name (`parent_of` for `child_of`; `name` itself
    /// for a forward name).
    pub(super) base: String,
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
pub(super) enum Step {
    Row(crate::RowAction<usize>),
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
pub(super) struct AggStep {
    pub(super) func: AggFn,
    pub(super) var: usize,
    pub(super) name: String,
    pub(super) out: usize,
}

/// One semi-naive variant of a recursive rule: the rule's steps (from
/// `start` to its `RuleEnd`) run with recursive read `occ` (of relation
/// `rel`) reading only the previous round's delta.
#[derive(Clone, Copy, Debug)]
pub(super) struct Variant {
    pub(super) start: usize,
    pub(super) occ: usize,
    pub(super) rel: usize,
}

/// A compiled plan: the flat program, the slots its root's rows bind, and
/// the columns (slots) of every relation, indexed by relation id.
pub(super) struct Program {
    pub(super) steps: Vec<Step>,
    pub(super) bound: BTreeSet<usize>,
    pub(super) rels: Vec<Vec<usize>>,
}

/// Pops one operator's bound slots (or row set).
pub(super) fn pop<T>(stack: &mut Vec<T>) -> T {
    stack
        .pop()
        .expect("invariant: post-order puts every input before its operator")
}
