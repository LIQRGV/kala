//! Decoding one frame payload into a [`LogEntry`], strictly: every byte is
//! consumed and every enum byte is a known one (see the
//! [module docs](super) for the layout).

use super::format::{
    EDIT_ATTRIBUTE, EDIT_MIGRATION, EDIT_NODE_TYPE, EDIT_SETTING, KIND_COMMIT, KIND_DEFINE_ATTR,
    KIND_DEFINE_EDGE, KIND_DEFINE_ENTITY, KIND_DEFINE_NODE_TYPE, KIND_DELETED, KIND_FACT,
    KIND_REVISED, KIND_SCHEMA, op_from, precision_from, value_type_from,
};
use super::{EntryBody, EntryId, LogEntry};
use crate::id::{AttrId, EntityId, NodeTypeId};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection, SchemaEdit};
use crate::tick::Tick;
use crate::value::Value;
use crate::world::TxId;

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

    /// An entity (or edge) ID: a `u64`.
    fn entity(&mut self) -> Result<EntityId, String> {
        Ok(EntityId::from_bytes(self.u64()?.to_be_bytes()))
    }

    /// An attribute ID: a `u32`.
    fn attr(&mut self) -> Result<AttrId, String> {
        Ok(AttrId(self.u32()?))
    }

    /// A story tick: an `i64`.
    fn tick(&mut self) -> Result<Tick, String> {
        Ok(Tick(i64::from_le_bytes(self.array()?)))
    }

    /// A string behind a presence flag: `has u8 | [str, if has = 1]`.
    fn optional_string(&mut self) -> Result<Option<String>, String> {
        Ok(if self.flag()? {
            Some(self.string()?)
        } else {
            None
        })
    }

    /// The [`Value`] encoding that fills the rest of the payload; `what`
    /// names the entry kind in the error.
    fn value(&mut self, what: &str) -> Result<Value, String> {
        Value::decode(self.rest()).map_err(|e| format!("{what} value: {e}"))
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

/// An attribute definition: the kind-2 fields after the id.
fn parse_attr(c: &mut Cursor<'_>) -> Result<AttrDef, String> {
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
    let inverse = c.optional_string()?;
    let inferred = c.flag()?;
    let deprecated = c.flag()?;
    Ok(AttrDef {
        name,
        value_type,
        kind,
        cardinality,
        indexed,
        direction,
        inverse,
        reverse_cardinality,
        inferred,
        deprecated,
    })
}

/// Parses one frame payload strictly.
pub(super) fn parse_payload(payload: &[u8]) -> Result<LogEntry, String> {
    let mut c = Cursor { bytes: payload };
    let kind = c.u8()?;
    let id = EntryId(c.u64()?);
    let body = match kind {
        KIND_SCHEMA => EntryBody::Schema(parse_schema_edit(&mut c)?),
        KIND_DEFINE_NODE_TYPE => EntryBody::DefineNodeType {
            id: NodeTypeId(c.u16()?),
            name: c.string()?,
        },
        KIND_DEFINE_ATTR => EntryBody::DefineAttr {
            id: c.attr()?,
            def: parse_attr(&mut c)?,
        },
        KIND_DEFINE_ENTITY => EntryBody::DefineEntity {
            id: c.entity()?,
            handle: c.optional_string()?,
        },
        KIND_FACT => parse_fact(&mut c)?,
        KIND_COMMIT => EntryBody::Commit {
            tx: TxId(c.u64()?),
            checksum: c.u32()?,
        },
        KIND_DEFINE_EDGE => parse_define_edge(&mut c)?,
        KIND_DELETED => parse_deleted(&mut c)?,
        KIND_REVISED => parse_revised(&mut c)?,
        b => return Err(format!("unknown entry kind {b}")),
    };
    c.finish()?;
    Ok(LogEntry { id, body })
}

/// A kind-9 `Schema` entry after its entry ID: the edit byte, then that
/// edit's fields.
fn parse_schema_edit(c: &mut Cursor<'_>) -> Result<SchemaEdit, String> {
    Ok(match c.u8()? {
        EDIT_ATTRIBUTE => SchemaEdit::Attribute {
            id: c.attr()?,
            def: parse_attr(c)?,
        },
        EDIT_NODE_TYPE => SchemaEdit::NodeType {
            id: NodeTypeId(c.u16()?),
            name: c.string()?,
            inferred: c.flag()?,
        },
        EDIT_SETTING => SchemaEdit::Setting {
            key: c.string()?,
            value: c.string()?,
        },
        EDIT_MIGRATION => SchemaEdit::Migration {
            source: c.attr()?,
            def: parse_attr(c)?,
            skip_unconvertible: c.flag()?,
        },
        b => return Err(format!("unknown schema edit {b}")),
    })
}

/// A kind-4 `Fact` entry after its entry ID.
fn parse_fact(c: &mut Cursor<'_>) -> Result<EntryBody, String> {
    let entity = c.entity()?;
    let attr = c.attr()?;
    let tick = c.tick()?;
    let op = op_from(c.u8()?)?;
    let precision = precision_from(c.u8()?)?;
    let value = c.value("fact")?;
    Ok(EntryBody::Fact {
        entity,
        attr,
        value,
        tick,
        op,
        precision,
    })
}

/// A kind-6 `DefineEdge` entry after its entry ID. Its id must be an edge
/// ID (the edge tag set, a non-zero sequence).
fn parse_define_edge(c: &mut Cursor<'_>) -> Result<EntryBody, String> {
    let raw = c.u64()?;
    let id = EntityId::from_bytes(raw.to_be_bytes())
        .as_edge()
        .ok_or_else(|| format!("edge id {raw:#018x} is not an edge ID"))?;
    Ok(EntryBody::DefineEdge {
        id,
        source: c.entity()?,
        attr: c.attr()?,
        target: c.entity()?,
    })
}

/// A kind-7 `Deleted` entry after its entry ID.
fn parse_deleted(c: &mut Cursor<'_>) -> Result<EntryBody, String> {
    let of = EntryId(c.u64()?);
    let rev = c.u32()?;
    let entity = c.entity()?;
    let attr = c.attr()?;
    let tick = c.tick()?;
    let value = c.value("deleted")?;
    Ok(EntryBody::Deleted {
        of,
        rev,
        entity,
        attr,
        value,
        tick,
    })
}

/// A kind-8 `Revised` entry after its entry ID.
fn parse_revised(c: &mut Cursor<'_>) -> Result<EntryBody, String> {
    let of = EntryId(c.u64()?);
    let rev = c.u32()?;
    let entity = c.entity()?;
    let attr = c.attr()?;
    let tick = c.tick()?;
    let op = op_from(c.u8()?)?;
    let precision = precision_from(c.u8()?)?;
    let value = c.value("revised")?;
    Ok(EntryBody::Revised {
        of,
        rev,
        entity,
        attr,
        value,
        tick,
        op,
        precision,
    })
}
