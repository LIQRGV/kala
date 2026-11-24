//! Fixture error types and bounded diagnostic formatting.
use std::fmt;

use kala_core::{Echo, Error};

/// The position of an op in a fixture, 0-based.
///
/// In the `transactions` form, `txn` is the transaction's index and `op` the
/// op's index in it; in the bare `operations` form, `txn` is the op's index
/// and `op` is 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRef {
    /// The transaction's index (the op's index in the bare form).
    pub txn: usize,
    /// The op's index in its transaction (0 in the bare form).
    pub op: usize,
    /// The op's own label for an assert / retract, the label it corrects for
    /// a `correct`, `None` for a `create` (or when the op is too malformed to
    /// say).
    pub label: Option<String>,
}

/// Why a fixture failed to load.
#[derive(Clone, Debug, PartialEq)]
pub enum FixtureErrorKind {
    /// The YAML parser refused the input: syntax, duplicate keys, merge keys,
    /// unsupported tags, several documents, or a parser budget or the input
    /// size limit exceeded.
    Yaml(String),
    /// The YAML is well-formed but not a fixture: an unknown key, a missing
    /// field, a scalar of the wrong type, both `transactions` and
    /// `operations`, a misplaced label, a loader limit exceeded...
    Invalid(String),
    /// No entity has this handle (yet).
    UnknownHandle(String),
    /// An edge triple `[source, attr, target]` names no interned edge.
    UnknownEdge(String),
    /// A `correct` names a label no op in the fixture has.
    UnknownLabel(String),
    /// A second op uses a label already used.
    DuplicateLabel(String),
    /// The store refused a write, a create, a correction or the schema.
    Store(Error),
}

/// A fixture load failure: where ([`OpRef`]; `None` for the schema, the
/// entities section and whole-input errors) and why.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureError {
    /// The op the error is about, if any.
    pub at: Option<OpRef>,
    /// What went wrong.
    pub kind: FixtureErrorKind,
}

impl fmt::Display for FixtureErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FixtureErrorKind::Yaml(m) => write!(f, "YAML error: {m}"),
            FixtureErrorKind::Invalid(m) => write!(f, "invalid fixture: {m}"),
            FixtureErrorKind::UnknownHandle(h) => write!(f, "unknown handle `{}`", Echo(h)),
            FixtureErrorKind::UnknownEdge(t) => write!(f, "no edge {}", Echo(t)),
            FixtureErrorKind::UnknownLabel(l) => write!(f, "unknown label `{}`", Echo(l)),
            FixtureErrorKind::DuplicateLabel(l) => write!(f, "label `{}` is used twice", Echo(l)),
            FixtureErrorKind::Store(e) => write!(f, "{e}"),
        }
    }
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(at) = &self.at {
            write!(f, "transaction {}, op {}", at.txn, at.op)?;
            if let Some(l) = &at.label {
                write!(f, " (label {})", Echo(l))?;
            }
            write!(f, ": ")?;
        }
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for FixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            FixtureErrorKind::Store(e) => Some(e),
            _ => None,
        }
    }
}

pub(super) fn err(at: Option<OpRef>, kind: FixtureErrorKind) -> FixtureError {
    FixtureError { at, kind }
}

pub(super) fn invalid(at: Option<OpRef>, msg: impl Into<String>) -> FixtureError {
    err(at, FixtureErrorKind::Invalid(msg.into()))
}

pub(super) fn store(at: Option<OpRef>, e: Error) -> FixtureError {
    err(at, FixtureErrorKind::Store(e))
}
