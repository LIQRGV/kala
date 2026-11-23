//! The numbers of the byte layout (see the [module docs](super)): lengths,
//! tags and the enum bytes both the encoder and the decoder use, kept side
//! by side so the two directions can't drift apart.

use crate::tick::Precision;
use crate::value::ValueType;
use crate::world::Op;

/// Header length: magic plus version.
pub(super) const HEADER_LEN: usize = 12;

pub(super) const KIND_DEFINE_NODE_TYPE: u8 = 1;
pub(super) const KIND_DEFINE_ATTR: u8 = 2;
pub(super) const KIND_DEFINE_ENTITY: u8 = 3;
pub(super) const KIND_FACT: u8 = 4;
pub(super) const KIND_COMMIT: u8 = 5;
pub(super) const KIND_DEFINE_EDGE: u8 = 6;
pub(super) const KIND_DELETED: u8 = 7;
pub(super) const KIND_REVISED: u8 = 8;
pub(super) const KIND_SCHEMA: u8 = 9;

// The `edit` byte of a kind-9 `Schema` entry.
pub(super) const EDIT_ATTRIBUTE: u8 = 0;
pub(super) const EDIT_NODE_TYPE: u8 = 1;
pub(super) const EDIT_SETTING: u8 = 2;
pub(super) const EDIT_MIGRATION: u8 = 3;

/// Length of a transaction's begin record.
pub(super) const BEGIN_LEN: usize = 25;

/// Tag byte of a begin record.
pub(super) const BEGIN_TAG: u8 = 0x07;

/// Bytes of a begin record covered by its own checksum.
pub(super) const BEGIN_COVERED: usize = 21;

pub(super) fn op_byte(op: Op) -> u8 {
    match op {
        Op::Assert => 0,
        Op::Retract => 1,
    }
}

pub(super) fn op_from(b: u8) -> Result<Op, String> {
    match b {
        0 => Ok(Op::Assert),
        1 => Ok(Op::Retract),
        _ => Err(format!("unknown op {b}")),
    }
}

pub(super) fn precision_from(b: u8) -> Result<Precision, String> {
    Precision::from_byte(b).ok_or_else(|| format!("unknown precision {b}"))
}

pub(super) fn value_type_byte(t: ValueType) -> u8 {
    match t {
        ValueType::Bool => 1,
        ValueType::I64 => 2,
        ValueType::F64 => 3,
        ValueType::Str => 4,
        ValueType::Ref => 5,
    }
}

pub(super) fn value_type_from(b: u8) -> Result<ValueType, String> {
    Ok(match b {
        1 => ValueType::Bool,
        2 => ValueType::I64,
        3 => ValueType::F64,
        4 => ValueType::Str,
        5 => ValueType::Ref,
        _ => return Err(format!("unknown value type {b}")),
    })
}
