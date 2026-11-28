//! Shared front-end preparation metadata.
use crate::Plan;
use kala_core::ValueType;

/// What a query parameter must be given, decided from where the query uses
/// it. Tools parse parameter text by it (an entity by ID or handle, a value
/// by the attribute's type, a tick, a canon).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// A scalar of any stored type, inferred from its supplied value.
    Scalar,
    /// A direct UNWIND source whose elements may be map rows.
    Unwind,
    /// An entity (or edge): used in an entity position or as a `ref` value.
    Entity,
    /// A value of this type: used as a non-`ref` value (or compared with one).
    Value(ValueType),
    /// A tick (`:at ?t`, `(at ?t ..)`): an `i64`.
    Tick,
    /// A canon (`:canon ?c`): a non-negative `i64`.
    Canon,
}

/// A query compiled and planned, with the kinds of its parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// The plan, ready for [`crate::run`] or [`crate::run_with`].
    pub plan: Plan,
    /// Inferred parameter kinds in front-end declaration/discovery order.
    pub params: Vec<(String, ParamKind)>,
}
