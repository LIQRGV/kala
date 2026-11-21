//! The log file: the world's only source of truth and its write-ahead log.
//!
//! Everything else (ID tables, schema, indexes) is a copy rebuilt from it.
//! Only committed transactions count; [`read_log`] returns exactly those.
//!
//! # Byte layout (format version 1)
//!
//! All integers are little-endian. A `str` is `u32 len | len bytes of UTF-8`.
//!
//! ```text
//! file    = header frame*
//! header  = magic "KALALOG\0" (8 bytes) | version u32           (12 bytes)
//! frame   = payload_len u32 | payload                            (payload_len bytes)
//! payload = kind u8 | entry_id u64 | body
//!
//! kind 1  DefineNodeType  id u16 | name str
//! kind 2  DefineAttr      id u32 | name str | value_type u8 | kind u8
//!                         | cardinality u8 | indexed u8
//! kind 3  DefineEntity    id u64 | has_handle u8 | [handle str, if has_handle = 1]
//! kind 4  Fact            entity u64 | attr u32 | tick i64 | op u8
//!                         | value (Value encoding, the rest of the payload)
//! kind 5  Commit          tx u64 | checksum u32
//!
//! value_type: 1 bool, 2 i64, 3 f64, 4 str, 5 ref (the Value tags)
//! kind:       0 state, 1 event
//! cardinality 0 one,   1 many
//! op:         0 assert, 1 retract
//! has_handle, indexed: 0 no, 1 yes
//! ```
//!
//! Every byte of a payload must be consumed and every enum byte must be one
//! listed above; anything else makes the frame unparseable.
//!
//! **Transactions.** A transaction is a run of consecutive frames ending in a
//! `Commit`. Its checksum is CRC32 (IEEE, `crc32fast`) over every byte of the
//! transaction's frames before the `Commit` frame, plus the `Commit` frame's
//! bytes up to (not including) the checksum field, i.e. its length prefix,
//! kind, entry ID and tx. Entry IDs start at 1 and grow by one per entry
//! across the whole log; tx numbers start at 0 (the genesis transaction
//! holding the schema) and grow by one per transaction. A fact's tx is the
//! tx of the `Commit` that ends its transaction.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::id::{AttrId, EntityId, NodeTypeId};
use crate::schema::{AttrDef, Cardinality, Kind};
use crate::tick::Tick;
use crate::value::{Value, ValueType};
use crate::world::{Op, TxId};

/// First 8 bytes of every log file.
pub const LOG_MAGIC: [u8; 8] = *b"KALALOG\0";

/// The log format version this build writes and reads (header bytes 8..12).
pub const LOG_FORMAT_VERSION: u32 = 1;

/// Header length: magic plus version.
const HEADER_LEN: usize = 12;

const KIND_DEFINE_NODE_TYPE: u8 = 1;
const KIND_DEFINE_ATTR: u8 = 2;
const KIND_DEFINE_ENTITY: u8 = 3;
const KIND_FACT: u8 = 4;
const KIND_COMMIT: u8 = 5;

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
/// version, or [`Error::Corrupt`] for a truncated header, a transaction
/// whose checksum matches but whose content is invalid (an unparseable entry,
/// an entry ID or tx out of sequence), a committed transaction found
/// after a damaged one, or a tail holding a frame shaped like the `Commit`
/// of a transaction newer than a torn one could be: that is damage or a
/// bug, not a torn write.
///
/// A torn tail (an incomplete last transaction, or a bad checksum on the
/// last one) is not returned and not an error; it is left in the file
/// (only [`World::open`](crate::World::open) cuts it). See `scan` in the
/// source for the exact torn-versus-corrupt rule. Takes no lock, so it
/// can read a log while a [`World`](crate::World) has it open.
pub fn read_log(path: &Path) -> Result<Vec<LogEntry>, Error> {
    let bytes = std::fs::read(path)?;
    Ok(scan(&bytes)?.entries)
}

/// The committed part of a log file, as found by [`scan`].
struct Scan {
    /// Entries of every committed transaction, in log order.
    entries: Vec<LogEntry>,
    /// Length of the valid prefix: the header plus every committed
    /// transaction. Bytes past it are a torn or damaged tail.
    valid_len: usize,
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
/// decoder of the log; reading and reopening both go through it.
///
/// # Where the valid prefix ends: torn tail or corruption
///
/// The valid prefix is the header plus every transaction up to the first
/// one that doesn't end in a checksum-valid `Commit` (an incomplete frame,
/// a frame length running past the end of the file, no `Commit` before the
/// end, or a `Commit` whose checksum doesn't match). What follows is judged
/// by parsing on, frame by frame, to the end of the file:
///
/// - If a later `Commit` has a matching checksum, a committed transaction
///   sits behind the damage: [`Error::Corrupt`]. Dropping it would silently
///   lose committed data. The checksum is tried over two spans: from just
///   past the previous `Commit` frame, and from just past the last frame
///   that didn't parse (so a transaction is still found when the damage hit
///   the `Commit` frame before it).
/// - Otherwise it is a **torn tail**, the trace of an append cut short by a
///   crash (only the last transaction can be torn: every append is fsynced
///   before the next starts). [`Scan::valid_len`] says where to cut.
///
/// A transaction whose checksum matches but whose content is invalid (an
/// unparseable entry, an entry ID or tx out of sequence) is
/// [`Error::Corrupt`] wherever it is.
///
/// - Otherwise, before calling it torn, every byte offset of the tail is
///   searched for a frame shaped like a **later** `Commit` (see
///   [`find_later_commit`]): if a crash cut the last append, the tail holds
///   at most that one transaction, so a `Commit` for a newer tx means
///   damage mid-log broke frame lengths and threw parsing out of step with
///   the real frames, with committed transactions behind it:
///   [`Error::Corrupt`].
///
/// **Residual risk.** Without a resynchronisation marker in the format, two
/// cases remain. Damage that also destroys every later `Commit` frame still
/// looks like a torn tail, so those transactions are cut (the cut bytes are
/// kept in a quarantine file by [`LogWriter::open`]). And user data in a
/// genuinely torn transaction that happens to contain a `Commit`-shaped
/// byte pattern makes a torn tail look like corruption: a false refusal,
/// which fails safe (nothing is cut).
fn scan(bytes: &[u8]) -> Result<Scan, Error> {
    let body = check_header(bytes)?;
    let mut out = Vec::new();
    let mut next_entry = 1u64;
    let mut next_tx = 0u64;
    // Just past the last committed transaction: the end of the valid prefix.
    let mut committed = body;
    // The current transaction: its bytes start at `txn`, its parsed entries
    // are `pending`, and `bad` holds the first entry that didn't parse.
    // Parse failures are only judged at the Commit: a bad checksum means a
    // torn or damaged tail, a good one means corruption.
    let mut txn = body;
    let mut pending: Vec<LogEntry> = Vec::new();
    let mut bad: Option<String> = None;
    // Just past the last frame of the current transaction that didn't parse
    // (`txn` if none did): where the next transaction starts if that frame
    // was a damaged Commit.
    let mut resync = body;
    // Set at the first Commit whose checksum doesn't match. From then on
    // nothing more is committed; a later valid Commit means corruption.
    let mut damaged = false;
    // `next_frame` checks each length against the bytes left, so nothing is
    // allocated from an unchecked length.
    let mut rest = body;
    while let Some((payload, after)) = next_frame(rest) {
        rest = after;
        let entry = match parse_payload(payload) {
            Ok(entry) => entry,
            Err(why) => {
                bad.get_or_insert(why);
                resync = rest;
                continue;
            }
        };
        let EntryBody::Commit { tx, checksum } = entry.body else {
            pending.push(entry);
            continue;
        };
        // Covered: every byte from the start of the transaction up to the
        // Commit frame's checksum field, its last 4 bytes. (A parsed Commit
        // frame is 4 + 21 bytes, so every span here is `>= 25` bytes.)
        let covers = |start: &[u8]| {
            let len = start.len() - rest.len();
            crc32fast::hash(&start[..len - 4]) == checksum
        };
        let valid = covers(txn);
        if damaged || !valid {
            if valid || (resync.len() != txn.len() && covers(resync)) {
                return Err(Error::Corrupt(format!(
                    "a committed transaction (tx#{}) follows a damaged one",
                    tx.0
                )));
            }
            damaged = true;
            pending.clear();
            bad = None;
            txn = rest;
            resync = rest;
            continue;
        }
        if let Some(why) = bad.take() {
            return Err(Error::Corrupt(format!("tx#{}: {why}", tx.0)));
        }
        pending.push(entry);
        for e in &pending {
            if e.id.0 != next_entry {
                return Err(Error::Corrupt(format!(
                    "entry#{} found where entry#{next_entry} was expected",
                    e.id.0
                )));
            }
            next_entry += 1;
        }
        if tx.0 != next_tx {
            return Err(Error::Corrupt(format!(
                "tx#{} found where tx#{next_tx} was expected",
                tx.0
            )));
        }
        next_tx += 1;
        out.append(&mut pending);
        committed = rest;
        txn = rest;
        resync = rest;
    }
    let valid_len = bytes.len() - committed.len();
    // `next_entry - 1` is the last committed entry ID (0 if none) and
    // `next_tx` the tx a torn transaction would have had.
    if let Some((at, tx)) = find_later_commit(committed, next_entry - 1, next_tx) {
        return Err(Error::Corrupt(format!(
            "committed data may follow damage at offset {valid_len}: \
             a frame shaped like the Commit of tx#{tx} is at offset {}",
            valid_len + at
        )));
    }
    Ok(Scan {
        entries: out,
        valid_len,
    })
}

/// Length of a `Commit` frame: the length prefix plus its 21-byte payload.
const COMMIT_FRAME_LEN: usize = 4 + COMMIT_PAYLOAD_LEN as usize;

/// Payload length of a `Commit`: kind, entry ID, tx, checksum.
const COMMIT_PAYLOAD_LEN: u32 = 1 + 8 + 8 + 4;

/// Searches every byte offset of `tail` (the bytes past the valid prefix)
/// for a frame shaped like the `Commit` of a transaction **newer** than the
/// torn one: length 21, kind 5, an entry ID in
/// `(last_entry, last_entry + tail.len()]` and a tx in
/// `[torn_tx + 1, torn_tx + tail.len() / 25]`, where `torn_tx` is the tx the
/// torn transaction would have had. Returns its offset in `tail` and its tx.
///
/// Why these bounds: a crash can tear only the last append, so a genuine
/// torn tail holds at most one transaction, whose `Commit` (if it got
/// written) has tx `torn_tx`. A commit for a later tx can only be there if
/// damage hid committed transactions. Each entry is at least one byte of
/// the tail, and each transaction at least one 25-byte `Commit` frame, so
/// a real later `Commit` always falls in these ranges; random bytes
/// (zero padding, garbage) almost never do. The checksum isn't checked: the
/// damage may have hit the very bytes it covers.
fn find_later_commit(tail: &[u8], last_entry: u64, torn_tx: u64) -> Option<(usize, u64)> {
    // usize fits u64 on every supported target; saturate rather than wrap.
    let tail_len = u64::try_from(tail.len()).unwrap_or(u64::MAX);
    let entry_max = last_entry.saturating_add(tail_len);
    let tx_min = torn_tx.saturating_add(1);
    let tx_max = torn_tx.saturating_add(tail_len / COMMIT_FRAME_LEN as u64);
    tail.windows(COMMIT_FRAME_LEN)
        .enumerate()
        .find_map(|(at, w)| {
            let field = |r: std::ops::Range<usize>| {
                let mut b = [0u8; 8];
                b[..r.len()].copy_from_slice(&w[r]);
                u64::from_le_bytes(b)
            };
            let shaped = field(0..4) == u64::from(COMMIT_PAYLOAD_LEN)
                && w[4] == KIND_COMMIT
                && (last_entry + 1..=entry_max).contains(&field(5..13));
            let tx = field(13..21);
            (shaped && (tx_min..=tx_max).contains(&tx)).then_some((at, tx))
        })
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
            EntryBody::DefineAttr {
                id,
                def: AttrDef {
                    name,
                    value_type,
                    kind,
                    cardinality,
                    indexed,
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
            let op = match c.u8()? {
                0 => Op::Assert,
                1 => Op::Retract,
                b => return Err(format!("unknown op {b}")),
            };
            let value = Value::decode(c.rest()).map_err(|e| format!("fact value: {e}"))?;
            EntryBody::Fact {
                entity,
                attr,
                value,
                tick,
                op,
            }
        }
        KIND_COMMIT => EntryBody::Commit {
            tx: TxId(c.u64()?),
            checksum: c.u32()?,
        },
        b => return Err(format!("unknown entry kind {b}")),
    };
    c.finish()?;
    Ok(LogEntry { id, body })
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
/// [`EntryBody`]; `Commit` is added by the writer itself). Fact values are
/// already encoded.
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
    Fact {
        entity: EntityId,
        attr: AttrId,
        value: &'a [u8],
        tick: Tick,
        op: Op,
    },
}

/// Too big for a `u32` length field: refused before anything is written.
fn too_big(what: &str) -> Error {
    Error::Io {
        kind: std::io::ErrorKind::InvalidInput,
        message: format!("{what} is too large for a log entry (over 4 GiB)"),
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
    let len = u32::try_from(buf.len() - start - 4).map_err(|_| too_big("an entry"))?;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Appends the frames of one transaction (its entries, then its `Commit`)
/// to `buf`, numbering entries from `first_id`. Returns the next entry ID.
fn encode_txn<'a>(
    buf: &mut Vec<u8>,
    first_id: u64,
    tx: TxId,
    entries: impl IntoIterator<Item = NewEntry<'a>>,
) -> Result<u64, Error> {
    let start = buf.len();
    let mut id = first_id;
    for entry in entries {
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
                Ok(())
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
            NewEntry::Fact {
                entity,
                attr,
                value,
                tick,
                op,
            } => push_frame(buf, KIND_FACT, id, |b| {
                b.extend_from_slice(&entity.raw().to_le_bytes());
                b.extend_from_slice(&attr.0.to_le_bytes());
                b.extend_from_slice(&tick.0.to_le_bytes());
                b.push(match op {
                    Op::Assert => 0,
                    Op::Retract => 1,
                });
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
    Ok(id + 1)
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
    /// The ID the next appended entry gets.
    next_entry: u64,
    /// Set by a failed append or fsync: the file may end in a torn
    /// transaction, so nothing more is appended until the world is reopened.
    failed: Option<Error>,
}

impl LogWriter {
    /// Creates a new log file at `path` (failing if anything exists there)
    /// holding the header and the genesis transaction tx#0 with the given
    /// schema entries, and makes it durable: the file's data and the
    /// directory entry are fsynced.
    pub(crate) fn create<'a>(
        path: &Path,
        schema: impl IntoIterator<Item = NewEntry<'a>>,
    ) -> Result<LogWriter, Error> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&LOG_MAGIC);
        buf.extend_from_slice(&LOG_FORMAT_VERSION.to_le_bytes());
        let next_entry = encode_txn(&mut buf, 1, TxId(0), schema)?;
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
            next_entry,
            failed: None,
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
    /// see `scan`) cut off, so the next append lands right after the last
    /// commit. The cut bytes are first saved, durably, to a quarantine file
    /// (see [`quarantine`]); if that fails, the open fails with
    /// [`Error::Io`] and the log is not cut. On any error the log is
    /// untouched. A clean log is never modified, so opening is idempotent.
    pub(crate) fn open<T>(
        path: &Path,
        replay: impl FnOnce(&[LogEntry]) -> Result<T, Error>,
    ) -> Result<(T, LogWriter), Error> {
        // One handle for reading, cutting and appending, so the bytes
        // replayed are the bytes appended after. `append` without `create`
        // never creates the file.
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        lock(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let Scan { entries, valid_len } = scan(&bytes)?;
        let replayed = replay(&entries)?;
        if valid_len != bytes.len() {
            // Keep the cut bytes before cutting: if the tail was misjudged
            // torn, nothing committed is lost for good.
            quarantine(path, valid_len, &bytes[valid_len..])?;
            // `valid_len <= bytes.len()`, a `usize` that came from an
            // in-memory buffer, so it fits a `u64`. The size change is
            // metadata: `sync_all` makes it durable. Appends (O_APPEND) then
            // start at the new end.
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        let next_entry = entries.last().map_or(1, |e| e.id.0 + 1);
        Ok((
            replayed,
            LogWriter {
                file,
                next_entry,
                failed: None,
            },
        ))
    }

    /// `Err` if an earlier append failed: the world refuses further writes.
    pub(crate) fn check(&self) -> Result<(), Error> {
        match &self.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Appends one transaction (its entries, then a `Commit` for `tx`) with
    /// a single write, then fsyncs. Returns only once the transaction is
    /// durable; on failure the writer is marked failed (the file may now
    /// end in a torn transaction).
    pub(crate) fn append<'a>(
        &mut self,
        tx: TxId,
        entries: impl IntoIterator<Item = NewEntry<'a>>,
    ) -> Result<(), Error> {
        self.check()?;
        let mut buf = Vec::new();
        // Encoding fails only for an oversized entry, before any byte is
        // written, so the writer stays usable.
        let next_entry = encode_txn(&mut buf, self.next_entry, tx, entries)?;
        let res = self
            .file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data());
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
        self.next_entry = next_entry;
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
/// cut afterwards. On failure the new file is removed (best effort).
fn quarantine(path: &Path, valid_len: usize, tail: &[u8]) -> Result<(), Error> {
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
    Ok(())
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
