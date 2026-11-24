//! The error type of the world API.

use std::fmt;

use crate::echo::{Echo, Quoted};
use crate::id::{EntityId, IdError};
use crate::log::EntryId;
use crate::tick::Tick;
use crate::value::{Value, ValueError, ValueType};

/// Why a metered read was interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkInterruptReason {
    /// The key limit was exceeded.
    Limit,
    /// The caller requested a stop.
    Stopped,
}

/// An entity named in an error: its ID, and the ID as the world formats it
/// (`CHARACTER#5`, see [`World::format_id`](crate::World::format_id)),
/// captured when the error is made because `Display` has no world. Front-ends
/// with a world show the handle instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorEntity {
    /// The entity.
    pub id: EntityId,
    /// `TYPE#n` (or `edge#n`).
    pub formatted: String,
}

/// One change point a migration can't convert, in [`Error::Unconvertible`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnconvertibleValue {
    /// The entity the change point belongs to.
    pub entity: ErrorEntity,
    /// Its tick.
    pub tick: Tick,
    /// The old value.
    pub value: Value,
}

/// Why a world operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A schema change cannot preserve the meaning of the stored history.
    Schema(String),
    /// The name resolves, but the attribute did not exist at this canon.
    AttributeNotDefined { name: String, canon: crate::TxId },
    /// New writes to a deprecated attribute are forbidden.
    DeprecatedAttribute(String),
    /// A bounded read stopped before producing an answer.
    WorkInterrupted {
        /// Whether the key bound or stop flag interrupted the read.
        reason: WorkInterruptReason,
    },
    /// The node type name isn't declared in the schema.
    UnknownNodeType(String),
    /// The handle is already used by another entity in this world.
    DuplicateHandle(String),
    /// Find-or-create found the handle on a different, permanent node type.
    HandleTypeMismatch { handle: String, node_type: String },
    /// A name is declared twice in the schema.
    DuplicateName(String),
    /// The handle isn't a valid handle (e.g. empty).
    InvalidHandle(String),
    /// A node type or attribute name in the schema isn't valid (e.g. empty).
    InvalidName(String),
    /// A schema name uses [`BUILTIN_NAME_PREFIX`](crate::BUILTIN_NAME_PREFIX),
    /// or a post-genesis edit introduces [`RESERVED_NAME_PREFIX`](crate::RESERVED_NAME_PREFIX).
    /// Both prefixes belong to Kala. Holds the name.
    ReservedName(String),
    /// A migration found change points whose value doesn't convert to the
    /// new type (or collides with another value of the same entity) and
    /// wasn't asked to skip them.
    Unconvertible {
        /// The migrated attribute.
        attr: String,
        /// Its old value type.
        from: ValueType,
        /// The new value type.
        to: ValueType,
        /// How many change points don't convert.
        count: usize,
        /// The first few of them, in history order.
        examples: Vec<UnconvertibleValue>,
    },
    /// A `directed -> bidirectional` migration found pairs stored both ways
    /// (A to B and B to A), which would read as duplicates.
    PairsBothWays {
        /// The migrated attribute.
        attr: String,
        /// The first few pairs, each ordered by entity ID.
        pairs: Vec<(ErrorEntity, ErrorEntity)>,
    },
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
    /// A scalar-only visitor was given a relationship attribute.
    ExpectedScalarAttribute(String),
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
    /// A transaction committing in weak mode corrected an entry whose
    /// revision chain a newer transaction corrected meanwhile (replaced or
    /// deleted): the correction was decided on a fact that is no longer
    /// the current one, so it is refused instead of being stacked on top.
    /// Nothing was written; look at the entry and correct it again. (In
    /// optimistic mode the same race is an [`Error::Conflict`].)
    AlreadyCorrected {
        /// The entry as the caller named it.
        entry: EntryId,
        /// The chain's revision now.
        rev: u32,
    },
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
    /// The log file's exclusive lock is held by another
    /// [`World`](crate::World) handle (in this process or another one) and
    /// stayed held for the whole busy timeout
    /// ([`World::set_busy_timeout`](crate::World::set_busy_timeout)): a
    /// commit, an open or a recovery cut is in progress there. Nothing was
    /// written; trying again later is safe. The lock is advisory, so it
    /// only keeps out other Kala worlds, not other programs writing the
    /// file.
    WorldLocked,
    /// [`World::open`](crate::World::open) found a *damaged* tail on the log
    /// (a committed-looking last transaction whose checksum fails; see
    /// [`CutCause::Damaged`](crate::CutCause::Damaged)), and nobody has
    /// looked at it yet: the world reads normally (without that
    /// transaction) but refuses every write until
    /// [`World::acknowledge_recovery`](crate::World::acknowledge_recovery)
    /// is called, so new writes don't silently land on top of lost canon.
    /// The damaged bytes are still in the file; the first commit after the
    /// acknowledgement cuts them to a quarantine file.
    RecoveryUnacknowledged,
    /// [`World::open_refusing_damaged`](crate::World::open_refusing_damaged),
    /// or a commit holding the writer lock, found a *damaged* last
    /// transaction of `bytes` bytes and left the file untouched. Cutting it
    /// (to a quarantine file) takes an acknowledgement: after a refused
    /// commit, [`World::acknowledge_recovery`](crate::World::acknowledge_recovery)
    /// and commit again.
    RecoveryRequired {
        /// The size of the damaged tail.
        bytes: u64,
    },
    /// The world was opened with
    /// [`World::open_read_only`](crate::World::open_read_only): every write
    /// (a no-op included) is refused and nothing is appended to the log.
    /// Open the world with [`World::open`](crate::World::open) to write.
    ReadOnly,
    /// The log file is no longer the log this handle applied: it is shorter
    /// than the handle's last transaction, that transaction's bytes no
    /// longer carry the chained checksum the handle recorded, the header
    /// changed, or the path now names another file. Another process cut a
    /// tail this handle had applied (a recovery or an fsync-failure
    /// truncation) and possibly wrote on, or the file was replaced. Found by
    /// [`World::catch_up`](crate::World::catch_up) or a commit; from then on
    /// the handle refuses every catch-up and every write with this error
    /// (its reads keep answering from what it had applied, which the file
    /// no longer backs). Reopen the world.
    LogDiverged,
    /// An optimistic commit found that a transaction committed after this
    /// one began wrote something this one had read (or, for a transaction
    /// marked [`Txn::set_ids_exposed`](crate::Txn::set_ids_exposed), that an
    /// ID it handed out would change). Nothing was written and the handle
    /// is caught up: begin a new transaction and run the same reads and
    /// writes again; they then see the newer state. See [`Conflict`].
    Conflict(Conflict),
}

/// Why an optimistic commit was refused ([`Error::Conflict`]): which read a
/// newer commit invalidated. It names the key as the transaction recorded
/// it, never what the newer commit wrote there.
///
/// A conflict is transient: nothing was written, and the same transaction
/// run again on the caught-up world reads the new state. Match on
/// [`Conflict::key`] (or [`ConflictKey::kind`] for a stable label).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The read that no longer holds.
    pub key: ConflictKey,
    /// Where in the transaction the read was made: the number of write
    /// operations ([`Txn::operation_count`](crate::Txn::operation_count))
    /// the transaction had made before it, so the read fed the operation
    /// with this 0-based index (or a later one). For
    /// [`ConflictKey::ExposedId`], the index of the operation that
    /// allocated the ID.
    pub operation: usize,
    /// How many transactions were committed since this one began.
    pub applied: usize,
}

/// The kind of read a [`Conflict`] names, with the key as the transaction
/// gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictKey {
    /// A read of an entity's attribute (`[:set …]`, `Txn::values_at`,
    /// `Txn::note_read`): a newer entry is on that entity and attribute.
    EntityAttribute {
        /// The entity read.
        entity: EntityId,
        /// The attribute read, by its name now.
        attr: String,
    },
    /// A reverse read ("who is in Varrow", `Txn::note_reverse_read`): a
    /// newer entry has that target as its value under the attribute.
    Reverse {
        /// The target read.
        target: EntityId,
        /// The attribute read, by its name now.
        attr: String,
    },
    /// A value lookup (`Txn::note_value_read`): a newer entry has that
    /// attribute and value. The value isn't repeated here.
    Value {
        /// The attribute looked up, by its name now.
        attr: String,
    },
    /// A read of a whole attribute (`Txn::note_read_attribute`, or a read
    /// set past [`MAX_READ_KEYS`](crate::MAX_READ_KEYS) keys, which is kept
    /// per attribute): a newer entry or schema change is on that attribute.
    Attribute {
        /// The attribute read, by its name now.
        attr: String,
    },
    /// A scan of every entity of a node type (`Txn::note_type_scan`): a
    /// newer commit created an entity of that type.
    TypeScan {
        /// The node type scanned.
        node_type: String,
    },
    /// Type scans or handle reads of a read set past its bound: a newer
    /// commit created an entity.
    AnyDefine,
    /// A handle that was free when read (find-or-create,
    /// `Txn::entity_by_handle`): a newer commit created an entity with it.
    Handle {
        /// The handle read.
        handle: String,
    },
    /// The entry (its revision chain) a correction read: a newer commit
    /// corrected it.
    Entry {
        /// The entry named in the correction or read.
        entry: EntryId,
    },
    /// A name that was undeclared when the transaction looked it up (an
    /// attribute or node type): a newer commit declared it.
    Undeclared {
        /// The name looked up.
        name: String,
    },
    /// `Txn::note_read_all`: any newer commit conflicts.
    All,
    /// The transaction was marked as having shown its provisional IDs
    /// (`Txn::set_ids_exposed`), and this entity or edge ID would be
    /// renumbered, or merged into one a newer commit defined.
    ExposedId {
        /// The provisional ID, as the transaction handed it out.
        id: EntityId,
    },
}

impl ConflictKey {
    /// A stable label for the kind of key, for adapters that map conflicts
    /// to their own codes.
    pub fn kind(&self) -> &'static str {
        match self {
            ConflictKey::EntityAttribute { .. } => "entity-attribute",
            ConflictKey::Reverse { .. } => "reverse",
            ConflictKey::Value { .. } => "value",
            ConflictKey::Attribute { .. } => "attribute",
            ConflictKey::TypeScan { .. } => "type-scan",
            ConflictKey::AnyDefine => "any-define",
            ConflictKey::Handle { .. } => "handle",
            ConflictKey::Entry { .. } => "entry",
            ConflictKey::Undeclared { .. } => "undeclared",
            ConflictKey::All => "all",
            ConflictKey::ExposedId { .. } => "exposed-id",
        }
    }
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("conflict: ")?;
        match &self.key {
            ConflictKey::EntityAttribute { attr, .. } => write!(
                f,
                "another transaction wrote {} of an entity this transaction read",
                Quoted(attr)
            )?,
            ConflictKey::Reverse { attr, .. } => write!(
                f,
                "another transaction wrote {} pointing at an entity this transaction read \
                 the incoming relationships of",
                Quoted(attr)
            )?,
            ConflictKey::Value { attr } => write!(
                f,
                "another transaction wrote a value of {} this transaction looked up",
                Quoted(attr)
            )?,
            ConflictKey::Attribute { attr } => write!(
                f,
                "another transaction changed {}, which this transaction read",
                Quoted(attr)
            )?,
            ConflictKey::TypeScan { node_type } => write!(
                f,
                "another transaction created a {} entity after this transaction scanned them",
                Echo(node_type)
            )?,
            ConflictKey::AnyDefine => f.write_str(
                "another transaction created an entity after this transaction scanned them",
            )?,
            ConflictKey::Handle { handle } => write!(
                f,
                "another transaction created the handle {}, which this transaction found free",
                Quoted(handle)
            )?,
            ConflictKey::Entry { entry } => write!(
                f,
                "another transaction corrected entry#{}, which this transaction read",
                entry.0
            )?,
            ConflictKey::Undeclared { name } => write!(
                f,
                "another transaction declared {}, which this transaction found undeclared",
                Quoted(name)
            )?,
            ConflictKey::All => {
                f.write_str("another transaction committed after this one read the world")?
            }
            ConflictKey::ExposedId { .. } => f.write_str(
                "another transaction took an ID this transaction had already handed out",
            )?,
        }
        let what = match self.key {
            ConflictKey::ExposedId { .. } => "allocated by",
            _ => "read before",
        };
        write!(
            f,
            " ({} newer transaction(s), {what} operation {}); nothing was written, \
             run the transaction again",
            self.applied,
            self.operation.saturating_add(1)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Schema(message) => f.write_str(message),
            Error::AttributeNotDefined { name, canon } => write!(
                f,
                "attribute {} is not defined at canon tx#{}",
                Quoted(name),
                canon.0
            ),
            Error::DeprecatedAttribute(name) => write!(
                f,
                "attribute {} is deprecated; new writes are refused",
                Quoted(name)
            ),
            Error::ExpectedScalarAttribute(name) => {
                write!(f, "expected a non-ref attribute, got {}", Quoted(name))
            }
            Error::WorkInterrupted { reason } => write!(f, "read interrupted: {reason:?}"),
            Error::UnknownNodeType(name) => write!(f, "unknown node type {}", Quoted(name)),
            Error::HandleTypeMismatch { handle, node_type } => write!(
                f,
                "handle {} is already in use by a {}",
                Quoted(handle),
                Echo(node_type)
            ),
            Error::DuplicateHandle(h) => write!(f, "handle {} is already in use", Quoted(h)),
            Error::DuplicateName(name) => {
                write!(f, "name {} is declared twice in the schema", Quoted(name))
            }
            Error::InvalidHandle(h) => write!(f, "invalid handle {}", Quoted(h)),
            Error::InvalidName(name) => write!(f, "invalid schema name {}", Quoted(name)),
            Error::Unconvertible {
                attr,
                from,
                to,
                count,
                examples,
            } => {
                write!(
                    f,
                    "{count} unconvertible change points migrating :{} ({} -> {}): ",
                    Echo(attr),
                    from.name(),
                    to.name()
                )?;
                write_list(f, examples, |f, e| {
                    write!(f, "{} @{} ", Echo(&e.entity.formatted), e.tick.0)?;
                    write_value(f, &e.value)
                })
            }
            Error::PairsBothWays { attr, pairs } => {
                write!(
                    f,
                    "cannot migrate :{}: pairs have history in both directions: ",
                    Echo(attr)
                )?;
                write_list(f, pairs, |f, (a, b)| {
                    write!(f, "{} and {}", Echo(&a.formatted), Echo(&b.formatted))
                })
            }
            Error::ReservedName(name) => write!(
                f,
                "schema name {} is reserved: names starting with {} or {} belong to Kala",
                Quoted(name),
                crate::RESERVED_NAME_PREFIX,
                crate::BUILTIN_NAME_PREFIX
            ),
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
            Error::AlreadyCorrected { entry, rev } => write!(
                f,
                "log entry#{} was already corrected by a newer transaction (now rev {rev}); \
                 nothing was written",
                entry.0
            ),
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
            Error::UnsupportedVersion(v) => {
                write!(f, "unsupported log format version {v}")?;
                if *v < crate::log::LOG_FORMAT_VERSION {
                    f.write_str(
                        ": this file was written by an older Kala log format, which this \
                         build doesn't read. Dump it with the older kala binary that wrote \
                         it (kala dump WORLD -o world.dump.edn), then load the dump with \
                         this one (kala transact --create NEW_WORLD world.dump.edn). The \
                         old file is left untouched",
                    )?;
                }
                Ok(())
            }
            Error::Corrupt(why) => write!(f, "corrupt log: {why}"),
            Error::WorldLocked => f.write_str(
                "world is locked by another writer (its log file's lock stayed held for the \
                 whole busy timeout); nothing was written",
            ),
            Error::RecoveryRequired { bytes } => write!(
                f,
                "the last transaction is damaged ({bytes} bytes); writing needs an \
                 acknowledgement (World::acknowledge_recovery), after which the next commit \
                 cuts it to a quarantine file"
            ),
            Error::RecoveryUnacknowledged => f.write_str(
                "the log ends in a damaged transaction, which opening it left out; \
                 acknowledge the recovery (World::acknowledge_recovery) before writing: the \
                 next commit then cuts it to a quarantine file",
            ),
            Error::ReadOnly => f.write_str("world is open read-only; writes are refused"),
            Error::LogDiverged => f.write_str(
                "the log file is no longer the log this world applied (it was cut, rewritten \
                 or replaced by another process); reopen the world",
            ),
            Error::Conflict(conflict) => conflict.fmt(f),
        }
    }
}

/// Writes `items` separated by `", "`, each as `write_item` writes it.
fn write_list<T>(
    f: &mut fmt::Formatter<'_>,
    items: &[T],
    write_item: impl Fn(&mut fmt::Formatter<'_>, &T) -> fmt::Result,
) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write_item(f, item)?;
    }
    Ok(())
}

/// A value as `kala asof` shows it: strings quoted (escaped and cut, see
/// [`Quoted`]), refs by ID, numbers and bools as written.
fn write_value(f: &mut fmt::Formatter<'_>, v: &Value) -> fmt::Result {
    match v {
        Value::Str(s) => write!(f, "{}", Quoted(s)),
        Value::I64(i) => write!(f, "{i}"),
        Value::F64(x) => write!(f, "{x}"),
        Value::Bool(b) => write!(f, "{b}"),
        // Never in a migration error today (ref values always convert);
        // shown without the world's type names, like a log with no DEFINE.
        Value::Ref(e) if e.is_edge() => write!(f, "edge#{}", e.raw() & !(1 << 63)),
        Value::Ref(e) => write!(f, "#{}#{}", e.node_type().0, e.seq()),
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
