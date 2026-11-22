//! The shared, language-neutral IR every query front-end compiles to, and its
//! stable text form ([`Plan::explain`]).
//!
//! A plan is a tree of [`Op`]s producing **sets** of rows (bindings from
//! [`Var`] to [`Value`], entities as [`Value::Ref`]). Every read operator
//! carries its own tick ([`TickSource`]); [`Plan::default_tick`] is what
//! `TickSource::Default` resolves to. Language semantics (set vs bag, NULLs,
//! relationship reuse) are decided by the front-end, never here.

use std::fmt::Write as _;

use kala_core::{Tick, Value, World};

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
    /// The built-in `:handle` pseudo-attribute (entity <-> handle string).
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

/// A plan operator. Every operator produces a set of rows.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
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
    /// Renders the plan as stable text: a header line
    /// (`Find ?a  in $who  at -210  canon 41`), then one line per operator,
    /// each child indented two spaces more than its parent. Every line ends
    /// with `\n`. Entity constants are shown with [`World::format_id`]; an
    /// ID unknown to `world` is still printed, never a panic.
    pub fn explain(&self, world: &World) -> String {
        let mut out = String::from("Find ");
        let vars: Vec<String> = self.find.iter().map(var_text).collect();
        out.push_str(&vars.join(" "));
        if !self.params.is_empty() {
            out.push_str("  in");
            for p in &self.params {
                out.push_str(" $");
                out.push_str(p);
            }
        }
        out.push_str("  at ");
        match &self.default_tick {
            TickSource::Default => out.push_str("latest"),
            other => out.push_str(&tick_text(other)),
        }
        out.push_str("  canon ");
        match &self.canon {
            CanonSource::Latest => out.push_str("latest"),
            CanonSource::Const(n) => {
                let _ = write!(out, "{n}");
            }
            CanonSource::Param(p) => {
                out.push('$');
                out.push_str(p);
            }
        }
        out.push('\n');
        explain_op(&self.root, world, 2, &mut out);
        out
    }
}

fn var_text(v: &Var) -> String {
    format!("?{}", v.0)
}

/// A tick source without the `@` prefix (`latest`, `-210`, `$t`, `?t`).
fn tick_text(t: &TickSource) -> String {
    match t {
        TickSource::Default => "latest".to_string(),
        TickSource::Const(Tick(n)) => n.to_string(),
        TickSource::Param(p) => format!("${p}"),
        TickSource::Var(v) => var_text(v),
    }
}

fn value_text(v: &Value, world: &World) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(f) => format!("{f:?}"),
        // `{:?}` on a str quotes it and escapes `"`, `\` and control chars
        // (but not `'`), which is the stable form the printer promises.
        Value::Str(s) => format!("{s:?}"),
        Value::Ref(id) => world.format_id(*id),
    }
}

fn term_text(t: &Term, world: &World) -> String {
    match t {
        Term::Var(v) => var_text(v),
        Term::Param(p) => format!("${p}"),
        Term::Const(v) => value_text(v, world),
        Term::Any => "_".to_string(),
    }
}

fn access_text(a: Access) -> &'static str {
    match a {
        Access::E => "E",
        Access::R => "R",
        Access::V => "V",
        Access::Scan => "scan",
        Access::Handle => "handle",
    }
}

/// Prints `root` and everything under it, children in order, each child
/// indented two more spaces. Walked with an explicit stack, not recursion, so
/// a very deep plan can't overflow the native stack.
fn explain_op(root: &Op, world: &World, indent: usize, out: &mut String) {
    let mut todo = vec![(root, indent)];
    while let Some((op, indent)) = todo.pop() {
        for _ in 0..indent {
            out.push(' ');
        }
        let child = indent + 2;
        match op {
            Op::Unit => out.push_str("Unit\n"),
            Op::NodeScan {
                input,
                var,
                node_type,
            } => {
                let _ = writeln!(out, "NodeScan {} {node_type}", var_text(var));
                todo.push((input, child));
            }
            Op::NodeSeek { input, var, entity } => {
                let _ = writeln!(
                    out,
                    "NodeSeek {} = {}",
                    var_text(var),
                    term_text(entity, world)
                );
                todo.push((input, child));
            }
            Op::Expand {
                input,
                entity,
                attr,
                value,
                tick,
                access,
            } => {
                let e = term_text(entity, world);
                let v = term_text(value, world);
                match attr {
                    AttrRef::Named(name) => {
                        let _ = writeln!(
                            out,
                            "Expand [{e} :{name} {v}] @{} via {}",
                            tick_text(tick),
                            access_text(*access)
                        );
                    }
                    // The handle table has no history, so its tick is not shown.
                    AttrRef::Handle => {
                        let _ =
                            writeln!(out, "Expand [{e} :handle {v}] via {}", access_text(*access));
                    }
                }
                todo.push((input, child));
            }
            Op::Filter { input, pred } => {
                let _ = writeln!(
                    out,
                    "Filter {} {} {}",
                    term_text(&pred.left, world),
                    pred.op.symbol(),
                    term_text(&pred.right, world)
                );
                todo.push((input, child));
            }
            // Children are pushed last-first so they print first-first.
            Op::Join { left, right } => {
                out.push_str("Join\n");
                todo.push((right, child));
                todo.push((left, child));
            }
            Op::Project { input, vars } => {
                let vs: Vec<String> = vars.iter().map(var_text).collect();
                let _ = writeln!(out, "Project {}", vs.join(" "));
                todo.push((input, child));
            }
            Op::Union { inputs } => {
                out.push_str("Union\n");
                todo.extend(inputs.iter().rev().map(|i| (i, child)));
            }
        }
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
        Op::NodeScan { input, .. }
        | Op::NodeSeek { input, .. }
        | Op::Expand { input, .. }
        | Op::Filter { input, .. }
        | Op::Project { input, .. } => take(input),
        Op::Join { left, right } => {
            take(left);
            take(right);
        }
        Op::Union { inputs } => todo.append(inputs),
    }
}
