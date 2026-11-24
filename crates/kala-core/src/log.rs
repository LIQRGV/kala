//! The log file: the world's only source of truth and its write-ahead log.
//!
//! Everything else (ID tables, schema, indexes) is a copy rebuilt from it.
//! Only committed transactions count; [`read_log`] returns exactly those.
//!
//! # Byte layout (format version 6)
//!
//! All integers are little-endian. A `str` is `u32 len | len bytes of UTF-8`.
//!
//! ```text
//! file    = header txn*
//! header  = magic "KALALOG\0" (8 bytes) | version u32           (12 bytes)
//! txn     = begin body
//! begin   = tag u8 = 0x07 | tx u64 | body_len u64 | body_crc u32
//!           | begin_crc u32                                      (25 bytes)
//! body    = frame*                                               (body_len bytes)
//! frame   = payload_len u32 | payload                            (payload_len bytes)
//! payload = kind u8 | entry_id u64 | entry
//!
//! kind 1  DefineNodeType  id u16 | name str
//! kind 2  DefineAttr      id u32 | name str | value_type u8 | kind u8
//!                         | cardinality u8 | indexed u8 | reverse_cardinality u8
//!                         | direction u8 | has_inverse u8
//!                         | [inverse str, if has_inverse = 1]
//!                         | inferred u8 | deprecated u8
//! kind 3  DefineEntity    id u64 | has_handle u8 | [handle str, if has_handle = 1]
//! kind 4  Fact            entity u64 | attr u32 | tick i64 | op u8
//!                         | precision u8
//!                         | value (Value encoding, the rest of the payload)
//! kind 5  Commit          tx u64 | checksum u32
//! kind 6  DefineEdge      id u64 | source u64 | attr u32 | target u64
//! kind 7  Deleted         of u64 | rev u32 | entity u64 | attr u32 | tick i64
//!                         | value (Value encoding, the rest of the payload)
//! kind 8  Revised         of u64 | rev u32 | entity u64 | attr u32 | tick i64
//!                         | op u8 | precision u8
//!                         | value (Value encoding, the rest of the payload)
//!
//! kind 9  Schema          edit u8 | fields (below)
//!
//! Schema edit 0: attribute id u32 | definition
//! Schema edit 1: node type id u16 | name str | inferred u8
//! Schema edit 2: setting key str | setting value str
//! Genesis may also contain Schema edit 2 world settings, validated exactly
//! like live settings and effective at tx#0. Existing genesis encodings are unchanged.
//!
//! Schema edit 3: migration source attr id u32 | definition | skip_unconvertible u8
//! `definition` is precisely the kind-2 fields after its id, including name,
//! inferred and deprecated. Edit 0 declares the next attribute ID or revises an
//! existing ID. Edit 1 declares the next node type ID or confirms its origin.
//! Genesis kind-1 node types are always explicitly declared.
//!
//! value_type: 1 bool, 2 i64, 3 f64, 4 str, 5 ref (the Value tags)
//! kind:       0 state, 1 event
//! cardinality 0 one,   1 many
//! reverse_cardinality: 0 none, 1 one, 2 many
//! op:         0 assert, 1 retract
//! precision:  0 exact, 1 day, 2 month, 3 year, 4 decade, 5 century
//! direction:  0 none (non-ref), 1 directed, 2 bidirectional
//! has_handle, indexed, has_inverse, inferred, deprecated, skip_unconvertible: 0 no, 1 yes
//! ```
//!
//! Every byte of a payload must be consumed and every enum byte must be one
//! listed above; anything else makes the entry unparseable. A `DefineEdge`
//! id must have the edge tag (bit 63) set and a non-zero sequence.
//!
//! **IDs are defined before use.** Inside a transaction the entries come in
//! operation order: schema revisions, entity definitions, edge definitions and
//! facts may interleave; an ID is always defined before use. A newly interned
//! edge precedes its fact. A moved correction's Deleted/Revised pair is adjacent.
//! The Commit is last. Genesis contains kinds 1 and 2 and optional kind 9/edit 2 settings, then Commit.
//! An edge `(source, attr, target)` is defined (with the next edge
//! sequence, counted across the whole log) in the transaction that first
//! stores an assert (a `Fact` or a `Revised`) of that `ref` triple, or a
//! verified migration copies its retract-only history (see below), so every
//! asserted `ref` triple and every fact on an edge (an edge property)
//! follows its edge's `DefineEdge`.
//!
//! **Corrections.** A `Deleted` entry is a `DELETED` marker: it says that,
//! from its transaction on, the key `(entity, attr, value, tick)` holds
//! nothing (readers skip that whole tick for that value). It points at the
//! base `Fact` entry `of` of the revision chain it corrects, never at
//! another correction, and carries the chain's next revision `rev` (1 for
//! the first correction). It sits at the chain's current key: the key of
//! the chain's current fact (the base, or its last `Revised`), stored as the
//! fact is (a bidirectional pair normalised, `entity <= value`). A `Revised`
//! entry is the chain's new current fact: at the same key with another op
//! or another precision (its own revision), or at another key directly
//! after the `Deleted` of the old key, sharing that `Deleted`'s revision.
//! The original entry is never touched.
//!
//! **Precision.** A `Fact` or `Revised` entry carries the
//! [`Precision`] of its tick (vague time) right after its op. It is a
//! payload, never part of the entry's key: two entries that differ only in
//! precision are at the same key.
//!
//! **Schema validation.** Names resolve through the latest table at every canon;
//! definitions, origins, deprecation and settings are revisioned by transaction.
//! A structural redefinition is refused after any fact entry for that ID. New
//! inferred definitions require infer mode and the exact inference defaults.
//! New names containing `@` are refused except the migration-generated suffix.
//! `kala_` schema names are always reserved; `_KALA_` names may originate only
//! at genesis. These rules cover node types, attributes and inverse names.
//! `edge` is reserved only for node types (the `edge#N` ID syntax).
//!
//! **Migration.** Edit 3 announces a copy-forward operation. Replay regenerates
//! the exact marker and following edit-0 definitions, edge definitions and Fact
//! entries from the history visible immediately before the marker (including
//! earlier operations of its transaction). Every generated payload must match,
//! in order, before Commit. Only these verified copies may contain a Fact that
//! normal write judgement would call a no-op, or an edge with only retracts.
//! This preserves original change points after corrections, without permitting
//! arbitrary copies or schema sequences. Deprecated attributes allow corrections
//! but refuse new asserts and retracts. Only verified migration copies may append
//! plain facts for deprecated edge properties. Live validation and replay agree.
//!
//! **Versions.** Versions 1 through 5 are [`Error::UnsupportedVersion`]; there
//! is no dual-format reading. A world written by an older Kala is carried
//! across with that older binary's `kala dump WORLD -o world.dump.edn` and
//! this binary's `kala transact --create NEW_WORLD world.dump.edn`, which
//! reproduces the same entry IDs and tx numbers. The old file is never
//! modified.
//!
//! **Transactions.** Every transaction is a begin record followed by its
//! body. The begin record is framing, not a log entry: it has no entry ID
//! and [`read_log`] never returns it. `begin_crc` is CRC32 (IEEE,
//! `crc32fast`) over the record's first 21 bytes, `body_len` the byte length
//! of the body and `body_crc` CRC32 over all of it. The body is the
//! transaction's entries, the last one being its only `Commit`.
//!
//! # The checksum chain (new in version 6)
//!
//! A transaction has three checksums. Two are **local** and say only "these
//! bytes arrived whole": `begin_crc` (the begin record) and `body_crc` (the
//! body). The third, the `Commit` checksum, is **chained**:
//!
//! ```text
//! link(-1) = CRC32(file header, all 12 bytes)
//! link(n)  = CRC32( link(n-1) as u32 LE
//!                 | body of tx n up to, not including, its checksum field )
//! Commit(n).checksum = link(n)
//! ```
//!
//! The covered part of the body is every frame before the `Commit` plus the
//! `Commit` frame's length prefix, kind, entry ID and tx; the begin record
//! is not covered (it is derived from the body). So `link(n)` depends on the
//! header and on every byte of every body up to tx n, and the genesis
//! transaction chains from the header.
//!
//! Why the chain sits in the `Commit` and the local checksums stay local:
//!
//! - **Recovery is unchanged.** Torn and damaged are judged from `begin_crc`
//!   and `body_crc` alone (rules 2, 3, 5, 6 below), which need no earlier
//!   transaction. A chain mismatch is only ever seen on a body whose
//!   `body_crc` is right, i.e. bytes that arrived exactly as some writer
//!   sent them: that is never an interrupted append, so it is rule 7,
//!   [`Error::Corrupt`], never a tail to cut.
//! - **Replay checks every link.** The scan carries `link(n-1)` forward and
//!   recomputes `link(n)` for each transaction. Replacing any transaction
//!   but the last by other bytes, even a perfectly valid transaction of the
//!   same length with the same tx and entry IDs, changes its link, and the
//!   next transaction's stored `Commit` checksum no longer matches. (The
//!   last transaction has no successor: replacing it gives another valid
//!   log, which only a handle that had applied the original can notice.)
//! - **One transaction proves the prefix.** A handle remembers a
//!   [`ChainTip`]: where the last transaction it applied starts and ends,
//!   its tx and `link(n)`. To learn whether the log it knows is still a
//!   prefix of the file, it re-reads just those bytes ([`chain_tip_holds`]):
//!   a sound begin record for that tx, a matching `body_crc`, and a `Commit`
//!   carrying the remembered checksum. Any writer that rewrote an earlier
//!   transaction had to write a different link there (replay would refuse
//!   its log otherwise), and a link's four bytes feed the next link through
//!   a CRC, which maps different 4-byte prefixes of otherwise equal input
//!   to different results: the difference reaches every later `Commit`
//!   whose own bytes are the same, including the tip's. The cost is the
//!   size of one transaction, whatever the log's length.
//!
//! What it is not: authentication. CRC32 is 32 bits and anyone can compute
//! it, so a program that edits the file on purpose can forge a chain, and
//! two *different* histories agree on a link by accident with probability
//! about 2^-32. The chain is for cooperating Kala writers (a recovery cut or
//! an fsync-failure truncation followed by new commits, a file replaced by
//! a restored copy) and for storage damage.
//!
//! **Numbering.** Entry IDs start at 1 and grow by one per entry across the whole
//! log; tx numbers start at 0 (the genesis transaction holding the schema)
//! and grow by one per transaction, and a `Commit`'s tx is its begin
//! record's. A fact's tx is the tx of its transaction.
//!
//! # Recovery: torn tail or corruption
//!
//! A log is scanned transaction by transaction from just past the header,
//! expecting tx 0 first. Each one is judged by the first rule that applies:
//!
//! 1. No bytes left: a clean end.
//! 2. Fewer than 25 bytes left: **torn**.
//! 3. `begin_crc` doesn't match: **torn** if every byte from here to the end
//!    of the file is `0x00` (a zero-filled tail, how some file systems expose
//!    an append that never reached the disk), else [`Error::Corrupt`].
//! 4. A tag other than `0x07`, or a tx other than the expected one:
//!    [`Error::Corrupt`].
//! 5. `body_len` is more than the bytes left after the begin record: **torn**
//!    (the last append was cut short).
//! 6. `body_crc` doesn't match: **damaged**. Every byte is there but they
//!    are wrong. If a valid begin record (tag `0x07`, matching `begin_crc`,
//!    a tx greater than the damaged one) starts at *any* byte offset after
//!    the damaged body, later transactions may be committed behind the
//!    damage: [`Error::Corrupt`]. Otherwise the damaged transaction is the
//!    last one and is cut like a torn tail: it was never acknowledged (its
//!    fsync can't have returned before the power went, and sectors may reach
//!    the disk out of order).
//! 7. The body's content is invalid (frames not exactly filling `body_len`,
//!    a frame declaring a payload longer than [`MAX_ENTRY_BYTES`] (checked
//!    before whether it fits, so it is never taken for a short frame), an
//!    unparseable entry, no `Commit`, more than one or not last, a
//!    `Commit` tx other than the begin record's, a `Commit` checksum that
//!    isn't the chained checksum of this body after the previous
//!    transaction's (see "The checksum chain"), an entry ID out of
//!    sequence): [`Error::Corrupt`].
//! 8. Otherwise the transaction is committed, and the scan moves on.
//!
//! **Torn** and **damaged** both mean the valid prefix ends where that
//! transaction starts: only the last append can be incomplete, because each
//! one is fsynced before the next starts. [`read_log`] ignores such a tail;
//! `World::open` saves it to a quarantine file and then cuts it, and its
//! [`RecoveryReport::cause`] says which of the two it was ([`CutCause`]).
//! Corruption is reported with the file left untouched. Because a damaged
//! length field is caught by `begin_crc`, damaged content by `body_crc`, and
//! a damaged transaction is only cut when no valid begin record follows it,
//! damage never makes committed transactions look like an incomplete tail.
//!
//! [`CutCause`]: crate::world::CutCause
//! [`RecoveryReport::cause`]: crate::world::RecoveryReport::cause

use std::path::Path;

use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, NodeTypeId};
use crate::schema::{AttrDef, SchemaEdit};
use crate::tick::{Precision, Tick};
use crate::value::Value;
use crate::world::{Op, TxId};

// One submodule per responsibility; everything other modules use is
// re-exported here, so paths stay `crate::log::…`.
mod decode; // payload bytes -> `LogEntry`
mod encode; // `NewEntry`s -> one transaction's bytes
mod fault; // injected append failures (test hooks)
mod file; // opening and reading the log file safely
mod format; // constants and enum bytes shared by both directions
mod scan; // judging transactions: recovery rules 1 to 8
mod survey; // doctor's collecting scan
mod writer; // the append side: create, open + cut, append

pub(crate) use encode::{EncodedTxn, NewEntry, encode_txn, revised_payload_len};
#[cfg(feature = "test-hooks")]
pub use fault::{LogFault, LogHook, on_log_hook};
pub(crate) use file::read_log_file;
pub(crate) use survey::{SurveyedTxn, TaggedFinding, survey};
pub use writer::DEFAULT_BUSY_TIMEOUT;
pub(crate) use writer::{LogWriter, Tail, WriterLock, file_len, read_tail_at};

/// First 8 bytes of every log file.
pub const LOG_MAGIC: [u8; 8] = *b"KALALOG\0";

/// The log format version this build writes and reads (header bytes 8..12).
pub const LOG_FORMAT_VERSION: u32 = 6;

/// The longest entry payload a log frame may hold: 16 MiB. A write whose
/// entry would be longer is refused before anything is written
/// ([`Error::EntryTooLarge`] for a value, judged against a `Revised` entry
/// carrying it; see `revised_payload_len`), and reading a frame that
/// declares a longer payload is [`Error::Corrupt`] (a doctor finding), never
/// a torn tail: no writer produces one, so it is damage. It also bounds what
/// one frame can make a reader allocate.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

/// A log entry's position in the whole log: 1, 2, 3, ... in log order,
/// never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(pub u64);

/// What one log entry says.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryBody {
    /// A schema revision or world setting, in operation order.
    Schema(SchemaEdit),
    /// Declares a node type.
    DefineNodeType {
        /// Its ID.
        id: NodeTypeId,
        /// Its name.
        name: String,
    },
    /// Declares an attribute.
    DefineAttr {
        /// Its ID.
        id: AttrId,
        /// Its declaration.
        def: AttrDef,
    },
    /// Creates an entity, with an optional handle.
    DefineEntity {
        /// Its ID (node type plus sequence number).
        id: EntityId,
        /// Its handle, if any.
        handle: Option<String>,
    },
    /// Interns an edge: gives the triple `(source, attr, target)` its edge
    /// ID. Written in the transaction that first asserts that `ref` fact,
    /// before the fact.
    DefineEdge {
        /// Its ID (the next edge sequence number).
        id: EdgeId,
        /// The entity (or edge) the `ref` fact is on.
        source: EntityId,
        /// The `ref` attribute.
        attr: AttrId,
        /// The fact's value: the entity (or edge) referred to.
        target: EntityId,
    },
    /// An assert or retract at a story tick. Its tx is the enclosing
    /// transaction's.
    Fact {
        /// The entity written.
        entity: EntityId,
        /// The attribute written.
        attr: AttrId,
        /// The value asserted or retracted.
        value: Value,
        /// Story tick of the change.
        tick: Tick,
        /// Assert or retract.
        op: Op,
        /// How precisely the tick is known (a payload, not part of the key).
        precision: Precision,
    },
    /// A `DELETED` marker (a correction): from its transaction on, the key
    /// `(entity, attr, value, tick)` holds nothing. Revision `rev` of the
    /// base `Fact` entry `of`; written at that chain's current key.
    Deleted {
        /// The base `Fact` entry of the corrected chain.
        of: EntryId,
        /// The chain's revision this correction makes (1 for the first).
        rev: u32,
        /// The entity of the deleted key (as stored: a bidirectional pair
        /// normalised).
        entity: EntityId,
        /// The attribute of the deleted key.
        attr: AttrId,
        /// The value of the deleted key.
        value: Value,
        /// The story tick of the deleted key.
        tick: Tick,
    },
    /// A revised fact (a correction): the new current fact of the chain of
    /// base `Fact` entry `of`, as revision `rev`. Either at the chain's
    /// current key with the op changed, or at a new key right after the
    /// `Deleted` marker of the old one (sharing its `rev`).
    Revised {
        /// The base `Fact` entry of the corrected chain.
        of: EntryId,
        /// The chain's revision this correction makes.
        rev: u32,
        /// The entity of the new fact (as stored: a bidirectional pair
        /// normalised).
        entity: EntityId,
        /// The attribute of the new fact.
        attr: AttrId,
        /// The value of the new fact.
        value: Value,
        /// The story tick of the new fact.
        tick: Tick,
        /// Assert or retract.
        op: Op,
        /// How precisely the tick is known (a payload, not part of the key).
        precision: Precision,
    },
    /// Ends a transaction.
    Commit {
        /// The transaction's number.
        tx: TxId,
        /// The chained checksum: CRC32 over the previous transaction's
        /// `Commit` checksum and this transaction's bytes (see "The
        /// checksum chain" in the module docs).
        checksum: u32,
    },
}

/// One log entry: its ID and what it says.
#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    /// The entry's position in the log.
    pub id: EntryId,
    /// The entry itself.
    pub body: EntryBody,
}

/// Reads the entries of every committed transaction in the log at `path`,
/// in log order. Never modifies the file.
///
/// Fails with [`Error::Io`] if the file can't be read, [`Error::BadMagic`]
/// if it isn't a Kala log, [`Error::UnsupportedVersion`] for another format
/// version (every older one included: there is no migration), or
/// [`Error::Corrupt`] for a truncated header or any damage the recovery
/// rules in the module docs don't classify as a torn tail.
///
/// A torn tail (a last transaction cut short, or a zero-filled tail) is not
/// returned and not an error; it is left in the file (only
/// [`World::open`](crate::World::open) cuts it). Takes no lock, so it can
/// read a log while a [`World`](crate::World) has it open.
///
/// Only a regular file (or a symlink to one) is read: a directory, FIFO,
/// device or socket is refused with [`Error::Io`] of kind `InvalidInput`
/// before anything is read (see `read_log_file`).
pub fn read_log(path: &Path) -> Result<Vec<LogEntry>, Error> {
    let bytes = read_log_file(path)?;
    Ok(scan::scan(&bytes)?.entries)
}

/// Where a log's checksum chain stands: the last committed transaction a
/// handle has applied. Re-reading just that transaction
/// ([`chain_tip_holds`]) tells whether the log the handle knows is still a
/// prefix of the file (see "The checksum chain" in the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainTip {
    /// The transaction's number.
    pub tx: TxId,
    /// Byte offset of its begin record in the file.
    pub offset: u64,
    /// Byte offset just past its body: where the next transaction starts.
    pub end: u64,
    /// Its `Commit` checksum: the chained checksum of the whole log up to
    /// and including this transaction.
    pub checksum: u32,
}

/// Whether the log file at `path` still holds, at `tip.offset..tip.end`,
/// the committed transaction `tip` describes: a sound begin record for
/// `tip.tx` whose body fills exactly that range and matches its `body_crc`,
/// ending in a `Commit` for `tip.tx` that carries `tip.checksum`. Because
/// that checksum is chained, `true` means every byte before `tip.end` is
/// what the handle that recorded `tip` applied (up to CRC32's strength; see
/// the module docs). `false` means the file diverged: it is shorter, or
/// those bytes are another transaction, or damaged.
///
/// Reads only that transaction, never the rest of the file, takes no lock
/// and modifies nothing. Fails with [`Error::Io`] if the file can't be read
/// (only a regular file is opened), or [`Error::BadMagic`] /
/// [`Error::UnsupportedVersion`] / [`Error::Corrupt`] for a header that
/// isn't this format's: then it isn't the same log at all.
pub fn chain_tip_holds(path: &Path, tip: &ChainTip) -> Result<bool, Error> {
    let Some(read) = file::read_range(path, tip.offset, tip.end)? else {
        return Ok(false);
    };
    scan::check_header(&read.header)?;
    Ok(scan::is_committed_txn(&read.bytes, tip.tx.0, tip.checksum))
}

/// The link the genesis transaction chains from: CRC32 of the whole file
/// header (magic and format version).
pub(crate) fn header_checksum() -> u32 {
    let mut hash = crc32fast::Hasher::new();
    hash.update(&LOG_MAGIC);
    hash.update(&LOG_FORMAT_VERSION.to_le_bytes());
    hash.finalize()
}

/// A transaction's chained checksum: CRC32 over the previous link (four
/// little-endian bytes), then `covered`, the transaction's body up to its
/// `Commit`'s checksum field.
pub(crate) fn chained_checksum(previous: u32, covered: &[u8]) -> u32 {
    let mut hash = crc32fast::Hasher::new();
    hash.update(&previous.to_le_bytes());
    hash.update(covered);
    hash.finalize()
}
