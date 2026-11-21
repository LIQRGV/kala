//! Byte layout of index keys.
//!
//! The forward index `E` holds one history per `(entity, attribute, value)`:
//!
//! ```text
//! b'E' | entity (8, BE) | attr (4, BE) | value encoding | ^tick (8) | ^tx (8)
//! ```
//!
//! Every part before the value is fixed-width and the value encoding is
//! self-delimiting, so `E|e|a|` and `E|e|a|v|` are exact prefixes: no other
//! entity, attribute or value can start with the same bytes. `^tick` and
//! `^tx` are inverted, so within one `E|e|a|v|` history the newest tick (and,
//! for one tick, the newest tx) sorts first; a seek to `E|e|a|v|^T` therefore
//! lands on the latest entry with tick `<= T`.
//!
//! Keys are built from encoded bytes only (the value is already
//! [`Value::encode`](crate::value::Value::encode)d), never from `Value`.

use crate::id::{AttrId, EntityId};
use crate::tick::Tick;
use crate::value::{ValueError, encoded_len};
use crate::world::TxId;

/// Tag byte of the forward index.
const E_TAG: u8 = b'E';

/// Length of `E|entity|attr|`, the part before the value.
pub(crate) const E_ATTR_PREFIX_LEN: usize = 1 + 8 + 4;

/// Length of the `^tick|^tx` suffix after the value.
const SUFFIX_LEN: usize = 8 + 8;

/// `E|entity|attr|`: the prefix of every history of one attribute.
pub(crate) fn e_attr_prefix(entity: EntityId, attr: AttrId) -> Vec<u8> {
    let mut k = Vec::with_capacity(E_ATTR_PREFIX_LEN + 16 + SUFFIX_LEN);
    k.push(E_TAG);
    k.extend_from_slice(&entity.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k
}

/// `E|entity|attr|value|`: the prefix of one value's history.
pub(crate) fn e_value_prefix(entity: EntityId, attr: AttrId, value: &[u8]) -> Vec<u8> {
    let mut k = e_attr_prefix(entity, attr);
    k.extend_from_slice(value);
    k
}

/// Appends `^tick` to a value prefix: the seek key for "latest entry with
/// tick `<= tick`". It is shorter than any full key with the same `^tick`,
/// so it sorts before all of them.
pub(crate) fn push_seek_tick(value_prefix: &mut Vec<u8>, tick: Tick) {
    value_prefix.extend_from_slice(&tick.encode_inverted());
}

/// The full `E` key of one fact.
pub(crate) fn e_fact_key(
    entity: EntityId,
    attr: AttrId,
    value: &[u8],
    tick: Tick,
    tx: TxId,
) -> Vec<u8> {
    let mut k = e_value_prefix(entity, attr, value);
    push_seek_tick(&mut k, tick);
    k.extend_from_slice(&(!tx.0).to_be_bytes());
    k
}

/// The tx of a full `E` key (its trailing `^tx`), or `None` if the key is
/// too short to have one.
pub(crate) fn e_key_tx(key: &[u8]) -> Option<TxId> {
    let start = key.len().checked_sub(8)?;
    let bytes: [u8; 8] = key.get(start..)?.try_into().ok()?;
    Some(TxId(!u64::from_be_bytes(bytes)))
}

/// The largest possible key under `value_prefix`; seek *exclusively* past
/// it to reach the next distinct value.
///
/// Every key under a value prefix is that prefix plus exactly
/// [`SUFFIX_LEN`] bytes, so no key under it is greater than the all-`0xFF`
/// suffix, and every key strictly greater belongs to a later value (or a
/// later attribute/entity, which the caller's prefix check catches).
pub(crate) fn past_value(value_prefix: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(value_prefix.len() + SUFFIX_LEN);
    k.extend_from_slice(value_prefix);
    k.extend_from_slice(&[0xFF; SUFFIX_LEN]);
    k
}

/// Splits the value encoding out of a full `E` key under `E|e|a|`: returns
/// the value bytes. Fails if the key is too short or its value part isn't a
/// well-formed encoding.
pub(crate) fn e_value_bytes(key: &[u8]) -> Result<&[u8], ValueError> {
    let rest = key
        .get(E_ATTR_PREFIX_LEN..)
        .ok_or(ValueError::Malformed("index key shorter than its prefix"))?;
    let len = encoded_len(rest)?;
    if rest.len() != len + SUFFIX_LEN {
        return Err(ValueError::Malformed("index key suffix is not ^tick|^tx"));
    }
    Ok(&rest[..len])
}

/// Splits a full `E` key under `E|e|a|` into its value encoding and tick.
/// Fails like [`e_value_bytes`] if the key isn't a well-formed `E` key.
pub(crate) fn e_value_and_tick(key: &[u8]) -> Result<(&[u8], Tick), ValueError> {
    let value = e_value_bytes(key)?;
    let start = E_ATTR_PREFIX_LEN + value.len();
    // `e_value_bytes` checked that exactly `^tick|^tx` follows the value.
    let tick: [u8; 8] = key
        .get(start..start + 8)
        .and_then(|b| b.try_into().ok())
        .ok_or(ValueError::Malformed("index key suffix is not ^tick|^tx"))?;
    Ok((value, Tick::decode_inverted(tick)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::NodeTypeId;
    use crate::value::Value;

    fn ent(t: u16, seq: u64) -> EntityId {
        EntityId::new(NodeTypeId(t), seq).expect("valid id")
    }

    fn venc(v: &Value) -> Vec<u8> {
        v.encode().unwrap()
    }

    fn s(x: &str) -> Value {
        Value::Str(x.to_string())
    }

    const MAX_SEQ: u64 = (1 << 48) - 1;

    #[test]
    fn attr_prefix_layout() {
        let k = e_attr_prefix(ent(1, 1), AttrId(7));
        assert_eq!(k.len(), E_ATTR_PREFIX_LEN);
        assert_eq!(k[0], b'E');
        assert_eq!(&k[1..9], &ent(1, 1).to_bytes());
        assert_eq!(&k[9..13], &7u32.to_be_bytes());
    }

    #[test]
    fn attr_prefix_does_not_overlap_neighbouring_attr_ids() {
        let e = ent(1, 1);
        let v = venc(&Value::I64(0));
        for (a, b) in [
            (0u32, 1u32),
            (1, 2),
            (255, 256),
            (u32::MAX - 1, u32::MAX),
            (0, u32::MAX),
        ] {
            let pa = e_attr_prefix(e, AttrId(a));
            let pb = e_attr_prefix(e, AttrId(b));
            let ka = e_fact_key(e, AttrId(a), &v, Tick(0), TxId(1));
            let kb = e_fact_key(e, AttrId(b), &v, Tick(0), TxId(1));
            assert!(!ka.starts_with(&pb), "attr {a} key under prefix of {b}");
            assert!(!kb.starts_with(&pa), "attr {b} key under prefix of {a}");
            assert!(ka.starts_with(&pa) && kb.starts_with(&pb));
            // Even the largest key of the lower attr sorts before the other's prefix.
            let max_a = past_value(&e_value_prefix(e, AttrId(a), &v));
            assert!(max_a < pb, "attr {a} range reaches attr {b}");
        }
    }

    #[test]
    fn attr_prefix_u32_max_is_well_formed() {
        let e = ent(1, 1);
        let p = e_attr_prefix(e, AttrId(u32::MAX));
        assert_eq!(&p[9..13], &[0xFF; 4]);
        let k = e_fact_key(
            e,
            AttrId(u32::MAX),
            &venc(&Value::Bool(true)),
            Tick(5),
            TxId(9),
        );
        assert!(k.starts_with(&p));
    }

    #[test]
    fn attr_prefix_does_not_overlap_neighbouring_entities() {
        let a = AttrId(3);
        let v = venc(&s("x"));
        let pairs = [
            (ent(1, 1), ent(1, 2)),
            (ent(1, 255), ent(1, 256)),
            // last seq of a type vs first seq of the next type
            (ent(1, MAX_SEQ), ent(2, 1)),
            (ent(0, MAX_SEQ), ent(1, 1)),
            (ent(1, 1), ent(32767, MAX_SEQ)),
        ];
        for (x, y) in pairs {
            assert!(x < y);
            let px = e_attr_prefix(x, a);
            let py = e_attr_prefix(y, a);
            let kx = e_fact_key(x, a, &v, Tick(i64::MIN), TxId(0));
            let ky = e_fact_key(y, a, &v, Tick(i64::MAX), TxId(u64::MAX));
            assert!(!kx.starts_with(&py) && !ky.starts_with(&px));
            assert!(kx.starts_with(&px) && ky.starts_with(&py));
            // every possible key of x (for any attr, incl. u32::MAX) sorts before y's keys
            let max_x = past_value(&e_value_prefix(x, AttrId(u32::MAX), &v));
            assert!(max_x < py, "{x:?} range reaches {y:?}");
        }
    }

    #[test]
    fn past_value_skips_exactly_one_values_keys() {
        let e = ent(1, 1);
        let a = AttrId(2);
        // "a" is a byte-prefix of "a\0b" and "ab" before encoding; the escape
        // encoding must keep their key ranges disjoint.
        let vals = [s(""), s("a"), s("a\0b"), s("ab"), s("b"), s("\0")];
        let mut encs: Vec<Vec<u8>> = vals.iter().map(venc).collect();
        encs.sort();
        for (i, v) in encs.iter().enumerate() {
            let prefix = e_value_prefix(e, a, v);
            let past = past_value(&prefix);
            assert!(past.starts_with(&prefix));
            assert_eq!(past.len(), prefix.len() + SUFFIX_LEN);
            // extreme keys of this value are within [prefix, past]
            let lo = e_fact_key(e, a, v, Tick(i64::MAX), TxId(u64::MAX));
            let hi = e_fact_key(e, a, v, Tick(i64::MIN), TxId(0));
            assert!(prefix <= lo && lo <= hi && hi <= past);
            for (j, w) in encs.iter().enumerate() {
                if i == j {
                    continue;
                }
                let k = e_fact_key(e, a, w, Tick(0), TxId(1));
                assert!(
                    !k.starts_with(&prefix),
                    "value {j} key under value {i} prefix"
                );
                if j < i {
                    assert!(k < prefix);
                } else {
                    assert!(k > past, "next value {j} key not past value {i}");
                }
            }
        }
    }

    #[test]
    fn fact_key_is_prefix_plus_inverted_tick_and_tx() {
        let e = ent(1, 1);
        let v = venc(&Value::I64(5));
        let k = e_fact_key(e, AttrId(1), &v, Tick(-3), TxId(4));
        let p = e_value_prefix(e, AttrId(1), &v);
        assert!(k.starts_with(&p));
        assert_eq!(k.len(), p.len() + SUFFIX_LEN);
        assert_eq!(&k[p.len()..p.len() + 8], &Tick(-3).encode_inverted());
        assert_eq!(&k[p.len() + 8..], &(!4u64).to_be_bytes());
        // newer tick sorts first; newer tx at one tick sorts first
        let newer = e_fact_key(e, AttrId(1), &v, Tick(-2), TxId(1));
        assert!(newer < k);
        let newer_tx = e_fact_key(e, AttrId(1), &v, Tick(-3), TxId(5));
        assert!(newer_tx < k);
    }

    #[test]
    fn seek_key_sorts_before_all_entries_at_that_tick_and_after_later_ticks() {
        let e = ent(1, 1);
        let v = venc(&Value::Bool(true));
        let mut seek = e_value_prefix(e, AttrId(1), &v);
        push_seek_tick(&mut seek, Tick(10));
        let at10 = e_fact_key(e, AttrId(1), &v, Tick(10), TxId(u64::MAX));
        let at11 = e_fact_key(e, AttrId(1), &v, Tick(11), TxId(0));
        let at9 = e_fact_key(e, AttrId(1), &v, Tick(9), TxId(7));
        assert!(at11 < seek);
        assert!(seek <= at10);
        assert!(seek < at9);
    }

    #[test]
    fn e_value_bytes_returns_value_part() {
        let e = ent(1, 1);
        for v in [
            Value::Bool(false),
            Value::I64(i64::MIN),
            Value::F64(1.5),
            s(""),
            s("a\0b\0"),
            Value::Ref(ent(2, 3)),
        ] {
            let ve = venc(&v);
            let k = e_fact_key(e, AttrId(9), &ve, Tick(1), TxId(1));
            assert_eq!(e_value_bytes(&k).unwrap(), &ve[..]);
        }
    }

    #[test]
    fn e_value_bytes_rejects_short_key() {
        assert!(e_value_bytes(&[]).is_err());
        assert!(e_value_bytes(b"E").is_err());
        let p = e_attr_prefix(ent(1, 1), AttrId(1));
        assert_eq!(p.len(), E_ATTR_PREFIX_LEN);
        // exactly the prefix: no value at all
        assert!(e_value_bytes(&p).is_err());
        assert!(e_value_bytes(&p[..E_ATTR_PREFIX_LEN - 1]).is_err());
    }

    #[test]
    fn e_value_bytes_rejects_wrong_suffix_length() {
        let e = ent(1, 1);
        let ve = venc(&Value::I64(1));
        let k = e_fact_key(e, AttrId(1), &ve, Tick(1), TxId(1));
        // one byte short, one byte too long, missing entirely
        assert!(e_value_bytes(&k[..k.len() - 1]).is_err());
        let mut long = k.clone();
        long.push(0);
        assert!(e_value_bytes(&long).is_err());
        let no_suffix = e_value_prefix(e, AttrId(1), &ve);
        assert!(e_value_bytes(&no_suffix).is_err());
        // only ^tick, no ^tx
        let mut half = no_suffix.clone();
        push_seek_tick(&mut half, Tick(1));
        assert!(e_value_bytes(&half).is_err());
    }

    #[test]
    fn e_value_bytes_rejects_unterminated_string() {
        let e = ent(1, 1);
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x04); // str tag
        k.extend_from_slice(b"abc"); // no 0x00 0x00 terminator
        k.extend_from_slice(&[0x11; SUFFIX_LEN]);
        assert!(e_value_bytes(&k).is_err());
    }

    #[test]
    fn e_value_bytes_rejects_unknown_tag_and_truncated_fixed_value() {
        let e = ent(1, 1);
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x7F);
        k.extend_from_slice(&[0; 20]);
        assert!(e_value_bytes(&k).is_err());
        // i64 tag with only 3 payload bytes and no suffix
        let mut k = e_attr_prefix(e, AttrId(1));
        k.extend_from_slice(&[0x02, 1, 2, 3]);
        assert!(e_value_bytes(&k).is_err());
    }

    #[test]
    fn e_key_tx_round_trips_with_e_fact_key() {
        let v = venc(&Value::I64(5));
        for tx in [0u64, 1, 42, u64::MAX] {
            for tick in [i64::MIN, -1, 0, i64::MAX] {
                let k = e_fact_key(ent(1, 1), AttrId(3), &v, Tick(tick), TxId(tx));
                assert_eq!(e_key_tx(&k), Some(TxId(tx)));
            }
        }
    }

    #[test]
    fn e_key_tx_is_none_for_too_short_keys() {
        assert_eq!(e_key_tx(&[]), None);
        assert_eq!(e_key_tx(&[1, 2, 3, 4, 5, 6, 7]), None);
        // exactly 8 bytes is "long enough" to read a tx from
        assert_eq!(e_key_tx(&[0xFF; 8]), Some(TxId(0)));
    }

    #[test]
    fn e_value_and_tick_round_trips_value_and_tick() {
        let e = ent(1, 1);
        for v in [
            Value::Bool(true),
            Value::I64(-7),
            Value::F64(2.5),
            s("a\0b"),
            Value::Ref(ent(2, 3)),
        ] {
            let ve = venc(&v);
            for tick in [i64::MIN, -1, 0, i64::MAX] {
                let k = e_fact_key(e, AttrId(4), &ve, Tick(tick), TxId(9));
                let (vb, t) = e_value_and_tick(&k).unwrap();
                assert_eq!(vb, &ve[..]);
                assert_eq!(t, Tick(tick));
            }
        }
    }

    #[test]
    fn e_value_and_tick_rejects_short_key() {
        assert!(e_value_and_tick(&[]).is_err());
        assert!(e_value_and_tick(b"E").is_err());
        let p = e_attr_prefix(ent(1, 1), AttrId(1));
        assert!(e_value_and_tick(&p).is_err());
        assert!(e_value_and_tick(&p[..E_ATTR_PREFIX_LEN - 1]).is_err());
    }

    #[test]
    fn e_value_and_tick_rejects_malformed_value_part() {
        let e = ent(1, 1);
        // unknown tag
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x7F);
        k.extend_from_slice(&[0; 20]);
        assert!(e_value_and_tick(&k).is_err());
        // unterminated string
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x04);
        k.extend_from_slice(b"abc");
        k.extend_from_slice(&[0x11; SUFFIX_LEN]);
        assert!(e_value_and_tick(&k).is_err());
        // truncated fixed-width value
        let mut k = e_attr_prefix(e, AttrId(1));
        k.extend_from_slice(&[0x02, 1, 2, 3]);
        assert!(e_value_and_tick(&k).is_err());
    }

    #[test]
    fn e_value_and_tick_rejects_wrong_suffix_length() {
        let e = ent(1, 1);
        let ve = venc(&Value::I64(1));
        let k = e_fact_key(e, AttrId(1), &ve, Tick(1), TxId(1));
        assert!(e_value_and_tick(&k[..k.len() - 1]).is_err());
        let mut long = k.clone();
        long.push(0);
        assert!(e_value_and_tick(&long).is_err());
        let mut half = e_value_prefix(e, AttrId(1), &ve);
        assert!(e_value_and_tick(&half).is_err());
        push_seek_tick(&mut half, Tick(1));
        assert!(e_value_and_tick(&half).is_err());
    }
}
