//! Language-neutral nullable expressions and row-preserving read operators.
use crate::{CmpOp, Term, TickSource, Var};
use kala_core::Value;

/// A nullable expression. Boolean operators use three-valued logic.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr<V = Var, C = Value> {
    /// A variable.
    Var(V),
    /// A scalar literal.
    Value(C),
    /// A missing value.
    Null,
    /// A scalar, NULL or list query parameter.
    Param(String),
    /// An optional property read at the variable's binding tick.
    Property {
        entity: V,
        key: String,
        tick: TickSource,
    },
    /// Construct a list in expression order.
    List(Vec<Self>),
    /// A nullable list/path function.
    Function(RowFn, Box<Self>),
    /// Two relationship identities/lists must not overlap.
    Disjoint(Box<Self>, Box<Self>),
    /// Construct a path by concatenating edge or path segments from a start.
    Path { start: V, segments: Vec<V> },
    /// A bag aggregate, lowered into `RowAction::Group` by the front-end.
    Aggregate {
        function: BagFn,
        distinct: bool,
        argument: Option<Box<Self>>,
    },
    /// Comparison (NULL propagates).
    Compare(CmpOp, Box<Self>, Box<Self>),
    /// Boolean conjunction.
    And(Box<Self>, Box<Self>),
    /// Boolean disjunction.
    Or(Box<Self>, Box<Self>),
    /// Boolean exclusive disjunction.
    Xor(Box<Self>, Box<Self>),
    /// Boolean negation.
    Not(Box<Self>),
    /// True exactly for NULL.
    IsNull(Box<Self>),
}
impl<V, C> Expr<V, C> {
    /// Every constant attribute read, including nested expressions.
    pub fn attributes<'a>(&'a self, out: &mut Vec<&'a str>) {
        let mut todo = vec![self];
        while let Some(e) = todo.pop() {
            match e {
                Self::Property { key, .. } => out.push(key),
                Self::Compare(_, a, b) | Self::And(a, b) | Self::Or(a, b) | Self::Xor(a, b) => {
                    todo.push(a);
                    todo.push(b);
                }
                Self::Not(e) | Self::IsNull(e) | Self::Function(_, e) => todo.push(e),
                Self::Disjoint(a, b) => {
                    todo.push(a);
                    todo.push(b);
                }
                Self::Path { .. } => {}
                Self::List(xs) => todo.extend(xs),
                Self::Aggregate { argument, .. } => todo.extend(argument.as_deref()),
                Self::Var(_) | Self::Value(_) | Self::Null | Self::Param(_) => {}
            }
        }
    }
}
/// Direction relative to a relationship's named attribute (inverse names allowed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Out,
    In,
    Either,
}
/// Nullable row operations. Hidden match/occurrence variables preserve bags;
/// explicit grouping and projection select which bindings survive.
#[derive(Clone, Debug, PartialEq)]
pub enum RowAction<V = Var, C = Value> {
    /// Read a catalogue or adapter-supplied row source.
    Source {
        source: crate::RowSource,
        columns: Vec<V>,
        tick: TickSource,
        ordinal: V,
    },
    /// Adapter rows after parameter resolution (also usable by native IR callers).
    Values {
        rows: Expr<V, C>,
        columns: Vec<V>,
        ordinal: V,
    },
    /// Static validation of a supplied parameter (lists validate every element).
    CheckParameter {
        name: String,
        expected: kala_core::ValueType,
    },
    /// Bounded expansion from a bound source. Each frontier hop seeks neighbors;
    /// the path history makes different trails distinct complete bindings.
    Trail {
        source: V,
        target: V,
        path: V,
        attr: String,
        direction: Orientation,
        tick: TickSource,
        min: usize,
        max: usize,
        excluded: Vec<V>,
        properties: Vec<(String, Expr<V, C>)>,
    },
    /// Project, distinct, order and paginate within a pipeline. The occurrence
    /// slot preserves bags and the requested order through set-based storage.
    Project {
        vars: Vec<V>,
        distinct: bool,
        order: Vec<(V, bool)>,
        skip: Option<Term>,
        limit: Option<Term>,
        ordinal: V,
    },
    /// Expand a list (NULL gives no rows), preserving duplicate occurrences.
    Unwind {
        expr: Expr<V, C>,
        var: V,
        ordinal: V,
    },
    /// Group complete bindings as a bag, skipping NULL aggregate operands.
    Group {
        keys: Vec<V>,
        aggregates: Vec<BagAggregate<V, C>>,
    },
    /// Scan or constrain nodes; every label is required. Unknown labels match nothing.
    Nodes {
        var: V,
        labels: Vec<String>,
        tick: TickSource,
    },
    /// Traverse one typed relationship and bind its identity.
    Edge {
        source: V,
        target: V,
        edge: V,
        attr: String,
        direction: Orientation,
        tick: TickSource,
    },
    /// Bind the result of a nullable expression.
    Bind { var: V, expr: Expr<V, C> },
    /// Retain only true (false and NULL are discarded).
    Select(Expr<V, C>),
}
impl<V, C> RowAction<V, C> {
    /// Every attribute this action can read, independent of its input rows.
    pub fn attributes<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Source { source, .. } => {
                if matches!(source, crate::RowSource::Labels) {
                    out.push("label");
                }
            }
            Self::Values { rows, .. } => rows.attributes(out),
            Self::CheckParameter { .. } | Self::Project { .. } => {}
            Self::Trail {
                attr, properties, ..
            } => {
                out.push(attr);
                for (k, e) in properties {
                    out.push(k);
                    e.attributes(out);
                }
            }
            Self::Unwind { expr, .. } => expr.attributes(out),
            Self::Group { aggregates, .. } => {
                for a in aggregates {
                    if let Some(e) = &a.argument {
                        e.attributes(out);
                    }
                }
            }
            Self::Nodes { labels, .. } => {
                if !labels.is_empty() {
                    out.push("label");
                }
            }
            Self::Edge { attr, .. } => out.push(attr),
            Self::Bind { expr, .. } | Self::Select(expr) => expr.attributes(out),
        }
    }
}
/// Final projection, optional distinctness, ordering, then pagination.
/// Only valid as the plan root; `Project` remains the set projection within a plan.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSpec {
    /// Output columns, in order.
    pub vars: Vec<Var>,
    /// Collapse duplicate projected rows before ordering and slicing.
    pub distinct: bool,
    /// Sort keys and descending flags. NULL is last ascending, first descending.
    pub order: Vec<(Var, bool)>,
    /// Non-negative integer literal or parameter.
    pub skip: Option<Term>,
    /// Non-negative integer literal or parameter.
    pub limit: Option<Term>,
}

/// Aggregate functions with nullable bag semantics (separate from set aggregates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BagFn {
    Count,
    Collect,
    Min,
    Max,
    Sum,
    Avg,
}
impl BagFn {
    /// Stable query/explain spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Collect => "collect",
            Self::Min => "min",
            Self::Max => "max",
            Self::Sum => "sum",
            Self::Avg => "avg",
        }
    }
}
/// One nullable aggregate over complete input bindings.
#[derive(Clone, Debug, PartialEq)]
pub struct BagAggregate<V = Var, C = Value> {
    pub output: V,
    pub function: BagFn,
    pub distinct: bool,
    /// None is count(*).
    pub argument: Option<Expr<V, C>>,
}

/// One bounded relationship pattern's hard hop limit.
pub const MAX_VAR_LENGTH: usize = 10;
/// Functions over query lists and path histories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowFn {
    Length,
    Nodes,
    Relationships,
    Size,
}
impl RowFn {
    /// Stable query/explain spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::Nodes => "nodes",
            Self::Relationships => "relationships",
            Self::Size => "size",
        }
    }
}
