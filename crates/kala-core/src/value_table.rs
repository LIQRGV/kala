//! The value table behind unindexed strings (CLAUDE.md "Value types (v1)":
//! long strings).
//!
//! An `indexed: false` string value never sits inside an index key: its `E`
//! keys carry a fixed-width [`ValueRef`] `(content hash, collision ordinal)`
//! instead (layout in the private `key` module), and this table maps each
//! distinct value to its reference and back. It is derived data, like the
//! indexes: rebuilt from the log on open, never stored.
//!
//! **Content-addressed.** A value is found by its full encoding (a std
//! `HashMap`, randomly seeded), never by scanning the values that share its
//! content hash, so inserting or finding a value costs `O(len)` however many
//! values collide. The content hash only picks the reference: a new value
//! gets `(hash, next ordinal for that hash)`, so two different values never
//! share a reference (and so never share a history), whatever the hash
//! function does.
//!
//! **Deterministic.** References are assigned in log order of the entries
//! that first store each value, both by a live commit and by a replay, so a
//! reopened world assigns exactly the references the live one did.

use std::collections::HashMap;
use std::sync::Arc;

use crate::key::ValueRef;

/// The function that hashes an unindexed string's UTF-8 content into the
/// first half of its [`ValueRef`].
pub(crate) type ContentHash = fn(&[u8]) -> u64;

/// FNV-1a, 64 bit: the content hash of every real world. A fixed, public
/// spec (no seed), so references are stable across builds and processes,
/// as later on-disk checkpoints will need. Collisions are harmless (see
/// the module docs), so its weakness against crafted input costs nothing
/// but an ordinal.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes
        .iter()
        .fold(OFFSET, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// Distinct unindexed string values and their references (see the module
/// docs). Values are keyed by their full [`Value`](crate::Value) encoding.
#[derive(Debug, Default, Clone)]
pub(crate) struct ValueTable {
    by_encoding: HashMap<Arc<[u8]>, ValueRef>,
    by_ref: HashMap<ValueRef, Arc<[u8]>>,
    /// Per content hash, the ordinal the next new value with that hash gets
    /// (`u32::MAX + 1` once every ordinal is spent, hence a u64).
    next_ordinal: HashMap<u64, u64>,
}

impl ValueTable {
    /// Whether the table holds no value.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_ref.is_empty()
    }

    /// The reference of the value with this full encoding, if it is in the
    /// table.
    pub(crate) fn get(&self, encoding: &[u8]) -> Option<ValueRef> {
        self.by_encoding.get(encoding).copied()
    }

    /// The full encoding of the value `r` refers to, if it is in the table.
    pub(crate) fn resolve(&self, r: ValueRef) -> Option<&[u8]> {
        self.by_ref.get(&r).map(|e| &e[..])
    }

    /// The ordinal the next new value with content hash `hash` gets, as a
    /// u64: `u32::MAX + 1` once every ordinal of that hash is spent.
    pub(crate) fn next_ordinal(&self, hash: u64) -> u64 {
        self.next_ordinal.get(&hash).copied().unwrap_or(0)
    }

    /// Adds the value with full encoding `encoding` under reference `r`,
    /// unless the value is already in the table. `r` must be the reference
    /// [`ValueTable::next_ordinal`] gave for its hash (callers assign it
    /// that way, so references stay deterministic).
    ///
    /// Copies the encoding once; both maps share that copy.
    pub(crate) fn insert(&mut self, encoding: &[u8], r: ValueRef) {
        if self.by_encoding.contains_key(encoding) {
            return;
        }
        debug_assert!(
            !self.by_ref.contains_key(&r),
            "invariant: a value reference is given to one value only"
        );
        let shared: Arc<[u8]> = Arc::from(encoding);
        self.by_encoding.insert(Arc::clone(&shared), r);
        self.by_ref.insert(r, shared);
        let next = u64::from(r.ordinal) + 1;
        let slot = self.next_ordinal.entry(r.hash).or_insert(0);
        *slot = (*slot).max(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_the_published_test_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn colliding_values_get_consecutive_ordinals_and_resolve_back() {
        let mut t = ValueTable::default();
        for (i, v) in [b"x".as_slice(), b"y", b"z"].iter().enumerate() {
            let ord = u32::try_from(t.next_ordinal(7)).unwrap();
            assert_eq!(ord as usize, i);
            t.insert(
                v,
                ValueRef {
                    hash: 7,
                    ordinal: ord,
                },
            );
        }
        assert_eq!(
            t.get(b"y"),
            Some(ValueRef {
                hash: 7,
                ordinal: 1
            })
        );
        assert_eq!(
            t.resolve(ValueRef {
                hash: 7,
                ordinal: 2
            }),
            Some(&b"z"[..])
        );
        // re-inserting a known value changes nothing
        t.insert(
            b"x",
            ValueRef {
                hash: 7,
                ordinal: 3,
            },
        );
        assert_eq!(t.next_ordinal(7), 3);
        assert_eq!(t.next_ordinal(8), 0);
    }

    #[test]
    fn the_last_ordinal_can_be_used_once() {
        let mut t = ValueTable::default();
        t.insert(
            b"a",
            ValueRef {
                hash: 1,
                ordinal: u32::MAX - 1,
            },
        );
        assert_eq!(t.next_ordinal(1), u64::from(u32::MAX));
        t.insert(
            b"b",
            ValueRef {
                hash: 1,
                ordinal: u32::MAX,
            },
        );
        assert_eq!(t.next_ordinal(1), u64::from(u32::MAX) + 1);
    }
}
