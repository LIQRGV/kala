//! Fixed-width IDs used in index keys.
//!
//! An [`EntityId`] is a `u64` laid out so that byte order in keys groups
//! entities by node type:
//!
//! | bits     | meaning                                              |
//! |----------|------------------------------------------------------|
//! | 63       | edge tag: always `0` for entities (reserved for edges) |
//! | 48..=62  | node type ID (15 bits)                               |
//! | 0..=47   | per-type sequence number, starting at 1              |
//!
//! Stored big-endian, so all IDs of one type share a two-byte key prefix and
//! "all characters" is a prefix scan.

use std::fmt;

/// Bits reserved for the per-type sequence number.
const SEQ_BITS: u32 = 48;
/// Bits reserved for the node type ID (bit 63 above them is the edge tag).
const TYPE_BITS: u32 = 15;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
const TYPE_MASK: u64 = (1 << TYPE_BITS) - 1;

/// Largest valid node type ID (`2^15 - 1`).
pub const MAX_NODE_TYPE: u16 = (1 << TYPE_BITS) - 1;
/// Largest valid per-type sequence number (`2^48 - 1`).
pub const MAX_SEQ: u64 = SEQ_MASK;

/// A node type ID (`CHARACTER`, `PLACE`, ...), assigned by the schema.
///
/// Only values `<=` [`MAX_NODE_TYPE`] fit in an [`EntityId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeTypeId(pub u16);

/// An attribute ID, assigned by the schema in declaration order. Stored as
/// 4 big-endian bytes in keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttrId(pub u32);

/// Why an [`EntityId`] can't be built from its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdError {
    /// The node type ID doesn't fit in 15 bits.
    NodeTypeOutOfRange(u16),
    /// The sequence number is `0` (sequences start at 1) or doesn't fit in 48 bits.
    SeqOutOfRange(u64),
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::NodeTypeOutOfRange(t) => {
                write!(f, "node type ID {t} is out of range (max {MAX_NODE_TYPE})")
            }
            IdError::SeqOutOfRange(s) => {
                write!(f, "sequence number {s} is out of range (1..={MAX_SEQ})")
            }
        }
    }
}

impl std::error::Error for IdError {}

/// An entity ID: a fixed-width `u64` that sorts as 8 big-endian bytes in keys.
///
/// See the module docs for the bit layout. Ordering (and byte order) is by
/// node type first, then by sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(u64);

impl EntityId {
    /// Builds an entity ID from a node type and a per-type sequence number.
    ///
    /// Fails if the type is above [`MAX_NODE_TYPE`] or the sequence is `0`
    /// or above [`MAX_SEQ`].
    #[inline]
    pub fn new(node_type: NodeTypeId, seq: u64) -> Result<EntityId, IdError> {
        if node_type.0 > MAX_NODE_TYPE {
            return Err(IdError::NodeTypeOutOfRange(node_type.0));
        }
        if seq == 0 || seq > MAX_SEQ {
            return Err(IdError::SeqOutOfRange(seq));
        }
        Ok(EntityId((u64::from(node_type.0) << SEQ_BITS) | seq))
    }

    /// The node type this entity belongs to.
    #[inline]
    pub fn node_type(self) -> NodeTypeId {
        // Masked to 15 bits, so the cast can't truncate.
        NodeTypeId(((self.0 >> SEQ_BITS) & TYPE_MASK) as u16)
    }

    /// The per-type sequence number (`CHARACTER#1` has seq 1).
    #[inline]
    pub fn seq(self) -> u64 {
        self.0 & SEQ_MASK
    }

    /// The raw `u64` (bit layout in the module docs).
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Builds an ID from its 8-byte big-endian key form.
    ///
    /// Does no validation: key and value decoding accept any `u64`, and ID
    /// validity (is it defined in this world?) is checked at the world level.
    #[inline]
    pub fn from_bytes(bytes: [u8; 8]) -> EntityId {
        EntityId(u64::from_be_bytes(bytes))
    }

    /// The 8-byte big-endian key form, so byte order equals numeric order.
    #[inline]
    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MAX_TYPE: u16 = (1 << 15) - 1;
    const MAX_SEQ: u64 = (1 << 48) - 1;

    fn id(t: u16, seq: u64) -> EntityId {
        EntityId::new(NodeTypeId(t), seq).expect("valid id")
    }

    #[test]
    fn layout_type_in_bits_48_to_62_seq_in_low_48() {
        let e = id(1, 1);
        assert_eq!(e.raw(), (1u64 << 48) | 1);
        let e = id(0x1234, 0xABCD_EF01_2345);
        assert_eq!(e.raw(), (0x1234u64 << 48) | 0xABCD_EF01_2345);
    }

    #[test]
    fn bit_63_is_zero_for_entities() {
        assert_eq!(id(MAX_TYPE, MAX_SEQ).raw() >> 63, 0);
        assert_eq!(id(0, 1).raw() >> 63, 0);
    }

    #[test]
    fn accessors_round_trip_examples() {
        let e = id(7, 42);
        assert_eq!(e.node_type(), NodeTypeId(7));
        assert_eq!(e.seq(), 42);
        let e = id(MAX_TYPE, MAX_SEQ);
        assert_eq!(e.node_type(), NodeTypeId(MAX_TYPE));
        assert_eq!(e.seq(), MAX_SEQ);
    }

    #[test]
    fn type_zero_is_allowed() {
        let e = id(0, 5);
        assert_eq!(e.node_type(), NodeTypeId(0));
        assert_eq!(e.seq(), 5);
    }

    #[test]
    fn rejects_type_at_or_above_2_pow_15() {
        assert!(EntityId::new(NodeTypeId(1 << 15), 1).is_err());
        assert!(EntityId::new(NodeTypeId(u16::MAX), 1).is_err());
    }

    #[test]
    fn rejects_seq_at_or_above_2_pow_48() {
        assert!(EntityId::new(NodeTypeId(1), 1 << 48).is_err());
        assert!(EntityId::new(NodeTypeId(1), u64::MAX).is_err());
    }

    #[test]
    fn rejects_seq_zero() {
        assert!(EntityId::new(NodeTypeId(1), 0).is_err());
        assert!(EntityId::new(NodeTypeId(0), 0).is_err());
    }

    #[test]
    fn accepts_boundaries() {
        assert!(EntityId::new(NodeTypeId(MAX_TYPE), 1).is_ok());
        assert!(EntityId::new(NodeTypeId(0), MAX_SEQ).is_ok());
    }

    #[test]
    fn to_bytes_is_big_endian_of_raw() {
        let e = id(0x0102, 0x0304_0506_0708);
        assert_eq!(e.to_bytes(), e.raw().to_be_bytes());
        assert_eq!(
            e.to_bytes(),
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
    }

    #[test]
    fn from_bytes_round_trips() {
        let e = id(300, 99);
        assert_eq!(EntityId::from_bytes(e.to_bytes()), e);
    }

    #[test]
    fn all_of_one_type_sorts_before_next_type_at_extremes() {
        assert!(id(1, MAX_SEQ) < id(2, 1));
        assert!(id(1, MAX_SEQ).to_bytes() < id(2, 1).to_bytes());
    }

    proptest! {
        #[test]
        fn prop_round_trip(t in 0u16..=MAX_TYPE, seq in 1u64..=MAX_SEQ) {
            let e = EntityId::new(NodeTypeId(t), seq).unwrap();
            prop_assert_eq!(e.node_type(), NodeTypeId(t));
            prop_assert_eq!(e.seq(), seq);
            prop_assert_eq!(EntityId::from_bytes(e.to_bytes()), e);
            prop_assert_eq!(e.to_bytes(), e.raw().to_be_bytes());
        }

        #[test]
        fn prop_same_type_shares_two_byte_prefix(
            t in 0u16..=MAX_TYPE, a in 1u64..=MAX_SEQ, b in 1u64..=MAX_SEQ,
        ) {
            let ea = EntityId::new(NodeTypeId(t), a).unwrap().to_bytes();
            let eb = EntityId::new(NodeTypeId(t), b).unwrap().to_bytes();
            prop_assert_eq!(&ea[0..2], &eb[0..2]);
            prop_assert_eq!(&ea[0..2], &t.to_be_bytes()[..]);
        }

        #[test]
        fn prop_distinct_types_never_interleave(
            ta in 0u16..=MAX_TYPE, tb in 0u16..=MAX_TYPE,
            sa in 1u64..=MAX_SEQ, sb in 1u64..=MAX_SEQ,
        ) {
            prop_assume!(ta < tb);
            let a = EntityId::new(NodeTypeId(ta), sa).unwrap();
            let b = EntityId::new(NodeTypeId(tb), sb).unwrap();
            prop_assert!(a < b);
            prop_assert!(a.to_bytes() < b.to_bytes());
        }

        #[test]
        fn prop_within_type_order_follows_seq(
            t in 0u16..=MAX_TYPE, a in 1u64..=MAX_SEQ, b in 1u64..=MAX_SEQ,
        ) {
            let ea = EntityId::new(NodeTypeId(t), a).unwrap();
            let eb = EntityId::new(NodeTypeId(t), b).unwrap();
            prop_assert_eq!(ea.cmp(&eb), a.cmp(&b));
            prop_assert_eq!(ea.to_bytes().cmp(&eb.to_bytes()), a.cmp(&b));
        }

        #[test]
        fn prop_out_of_range_rejected(t in (1u16<<15)..=u16::MAX, seq in 1u64..=MAX_SEQ) {
            prop_assert!(EntityId::new(NodeTypeId(t), seq).is_err());
        }

        #[test]
        fn prop_seq_too_big_rejected(t in 0u16..=MAX_TYPE, seq in (1u64<<48)..=u64::MAX) {
            prop_assert!(EntityId::new(NodeTypeId(t), seq).is_err());
        }
    }
}
