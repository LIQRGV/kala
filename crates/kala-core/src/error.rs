//! The error type of the world API.

use std::fmt;

use crate::id::{EntityId, IdError};
use crate::tick::Tick;
use crate::value::{ValueError, ValueType};

/// Why a world operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The node type name isn't declared in the schema.
    UnknownNodeType(String),
    /// The handle is already used by another entity in this world.
    DuplicateHandle(String),
    /// A name is declared twice in the schema.
    DuplicateName(String),
    /// The handle isn't a valid handle (e.g. empty).
    InvalidHandle(String),
    /// A node type or attribute name in the schema isn't valid (e.g. empty).
    InvalidName(String),
    /// The attribute name isn't declared in the schema.
    UnknownAttribute(String),
    /// The entity (a write's subject or a `ref` value) isn't defined in
    /// this world.
    UnknownEntity(EntityId),
    /// The value's type isn't the attribute's declared type.
    WrongValueType {
        /// The attribute written.
        attr: String,
        /// Its declared value type.
        expected: ValueType,
        /// The type of the value given.
        got: ValueType,
    },
    /// The value can't be stored: NaN, or a string too long for an indexed
    /// attribute.
    Value(ValueError),
    /// A retract of an event attribute. Events are never retracted in
    /// story time; un-canon one with a correction instead.
    RetractEvent(String),
    /// The opposite op is already stored for the same `(entity, attribute,
    /// value)` at exactly this tick. Tick order can't decide between them;
    /// changing that moment is a correction.
    SameTickConflict {
        /// The entity written.
        entity: EntityId,
        /// The attribute written.
        attr: String,
        /// The tick both ops are at.
        tick: Tick,
    },
    /// An ID can't be built: too many node types in the schema, or a node
    /// type's sequence numbers are exhausted.
    Id(IdError),
    /// The transaction already failed: an earlier write in it returned an
    /// error, so it can't take more writes and its commit applies nothing.
    TransactionAborted,
    /// Reading or writing the log file failed. Kept as kind + message (not
    /// the `std::io::Error` itself) so `Error` stays `Clone` and `PartialEq`.
    /// After a failed append the world refuses further writes with this
    /// error; reopen it to recover.
    Io {
        /// The I/O error kind (e.g. `AlreadyExists` for `World::create` on
        /// an existing path).
        kind: std::io::ErrorKind,
        /// The I/O error's message.
        message: String,
    },
    /// The file doesn't start with the Kala log magic number.
    BadMagic,
    /// The log file has a format version this build can't read.
    UnsupportedVersion(u32),
    /// The log file is damaged in a way that isn't a torn tail.
    Corrupt(String),
    /// The log file is already open in another [`World`](crate::World)
    /// (in this process or another one): it holds the file's exclusive
    /// lock. The lock is advisory, so it only keeps out other Kala worlds,
    /// not other programs writing the file.
    WorldLocked,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownNodeType(name) => write!(f, "unknown node type {name:?}"),
            Error::DuplicateHandle(h) => write!(f, "handle {h:?} is already in use"),
            Error::DuplicateName(name) => {
                write!(f, "name {name:?} is declared twice in the schema")
            }
            Error::InvalidHandle(h) => write!(f, "invalid handle {h:?}"),
            Error::InvalidName(name) => write!(f, "invalid schema name {name:?}"),
            Error::UnknownAttribute(a) => write!(f, "unknown attribute {a:?}"),
            Error::UnknownEntity(e) => write!(f, "unknown entity {e:?}"),
            Error::WrongValueType {
                attr,
                expected,
                got,
            } => write!(
                f,
                "attribute {attr:?} holds {expected:?} values, got a {got:?} value"
            ),
            Error::Value(e) => write!(f, "invalid value: {e}"),
            Error::RetractEvent(a) => write!(
                f,
                "attribute {a:?} is an event and can't be retracted (use a correction)"
            ),
            Error::SameTickConflict { entity, attr, tick } => write!(
                f,
                "the opposite op is already stored for {entity:?} {attr:?} at tick {}",
                tick.0
            ),
            Error::Id(e) => write!(f, "invalid ID: {e}"),
            Error::TransactionAborted => write!(
                f,
                "the transaction was aborted by an earlier error and applies nothing"
            ),
            Error::Io { kind, message } => write!(f, "log I/O error ({kind:?}): {message}"),
            Error::BadMagic => f.write_str("not a Kala log file (bad magic number)"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported log format version {v}"),
            Error::Corrupt(why) => write!(f, "corrupt log: {why}"),
            Error::WorldLocked => f.write_str("world is open elsewhere (its log file is locked)"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Id(e) => Some(e),
            Error::Value(e) => Some(e),
            _ => None,
        }
    }
}

impl From<IdError> for Error {
    fn from(e: IdError) -> Error {
        Error::Id(e)
    }
}

impl From<ValueError> for Error {
    fn from(e: ValueError) -> Error {
        Error::Value(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io {
            kind: e.kind(),
            message: e.to_string(),
        }
    }
}
