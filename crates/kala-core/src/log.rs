//! The log file: the world's only source of truth and its write-ahead log.
//!
//! Everything else (ID tables, schema, indexes) is a copy rebuilt from it.
//! Only committed transactions count; [`read_log`] returns exactly those.
//!
//! # Byte layout (format version 4)
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
//! value_type: 1 bool, 2 i64, 3 f64, 4 str, 5 ref (the Value tags)
//! kind:       0 state, 1 event
//! cardinality 0 one,   1 many
//! reverse_cardinality: 0 none, 1 one, 2 many
//! op:         0 assert, 1 retract
//! precision:  0 exact, 1 day, 2 month, 3 year, 4 decade, 5 century
//! direction:  0 none (non-ref), 1 directed, 2 bidirectional
//! has_handle, indexed, has_inverse: 0 no, 1 yes
//! ```
//!
//! Every byte of a payload must be consumed and every enum byte must be one
//! listed above; anything else makes the entry unparseable. A `DefineEdge`
//! id must have the edge tag (bit 63) set and a non-zero sequence.
//!
//! **IDs are defined before use.** Inside a transaction the entries come in
//! this order: `DefineEntity`s, then `DefineEdge`s, then `Fact`s,
//! `Deleted` markers and `Revised` facts in write order, then the `Commit`.
//! An edge `(source, attr, target)` is defined (with the next edge
//! sequence, counted across the whole log) in the transaction that first
//! stores an assert (a `Fact` or a `Revised`) of that `ref` triple, so every
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
//! **Versions.** Format version 2 had no corrections and version 3 no
//! precision; a version 1, 2 or 3 file is [`Error::UnsupportedVersion`]
//! (no migration).
//!
//! **Transactions.** Every transaction is a begin record followed by its
//! body. The begin record is framing, not a log entry: it has no entry ID
//! and [`read_log`] never returns it. `begin_crc` is CRC32 (IEEE,
//! `crc32fast`) over the record's first 21 bytes, `body_len` the byte length
//! of the body and `body_crc` CRC32 over all of it. The body is the
//! transaction's entries, the last one being its only `Commit`. The `Commit`
//! checksum is CRC32 over every byte of the body before the `Commit` frame,
//! plus the `Commit` frame's bytes up to (not including) the checksum field,
//! i.e. its length prefix, kind, entry ID and tx; it doesn't cover the begin
//! record. Entry IDs start at 1 and grow by one per entry across the whole
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
//!    `Commit` tx other than the begin record's, a `Commit` checksum mismatch,
//!    an entry ID out of sequence): [`Error::Corrupt`].
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

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::doctor::Finding;
use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, NodeTypeId};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection};
use crate::tick::{Precision, Tick};
use crate::value::{Value, ValueType};
use crate::world::{CutCause, Op, RecoveryReport, TxId};

/// First 8 bytes of every log file.
pub const LOG_MAGIC: [u8; 8] = *b"KALALOG\0";

/// The log format version this build writes and reads (header bytes 8..12).
pub const LOG_FORMAT_VERSION: u32 = 4;

/// The longest entry payload a log frame may hold: 16 MiB. A write whose
/// entry would be longer is refused before anything is written
/// ([`Error::EntryTooLarge`] for a value, judged against a `Revised` entry
/// carrying it; see `revised_payload_len`), and reading a frame that
/// declares a longer payload is [`Error::Corrupt`] (a doctor finding), never
/// a torn tail: no writer produces one, so it is damage. It also bounds what
/// one frame can make a reader allocate.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

/// Payload bytes of a `Revised` entry besides its value: kind, entry ID,
/// of, rev, entity, attr, tick, op, precision. The largest fixed part of any
/// entry that carries a value (`Fact` and `Deleted` are smaller).
const REVISED_FIXED_LEN: usize = 1 + 8 + 8 + 4 + 8 + 4 + 8 + 1 + 1;

/// The payload length of a `Revised` entry whose value encoding is
/// `value_len` bytes: the size a value is judged by against
/// [`MAX_ENTRY_BYTES`], so any value accepted can later be corrected.
pub(crate) fn revised_payload_len(value_len: usize) -> usize {
    REVISED_FIXED_LEN.saturating_add(value_len)
}

/// Header length: magic plus version.
const HEADER_LEN: usize = 12;

const KIND_DEFINE_NODE_TYPE: u8 = 1;
const KIND_DEFINE_ATTR: u8 = 2;
const KIND_DEFINE_ENTITY: u8 = 3;
const KIND_FACT: u8 = 4;
const KIND_COMMIT: u8 = 5;
const KIND_DEFINE_EDGE: u8 = 6;
const KIND_DELETED: u8 = 7;
const KIND_REVISED: u8 = 8;

/// A log entry's position in the whole log: 1, 2, 3, ... in log order,
/// never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(pub u64);

/// What one log entry says.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryBody {
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
        /// CRC32 of the transaction's bytes (see the module docs).
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
/// version (including versions 1 and 2: there is no migration), or
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
/// before anything is read (see [`read_log_file`]).
pub fn read_log(path: &Path) -> Result<Vec<LogEntry>, Error> {
    let bytes = read_log_file(path)?;
    Ok(scan(&bytes)?.entries)
}

/// Reads the bytes of the log file at `path`, for the readers that take no
/// lock ([`read_log`], [`World::open_read_only`](crate::World::open_read_only),
/// doctor). Refuses anything but a regular file as [`open_regular`] does,
/// and reads at most the length the opened file had (a snapshot: bytes a
/// writer appends meanwhile are left for the next read).
pub(crate) fn read_log_file(path: &Path) -> Result<Vec<u8>, Error> {
    let mut file = open_regular(path, OpenOptions::new().read(true))?;
    read_to_len(&mut file)
}

/// Opens `path` with `opts`, refusing anything but a regular file (after
/// following symlinks) with [`Error::Io`] of kind `InvalidInput`.
///
/// Two checks, because a log reader must never hang or read without bound:
/// - Before opening, the path's metadata: opening a FIFO for reading
///   blocks until a writer shows up, so a FIFO (or a device such as
///   `/dev/zero`, which never ends) must be turned away before `open`.
/// - After opening, the handle's own metadata (`fstat`): that is the file
///   actually read, so a path swapped between the two calls is still
///   refused. (Only a swap to a FIFO in that window could still make the
///   `open` itself wait; that needs write access to the directory.)
fn open_regular(path: &Path, opts: &OpenOptions) -> Result<File, Error> {
    refuse_non_regular(&std::fs::metadata(path)?)?;
    let file = opts.open(path)?;
    refuse_non_regular(&file.metadata()?)?;
    Ok(file)
}

/// `Err(Io { kind: InvalidInput })` unless `meta` is a regular file's.
fn refuse_non_regular(meta: &std::fs::Metadata) -> Result<(), Error> {
    let ft = meta.file_type();
    if ft.is_file() {
        return Ok(());
    }
    let what = if ft.is_dir() {
        "a directory"
    } else if ft.is_symlink() {
        "a symlink"
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if ft.is_fifo() {
                "a FIFO"
            } else if ft.is_char_device() || ft.is_block_device() {
                "a device"
            } else if ft.is_socket() {
                "a socket"
            } else {
                "not a regular file"
            }
        }
        #[cfg(not(unix))]
        {
            "not a regular file"
        }
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("not a regular file ({what})"),
    )
    .into())
}

/// Reads `file` from its current position up to the length its metadata
/// gives now, never more: a file that grows while being read (or claims an
/// endless length) can't make this read without bound. The buffer is
/// reserved fallibly, so a length too big for memory is an error, not an
/// abort.
fn read_to_len(file: &mut File) -> Result<Vec<u8>, Error> {
    let len = file.metadata()?.len();
    let too_big = || {
        Error::from(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!("log file of {len} bytes doesn't fit in memory"),
        ))
    };
    let cap = usize::try_from(len).map_err(|_| too_big())?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(cap).map_err(|_| too_big())?;
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The committed part of a log file, as found by [`scan`].
struct Scan {
    /// Entries of every committed transaction, in log order.
    entries: Vec<LogEntry>,
    /// Length of the valid prefix: the header plus every committed
    /// transaction. Bytes past it are a torn or damaged tail.
    valid_len: usize,
    /// Why the bytes past `valid_len` aren't committed; `None` if there are
    /// none.
    cause: Option<CutCause>,
}

/// Checks the header and returns the bytes after it.
fn check_header(bytes: &[u8]) -> Result<&[u8], Error> {
    let magic_len = bytes.len().min(LOG_MAGIC.len());
    if bytes[..magic_len] != LOG_MAGIC[..magic_len] {
        return Err(Error::BadMagic);
    }
    let Some((header, rest)) = bytes.split_first_chunk::<HEADER_LEN>() else {
        return Err(Error::Corrupt("truncated log header".to_owned()));
    };
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != LOG_FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    Ok(rest)
}

/// Splits the next frame off `bytes`: `(payload, rest)`, or `None` if the
/// frame is incomplete. The length is checked against the bytes left.
fn next_frame(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, after_len) = bytes.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    if after_len.len() < len {
        return None;
    }
    Some(after_len.split_at(len))
}

/// Scans a whole log file's bytes: see [`read_log`]. This is the one
/// decoder of the log; reading and reopening both go through it, and
/// doctor's [`survey`] judges transactions with the same two steps
/// ([`begin_step`], [`body_step`]).
///
/// Transactions are judged one at a time from just past the header, by the
/// rules in the module docs ("Recovery"). The first one that is torn ends
/// the valid prefix ([`Scan::valid_len`]); anything else that isn't a
/// committed transaction is [`Error::Corrupt`].
fn scan(bytes: &[u8]) -> Result<Scan, Error> {
    let mut rest = check_header(bytes)?;
    let mut entries = Vec::new();
    let mut next_entry = 1u64;
    let mut next_tx = 0u64;
    let cause = loop {
        let at = bytes.len() - rest.len();
        let (begin, after_begin) = match begin_step(rest) {
            BeginStep::End => break None,
            BeginStep::Torn => break Some(CutCause::Torn),
            BeginStep::Damaged => {
                return Err(Error::Corrupt(format!(
                    "damaged transaction header at offset {at}"
                )));
            }
            BeginStep::UnknownTag(tag) => {
                return Err(Error::Corrupt(format!(
                    "unknown record tag {tag:#04x} at offset {at}"
                )));
            }
            BeginStep::Begin(begin, after_begin) => (begin, after_begin),
        };
        // Rule 4: a sound begin record that isn't the next transaction's.
        if begin.tx != next_tx {
            return Err(Error::Corrupt(format!(
                "tx#{} found at offset {at} where tx#{next_tx} was expected",
                begin.tx
            )));
        }
        let BodyStep::Body {
            verdict,
            rest: after_body,
        } = body_step(&begin, after_begin)
        else {
            break Some(CutCause::Torn);
        };
        let mut txn = match verdict {
            Ok(txn) => txn,
            Err(BodyFault::Damaged { later: None }) => break Some(CutCause::Damaged),
            Err(BodyFault::Damaged { later: Some(later) }) => {
                return Err(Error::Corrupt(format!(
                    "tx#{}: body checksum mismatch at offset {at}, followed by a valid \
                     begin record at offset {}",
                    begin.tx,
                    bytes.len() - after_body.len() + later
                )));
            }
            Err(fault) => return Err(Error::Corrupt(format!("tx#{}: {fault}", begin.tx))),
        };
        // Rule 7, last part: entry IDs continue the log without gaps.
        for (e, expected) in txn.iter().zip(next_entry..) {
            if e.id.0 != expected {
                return Err(Error::Corrupt(format!(
                    "tx#{}: entry#{} found where entry#{expected} was expected",
                    begin.tx, e.id.0
                )));
            }
        }
        // Rule 8: committed.
        next_entry += txn.len() as u64;
        next_tx += 1;
        entries.append(&mut txn);
        rest = after_body;
    };
    Ok(Scan {
        entries,
        valid_len: bytes.len() - rest.len(),
        cause,
    })
}

/// The first half of judging the transaction at the start of `rest`
/// (recovery rules 1 to 4, all but the tx check, which is the caller's).
enum BeginStep<'a> {
    /// Rule 1: no bytes left, a clean end.
    End,
    /// Rule 2 (not even a whole begin record), or rule 3 with a zero-filled
    /// tail: the last append never fully reached the disk.
    Torn,
    /// Rule 3: a begin record whose checksum fails, not a zero-filled tail.
    /// None of its fields can be trusted, so nothing after it can be framed.
    Damaged,
    /// Rule 4: a sound begin record with a tag other than `0x07`.
    UnknownTag(u8),
    /// A sound begin record and the bytes after it.
    Begin(Begin, &'a [u8]),
}

fn begin_step(rest: &[u8]) -> BeginStep<'_> {
    if rest.is_empty() {
        return BeginStep::End;
    }
    let Some((begin, after_begin)) = rest.split_first_chunk::<BEGIN_LEN>() else {
        return BeginStep::Torn;
    };
    let begin = Begin::parse(begin);
    // Only a zero-filled tail (how some file systems expose an append that
    // never reached the disk) is a torn write; anything else may hide
    // committed data behind it.
    if !begin.crc_ok {
        return if rest.iter().all(|&b| b == 0) {
            BeginStep::Torn
        } else {
            BeginStep::Damaged
        };
    }
    if begin.tag != BEGIN_TAG {
        return BeginStep::UnknownTag(begin.tag);
    }
    BeginStep::Begin(begin, after_begin)
}

/// The second half of judging a transaction with a sound begin record
/// (recovery rules 5 to 7, all but the entry-ID check, which is the
/// caller's: the scan and doctor treat gaps differently).
enum BodyStep<'a> {
    /// Rule 5: the body runs past the end of the file.
    Torn,
    /// The body is all there: its parsed entries (the `Commit` last) or why
    /// they aren't a committed transaction, and the bytes after the body.
    Body {
        verdict: Result<Vec<LogEntry>, BodyFault>,
        rest: &'a [u8],
    },
}

/// Why a whole transaction body (rule 6 or 7) isn't a committed transaction.
#[derive(Debug)]
enum BodyFault {
    /// Rule 6: `body_crc` fails. `later` is the offset, from just past the
    /// body, of a later valid begin record, if any: without one the
    /// transaction is a damaged tail.
    Damaged { later: Option<usize> },
    /// Rule 7: the body has no `Commit` (its frames end without one).
    NoCommit,
    /// Rule 7: the `Commit` checksum doesn't match.
    CommitChecksum,
    /// Rule 7: anything else; `frame` is the index of the offending frame.
    Invalid { frame: usize, why: String },
}

impl std::fmt::Display for BodyFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyFault::Damaged { .. } => f.write_str("body checksum mismatch"),
            BodyFault::NoCommit => f.write_str("no Commit"),
            BodyFault::CommitChecksum => f.write_str("Commit checksum mismatch"),
            BodyFault::Invalid { why, .. } => f.write_str(why),
        }
    }
}

fn body_step<'a>(begin: &Begin, after_begin: &'a [u8]) -> BodyStep<'a> {
    // The length is checked against the bytes left before it is used.
    let Some(body_len) = usize::try_from(begin.body_len)
        .ok()
        .filter(|&n| n <= after_begin.len())
    else {
        return BodyStep::Torn;
    };
    let (body, rest) = after_begin.split_at(body_len);
    // Every byte is there but they don't match: damage. Cut only if nothing
    // that could be a later committed transaction follows.
    let verdict = if crc32fast::hash(body) != begin.body_crc {
        Err(BodyFault::Damaged {
            later: later_begin(rest, begin.tx),
        })
    } else {
        parse_body(body, begin.tx)
    };
    BodyStep::Body { verdict, rest }
}

/// The offset in `bytes` of the first valid begin record (tag `0x07`,
/// matching `begin_crc`) whose tx is greater than `tx`, trying every byte
/// offset: a damaged transaction followed by one may hide committed data, so
/// it must not be cut (recovery rule 6).
fn later_begin(bytes: &[u8], tx: u64) -> Option<usize> {
    (0..bytes.len().saturating_sub(BEGIN_LEN - 1)).find(|&i| {
        // The tag is checked first so most offsets cost no checksum.
        bytes[i] == BEGIN_TAG
            && bytes[i..]
                .first_chunk::<BEGIN_LEN>()
                .map(Begin::parse)
                .is_some_and(|b| b.crc_ok && b.tx > tx)
    })
}

/// Length of a transaction's begin record.
const BEGIN_LEN: usize = 25;

/// Tag byte of a begin record.
const BEGIN_TAG: u8 = 0x07;

/// Bytes of a begin record covered by its own checksum.
const BEGIN_COVERED: usize = 21;

/// A decoded begin record (no field is trusted unless `crc_ok`).
struct Begin {
    tag: u8,
    tx: u64,
    body_len: u64,
    body_crc: u32,
    /// Whether the record's own checksum matches its first 21 bytes.
    crc_ok: bool,
}

impl Begin {
    fn parse(b: &[u8; BEGIN_LEN]) -> Begin {
        let u64_at = |i: usize| {
            let mut x = [0u8; 8];
            x.copy_from_slice(&b[i..i + 8]);
            u64::from_le_bytes(x)
        };
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Begin {
            tag: b[0],
            tx: u64_at(1),
            body_len: u64_at(9),
            body_crc: u32_at(17),
            crc_ok: crc32fast::hash(&b[..BEGIN_COVERED]) == u32_at(BEGIN_COVERED),
        }
    }
}

/// Parses a transaction body whose framing checked out: frames exactly
/// filling it, exactly one `Commit`, last, for `tx`, with a matching
/// checksum. Returns its entries, the `Commit` included. Entry IDs are not
/// checked here (see [`BodyStep`]).
fn parse_body(body: &[u8], tx: u64) -> Result<Vec<LogEntry>, BodyFault> {
    let mut entries = Vec::new();
    let mut rest = body;
    loop {
        let invalid = |why: String| BodyFault::Invalid {
            frame: entries.len(),
            why,
        };
        // Checked before the frame is split off: a declared length over the
        // limit is damage whatever bytes follow (no writer produces one), so
        // it is never mistaken for a frame cut short.
        if let Some(len) = rest.first_chunk::<4>().map(|b| u32::from_le_bytes(*b))
            && usize::try_from(len).map_or(true, |len| len > MAX_ENTRY_BYTES)
        {
            return Err(invalid(format!(
                "a frame declares a {len}-byte entry, over the {MAX_ENTRY_BYTES}-byte limit"
            )));
        }
        let Some((payload, after)) = next_frame(rest) else {
            return Err(if rest.is_empty() {
                BodyFault::NoCommit
            } else {
                invalid("a frame runs past the end of the transaction".to_owned())
            });
        };
        rest = after;
        let entry = parse_payload(payload).map_err(invalid)?;
        if let EntryBody::Commit { tx: ctx, checksum } = entry.body {
            if !rest.is_empty() {
                return Err(invalid("bytes after the Commit".to_owned()));
            }
            if ctx.0 != tx {
                return Err(invalid(format!("its Commit says tx#{}", ctx.0)));
            }
            // Covered: every byte of the body except the Commit's own
            // checksum field, its last 4 bytes (a parsed Commit frame is
            // 25 bytes, so the body is at least that long).
            if crc32fast::hash(&body[..body.len() - 4]) != checksum {
                return Err(BodyFault::CommitChecksum);
            }
            entries.push(entry);
            return Ok(entries);
        }
        entries.push(entry);
    }
}

/// One transaction [`survey`] found whole and parseable, for doctor to
/// replay: its tx (from its begin record) and its entries, `Commit` last.
#[derive(Debug)]
pub(crate) struct SurveyedTxn {
    pub(crate) tx: TxId,
    pub(crate) entries: Vec<LogEntry>,
}

/// A finding of [`survey`], tagged with the tx it belongs to (doctor's
/// `since` filter).
pub(crate) type TaggedFinding = (Option<TxId>, Finding);

/// Doctor's scan of a whole log file's bytes: the same judgement as
/// [`scan`], but collecting instead of failing fast. Never modifies
/// anything.
///
/// Fails only where nothing can be scanned: [`Error::BadMagic`],
/// [`Error::UnsupportedVersion`] or [`Error::Corrupt`] for a truncated
/// header. Otherwise every problem is a finding, and the scan goes on past
/// a bad transaction whenever its begin record is sound (its `body_len` is
/// trusted to skip it): a body checksum mismatch with a valid begin record
/// after it, a `Commit` checksum mismatch, a missing `Commit` or any other
/// invalid body is reported and that transaction skipped; a tx out of
/// sequence (a [`Finding::TxGap`]) or entry IDs out of sequence (an
/// [`Finding::EntryIdGap`]) are reported and the transaction kept, unless it
/// would rewind (a repeated tx or entry ID), when it is skipped too. After a
/// skipped transaction the next one's first entry ID is taken as it comes
/// (the skipped one's IDs can't be trusted). The scan stops at a tail
/// `World::open` would cut ([`Finding::IncompleteTail`] at the offset where
/// the valid prefix ends) and at a begin record that can't be trusted.
pub(crate) fn survey(bytes: &[u8]) -> Result<(Vec<SurveyedTxn>, Vec<TaggedFinding>), Error> {
    let mut rest = check_header(bytes)?;
    let mut txns = Vec::new();
    let mut found = Vec::new();
    let mut next_tx = 0u64;
    // `None` right after a skipped transaction: resynchronise.
    let mut next_entry = Some(1u64);
    // Past every entry ID seen: a lower one would rewind the log.
    let mut floor = 1u64;
    loop {
        // An in-memory buffer's offsets always fit a u64.
        let at = (bytes.len() - rest.len()) as u64;
        let expected_tx = TxId(next_tx);
        let (begin, after_begin) = match begin_step(rest) {
            BeginStep::End => break,
            BeginStep::Torn => {
                found.push((Some(expected_tx), Finding::IncompleteTail { offset: at }));
                break;
            }
            BeginStep::Damaged => {
                let f = Finding::ChecksumMismatch {
                    tx: expected_tx,
                    offset: at,
                };
                found.push((Some(expected_tx), f));
                break;
            }
            BeginStep::UnknownTag(tag) => {
                let f = Finding::InvalidEntry {
                    entry: EntryId(next_entry.unwrap_or(floor)),
                    reason: format!("unknown record tag {tag:#04x} at offset {at}"),
                };
                found.push((Some(expected_tx), f));
                break;
            }
            BeginStep::Begin(begin, after_begin) => (begin, after_begin),
        };
        let tx = TxId(begin.tx);
        // A repeated or backwards tx is judged at the expected one, so
        // `since` can't hide it behind an older number.
        let tag = Some(tx.max(expected_tx));
        let mut keep = true;
        if tx != expected_tx {
            let f = Finding::TxGap {
                expected: expected_tx,
                found: tx,
            };
            found.push((tag, f));
            keep = tx > expected_tx;
        }
        let BodyStep::Body {
            verdict,
            rest: after_body,
        } = body_step(&begin, after_begin)
        else {
            found.push((tag, Finding::IncompleteTail { offset: at }));
            break;
        };
        rest = after_body;
        next_tx = next_tx.max(begin.tx.saturating_add(1));
        let entries = match verdict {
            Ok(entries) => entries,
            Err(BodyFault::Damaged { later: None }) => {
                found.push((tag, Finding::IncompleteTail { offset: at }));
                break;
            }
            Err(fault) => {
                let f = match fault {
                    BodyFault::Damaged { .. } | BodyFault::CommitChecksum => {
                        Finding::ChecksumMismatch { tx, offset: at }
                    }
                    BodyFault::NoCommit => Finding::MissingCommit { tx, offset: at },
                    BodyFault::Invalid { frame, why } => Finding::InvalidEntry {
                        entry: EntryId(next_entry.unwrap_or(floor).saturating_add(frame as u64)),
                        reason: format!("tx#{}: {why}", tx.0),
                    },
                };
                found.push((tag, f));
                next_entry = None;
                continue;
            }
        };
        for e in &entries {
            if let Some(expected) = next_entry
                && e.id.0 != expected
            {
                let f = Finding::EntryIdGap {
                    expected: EntryId(expected),
                    found: e.id,
                };
                found.push((tag, f));
            }
            if e.id.0 < floor {
                // Kept entries must ascend (doctor's rebuild finds them by ID).
                keep = false;
            }
            floor = floor.max(e.id.0.saturating_add(1));
            next_entry = Some(e.id.0.saturating_add(1));
        }
        if keep {
            txns.push(SurveyedTxn { tx, entries });
        }
    }
    Ok((txns, found))
}

/// A bounds-checked reader over one payload. Every read fails rather than
/// running past the end.
struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.bytes.len() < n {
            return Err("entry truncated".to_owned());
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn flag(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(format!("flag byte {b} is not 0 or 1")),
        }
    }

    /// A `u32 len | UTF-8` string. The length is checked against the bytes
    /// left before anything is allocated.
    fn string(&mut self) -> Result<String, String> {
        let len = usize::try_from(self.u32()?).map_err(|_| "string length overflows usize")?;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "string is not UTF-8".to_owned())
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.bytes)
    }

    fn finish(&self) -> Result<(), String> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err("trailing bytes in entry".to_owned())
        }
    }
}

/// Parses one frame payload strictly.
fn parse_payload(payload: &[u8]) -> Result<LogEntry, String> {
    let mut c = Cursor { bytes: payload };
    let kind = c.u8()?;
    let id = EntryId(c.u64()?);
    let body = match kind {
        KIND_DEFINE_NODE_TYPE => EntryBody::DefineNodeType {
            id: NodeTypeId(c.u16()?),
            name: c.string()?,
        },
        KIND_DEFINE_ATTR => {
            let id = AttrId(c.u32()?);
            let name = c.string()?;
            let value_type = value_type_from(c.u8()?)?;
            let kind = match c.u8()? {
                0 => Kind::State,
                1 => Kind::Event,
                b => return Err(format!("unknown attribute kind {b}")),
            };
            let cardinality = match c.u8()? {
                0 => Cardinality::One,
                1 => Cardinality::Many,
                b => return Err(format!("unknown cardinality {b}")),
            };
            let indexed = c.flag()?;
            let reverse_cardinality = match c.u8()? {
                0 => None,
                1 => Some(Cardinality::One),
                2 => Some(Cardinality::Many),
                b => return Err(format!("unknown reverse cardinality {b}")),
            };
            let direction = match c.u8()? {
                0 => None,
                1 => Some(RefDirection::Directed),
                2 => Some(RefDirection::Bidirectional),
                b => return Err(format!("unknown direction {b}")),
            };
            let inverse = if c.flag()? { Some(c.string()?) } else { None };
            EntryBody::DefineAttr {
                id,
                def: AttrDef {
                    name,
                    value_type,
                    kind,
                    cardinality,
                    indexed,
                    direction,
                    inverse,
                    reverse_cardinality,
                },
            }
        }
        KIND_DEFINE_ENTITY => {
            let id = EntityId::from_bytes(c.u64()?.to_be_bytes());
            let handle = if c.flag()? { Some(c.string()?) } else { None };
            EntryBody::DefineEntity { id, handle }
        }
        KIND_FACT => {
            let entity = EntityId::from_bytes(c.u64()?.to_be_bytes());
            let attr = AttrId(c.u32()?);
            let tick = Tick(i64::from_le_bytes(c.array()?));
            let op = op_from(c.u8()?)?;
            let precision = precision_from(c.u8()?)?;
            let value = Value::decode(c.rest()).map_err(|e| format!("fact value: {e}"))?;
            EntryBody::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            }
        }
        KIND_COMMIT => EntryBody::Commit {
            tx: TxId(c.u64()?),
            checksum: c.u32()?,
        },
        KIND_DEFINE_EDGE => {
            let raw = c.u64()?;
            let id = EntityId::from_bytes(raw.to_be_bytes())
                .as_edge()
                .ok_or_else(|| format!("edge id {raw:#018x} is not an edge ID"))?;
            EntryBody::DefineEdge {
                id,
                source: EntityId::from_bytes(c.u64()?.to_be_bytes()),
                attr: AttrId(c.u32()?),
                target: EntityId::from_bytes(c.u64()?.to_be_bytes()),
            }
        }
        KIND_DELETED => {
            let of = EntryId(c.u64()?);
            let rev = c.u32()?;
            let entity = EntityId::from_bytes(c.u64()?.to_be_bytes());
            let attr = AttrId(c.u32()?);
            let tick = Tick(i64::from_le_bytes(c.array()?));
            let value = Value::decode(c.rest()).map_err(|e| format!("deleted value: {e}"))?;
            EntryBody::Deleted {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
            }
        }
        KIND_REVISED => {
            let of = EntryId(c.u64()?);
            let rev = c.u32()?;
            let entity = EntityId::from_bytes(c.u64()?.to_be_bytes());
            let attr = AttrId(c.u32()?);
            let tick = Tick(i64::from_le_bytes(c.array()?));
            let op = op_from(c.u8()?)?;
            let precision = precision_from(c.u8()?)?;
            let value = Value::decode(c.rest()).map_err(|e| format!("revised value: {e}"))?;
            EntryBody::Revised {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            }
        }
        b => return Err(format!("unknown entry kind {b}")),
    };
    c.finish()?;
    Ok(LogEntry { id, body })
}

fn op_from(b: u8) -> Result<Op, String> {
    match b {
        0 => Ok(Op::Assert),
        1 => Ok(Op::Retract),
        _ => Err(format!("unknown op {b}")),
    }
}

fn precision_from(b: u8) -> Result<Precision, String> {
    Precision::from_byte(b).ok_or_else(|| format!("unknown precision {b}"))
}

fn op_byte(op: Op) -> u8 {
    match op {
        Op::Assert => 0,
        Op::Retract => 1,
    }
}

fn value_type_from(b: u8) -> Result<ValueType, String> {
    Ok(match b {
        1 => ValueType::Bool,
        2 => ValueType::I64,
        3 => ValueType::F64,
        4 => ValueType::Str,
        5 => ValueType::Ref,
        _ => return Err(format!("unknown value type {b}")),
    })
}

fn value_type_byte(t: ValueType) -> u8 {
    match t {
        ValueType::Bool => 1,
        ValueType::I64 => 2,
        ValueType::F64 => 3,
        ValueType::Str => 4,
        ValueType::Ref => 5,
    }
}

/// An entry to append, borrowing from the caller (the writer's view of
/// [`EntryBody`]; `Commit` is added by the writer itself). Fact, `Deleted`
/// and `Revised` values are already encoded.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NewEntry<'a> {
    NodeType {
        id: NodeTypeId,
        name: &'a str,
    },
    Attr {
        id: AttrId,
        def: &'a AttrDef,
    },
    Entity {
        id: EntityId,
        handle: Option<&'a str>,
    },
    Edge {
        id: EdgeId,
        source: EntityId,
        attr: AttrId,
        target: EntityId,
    },
    Fact {
        entity: EntityId,
        attr: AttrId,
        value: &'a [u8],
        tick: Tick,
        op: Op,
        precision: Precision,
    },
    Deleted {
        of: EntryId,
        rev: u32,
        entity: EntityId,
        attr: AttrId,
        value: &'a [u8],
        tick: Tick,
    },
    Revised {
        of: EntryId,
        rev: u32,
        entity: EntityId,
        attr: AttrId,
        value: &'a [u8],
        tick: Tick,
        op: Op,
        precision: Precision,
    },
}

/// Too big for a log entry: refused before anything is written. Values are
/// refused earlier, by the write, with [`Error::EntryTooLarge`]; this is
/// what is left (a name or handle that long).
fn too_big(what: &str) -> Error {
    Error::Io {
        kind: std::io::ErrorKind::InvalidInput,
        message: format!("{what} is too large for a log entry (over {MAX_ENTRY_BYTES} bytes)"),
    }
}

fn push_str(buf: &mut Vec<u8>, s: &str) -> Result<(), Error> {
    let len = u32::try_from(s.len()).map_err(|_| too_big("a string"))?;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Appends one frame (`kind`, `id`, then whatever `body` writes) to `buf`.
fn push_frame(
    buf: &mut Vec<u8>,
    kind: u8,
    id: u64,
    body: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
) -> Result<(), Error> {
    let start = buf.len();
    buf.extend_from_slice(&[0; 4]); // length, patched below
    buf.push(kind);
    buf.extend_from_slice(&id.to_le_bytes());
    body(buf)?;
    // A frame no reader would accept is never written.
    let len = buf.len() - start - 4;
    if len > MAX_ENTRY_BYTES {
        return Err(too_big("an entry"));
    }
    let len = u32::try_from(len).map_err(|_| too_big("an entry"))?;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

impl NewEntry<'_> {
    /// What the entry says once appended: the [`EntryBody`] [`read_log`]
    /// returns for it.
    fn to_body(self) -> EntryBody {
        match self {
            NewEntry::NodeType { id, name } => EntryBody::DefineNodeType {
                id,
                name: name.to_owned(),
            },
            NewEntry::Attr { id, def } => EntryBody::DefineAttr {
                id,
                def: def.clone(),
            },
            NewEntry::Entity { id, handle } => EntryBody::DefineEntity {
                id,
                handle: handle.map(str::to_owned),
            },
            NewEntry::Edge {
                id,
                source,
                attr,
                target,
            } => EntryBody::DefineEdge {
                id,
                source,
                attr,
                target,
            },
            NewEntry::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => EntryBody::Fact {
                entity,
                attr,
                // Invariant: fact values are only ever encoded by
                // `Value::encode` (validated writes), so they decode.
                value: Value::decode(value)
                    .expect("invariant: a fact to append holds a canonical value encoding"),
                tick,
                op,
                precision,
            },
            NewEntry::Deleted {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
            } => EntryBody::Deleted {
                of,
                rev,
                entity,
                attr,
                // Invariant: as for a fact, the value is the encoding of a
                // stored fact's value.
                value: Value::decode(value)
                    .expect("invariant: a DELETED marker holds a canonical value encoding"),
                tick,
            },
            NewEntry::Revised {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => EntryBody::Revised {
                of,
                rev,
                entity,
                attr,
                // Invariant: a revised value is validated and encoded by
                // `Value::encode`, as for a fact.
                value: Value::decode(value)
                    .expect("invariant: a revised fact holds a canonical value encoding"),
                tick,
                op,
                precision,
            },
        }
    }
}

/// One encoded transaction, ready to append: its bytes and the entries
/// they hold, as [`read_log`] will return them.
#[derive(Debug)]
pub(crate) struct EncodedTxn {
    /// Begin record plus body (no file header).
    pub(crate) bytes: Vec<u8>,
    /// Its entries in log order, the `Commit` (with its checksum) last.
    pub(crate) entries: Vec<LogEntry>,
}

/// Encodes one transaction: its begin record, then its body (its entries'
/// frames, then its `Commit` for `tx`), numbering entries from `first_id`.
/// Encoding touches no file, so an in-memory world numbers and checksums
/// its entries exactly as a file-backed one. Fails only for an entry too
/// large for its length fields.
pub(crate) fn encode_txn<'a>(
    first_id: EntryId,
    tx: TxId,
    entries: impl IntoIterator<Item = NewEntry<'a>>,
) -> Result<EncodedTxn, Error> {
    let mut bytes = Vec::new();
    // The frame encoders below all write through this one buffer.
    let buf = &mut bytes;
    let mut listed = Vec::new();
    let begin = buf.len();
    buf.extend_from_slice(&[0; BEGIN_LEN]); // patched once the body is known
    let start = buf.len();
    let mut id = first_id.0;
    for entry in entries {
        listed.push(LogEntry {
            id: EntryId(id),
            body: entry.to_body(),
        });
        match entry {
            NewEntry::NodeType { id: t, name } => {
                push_frame(buf, KIND_DEFINE_NODE_TYPE, id, |b| {
                    b.extend_from_slice(&t.0.to_le_bytes());
                    push_str(b, name)
                })?
            }
            NewEntry::Attr { id: a, def } => push_frame(buf, KIND_DEFINE_ATTR, id, |b| {
                b.extend_from_slice(&a.0.to_le_bytes());
                push_str(b, &def.name)?;
                b.push(value_type_byte(def.value_type));
                b.push(match def.kind {
                    Kind::State => 0,
                    Kind::Event => 1,
                });
                b.push(match def.cardinality {
                    Cardinality::One => 0,
                    Cardinality::Many => 1,
                });
                b.push(u8::from(def.indexed));
                b.push(match def.reverse_cardinality {
                    None => 0,
                    Some(Cardinality::One) => 1,
                    Some(Cardinality::Many) => 2,
                });
                b.push(match def.direction {
                    None => 0,
                    Some(RefDirection::Directed) => 1,
                    Some(RefDirection::Bidirectional) => 2,
                });
                match &def.inverse {
                    Some(inverse) => {
                        b.push(1);
                        push_str(b, inverse)
                    }
                    None => {
                        b.push(0);
                        Ok(())
                    }
                }
            })?,
            NewEntry::Entity { id: e, handle } => push_frame(buf, KIND_DEFINE_ENTITY, id, |b| {
                b.extend_from_slice(&e.raw().to_le_bytes());
                match handle {
                    Some(h) => {
                        b.push(1);
                        push_str(b, h)
                    }
                    None => {
                        b.push(0);
                        Ok(())
                    }
                }
            })?,
            NewEntry::Edge {
                id: e,
                source,
                attr,
                target,
            } => push_frame(buf, KIND_DEFINE_EDGE, id, |b| {
                b.extend_from_slice(&e.raw().to_le_bytes());
                b.extend_from_slice(&source.raw().to_le_bytes());
                b.extend_from_slice(&attr.0.to_le_bytes());
                b.extend_from_slice(&target.raw().to_le_bytes());
                Ok(())
            })?,
            NewEntry::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => push_frame(buf, KIND_FACT, id, |b| {
                b.extend_from_slice(&entity.raw().to_le_bytes());
                b.extend_from_slice(&attr.0.to_le_bytes());
                b.extend_from_slice(&tick.0.to_le_bytes());
                b.push(op_byte(op));
                b.push(precision.to_byte());
                b.extend_from_slice(value);
                Ok(())
            })?,
            NewEntry::Deleted {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
            } => push_frame(buf, KIND_DELETED, id, |b| {
                b.extend_from_slice(&of.0.to_le_bytes());
                b.extend_from_slice(&rev.to_le_bytes());
                b.extend_from_slice(&entity.raw().to_le_bytes());
                b.extend_from_slice(&attr.0.to_le_bytes());
                b.extend_from_slice(&tick.0.to_le_bytes());
                b.extend_from_slice(value);
                Ok(())
            })?,
            NewEntry::Revised {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => push_frame(buf, KIND_REVISED, id, |b| {
                b.extend_from_slice(&of.0.to_le_bytes());
                b.extend_from_slice(&rev.to_le_bytes());
                b.extend_from_slice(&entity.raw().to_le_bytes());
                b.extend_from_slice(&attr.0.to_le_bytes());
                b.extend_from_slice(&tick.0.to_le_bytes());
                b.push(op_byte(op));
                b.push(precision.to_byte());
                b.extend_from_slice(value);
                Ok(())
            })?,
        }
        id += 1;
    }
    push_frame(buf, KIND_COMMIT, id, |b| {
        b.extend_from_slice(&tx.0.to_le_bytes());
        b.extend_from_slice(&[0; 4]); // checksum, patched below
        Ok(())
    })?;
    let end = buf.len();
    let checksum = crc32fast::hash(&buf[start..end - 4]);
    buf[end - 4..].copy_from_slice(&checksum.to_le_bytes());

    // `end - start` is the length of an in-memory buffer, so it fits a u64.
    let body_len = (end - start) as u64;
    let body_crc = crc32fast::hash(&buf[start..end]);
    let rec = &mut buf[begin..start];
    rec[0] = BEGIN_TAG;
    rec[1..9].copy_from_slice(&tx.0.to_le_bytes());
    rec[9..17].copy_from_slice(&body_len.to_le_bytes());
    rec[17..21].copy_from_slice(&body_crc.to_le_bytes());
    let begin_crc = crc32fast::hash(&rec[..BEGIN_COVERED]);
    rec[BEGIN_COVERED..].copy_from_slice(&begin_crc.to_le_bytes());
    listed.push(LogEntry {
        id: EntryId(id),
        body: EntryBody::Commit { tx, checksum },
    });
    Ok(EncodedTxn {
        bytes,
        entries: listed,
    })
}

/// A failure to inject into the next log append, for tests of the
/// writer's failure handling (see `World::inject_log_fault`). Not part of
/// the supported API.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFault {
    /// The next append's fsync fails after its bytes were written.
    FailNextSync,
    /// The next append's write fails before any byte is written.
    FailNextWrite,
}

/// The error an injected [`LogFault`] produces.
fn injected(what: &str) -> std::io::Error {
    std::io::Error::other(format!("injected log fault: {what} failed"))
}

/// The append side of an open log file.
///
/// It holds an exclusive lock on the file for its whole lifetime (released
/// when the file handle is closed, i.e. when the writer is dropped), so only
/// one [`World`](crate::World) at a time appends to, or cuts, a log. The lock
/// is **advisory** (`flock`-style): it keeps out other Kala worlds, in this
/// process or another, but not programs that ignore it. [`read_log`] takes
/// no lock.
#[derive(Debug)]
pub(crate) struct LogWriter {
    /// The log file, locked exclusively.
    file: File,
    /// Where the log file is: doctor re-reads it from disk.
    path: PathBuf,
    /// The file's length: just past the last committed transaction. A
    /// transaction whose fsync failed is cut back to it.
    len: u64,
    /// Set by a failed append or fsync: the file may end in a torn
    /// transaction, so nothing more is appended until the world is reopened.
    failed: Option<Error>,
    /// Faults injected by tests, consumed by the next append that reaches
    /// them. Both `false` in normal use.
    fail_next_write: bool,
    fail_next_sync: bool,
}

impl LogWriter {
    /// Creates a new log file at `path` (failing if anything exists there)
    /// holding the header and `genesis`, the encoded genesis transaction
    /// tx#0 (see [`encode_txn`]), and makes it durable: the file's data and
    /// the directory entry are fsynced.
    pub(crate) fn create(path: &Path, genesis: &EncodedTxn) -> Result<LogWriter, Error> {
        let mut buf = Vec::with_capacity(HEADER_LEN + genesis.bytes.len());
        buf.extend_from_slice(&LOG_MAGIC);
        buf.extend_from_slice(&LOG_FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&genesis.bytes);
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)?;
        lock(&file)?;
        file.write_all(&buf)?;
        file.sync_data()?;
        sync_parent_dir(path)?;
        Ok(LogWriter {
            file,
            path: path.to_owned(),
            // An in-memory buffer's length always fits a u64.
            len: buf.len() as u64,
            failed: None,
            fail_next_write: false,
            fail_next_sync: false,
        })
    }

    /// Opens an existing log file at `path` for reading and appending (never
    /// creating it), passes the entries of its committed transactions (in
    /// log order) to `replay`, and returns `replay`'s result with a writer
    /// that appends after them.
    ///
    /// The file is locked exclusively before anything is read; if another
    /// writer holds the lock this fails with [`Error::WorldLocked`] and the
    /// file is untouched.
    ///
    /// Fails as [`read_log`] does, or with `replay`'s error. Only once both
    /// succeeded is a torn tail (bytes past the last committed transaction,
    /// see the module docs) cut off, so the next append lands right after
    /// the last commit. The cut bytes are first saved, durably, to a
    /// quarantine file (see [`quarantine`]); if that fails, the open fails
    /// with [`Error::Io`] and the log is not cut. On any error the log is
    /// untouched. A clean log is never modified, so opening is idempotent.
    /// The returned [`RecoveryReport`] says what was cut and where it went.
    ///
    /// With `cut_damaged` false, a *damaged* tail (see
    /// [`CutCause::Damaged`]) is not cut: the open fails with
    /// [`Error::RecoveryRequired`] under the lock, before anything is
    /// quarantined or cut, and the file is untouched. Torn tails are cut
    /// either way.
    pub(crate) fn open<T>(
        path: &Path,
        replay: impl FnOnce(&[LogEntry]) -> Result<T, Error>,
        cut_damaged: bool,
    ) -> Result<(T, LogWriter, RecoveryReport), Error> {
        // One handle for reading, cutting and appending, so the bytes
        // replayed are the bytes appended after. `append` without `create`
        // never creates the file.
        // Only a regular file: see `open_regular`.
        let mut file = open_regular(path, OpenOptions::new().read(true).append(true))?;
        lock(&file)?;
        // The length is taken under the lock, so no writer appends past it.
        let bytes = read_to_len(&mut file)?;
        let Scan {
            entries,
            valid_len,
            cause,
        } = scan(&bytes)?;
        if !cut_damaged && valid_len != bytes.len() && cause == Some(CutCause::Damaged) {
            return Err(Error::RecoveryRequired {
                bytes: (bytes.len() - valid_len) as u64,
            });
        }
        let replayed = replay(&entries)?;
        let mut report = RecoveryReport::default();
        if valid_len != bytes.len() {
            // Keep the cut bytes before cutting: if the tail was misjudged
            // torn, nothing committed is lost for good.
            let tail = &bytes[valid_len..];
            report.quarantine = Some(quarantine(path, valid_len, tail)?);
            // `valid_len <= bytes.len()`, a `usize` that came from an
            // in-memory buffer, so it fits a `u64`. The size change is
            // metadata: `sync_all` makes it durable. Appends (O_APPEND) then
            // start at the new end.
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
            report.bytes_cut = tail.len() as u64;
            report.cause = cause;
        }
        Ok((
            replayed,
            LogWriter {
                file,
                path: path.to_owned(),
                len: valid_len as u64,
                failed: None,
                fail_next_write: false,
                fail_next_sync: false,
            },
            report,
        ))
    }

    /// The read-only counterpart of [`LogWriter::open`]: reads the log file
    /// at `path` (never creating it) without taking the lock, passes the
    /// entries of its committed transactions to `replay`, and returns
    /// `replay`'s result with the [`RecoveryReport`] that `open` would
    /// produce for the same bytes, minus the quarantine: `bytes_cut` and
    /// `cause` say what `open` would cut, `quarantine` is `None`. Nothing is
    /// written, cut or created.
    ///
    /// Fails exactly as `open` does before it would modify the file (as
    /// [`read_log`] does, or with `replay`'s error); never with
    /// [`Error::WorldLocked`].
    pub(crate) fn read_committed<T>(
        path: &Path,
        replay: impl FnOnce(&[LogEntry]) -> Result<T, Error>,
    ) -> Result<(T, RecoveryReport), Error> {
        let bytes = read_log_file(path)?;
        let Scan {
            entries,
            valid_len,
            cause,
        } = scan(&bytes)?;
        let replayed = replay(&entries)?;
        let mut report = RecoveryReport::default();
        if valid_len != bytes.len() {
            // As in `open`: a `usize` length of an in-memory buffer fits a u64.
            report.bytes_cut = (bytes.len() - valid_len) as u64;
            report.cause = cause;
        }
        Ok((replayed, report))
    }

    /// The path the log file was created or opened at.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `Err` if an earlier append failed: the world refuses further writes.
    pub(crate) fn check(&self) -> Result<(), Error> {
        match &self.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Arms a fault for the next append (see [`LogFault`]).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn inject_fault(&mut self, fault: LogFault) {
        match fault {
            LogFault::FailNextWrite => self.fail_next_write = true,
            LogFault::FailNextSync => self.fail_next_sync = true,
        }
    }

    /// Appends one encoded transaction (from [`encode_txn`], numbered to
    /// continue the log) with a single write, then fsyncs. Returns only
    /// once the transaction is durable. On failure the writer is marked
    /// failed and refuses further appends. If the write went through but the
    /// fsync failed, the file is first cut back (best effort) to its length
    /// before the transaction: the caller is told the transaction failed, so
    /// it must not reappear on reopen if the bytes reach the disk anyway. A
    /// failed write may leave a torn tail, which reopening cuts.
    pub(crate) fn append(&mut self, txn: &EncodedTxn) -> Result<(), Error> {
        self.check()?;
        let buf = &txn.bytes;
        let res = if std::mem::take(&mut self.fail_next_write) {
            Err(injected("write"))
        } else {
            self.file.write_all(buf)
        };
        let res = res.and_then(|()| {
            let synced = if std::mem::take(&mut self.fail_next_sync) {
                Err(injected("fsync"))
            } else {
                self.file.sync_data()
            };
            if synced.is_err() {
                // Best effort: if the cut fails too, the writer is failed
                // anyway and reopening judges whatever reached the disk.
                let _ = self.file.set_len(self.len);
                let _ = self.file.sync_data();
            }
            synced
        });
        if let Err(e) = res {
            let err = Error::from(e);
            self.failed = Some(match &err {
                Error::Io { kind, message } => Error::Io {
                    kind: *kind,
                    message: format!(
                        "an earlier log append failed ({message}); reopen the world to recover"
                    ),
                },
                other => other.clone(),
            });
            return Err(err);
        }
        // An in-memory buffer's length always fits a u64.
        self.len += buf.len() as u64;
        Ok(())
    }
}

/// Takes the exclusive advisory lock on a log file without waiting:
/// [`Error::WorldLocked`] if another handle holds it.
fn lock(file: &File) -> Result<(), Error> {
    file.try_lock().map_err(|e| match e {
        TryLockError::WouldBlock => Error::WorldLocked,
        TryLockError::Error(e) => Error::from(e),
    })
}

/// Durably saves `tail`, the bytes about to be cut off the log at `path`
/// after its first `valid_len` bytes, to a new file next to it:
/// `<path>.torn-<valid_len>`, or `<path>.torn-<valid_len>-1`, `-2`, ... if
/// that exists (an earlier quarantine is never overwritten). The file and
/// its directory entry are fsynced before this returns, so the log may be
/// cut afterwards. Returns the new file's path. On failure the new file is
/// removed (best effort).
fn quarantine(path: &Path, valid_len: usize, tail: &[u8]) -> Result<PathBuf, Error> {
    let base = {
        let mut s = OsString::from(path.as_os_str());
        s.push(format!(".torn-{valid_len}"));
        s
    };
    let mut suffix = 0u64;
    let (qpath, mut qfile) = loop {
        let mut candidate = base.clone();
        if suffix > 0 {
            candidate.push(format!("-{suffix}"));
        }
        let candidate = PathBuf::from(candidate);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => break (candidate, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix = suffix.checked_add(1).ok_or_else(|| {
                    Error::from(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
                })?;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let res = qfile
        .write_all(tail)
        .and_then(|()| qfile.sync_all())
        .map_err(Error::from)
        .and_then(|()| sync_parent_dir(&qpath));
    if let Err(e) = res {
        drop(qfile);
        // Best effort: the log is left untouched either way.
        let _ = std::fs::remove_file(&qpath);
        return Err(e);
    }
    Ok(qpath)
}

/// Fsyncs the directory holding `path`, so a newly created file's directory
/// entry survives a crash.
fn sync_parent_dir(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
