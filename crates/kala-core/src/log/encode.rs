//! Encoding one transaction's bytes: its entries' frames, its `Commit` and
//! its begin record (see the [module docs](super) for the layout). Touches
//! no file.

use super::format::{
    BEGIN_COVERED, BEGIN_LEN, BEGIN_TAG, EDIT_ATTRIBUTE, EDIT_MIGRATION, EDIT_NODE_TYPE,
    EDIT_SETTING, KIND_COMMIT, KIND_DEFINE_ATTR, KIND_DEFINE_EDGE, KIND_DEFINE_ENTITY,
    KIND_DEFINE_NODE_TYPE, KIND_DELETED, KIND_FACT, KIND_REVISED, KIND_SCHEMA, op_byte,
    value_type_byte,
};
use super::{EntryBody, EntryId, LogEntry, MAX_ENTRY_BYTES};
use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, NodeTypeId};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection, SchemaEdit};
use crate::tick::{Precision, Tick};
use crate::value::Value;
use crate::world::{Op, TxId};

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

/// An entry to append, borrowing from the caller (the writer's view of
/// [`EntryBody`]; `Commit` is added by the writer itself). Fact, `Deleted`
/// and `Revised` values are already encoded.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NewEntry<'a> {
    Schema(&'a SchemaEdit),
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

/// A `str`: `u32 len | len bytes of UTF-8`.
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

/// A string behind a presence flag: `has u8 | [str, if has = 1]`.
fn push_optional_str(b: &mut Vec<u8>, s: Option<&str>) -> Result<(), Error> {
    match s {
        Some(s) => {
            b.push(EDIT_NODE_TYPE);
            push_str(b, s)
        }
        None => {
            b.push(EDIT_ATTRIBUTE);
            Ok(())
        }
    }
}

/// The key fields `Fact`, `Deleted` and `Revised` entries share:
/// `entity u64 | attr u32 | tick i64`.
fn push_key(b: &mut Vec<u8>, entity: EntityId, attr: AttrId, tick: Tick) {
    b.extend_from_slice(&entity.raw().to_le_bytes());
    b.extend_from_slice(&attr.0.to_le_bytes());
    b.extend_from_slice(&tick.0.to_le_bytes());
}

/// What a correction (`Deleted` or `Revised`) starts with: `of u64 | rev u32`.
fn push_chain(b: &mut Vec<u8>, of: EntryId, rev: u32) {
    b.extend_from_slice(&of.0.to_le_bytes());
    b.extend_from_slice(&rev.to_le_bytes());
}

/// `op u8 | precision u8`, as `Fact` and `Revised` entries carry them.
fn push_op(b: &mut Vec<u8>, op: Op, precision: Precision) {
    b.push(op_byte(op));
    b.push(precision.to_byte());
}

/// An attribute definition: the kind-2 fields after the id.
fn push_attr(b: &mut Vec<u8>, def: &AttrDef) -> Result<(), Error> {
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
    push_optional_str(b, def.inverse.as_deref())?;
    b.push(u8::from(def.inferred));
    b.push(u8::from(def.deprecated));
    Ok(())
}

/// A kind-9 `Schema` entry after its entry ID: the edit byte, then that
/// edit's fields.
fn push_schema(b: &mut Vec<u8>, edit: &SchemaEdit) -> Result<(), Error> {
    match edit {
        SchemaEdit::Attribute { id, def } => {
            b.push(EDIT_ATTRIBUTE);
            b.extend_from_slice(&id.0.to_le_bytes());
            push_attr(b, def)?;
        }
        SchemaEdit::NodeType { id, name, inferred } => {
            b.push(EDIT_NODE_TYPE);
            b.extend_from_slice(&id.0.to_le_bytes());
            push_str(b, name)?;
            b.push(u8::from(*inferred));
        }
        SchemaEdit::Setting { key, value } => {
            b.push(EDIT_SETTING);
            push_str(b, key)?;
            push_str(b, value)?;
        }
        SchemaEdit::Migration {
            source,
            def,
            skip_unconvertible,
        } => {
            b.push(EDIT_MIGRATION);
            b.extend_from_slice(&source.0.to_le_bytes());
            push_attr(b, def)?;
            b.push(u8::from(*skip_unconvertible));
        }
    }
    Ok(())
}

impl NewEntry<'_> {
    /// What the entry says once appended: the [`EntryBody`]
    /// [`read_log`](super::read_log) returns for it.
    fn to_body(self) -> EntryBody {
        match self {
            NewEntry::Schema(edit) => EntryBody::Schema(edit.clone()),
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

    /// Appends the entry's frame, as entry `id`, to `buf`.
    fn push_frame(self, buf: &mut Vec<u8>, id: u64) -> Result<(), Error> {
        match self {
            NewEntry::Schema(edit) => push_frame(buf, KIND_SCHEMA, id, |b| push_schema(b, edit)),
            NewEntry::NodeType { id: t, name } => push_frame(buf, KIND_DEFINE_NODE_TYPE, id, |b| {
                b.extend_from_slice(&t.0.to_le_bytes());
                push_str(b, name)
            }),
            NewEntry::Attr { id: a, def } => push_frame(buf, KIND_DEFINE_ATTR, id, |b| {
                b.extend_from_slice(&a.0.to_le_bytes());
                push_attr(b, def)
            }),
            NewEntry::Entity { id: e, handle } => push_frame(buf, KIND_DEFINE_ENTITY, id, |b| {
                b.extend_from_slice(&e.raw().to_le_bytes());
                push_optional_str(b, handle)
            }),
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
            }),
            NewEntry::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => push_frame(buf, KIND_FACT, id, |b| {
                push_key(b, entity, attr, tick);
                push_op(b, op, precision);
                b.extend_from_slice(value);
                Ok(())
            }),
            NewEntry::Deleted {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
            } => push_frame(buf, KIND_DELETED, id, |b| {
                push_chain(b, of, rev);
                push_key(b, entity, attr, tick);
                b.extend_from_slice(value);
                Ok(())
            }),
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
                push_chain(b, of, rev);
                push_key(b, entity, attr, tick);
                push_op(b, op, precision);
                b.extend_from_slice(value);
                Ok(())
            }),
        }
    }
}

/// One encoded transaction, ready to append: its bytes and the entries
/// they hold, as [`read_log`](super::read_log) will return them.
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
/// large for its length fields. `previous_checksum` is the link the
/// transaction chains from: the `Commit` checksum of the transaction before
/// it, or [`header_checksum`](super::header_checksum) for genesis.
pub(crate) fn encode_txn<'a>(
    first_id: EntryId,
    tx: TxId,
    previous_checksum: u32,
    entries: impl IntoIterator<Item = NewEntry<'a>>,
) -> Result<EncodedTxn, Error> {
    let mut bytes = vec![0; BEGIN_LEN]; // the begin record, patched once the body is known
    let mut listed = Vec::new();
    let mut id = first_id.0;
    for entry in entries {
        listed.push(LogEntry {
            id: EntryId(id),
            body: entry.to_body(),
        });
        entry.push_frame(&mut bytes, id)?;
        id += 1;
    }
    let checksum = push_commit(&mut bytes, id, tx, previous_checksum)?;
    listed.push(LogEntry {
        id: EntryId(id),
        body: EntryBody::Commit { tx, checksum },
    });
    let (begin, body) = bytes.split_at_mut(BEGIN_LEN);
    patch_begin(begin, tx, body);
    Ok(EncodedTxn {
        bytes,
        entries: listed,
    })
}

/// Appends the `Commit` frame, as entry `id` of transaction `tx`, to `buf`
/// (a begin record's bytes, then every other frame of the body) and returns
/// its checksum: the chained checksum of `previous_checksum` and the whole
/// body up to the checksum field.
fn push_commit(buf: &mut Vec<u8>, id: u64, tx: TxId, previous_checksum: u32) -> Result<u32, Error> {
    push_frame(buf, KIND_COMMIT, id, |b| {
        b.extend_from_slice(&tx.0.to_le_bytes());
        b.extend_from_slice(&[0; 4]); // checksum, patched below
        Ok(())
    })?;
    let end = buf.len();
    let checksum = super::chained_checksum(previous_checksum, &buf[BEGIN_LEN..end - 4]);
    buf[end - 4..].copy_from_slice(&checksum.to_le_bytes());
    Ok(checksum)
}

/// Fills in `rec`, the begin record of transaction `tx`, for its finished
/// `body`.
fn patch_begin(rec: &mut [u8], tx: TxId, body: &[u8]) {
    // `body.len()` is the length of an in-memory buffer, so it fits a u64.
    let body_len = body.len() as u64;
    let body_crc = crc32fast::hash(body);
    rec[0] = BEGIN_TAG;
    rec[1..9].copy_from_slice(&tx.0.to_le_bytes());
    rec[9..17].copy_from_slice(&body_len.to_le_bytes());
    rec[17..21].copy_from_slice(&body_crc.to_le_bytes());
    let begin_crc = crc32fast::hash(&rec[..BEGIN_COVERED]);
    rec[BEGIN_COVERED..].copy_from_slice(&begin_crc.to_le_bytes());
}
