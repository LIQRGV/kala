//! The storage seam: the ordered key-value contract every index is written
//! against.
//!
//! The temporal layer ([`crate::world`]) and doctor read and write the
//! indexes only through [`OrderedMap`]: point reads and writes by full key,
//! and forward scans in byte order from a lower bound. Nothing outside this
//! module names the container behind it, so replacing the in-memory map with
//! on-disk sorted files (or an embedded store) is a change to this module,
//! not to the layers above.
//!
//! The contract is deliberately small:
//!
//! - keys are byte strings, ordered as bytes (layouts in the private `key`
//!   module);
//! - a scan starts at a lower bound and walks forward; callers stop it
//!   themselves by checking the key prefix, so there is no upper bound and
//!   no reverse scan;
//! - the store needs no crash safety of its own: every map is derived from
//!   the log and rebuilt from it.

use std::collections::BTreeMap;
use std::ops::Bound;

/// An ordered map from byte-string keys to `V`, sorted by key bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrderedMap<V>(BTreeMap<Vec<u8>, V>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        OrderedMap(BTreeMap::new())
    }
}

impl<V> OrderedMap<V> {
    /// An empty map.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// How many keys the map holds.
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map holds no key.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value at exactly `key`.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&V> {
        self.0.get(key)
    }

    /// Whether exactly `key` is present.
    pub(crate) fn contains_key(&self, key: &[u8]) -> bool {
        self.0.contains_key(key)
    }

    /// Stores `value` at `key`, returning the value it replaced.
    pub(crate) fn insert(&mut self, key: Vec<u8>, value: V) -> Option<V> {
        self.0.insert(key, value)
    }

    /// Stores `value` at `key`, replacing what is there. Like
    /// [`insert`](Self::insert), but takes the key by reference: an
    /// existing key is updated in place.
    pub(crate) fn set(&mut self, key: &[u8], value: V) {
        match self.0.get_mut(key) {
            Some(current) => *current = value,
            None => {
                self.0.insert(key.to_vec(), value);
            }
        }
    }

    /// Removes `key`, returning its value.
    pub(crate) fn remove(&mut self, key: &[u8]) -> Option<V> {
        self.0.remove(key)
    }

    /// Every entry from `lower` on, in key order. The caller ends the scan
    /// (usually when the key leaves a prefix).
    pub(crate) fn scan<'m>(
        &'m self,
        lower: Bound<&[u8]>,
    ) -> impl Iterator<Item = (&'m [u8], &'m V)> + use<'m, V> {
        self.0
            .range::<[u8], _>((lower, Bound::Unbounded))
            .map(|(k, v)| (k.as_slice(), v))
    }

    /// Every entry at or after `key`, in key order: a seek.
    pub(crate) fn scan_from<'m>(
        &'m self,
        key: &[u8],
    ) -> impl Iterator<Item = (&'m [u8], &'m V)> + use<'m, V> {
        self.scan(Bound::Included(key))
    }

    /// Every entry, in key order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], &V)> {
        self.0.iter().map(|(k, v)| (k.as_slice(), v))
    }

    /// Every key, in order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.0.keys().map(Vec::as_slice)
    }
}

/// Damage for the unit tests that check doctor notices a tampered index.
#[cfg(test)]
impl<V> OrderedMap<V> {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.0.values_mut()
    }
}

impl<V> FromIterator<(Vec<u8>, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (Vec<u8>, V)>>(iter: I) -> Self {
        OrderedMap(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(keys: &[&[u8]]) -> OrderedMap<usize> {
        keys.iter()
            .enumerate()
            .map(|(i, k)| (k.to_vec(), i))
            .collect()
    }

    #[test]
    fn scans_walk_forward_in_byte_order_from_the_bound() {
        let m = map(&[b"b", b"a", b"ab", b"\xff", b""]);
        let all: Vec<&[u8]> = m.keys().collect();
        assert_eq!(all, [&b""[..], b"a", b"ab", b"b", b"\xff"]);
        let from: Vec<&[u8]> = m.scan_from(b"ab").map(|(k, _)| k).collect();
        assert_eq!(from, [&b"ab"[..], b"b", b"\xff"]);
        let after: Vec<&[u8]> = m.scan(Bound::Excluded(b"ab")).map(|(k, _)| k).collect();
        assert_eq!(after, [&b"b"[..], b"\xff"]);
        // A bound between keys starts at the next key.
        assert_eq!(m.scan_from(b"aa").next().map(|(k, _)| k), Some(&b"ab"[..]));
        assert_eq!(m.scan(Bound::Excluded(b"\xff")).next(), None);
    }

    #[test]
    fn point_writes_replace_and_remove() {
        let mut m = OrderedMap::new();
        assert!(m.is_empty());
        assert_eq!(m.insert(b"k".to_vec(), 1), None);
        assert_eq!(m.insert(b"k".to_vec(), 2), Some(1));
        assert_eq!(
            (m.len(), m.get(b"k"), m.contains_key(b"x")),
            (1, Some(&2), false)
        );
        m.set(b"k", 3);
        m.set(b"new", 4);
        assert_eq!((m.get(b"k"), m.get(b"new")), (Some(&3), Some(&4)));
        assert_eq!(m.remove(b"k"), Some(3));
        assert_eq!(m.get(b"k"), None);
    }
}
