//! The error type of the world API.

use std::fmt;

use crate::echo::Quoted;
use crate::id::{EntityId, IdError};
use crate::log::EntryId;
use crate::tick::Tick;
use crate::value::{ValueError, ValueType};

/// Why a metered read was interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkInterruptReason {
    /// The key limit was exceeded.
    Limit,
    /// The caller requested a stop.
    Stopped,
}

/// Why a world operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A bounded read stopped before producing an answer.
    WorkInterrupted {
        /// Whether the key bound or stop flag interrupted the read.
        reason: WorkInterruptReason,
    },
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
    /// An attribute's direction, inverse or reverse cardinality doesn't fit
    /// its declaration: a non-ref attribute with a direction, an inverse or
    /// a reverse cardinality, a ref attribute without a direction, or an
    /// inverse or a reverse cardinality on a bidirectional attribute.
    /// Holds the attribute's name.
    InvalidDirection(String),
    /// The attribute name isn't declared in the schema.
    UnknownAttribute(String),
    /// The attribute is declared `indexed: false`, so it has no `V`
    /// entries and can't be looked up by value. Holds the attribute name.
    NotIndexed(String),
    /// The attribute isn't a `ref` attribute, so it has no neighbors.
    /// Holds the attribute name as given.
    NotARef(String),
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
    /// The value is too large for the log: an entry carrying it would be
    /// longer than [`MAX_ENTRY_BYTES`](crate::log::MAX_ENTRY_BYTES), one
    /// log frame. It is judged against the largest entry the value could
    /// ever need, a `Revised` correction carrying it, so every accepted value
    /// can later be deleted or replaced in place. Refused before anything is
    /// written.
    EntryTooLarge {
        /// The attribute written.
        attr: String,
        /// The payload length of a `Revised` entry carrying the value.
        len: usize,
        /// The limit, [`MAX_ENTRY_BYTES`](crate::log::MAX_ENTRY_BYTES).
        max: usize,
    },
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
    /// A correction names an entry ID that no committed log entry has.
    UnknownEntry(EntryId),
    /// A correction names an entry that isn't a fact: a `DEFINE`, a
    /// `Commit` or a `DELETED` marker. Holds the ID given.
    NotAFact(EntryId),
    /// A correction names an entry whose revision chain is currently
    /// deleted: there is nothing left to correct. Holds the ID given.
    AlreadyDeleted(EntryId),
    /// The transaction already wrote an entry at this key `(entity,
    /// attribute, value, tick)`, and a transaction stores at most one entry
    /// per key (e.g. a correction of a chain it already corrected, or a
    /// write stored at a key it deleted).
    TxnKeyConflict {
        /// The entity of the key (as stored: a bidirectional pair
        /// normalised).
        entity: EntityId,
        /// The attribute of the key.
        attr: String,
        /// The tick of the key.
        tick: Tick,
    },
    /// The revision chain of this base entry already has `u32::MAX`
    /// revisions, the most a correction entry can number; it can't be
    /// corrected again.
    TooManyRevisions(EntryId),
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
    /// [`World::open`](crate::World::open) cut a *damaged* tail off the log
    /// (a committed-looking last transaction whose checksum fails; see
    /// [`CutCause::Damaged`](crate::CutCause::Damaged)), and nobody has
    /// looked at it yet: the world reads normally but refuses every write
    /// until [`World::acknowledge_recovery`](crate::World::acknowledge_recovery)
    /// is called, so new writes don't silently land on top of lost canon.
    RecoveryUnacknowledged,
    /// The world was opened with
    /// [`World::open_read_only`](crate::World::open_read_only): every write
    /// (a no-op included) is refused and nothing is appended to the log.
    /// Open the world with [`World::open`](crate::World::open) to write.
    ReadOnly,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::WorkInterrupted { reason } => write!(f, "read interrupted: {reason:?}"),
            Error::UnknownNodeType(name) => write!(f, "unknown node type {}", Quoted(name)),
            Error::DuplicateHandle(h) => write!(f, "handle {} is already in use", Quoted(h)),
            Error::DuplicateName(name) => {
                write!(f, "name {} is declared twice in the schema", Quoted(name))
            }
            Error::InvalidHandle(h) => write!(f, "invalid handle {}", Quoted(h)),
            Error::InvalidName(name) => write!(f, "invalid schema name {}", Quoted(name)),
            Error::InvalidDirection(a) => {
                write!(
                    f,
                    "attribute {} has an invalid direction or inverse",
                    Quoted(a)
                )
            }
            Error::UnknownAttribute(a) => write!(f, "unknown attribute {}", Quoted(a)),
            Error::NotIndexed(a) => {
                write!(
                    f,
                    "attribute {} is not indexed, so it can't be looked up by value",
                    Quoted(a)
                )
            }
            Error::NotARef(a) => write!(f, "attribute {} is not a ref attribute", Quoted(a)),
            Error::UnknownEntity(e) => write!(f, "unknown entity {e:?}"),
            Error::WrongValueType {
                attr,
                expected,
                got,
            } => write!(
                f,
                "attribute {} holds {expected:?} values, got a {got:?} value",
                Quoted(attr)
            ),
            Error::Value(e) => write!(f, "invalid value: {e}"),
            Error::EntryTooLarge { attr, len, max } => write!(
                f,
                "a value of attribute {} needs a {len}-byte log entry, over the \
                 {max}-byte limit",
                Quoted(attr)
            ),
            Error::RetractEvent(a) => write!(
                f,
                "attribute {} is an event and can't be retracted (use a correction)",
                Quoted(a)
            ),
            Error::SameTickConflict { entity, attr, tick } => write!(
                f,
                "the opposite op is already stored for {entity:?} {} at tick {}",
                Quoted(attr),
                tick.0
            ),
            Error::UnknownEntry(id) => write!(f, "no committed log entry#{}", id.0),
            Error::NotAFact(id) => write!(f, "log entry#{} is not a fact", id.0),
            Error::AlreadyDeleted(id) => {
                write!(f, "log entry#{} is already deleted by a correction", id.0)
            }
            Error::TxnKeyConflict { entity, attr, tick } => write!(
                f,
                "the transaction already wrote an entry for {entity:?} {} at tick {}",
                Quoted(attr),
                tick.0
            ),
            Error::TooManyRevisions(id) => write!(
                f,
                "log entry#{} has the most revisions a correction can number",
                id.0
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
            Error::RecoveryUnacknowledged => f.write_str(
                "opening the log cut off a damaged transaction; acknowledge the recovery \
                 (World::acknowledge_recovery) before writing",
            ),
            Error::ReadOnly => f.write_str("world is open read-only; writes are refused"),
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
