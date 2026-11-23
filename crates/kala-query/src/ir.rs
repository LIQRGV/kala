//! The shared, language-neutral IR every query front-end compiles to, and its
//! stable text form ([`Plan::explain`]).
//!
//! A plan is a tree of [`Op`]s over bindings from [`Var`] to values. Set
//! operators preserve their original semantics; `Row` adds nullable reads and
//! expressions, and a final `Output` selects bags, ordering and slicing. Every read operator
//! carries its own tick ([`TickSource`]); [`Plan::default_tick`] is what
//! `TickSource::Default` resolves to. Language semantics (set vs bag, NULLs,
//! relationship reuse) are decided by the front-end, never here.

use kala_core::{Tick, Value};

mod explain;

pub use explain::{MAX_EXPLAIN_BYTES, precision_text};

/// A query variable, named without its leading `?` (`Var("a".into())` is `?a`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Var(pub String);

/// One position of a pattern or predicate.
#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    /// A variable: a constraint if already bound, else bound by the read.
    Var(Var),
    /// A query parameter, named without its `?`/`$` prefix.
    Param(String),
    /// A constant value.
    Const(Value),
    /// A constant op (`:assert` / `:retract`), compared with an op a
    /// history read bound. Never a stored value.
    Op(kala_core::Op),
    /// A history precision constant, never a stored value.
    Precision(kala_core::Precision),
    /// `_`: matches anything, binds nothing.
    Any,
}

/// Where a read operator's tick comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TickSource {
    /// The plan's default tick (for [`Plan::default_tick`] itself: the
    /// view's latest tick).
    Default,
    /// A constant tick.
    Const(Tick),
    /// A query parameter (an `I64`).
    Param(String),
    /// A variable bound earlier in the query (an `I64`). Never valid as a
    /// plan's default tick.
    Var(Var),
}

/// Which canon (`tx <= N`) a plan reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonSource {
    /// The latest committed canon.
    Latest,
    /// A constant `tx` number.
    Const(u64),
    /// A query parameter (a non-negative `I64`).
    Param(String),
}

/// The attribute an [`Op::Expand`] reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrRef {
    /// A schema attribute by name (inverse names allowed).
    Named(String),
    /// The built-in `:kala_handle` pseudo-attribute (entity <-> handle string).
    Handle,
}

/// Which index serves an [`Op::Expand`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Forward index: needs the entity bound.
    E,
    /// Reverse index: needs a ref value bound.
    R,
    /// Value index: needs the value bound (indexed attribute).
    V,
    /// Full attribute scan: needs nothing bound.
    Scan,
    /// Handle table: only for [`AttrRef::Handle`].
    Handle,
}

/// A comparison operator in a [`Predicate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CmpOp {
    /// The operator's printed symbol (`=`, `!=`, `<`, `<=`, `>`, `>=`).
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// A row filter `left op right`.
#[derive(Clone, Debug, PartialEq)]
pub struct Predicate {
    /// The comparison.
    pub op: CmpOp,
    /// Left operand.
    pub left: Term,
    /// Right operand.
    pub right: Term,
}

/// The op position of an [`Op::History`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpTerm {
    /// `_`: any op, binds nothing.
    Any,
    /// A variable: a constraint if already bound, else bound to the op.
    Var(Var),
    /// Only change points with this op.
    Const(kala_core::Op),
}

/// A function over row values, applied by [`Op::Apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Func {
    /// Interval-set intersection (`intersect`).
    Intersect,
    /// Interval-set union (`union`).
    Union,
    /// Interval-set difference (`difference`).
    Difference,
    /// Whether an interval set is non-empty (`not-empty`, a filter).
    NotEmpty,
    /// Whether an interval set contains a tick (`contains`, a filter).
    Contains,
}

impl Func {
    /// The function's printed name.
    pub fn name(self) -> &'static str {
        match self {
            Func::Intersect => "intersect",
            Func::Union => "union",
            Func::Difference => "difference",
            Func::NotEmpty => "not-empty",
            Func::Contains => "contains",
        }
    }
}

/// An aggregate function of [`Op::Aggregate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AggFn {
    /// The number of rows in the group (`count`).
    Count,
    /// The number of distinct values in the group (`count-distinct`).
    CountDistinct,
    /// The least value (`min`).
    Min,
    /// The greatest value (`max`).
    Max,
    /// The sum of numbers (`sum`).
    Sum,
}

impl AggFn {
    /// The function's printed name.
    pub fn name(self) -> &'static str {
        match self {
            AggFn::Count => "count",
            AggFn::CountDistinct => "count-distinct",
            AggFn::Min => "min",
            AggFn::Max => "max",
            AggFn::Sum => "sum",
        }
    }
}

/// One aggregate of an [`Op::Aggregate`]: `func` over `var`, bound to `out`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agg {
    /// The aggregate function.
    pub func: AggFn,
    /// The aggregated variable.
    pub var: Var,
    /// The variable bound to the result.
    pub out: Var,
}

/// A relation defined inside an [`Op::Fixpoint`]: its rows are the union of
/// its rules' rows, each rule binding `vars` (in order: [`Op::Rel`]'s
/// arguments are matched to them by position).
#[derive(Clone, Debug, PartialEq)]
pub struct RelDef {
    /// The relation's name, unique within its `Fixpoint`.
    pub name: String,
    /// The relation's columns.
    pub vars: Vec<Var>,
    /// The rule bodies; each may read any relation of the `Fixpoint` with
    /// [`Op::Rel`].
    pub rules: Vec<Op>,
}

impl RelDef {
    /// Whether the two definitions have the same name, columns and number
    /// of rules (the rules themselves are not compared).
    fn same_shape(&self, other: &RelDef) -> bool {
        self.name == other.name && self.vars == other.vars && self.rules.len() == other.rules.len()
    }
}

/// A plan operator. Every operator produces a set of rows.
///
/// `Clone`, `PartialEq` and `Drop` walk the tree with an explicit stack, so
/// a deep plan costs heap, not native stack (`Debug` is derived and
/// recurses).
#[derive(Debug)]
pub enum Op {
    /// Nullable, row-preserving read or expression.
    Row {
        input: Box<Op>,
        action: crate::RowAction,
    },
    /// Final bag/set projection, ordering and slicing.
    Output {
        input: Box<Op>,
        spec: crate::OutputSpec,
    },
    /// Exactly one empty row.
    Unit,
    /// For each input row, bind `var` to every entity of `node_type` defined
    /// at the canon.
    NodeScan {
        /// Input rows.
        input: Box<Op>,
        /// The variable to bind.
        var: Var,
        /// The node type name (`CHARACTER`).
        node_type: String,
    },
    /// For each input row, bind `var` to one known entity (constant, param
    /// or bound var); an entity undefined at the canon gives 0 rows.
    NodeSeek {
        /// Input rows.
        input: Box<Op>,
        /// The variable to bind.
        var: Var,
        /// The entity.
        entity: Term,
    },
    /// For each input row, read "(entity, attr, value) holds at tick" through
    /// the index named by `access`, binding unbound vars.
    Expand {
        /// Input rows.
        input: Box<Op>,
        /// Entity position.
        entity: Term,
        /// The attribute.
        attr: AttrRef,
        /// Value position.
        value: Term,
        /// The read's tick (ignored for [`AttrRef::Handle`]).
        tick: TickSource,
        /// Which index serves the read; must be feasible for the row.
        access: Access,
    },
    /// Keep the input rows satisfying `pred`.
    Filter {
        /// Input rows.
        input: Box<Op>,
        /// The predicate.
        pred: Predicate,
    },
    /// Natural join on shared vars; Cartesian product if there are none.
    Join {
        /// Left input.
        left: Box<Op>,
        /// Right input.
        right: Box<Op>,
    },
    /// Keep only `vars` (as a set).
    Project {
        /// Input rows.
        input: Box<Op>,
        /// The variables kept.
        vars: Vec<Var>,
    },
    /// Set union; every input must bind the same vars.
    Union {
        /// The inputs.
        inputs: Vec<Op>,
    },
    /// For each input row, bind the change points of `(entity, attr,
    /// value)`: every tick where a fact changed, at any tick, at the
    /// plan's canon.
    History {
        /// Input rows.
        input: Box<Op>,
        /// Entity position.
        entity: Term,
        /// The attribute.
        attr: String,
        /// Value position.
        value: Term,
        /// The change point's tick (an `I64`).
        tick: Term,
        /// The change point's op.
        op: OpTerm,
        /// The precision position (Any, Var, or Precision).
        precision: Term,
    },
    /// For each input row and each matching `(entity, value)`, bind `out` to
    /// the interval set where `(entity, attr, value)` holds.
    During {
        /// Input rows.
        input: Box<Op>,
        /// Entity position.
        entity: Term,
        /// The attribute.
        attr: String,
        /// Value position.
        value: Term,
        /// The variable bound to the interval set.
        out: Var,
    },
    /// For each input row, apply `func` to `args`: bind the result to `out`,
    /// or with no `out` keep the rows where it is true (a filter).
    Apply {
        /// Input rows.
        input: Box<Op>,
        /// The function.
        func: Func,
        /// The arguments.
        args: Vec<Term>,
        /// The variable bound to the result, if any.
        out: Option<Var>,
    },
    /// Correlated anti-join: keep the input rows for which `body`,
    /// evaluated from that row, produces no rows. Variables only the body
    /// binds are local to it.
    ///
    /// Every `Unit` inside `body` stands for the input row (the executor
    /// runs the body once from all input rows and matches its rows back on
    /// the input's variables), so the body must keep every input variable
    /// (no `Project` dropping one). The body may read relations of
    /// enclosing `Fixpoint`s, but not one still being computed (a relation
    /// of the `Fixpoint` whose rules contain the `Not`), and may not
    /// contain a `Fixpoint`: both are plan errors.
    Not {
        /// Input rows.
        input: Box<Op>,
        /// The negated subplan, starting from the input row.
        body: Box<Op>,
    },
    /// Anti-join of two independent subplans: the `left` rows matching no
    /// `right` row on their shared vars.
    Difference {
        /// The rows kept.
        left: Box<Op>,
        /// The rows removed.
        right: Box<Op>,
    },
    /// Semi-join of two independent subplans: the `left` rows matching some
    /// `right` row on their shared vars; binds only `left`'s vars.
    Intersect {
        /// The rows kept.
        left: Box<Op>,
        /// The rows required.
        right: Box<Op>,
    },
    /// Groups the input rows by `group` and binds each of `aggs` per group.
    Aggregate {
        /// Input rows.
        input: Box<Op>,
        /// The grouping variables.
        group: Vec<Var>,
        /// The aggregates.
        aggs: Vec<Agg>,
    },
    /// Defines the (possibly mutually recursive) relations `rels` and runs
    /// `body`, which reads them with [`Op::Rel`].
    Fixpoint {
        /// The relations.
        rels: Vec<RelDef>,
        /// The plan reading them.
        body: Box<Op>,
    },
    /// For each input row, join the rows of the relation `name` (defined by
    /// an enclosing [`Op::Fixpoint`]), matched to `args` by position.
    Rel {
        /// Input rows.
        input: Box<Op>,
        /// The relation's name.
        name: String,
        /// One term per relation column.
        args: Vec<Term>,
    },
}

impl Op {
    /// The operator's variant name (`Expand`, `Fixpoint`, ...), as printed
    /// by [`Plan::explain`] and named in errors.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Row { .. } => "Row",
            Op::Output { .. } => "Output",
            Op::Unit => "Unit",
            Op::NodeScan { .. } => "NodeScan",
            Op::NodeSeek { .. } => "NodeSeek",
            Op::Expand { .. } => "Expand",
            Op::Filter { .. } => "Filter",
            Op::Join { .. } => "Join",
            Op::Project { .. } => "Project",
            Op::Union { .. } => "Union",
            Op::History { .. } => "History",
            Op::During { .. } => "During",
            Op::Apply { .. } => "Apply",
            Op::Not { .. } => "Not",
            Op::Difference { .. } => "Difference",
            Op::Intersect { .. } => "Intersect",
            Op::Aggregate { .. } => "Aggregate",
            Op::Fixpoint { .. } => "Fixpoint",
            Op::Rel { .. } => "Rel",
        }
    }

    /// Appends the operator's direct children to `out`, in one fixed order:
    /// inputs (left before right, a `Not`'s input before its body), a
    /// `Union`'s inputs in order, a `Fixpoint`'s body then every relation's
    /// rules in order. [`Op::rebuild`] consumes children in this order.
    pub(crate) fn children<'p>(&'p self, out: &mut Vec<&'p Op>) {
        match self {
            Op::Unit => {}
            Op::Row { input, .. }
            | Op::Output { input, .. }
            | Op::NodeScan { input, .. }
            | Op::NodeSeek { input, .. }
            | Op::Expand { input, .. }
            | Op::Filter { input, .. }
            | Op::Project { input, .. }
            | Op::History { input, .. }
            | Op::During { input, .. }
            | Op::Apply { input, .. }
            | Op::Aggregate { input, .. }
            | Op::Rel { input, .. } => out.push(input),
            Op::Join { left, right }
            | Op::Not {
                input: left,
                body: right,
            }
            | Op::Difference { left, right }
            | Op::Intersect { left, right } => {
                out.push(left);
                out.push(right);
            }
            Op::Union { inputs } => out.extend(inputs),
            Op::Fixpoint { rels, body } => {
                out.push(body);
                for rel in rels {
                    out.extend(&rel.rules);
                }
            }
        }
    }

    /// A copy of this operator's own fields with the given children (taken
    /// from `kids` in [`Op::children`] order, exactly as many as it has).
    fn rebuild(&self, kids: &mut impl Iterator<Item = Op>) -> Op {
        let mut next = || {
            kids.next()
                .expect("invariant: rebuild gets one child per Op::children entry")
        };
        let mut input = || Box::new(next());
        match self {
            Op::Row { action, .. } => Op::Row {
                input: input(),
                action: action.clone(),
            },
            Op::Output { spec, .. } => Op::Output {
                input: input(),
                spec: spec.clone(),
            },
            Op::Unit => Op::Unit,
            Op::NodeScan { var, node_type, .. } => Op::NodeScan {
                input: input(),
                var: var.clone(),
                node_type: node_type.clone(),
            },
            Op::NodeSeek { var, entity, .. } => Op::NodeSeek {
                input: input(),
                var: var.clone(),
                entity: entity.clone(),
            },
            Op::Expand {
                entity,
                attr,
                value,
                tick,
                access,
                ..
            } => Op::Expand {
                input: input(),
                entity: entity.clone(),
                attr: attr.clone(),
                value: value.clone(),
                tick: tick.clone(),
                access: *access,
            },
            Op::Filter { pred, .. } => Op::Filter {
                input: input(),
                pred: pred.clone(),
            },
            Op::Join { .. } => Op::Join {
                left: input(),
                right: input(),
            },
            Op::Project { vars, .. } => Op::Project {
                input: input(),
                vars: vars.clone(),
            },
            Op::Union { inputs } => Op::Union {
                inputs: inputs.iter().map(|_| *input()).collect(),
            },
            Op::History {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
                ..
            } => Op::History {
                input: input(),
                entity: entity.clone(),
                attr: attr.clone(),
                value: value.clone(),
                tick: tick.clone(),
                op: op.clone(),
                precision: precision.clone(),
            },
            Op::During {
                entity,
                attr,
                value,
                out,
                ..
            } => Op::During {
                input: input(),
                entity: entity.clone(),
                attr: attr.clone(),
                value: value.clone(),
                out: out.clone(),
            },
            Op::Apply {
                func, args, out, ..
            } => Op::Apply {
                input: input(),
                func: *func,
                args: args.clone(),
                out: out.clone(),
            },
            Op::Not { .. } => Op::Not {
                input: input(),
                body: input(),
            },
            Op::Difference { .. } => Op::Difference {
                left: input(),
                right: input(),
            },
            Op::Intersect { .. } => Op::Intersect {
                left: input(),
                right: input(),
            },
            Op::Aggregate { group, aggs, .. } => Op::Aggregate {
                input: input(),
                group: group.clone(),
                aggs: aggs.clone(),
            },
            Op::Fixpoint { rels, .. } => {
                let body = input();
                let rels = rels
                    .iter()
                    .map(|r| RelDef {
                        name: r.name.clone(),
                        vars: r.vars.clone(),
                        rules: r.rules.iter().map(|_| *input()).collect(),
                    })
                    .collect();
                Op::Fixpoint { rels, body }
            }
            Op::Rel { name, args, .. } => Op::Rel {
                input: input(),
                name: name.clone(),
                args: args.clone(),
            },
        }
    }

    /// Whether the two operators are the same variant with equal own fields
    /// (children not compared, but their arrangement is: a `Union`'s input
    /// count, each `Fixpoint` relation's rule count).
    fn shallow_eq(&self, other: &Op) -> bool {
        match (self, other) {
            (Op::Row { action: a, .. }, Op::Row { action: b, .. }) => a == b,
            (Op::Output { spec: a, .. }, Op::Output { spec: b, .. }) => a == b,
            (Op::Unit, Op::Unit)
            | (Op::Join { .. }, Op::Join { .. })
            | (Op::Not { .. }, Op::Not { .. })
            | (Op::Difference { .. }, Op::Difference { .. })
            | (Op::Intersect { .. }, Op::Intersect { .. }) => true,
            (
                Op::NodeScan {
                    var: v1,
                    node_type: n1,
                    ..
                },
                Op::NodeScan {
                    var: v2,
                    node_type: n2,
                    ..
                },
            ) => v1 == v2 && n1 == n2,
            (
                Op::NodeSeek {
                    var: v1,
                    entity: e1,
                    ..
                },
                Op::NodeSeek {
                    var: v2,
                    entity: e2,
                    ..
                },
            ) => v1 == v2 && e1 == e2,
            (
                Op::Expand {
                    entity: e1,
                    attr: a1,
                    value: v1,
                    tick: t1,
                    access: x1,
                    ..
                },
                Op::Expand {
                    entity: e2,
                    attr: a2,
                    value: v2,
                    tick: t2,
                    access: x2,
                    ..
                },
            ) => e1 == e2 && a1 == a2 && v1 == v2 && t1 == t2 && x1 == x2,
            (Op::Filter { pred: p1, .. }, Op::Filter { pred: p2, .. }) => p1 == p2,
            (Op::Project { vars: v1, .. }, Op::Project { vars: v2, .. }) => v1 == v2,
            (Op::Union { inputs: i1 }, Op::Union { inputs: i2 }) => i1.len() == i2.len(),
            (
                Op::History {
                    entity: e1,
                    attr: a1,
                    value: v1,
                    tick: t1,
                    op: o1,
                    precision: p1,
                    ..
                },
                Op::History {
                    entity: e2,
                    attr: a2,
                    value: v2,
                    tick: t2,
                    op: o2,
                    precision: p2,
                    ..
                },
            ) => e1 == e2 && a1 == a2 && v1 == v2 && t1 == t2 && o1 == o2 && p1 == p2,
            (
                Op::During {
                    entity: e1,
                    attr: a1,
                    value: v1,
                    out: o1,
                    ..
                },
                Op::During {
                    entity: e2,
                    attr: a2,
                    value: v2,
                    out: o2,
                    ..
                },
            ) => e1 == e2 && a1 == a2 && v1 == v2 && o1 == o2,
            (
                Op::Apply {
                    func: f1,
                    args: a1,
                    out: o1,
                    ..
                },
                Op::Apply {
                    func: f2,
                    args: a2,
                    out: o2,
                    ..
                },
            ) => f1 == f2 && a1 == a2 && o1 == o2,
            (
                Op::Aggregate {
                    group: g1,
                    aggs: a1,
                    ..
                },
                Op::Aggregate {
                    group: g2,
                    aggs: a2,
                    ..
                },
            ) => g1 == g2 && a1 == a2,
            (Op::Fixpoint { rels: r1, .. }, Op::Fixpoint { rels: r2, .. }) => {
                r1.len() == r2.len() && r1.iter().zip(r2).all(|(a, b)| a.same_shape(b))
            }
            (
                Op::Rel {
                    name: n1, args: a1, ..
                },
                Op::Rel {
                    name: n2, args: a2, ..
                },
            ) => n1 == n2 && a1 == a2,
            _ => false,
        }
    }
}

impl Clone for Op {
    /// Clones the tree bottom-up with an explicit stack (each operator is
    /// rebuilt once its children's clones are done), so a deep plan can't
    /// overflow the native stack.
    fn clone(&self) -> Op {
        enum Todo<'p> {
            Enter(&'p Op),
            /// The operator and its child count: its children's clones are
            /// the top of `done`.
            Leave(&'p Op, usize),
        }
        let mut todo = vec![Todo::Enter(self)];
        let mut done: Vec<Op> = Vec::new();
        let mut kids = Vec::new();
        while let Some(t) = todo.pop() {
            match t {
                Todo::Enter(op) => {
                    kids.clear();
                    op.children(&mut kids);
                    todo.push(Todo::Leave(op, kids.len()));
                    // Reversed: children are entered, and finished, in order.
                    todo.extend(kids.iter().rev().map(|k| Todo::Enter(k)));
                }
                Todo::Leave(op, n) => {
                    let start = done.len() - n;
                    let copy = op.rebuild(&mut done.drain(start..));
                    done.push(copy);
                }
            }
        }
        done.pop()
            .expect("invariant: the root's clone is the last one finished")
    }
}

impl PartialEq for Op {
    /// Structural equality, compared pairwise with an explicit stack.
    fn eq(&self, other: &Op) -> bool {
        let mut todo = vec![(self, other)];
        let (mut ka, mut kb) = (Vec::new(), Vec::new());
        while let Some((a, b)) = todo.pop() {
            if !a.shallow_eq(b) {
                return false;
            }
            ka.clear();
            kb.clear();
            a.children(&mut ka);
            b.children(&mut kb);
            // Equal own fields imply equal child counts; checked anyway.
            if ka.len() != kb.len() {
                return false;
            }
            todo.extend(ka.iter().copied().zip(kb.iter().copied()));
        }
        true
    }
}

/// A complete query plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The result columns, in order.
    pub find: Vec<Var>,
    /// The parameter names the plan expects, in declaration order.
    pub params: Vec<String>,
    /// What [`TickSource::Default`] resolves to: `Default` (the view's latest
    /// tick), `Const` or `Param`; never `Var`.
    pub default_tick: TickSource,
    /// Which canon the plan reads.
    pub canon: CanonSource,
    /// The operator tree.
    pub root: Op,
}

impl Plan {
    /// The name of every schema attribute the plan reads (`Expand`,
    /// `History`, `During`, in every subplan and every `Fixpoint` rule),
    /// possibly repeated. The `:kala_handle` pseudo-attribute is not a schema
    /// attribute and is left out.
    ///
    /// For callers that refuse plans by attribute. Every attribute
    /// reference is a constant name today; a non-constant one added to
    /// [`AttrRef`] must be surfaced here (the match below is exhaustive for
    /// that reason) so such callers can fail closed.
    pub fn attribute_names(&self) -> Vec<&str> {
        let mut out = Vec::new();
        let mut stack = vec![&self.root];
        while let Some(op) = stack.pop() {
            match op {
                Op::Row { action, .. } => action.attributes(&mut out),
                Op::Output { .. } => {}
                Op::Expand { attr, .. } => match attr {
                    AttrRef::Named(name) => out.push(name.as_str()),
                    AttrRef::Handle => {}
                },
                Op::History { attr, .. } | Op::During { attr, .. } => out.push(attr.as_str()),
                // Listed, not `_`: a new operator that names an attribute
                // must be added above, or the compiler asks for a decision.
                Op::Unit
                | Op::NodeScan { .. }
                | Op::NodeSeek { .. }
                | Op::Filter { .. }
                | Op::Join { .. }
                | Op::Project { .. }
                | Op::Union { .. }
                | Op::Apply { .. }
                | Op::Not { .. }
                | Op::Difference { .. }
                | Op::Intersect { .. }
                | Op::Aggregate { .. }
                | Op::Fixpoint { .. }
                | Op::Rel { .. } => {}
            }
            op.children(&mut stack);
        }
        out
    }
}

impl Drop for Op {
    /// Drops the tree with an explicit stack. The derived drop would recurse
    /// once per nesting level and overflow the native stack on a deep plan
    /// (a long left-deep `Expand` chain); here each operator's children are
    /// detached (replaced by `Unit`) before it is dropped, so every drop that
    /// actually runs is shallow.
    fn drop(&mut self) {
        let mut todo = Vec::new();
        detach_children(self, &mut todo);
        while let Some(mut op) = todo.pop() {
            detach_children(&mut op, &mut todo);
            // `op` now has only `Unit` children: dropping it doesn't recurse.
        }
    }
}

/// Moves `op`'s non-`Unit` children into `todo`, leaving `Unit` in their
/// place. Skipping `Unit`s keeps dropping a leaf-only operator allocation-free.
fn detach_children(op: &mut Op, todo: &mut Vec<Op>) {
    let mut take = |child: &mut Op| {
        if !matches!(child, Op::Unit) {
            todo.push(std::mem::replace(child, Op::Unit));
        }
    };
    match op {
        Op::Unit => {}
        Op::Row { input, .. }
        | Op::Output { input, .. }
        | Op::NodeScan { input, .. }
        | Op::NodeSeek { input, .. }
        | Op::Expand { input, .. }
        | Op::Filter { input, .. }
        | Op::Project { input, .. }
        | Op::History { input, .. }
        | Op::During { input, .. }
        | Op::Apply { input, .. }
        | Op::Aggregate { input, .. }
        | Op::Rel { input, .. } => take(input),
        Op::Join { left, right }
        | Op::Not {
            input: left,
            body: right,
        }
        | Op::Difference { left, right }
        | Op::Intersect { left, right } => {
            take(left);
            take(right);
        }
        Op::Union { inputs } => todo.append(inputs),
        Op::Fixpoint { rels, body } => {
            take(body);
            for rel in rels {
                todo.append(&mut rel.rules);
            }
        }
    }
}
