//! The log file: the world's only source of truth and its write-ahead log.
//!
//! Everything else (ID tables, schema, indexes) is a copy rebuilt from it.
//! Only committed transactions count; [`read_log`] returns exactly those.
//!
//! # Byte layout (format version 2)
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
//!                         | cardinality u8 | indexed u8 | direction u8
//!                         | has_inverse u8 | [inverse str, if has_inverse = 1]
//! kind 3  DefineEntity    id u64 | has_handle u8 | [handle str, if has_handle = 1]
//! kind 4  Fact            entity u64 | attr u32 | tick i64 | op u8
//!                         | value (Value encoding, the rest of the payload)
//! kind 5  Commit          tx u64 | checksum u32
//! kind 6  DefineEdge      id u64 | source u64 | attr u32 | target u64
//!
//! value_type: 1 bool, 2 i64, 3 f64, 4 str, 5 ref (the Value tags)
//! kind:       0 state, 1 event
//! cardinality 0 one,   1 many
//! op:         0 assert, 1 retract
//! direction:  0 none (non-ref), 1 directed, 2 bidirectional
//! has_handle, indexed, has_inverse: 0 no, 1 yes
//! ```
//!
//! Every byte of a payload must be consumed and every enum byte must be one
//! listed above; anything else makes the entry unparseable. A `DefineEdge`
//! id must have the edge tag (bit 63) set and a non-zero sequence.
//!
//! **IDs are defined before use.** Inside a transaction the entries come in
//! this order: `DefineEntity`s, then `DefineEdge`s, then `Fact`s, then the
//! `Commit`. An edge `(source, attr, target)` is defined (with the next edge
//! sequence, counted across the whole log) in the transaction that first
//! stores an assert of that `ref` fact, so every `ref` fact and every fact on
//! an edge (an edge property) follows its edge's `DefineEdge`.
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
//! 6. `body_crc` doesn't match: [`Error::Corrupt`]. Every byte is there, so
//!    this is damage, not a torn write, and is never cut.
//! 7. The body's content is invalid (frames not exactly filling `body_len`,
//!    an unparseable entry, no `Commit`, more than one or not last, a
//!    `Commit` tx other than the begin record's, a `Commit` checksum mismatch,
//!    an entry ID out of sequence): [`Error::Corrupt`].
//! 8. Otherwise the transaction is committed, and the scan moves on.
//!
//! **Torn** means the valid prefix ends where that transaction starts: only
//! the last append can be torn, because each one is fsynced before the next
//! starts. [`read_log`] ignores a torn tail; `World::open` saves it to a
//! quarantine file and then cuts it. Corruption is reported with the file
//! left untouched. Because a damaged length field is caught by `begin_crc`
//! and damaged content by `body_crc`, damage never makes committed
//! transactions look like a torn tail.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, NodeTypeId};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection};
use crate::tick::Tick;
use crate::value::{Value, ValueType};
use crate::world::{Op, RecoveryReport, TxId};

/// First 8 bytes of every log file.
pub const LOG_MAGIC: [u8; 8] = *b"KALALOG\0";

/// The log format version this build writes and reads (header bytes 8..12).
pub const LOG_FORMAT_VERSION: u32 = 2;

/// Header length: magic plus version.
const HEADER_LEN: usize = 12;

const KIND_DEFINE_NODE_TYPE: u8 = 1;
const KIND_DEFINE_ATTR: u8 = 2;
const KIND_DEFINE_ENTITY: u8 = 3;
const KIND_FACT: u8 = 4;
const KIND_COMMIT: u8 = 5;
const KIND_DEFINE_EDGE: u8 = 6;

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
/// version (including version 1: there is no migration), or
/// [`Error::Corrupt`] for a truncated header or any damage the recovery
/// rules in the module docs don't classify as a torn tail.
///
/// A torn tail (a last transaction cut short, or a zero-filled tail) is not
/// returned and not an error; it is left in the file (only
/// [`World::open`](crate::World::open) cuts it). Takes no lock, so it can
/// read a log while a [`World`](crate::World) has it open.
pub fn read_log(path: &Path) -> Result<Vec<LogEntry>, Error> {
    let bytes = std::fs::read(path)?;
    Ok(scan(&bytes)?.entries)
}

/// The committed part of a log file, as found by [`scan`].
struct Scan {
    /// Entries of every committed transaction, in log order.
    entries: Vec<LogEntry>,
    /// Length of the valid prefix: the header plus every committed
    /// transaction. Bytes past it are a torn tail.
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
/// Transactions are judged one at a time from just past the header, by the
/// rules in the module docs ("Recovery"). The first one that is torn ends
/// the valid prefix ([`Scan::valid_len`]); anything else that isn't a
/// committed transaction is [`Error::Corrupt`].
fn scan(bytes: &[u8]) -> Result<Scan, Error> {
    let mut rest = check_header(bytes)?;
    let mut entries = Vec::new();
    let mut next_entry = 1u64;
    let mut next_tx = 0u64;
    loop {
        let at = bytes.len() - rest.len();
        // Rule 1: a clean end.
        if rest.is_empty() {
            break;
        }
        // Rule 2: not even a whole begin record: the last append was cut.
        let Some((begin, after_begin)) = rest.split_first_chunk::<BEGIN_LEN>() else {
            break;
        };
        let begin = Begin::parse(begin);
        // Rule 3: a damaged begin record. Only a zero-filled tail (how some
        // file systems expose an append that never reached the disk) is a
        // torn write; anything else may hide committed data behind it.
        if !begin.crc_ok {
            if rest.iter().all(|&b| b == 0) {
                break;
            }
            return Err(Error::Corrupt(format!(
                "damaged transaction header at offset {at}"
            )));
        }
        // Rule 4: a sound begin record that isn't the next transaction's.
        if begin.tag != BEGIN_TAG {
            return Err(Error::Corrupt(format!(
                "unknown record tag {:#04x} at offset {at}",
                begin.tag
            )));
        }
        if begin.tx != next_tx {
            return Err(Error::Corrupt(format!(
                "tx#{} found at offset {at} where tx#{next_tx} was expected",
                begin.tx
            )));
        }
        // Rule 5: the body runs past the end of the file: cut short. The
        // length is checked against the bytes left before it is used.
        let Some(body_len) = usize::try_from(begin.body_len)
            .ok()
            .filter(|&n| n <= after_begin.len())
        else {
            break;
        };
        let (body, after_body) = after_begin.split_at(body_len);
        // Rule 6: every byte is there but they don't match: damage, never a
        // torn write.
        if crc32fast::hash(body) != begin.body_crc {
            return Err(Error::Corrupt(format!(
                "tx#{}: body checksum mismatch at offset {at}",
                begin.tx
            )));
        }
        // Rule 7: sound framing around content a writer can't produce.
        let mut txn = parse_body(body, begin.tx, next_entry)
            .map_err(|why| Error::Corrupt(format!("tx#{}: {why}", begin.tx)))?;
        // Rule 8: committed.
        next_entry += txn.len() as u64;
        next_tx += 1;
        entries.append(&mut txn);
        rest = after_body;
    }
    Ok(Scan {
        entries,
        valid_len: bytes.len() - rest.len(),
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
/// filling it, entry IDs consecutive from `first_entry`, exactly one
/// `Commit`, last, for `tx`, with a matching checksum. Returns its entries,
/// the `Commit` included.
fn parse_body(body: &[u8], tx: u64, first_entry: u64) -> Result<Vec<LogEntry>, String> {
    let mut entries = Vec::new();
    let mut rest = body;
    let mut next_entry = first_entry;
    loop {
        let Some((payload, after)) = next_frame(rest) else {
            return Err(if rest.is_empty() {
                "no Commit".to_owned()
            } else {
                "a frame runs past the end of the transaction".to_owned()
            });
        };
        rest = after;
        let entry = parse_payload(payload)?;
        if entry.id.0 != next_entry {
            return Err(format!(
                "entry#{} found where entry#{next_entry} was expected",
                entry.id.0
            ));
        }
        next_entry += 1;
        if let EntryBody::Commit { tx: ctx, checksum } = entry.body {
            if !rest.is_empty() {
                return Err("bytes after the Commit".to_owned());
            }
            if ctx.0 != tx {
                return Err(format!("its Commit says tx#{}", ctx.0));
            }
            // Covered: every byte of the body except the Commit's own
            // checksum field, its last 4 bytes (a parsed Commit frame is
            // 25 bytes, so the body is at least that long).
            if crc32fast::hash(&body[..body.len() - 4]) != checksum {
                return Err("Commit checksum mismatch".to_owned());
            }
            entries.push(entry);
            return Ok(entries);
        }
        entries.push(entry);
    }
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

/// Appends one transaction to `buf`: its begin record, then its body (its
/// entries' frames, then its `Commit`), numbering entries from `first_id`.
/// Returns the next entry ID.
fn encode_txn<'a>(
    buf: &mut Vec<u8>,
    first_id: u64,
    tx: TxId,
    entries: impl IntoIterator<Item = NewEntry<'a>>,
) -> Result<u64, Error> {
    let begin = buf.len();
    buf.extend_from_slice(&[0; BEGIN_LEN]); // patched once the body is known
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
    /// see the module docs) cut off, so the next append lands right after
    /// the last commit. The cut bytes are first saved, durably, to a
    /// quarantine file (see [`quarantine`]); if that fails, the open fails
    /// with [`Error::Io`] and the log is not cut. On any error the log is
    /// untouched. A clean log is never modified, so opening is idempotent.
    /// The returned [`RecoveryReport`] says what was cut and where it went.
    pub(crate) fn open<T>(
        path: &Path,
        replay: impl FnOnce(&[LogEntry]) -> Result<T, Error>,
    ) -> Result<(T, LogWriter, RecoveryReport), Error> {
        // One handle for reading, cutting and appending, so the bytes
        // replayed are the bytes appended after. `append` without `create`
        // never creates the file.
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        lock(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let Scan { entries, valid_len } = scan(&bytes)?;
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
        }
        let next_entry = entries.last().map_or(1, |e| e.id.0 + 1);
        Ok((
            replayed,
            LogWriter {
                file,
                next_entry,
                failed: None,
            },
            report,
        ))
    }

    /// `Err` if an earlier append failed: the world refuses further writes.
    pub(crate) fn check(&self) -> Result<(), Error> {
        match &self.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Appends one transaction (its begin record, its entries, then a
    /// `Commit` for `tx`) with a single write, then fsyncs. Returns only once the transaction is
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
