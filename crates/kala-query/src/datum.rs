//! [`Datum`]: one cell of a query's answer.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use kala_core::{IntervalSet, Op, Precision, Value};

/// One cell of a [`ResultSet`](crate::ResultSet) row: a stored [`Value`],
/// a change point's [`Op`] (bound by a history read) or an [`IntervalSet`]
/// (built during the query, never stored).
///
/// Ordered by kind first, so rows sort deterministically whatever they
/// hold: every `Value` (in [`Value`]'s order) sorts before every `Op`
/// (`Assert` before `Retract`), which sorts before every `Intervals` (in
/// [`IntervalSet`]'s order). Equality never holds across kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Datum {
    /// Missing value (never stored).
    Null,
    /// Sorted property values; lists preserve their supplied order.
    List(Vec<Datum>),
    /// A stored value.
    Value(Value),
    /// A change point's op.
    Op(Op),
    /// The precision attached to a change point.
    Precision(Precision),
    /// An interval set.
    Intervals(IntervalSet),
}

impl Datum {
    /// The value, if this is one.
    pub fn as_value(&self) -> Option<&Value> {
        match self {
            Datum::Value(v) => Some(v),
            Datum::Null
            | Datum::List(_)
            | Datum::Op(_)
            | Datum::Precision(_)
            | Datum::Intervals(_) => None,
        }
    }

    /// The kind's rank in the cross-kind order.
    fn rank(&self) -> u8 {
        match self {
            Datum::Null => 5,
            Datum::List(_) => 4,
            Datum::Value(_) => 0,
            Datum::Op(_) => 1,
            Datum::Intervals(_) => 2,
            Datum::Precision(_) => 3,
        }
    }
}

/// `Assert < Retract`. (kala-core's `Op` has no order of its own.)
pub(crate) fn op_rank(op: Op) -> u8 {
    match op {
        Op::Assert => 0,
        Op::Retract => 1,
    }
}

impl Ord for Datum {
    fn cmp(&self, other: &Datum) -> Ordering {
        match (self, other) {
            (Datum::List(a), Datum::List(b)) => a.cmp(b),
            (Datum::Value(a), Datum::Value(b)) => a.cmp(b),
            (Datum::Precision(a), Datum::Precision(b)) => (*a as u8).cmp(&(*b as u8)),
            (Datum::Op(a), Datum::Op(b)) => op_rank(*a).cmp(&op_rank(*b)),
            (Datum::Intervals(a), Datum::Intervals(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for Datum {
    fn partial_cmp(&self, other: &Datum) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for Datum {
    /// Hashes the kind, then what `eq` compares.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.rank().hash(state);
        match self {
            Datum::Null => {}
            Datum::List(v) => v.hash(state),
            Datum::Value(v) => v.hash(state),
            Datum::Op(op) => op_rank(*op).hash(state),
            Datum::Precision(p) => (*p as u8).hash(state),
            Datum::Intervals(s) => s.hash(state),
        }
    }
}

impl From<Value> for Datum {
    fn from(v: Value) -> Datum {
        Datum::Value(v)
    }
}

impl From<Op> for Datum {
    fn from(op: Op) -> Datum {
        Datum::Op(op)
    }
}

impl From<IntervalSet> for Datum {
    fn from(s: IntervalSet) -> Datum {
        Datum::Intervals(s)
    }
}

impl PartialEq<Value> for Datum {
    fn eq(&self, other: &Value) -> bool {
        self.as_value() == Some(other)
    }
}

impl PartialEq<Datum> for Value {
    fn eq(&self, other: &Datum) -> bool {
        other == self
    }
}
