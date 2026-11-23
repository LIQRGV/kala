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
    /// A scalar query parameter.
    Param(String),
    /// An optional property read at the variable's binding tick.
    Property {
        entity: V,
        key: String,
        tick: TickSource,
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
                Self::Not(e) | Self::IsNull(e) => todo.push(e),
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
/// An operation retaining all input bindings. Hidden match variables preserve multiplicity.
#[derive(Clone, Debug, PartialEq)]
pub enum RowAction<V = Var, C = Value> {
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
