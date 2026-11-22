//! Slice 6, item 0(b): `Eq`, `Hash`, `PartialOrd`, `Ord` for `Value` (RED step).
//!
//! Contract: `Ord` is a total order equal to the byte order of the value's
//! order-preserving encoding (`Value::encode`: type tag first, so values of
//! different types order by type); `a == b` exactly when the canonical
//! encodings are equal (so `F64(-0.0) == F64(0.0)`, hashing alike).
//! A hand-built `F64(NaN)` must not panic in any comparison; any consistent
//! total order is fine.
//!
//! Expected: `impl Eq + Hash + PartialOrd + Ord for Value`, with the existing
//! `PartialEq` kept consistent with the encoding equality.

use kala_core::*;
use proptest::prelude::*;
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};

fn h(v: &Value) -> u64 {
    let mut s = DefaultHasher::new();
    v.hash(&mut s);
    s.finish()
}

fn enc(v: &Value) -> Vec<u8> {
    v.encode().unwrap()
}

fn ent(ty: u16, seq: u64) -> EntityId {
    EntityId::new(NodeTypeId(ty), seq).unwrap()
}

fn s(x: &str) -> Value {
    Value::Str(x.to_owned())
}

/// Asserts the slice is strictly ascending under `Ord`.
fn strictly_ascending(vs: &[Value]) {
    for i in 0..vs.len() {
        for j in 0..vs.len() {
            let want = i.cmp(&j);
            assert_eq!(
                vs[i].cmp(&vs[j]),
                want,
                "{:?} vs {:?} (expected {:?})",
                vs[i],
                vs[j],
                want
            );
            assert_eq!(vs[i].partial_cmp(&vs[j]), Some(want));
            assert_eq!(vs[i] == vs[j], i == j);
        }
    }
}

// ---------------------------------------------------------- per-type order

#[test]
fn bool_false_before_true() {
    strictly_ascending(&[Value::Bool(false), Value::Bool(true)]);
}

#[test]
fn i64_numeric_order_with_negatives_and_extremes() {
    strictly_ascending(&[
        Value::I64(i64::MIN),
        Value::I64(i64::MIN + 1),
        Value::I64(-256),
        Value::I64(-1),
        Value::I64(0),
        Value::I64(1),
        Value::I64(255),
        Value::I64(256),
        Value::I64(i64::MAX - 1),
        Value::I64(i64::MAX),
    ]);
}

#[test]
fn f64_numeric_order_with_infinities_and_subnormals() {
    strictly_ascending(&[
        Value::F64(f64::NEG_INFINITY),
        Value::F64(f64::MIN),
        Value::F64(-1.5),
        Value::F64(-1.0),
        Value::F64(-f64::MIN_POSITIVE),
        Value::F64(-5e-324),
        Value::F64(0.0),
        Value::F64(5e-324),
        Value::F64(f64::MIN_POSITIVE),
        Value::F64(1.0),
        Value::F64(1.5),
        Value::F64(f64::MAX),
        Value::F64(f64::INFINITY),
    ]);
}

#[test]
fn negative_zero_equals_zero_and_hashes_alike() {
    let neg = Value::F64(-0.0);
    let pos = Value::F64(0.0);
    assert_eq!(neg, pos);
    assert_eq!(neg.cmp(&pos), Ordering::Equal);
    assert_eq!(neg.partial_cmp(&pos), Some(Ordering::Equal));
    assert_eq!(h(&neg), h(&pos));
    assert_eq!(enc(&neg), enc(&pos), "the canonical encoding agrees");
    // between the two neighbours, as one point
    assert!(Value::F64(-5e-324) < neg);
    assert!(neg < Value::F64(5e-324));
    let set: HashSet<Value> = [neg.clone(), pos.clone()].into_iter().collect();
    assert_eq!(set.len(), 1);
    let bt: BTreeSet<Value> = [neg, pos].into_iter().collect();
    assert_eq!(bt.len(), 1);
}

#[test]
fn value_f64_constructor_result_equals_hand_built() {
    assert_eq!(Value::f64(-0.0).unwrap(), Value::F64(0.0));
    assert_eq!(Value::f64(-0.0).unwrap(), Value::F64(-0.0));
}

#[test]
fn strings_order_by_utf8_bytes_not_by_chars_or_locale() {
    strictly_ascending(&[
        s(""),
        s("\0"),
        s("\0a"),
        s("A"),
        s("Z"),
        s("a"),
        s("ab"),
        s("b"),
        s("é"),         // U+00E9: 0xC3 0xA9
        s("\u{FF21}"),  // fullwidth A: 0xEF ...
        s("\u{1F600}"), // 4-byte: 0xF0 ...
    ]);
    // a prefix sorts first
    assert!(s("Edwin") < s("Edwin2"));
    // embedded NUL does not break prefix order
    assert!(s("a") < s("a\0"));
    assert!(s("a\0") < s("a\0\0"));
    assert!(s("a\0\u{1}") < s("a\u{1}"));
}

#[test]
fn refs_order_by_id_type_then_sequence() {
    strictly_ascending(&[
        Value::Ref(ent(0, 1)),
        Value::Ref(ent(0, 2)),
        Value::Ref(ent(0, 1 << 40)),
        Value::Ref(ent(1, 1)),
        Value::Ref(ent(1, 2)),
        Value::Ref(ent(300, 1)),
    ]);
    // equal ids are equal
    assert_eq!(Value::Ref(ent(2, 7)), Value::Ref(ent(2, 7)));
    assert_eq!(h(&Value::Ref(ent(2, 7))), h(&Value::Ref(ent(2, 7))));
}

#[test]
fn different_types_order_by_type_tag_first() {
    // The documented encoding tags order the types; read the real order from
    // the encoding and require Ord to follow it, and require it to be fixed:
    // every value of one type is below every value of a later-tagged type.
    let samples = [
        Value::Bool(true),
        Value::I64(i64::MAX),
        Value::F64(f64::INFINITY),
        s("zzz"),
        Value::Ref(ent(0, 1)),
        Value::Bool(false),
        Value::I64(i64::MIN),
        Value::F64(f64::NEG_INFINITY),
        s(""),
    ];
    for a in &samples {
        for b in &samples {
            let want = enc(a).cmp(&enc(b));
            assert_eq!(a.cmp(b), want, "{a:?} vs {b:?}");
            if a.value_type() != b.value_type() {
                assert_ne!(a.cmp(b), Ordering::Equal);
                assert_ne!(a, b);
                // type decides regardless of payload
                let (ta, tb) = (enc(a)[0], enc(b)[0]);
                assert_eq!(a.cmp(b), ta.cmp(&tb));
            }
        }
    }
    // same payload bits, different type, never equal
    assert_ne!(Value::I64(0), Value::F64(0.0));
    assert_ne!(Value::I64(1), Value::Bool(true));
    assert_ne!(s(""), Value::Bool(false));
}

// ------------------------------------------------------------- collections

#[test]
fn usable_in_btreeset_hashset_and_as_map_keys() {
    let vals = vec![
        s("b"),
        Value::I64(3),
        Value::Bool(true),
        s("a"),
        Value::I64(-3),
        Value::F64(1.5),
        Value::Ref(ent(0, 1)),
        s("a"), // duplicate
        Value::I64(3),
    ];
    let bt: BTreeSet<Value> = vals.iter().cloned().collect();
    assert_eq!(bt.len(), 7);
    let order: Vec<Vec<u8>> = bt.iter().map(enc).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(
        order, sorted,
        "BTreeSet iteration follows the encoding bytes"
    );

    let hs: HashSet<Value> = vals.iter().cloned().collect();
    assert_eq!(hs.len(), 7);
    assert!(hs.contains(&Value::I64(3)));
    assert!(!hs.contains(&Value::I64(4)));
    assert!(
        !hs.contains(&Value::F64(3.0)),
        "3 and 3.0 are different values"
    );

    let mut m: HashMap<Value, usize> = HashMap::new();
    for v in &vals {
        *m.entry(v.clone()).or_default() += 1;
    }
    assert_eq!(m[&s("a")], 2);
    assert_eq!(m[&Value::I64(3)], 2);
}

#[test]
fn vec_of_values_sorts_with_sort() {
    let mut v = [
        s("b"),
        Value::I64(2),
        Value::Bool(true),
        s("a"),
        Value::I64(-2),
    ];
    v.sort();
    let encs: Vec<_> = v.iter().map(enc).collect();
    let mut want = encs.clone();
    want.sort();
    assert_eq!(encs, want);
}

#[test]
fn min_max_and_tuples_of_values_compare() {
    assert_eq!(Value::I64(1).max(Value::I64(2)), Value::I64(2));
    assert!((ent(0, 1), Value::I64(1)) < (ent(0, 1), Value::I64(2)));
    assert!((ent(0, 1), Value::I64(9)) < (ent(0, 2), Value::I64(0)));
}

// ------------------------------------------------------------------- NaN

#[test]
fn nan_comparisons_do_not_panic_and_are_consistent() {
    let nan = Value::F64(f64::NAN);
    let others = [
        Value::F64(f64::NEG_INFINITY),
        Value::F64(0.0),
        Value::F64(f64::INFINITY),
        Value::Bool(true),
        Value::I64(0),
        s("x"),
        Value::Ref(ent(0, 1)),
        Value::F64(f64::NAN),
    ];
    // Eq must be reflexive, so a NaN equals itself (a total order).
    assert_eq!(nan.cmp(&nan), Ordering::Equal);
    assert!(nan == nan.clone());
    assert_eq!(h(&nan), h(&nan.clone()));
    for o in &others {
        let ab = nan.cmp(o);
        let ba = o.cmp(&nan);
        assert_eq!(ab, ba.reverse(), "antisymmetric against {o:?}");
        assert_eq!(nan.partial_cmp(o), Some(ab));
        assert_eq!(ab == Ordering::Equal, nan == *o);
        if nan == *o {
            assert_eq!(h(&nan), h(o));
        }
    }
    // usable (and not panicking) inside ordered collections
    let mut bt = BTreeSet::new();
    bt.insert(nan.clone());
    bt.insert(Value::F64(1.0));
    bt.insert(nan);
    assert!(bt.len() <= 2);
}

#[test]
fn nan_with_different_bit_patterns_does_not_panic() {
    let a = Value::F64(f64::from_bits(0x7ff8_0000_0000_0001));
    let b = Value::F64(f64::from_bits(0xfff8_0000_0000_0000));
    let _ = a.cmp(&b);
    let _ = b.cmp(&a);
    let _ = a == b;
    let _ = h(&a);
    assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
}

// ---------------------------------------------------------------- proptests

fn f64_strat() -> impl Strategy<Value = f64> {
    prop_oneof![
        4 => any::<f64>().prop_filter("no NaN", |x| !x.is_nan()),
        1 => Just(0.0),
        1 => Just(-0.0),
        1 => Just(f64::INFINITY),
        1 => Just(f64::NEG_INFINITY),
        1 => Just(f64::MIN_POSITIVE),
        1 => Just(5e-324),
        1 => Just(-5e-324),
    ]
}

fn value_strat() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        prop_oneof![any::<i64>(), Just(i64::MIN), Just(i64::MAX), -3i64..3].prop_map(Value::I64),
        f64_strat().prop_map(Value::F64),
        prop_oneof![
            any::<String>(),
            "[a\\x00\\x01b]{0,4}",
            "[aAéZ\u{1F600}]{0,3}"
        ]
        .prop_map(Value::Str),
        (0u16..4, 1u64..6).prop_map(|(t, q)| Value::Ref(ent(t, q))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn cmp_equals_encoding_byte_order(a in value_strat(), b in value_strat()) {
        prop_assert_eq!(a.cmp(&b), enc(&a).cmp(&enc(&b)));
        prop_assert_eq!(a.partial_cmp(&b), Some(a.cmp(&b)));
    }

    #[test]
    fn eq_exactly_when_encodings_equal_and_hash_agrees(a in value_strat(), b in value_strat()) {
        prop_assert_eq!(a == b, enc(&a) == enc(&b));
        prop_assert_eq!(a == b, a.cmp(&b) == Ordering::Equal);
        if a == b {
            prop_assert_eq!(h(&a), h(&b));
        }
        prop_assert!(a == a.clone());
        prop_assert_eq!(h(&a), h(&a.clone()));
    }

    #[test]
    fn ord_is_antisymmetric(a in value_strat(), b in value_strat()) {
        prop_assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
    }

    #[test]
    fn ord_is_transitive(a in value_strat(), b in value_strat(), c in value_strat()) {
        if a <= b && b <= c {
            prop_assert!(a <= c);
        }
        if a < b && b < c {
            prop_assert!(a < c);
        }
        // sorting any triple gives the same order as sorting encodings
        let mut v = [a, b, c];
        v.sort();
        let e: Vec<_> = v.iter().map(enc).collect();
        let mut sorted = e.clone();
        sorted.sort();
        prop_assert_eq!(e, sorted);
    }

    #[test]
    fn decode_of_encode_is_equal(a in value_strat()) {
        let back = Value::decode(&enc(&a)).unwrap();
        prop_assert_eq!(back, a);
    }

    #[test]
    fn nan_never_panics_against_anything(a in value_strat(), bits in any::<u64>()) {
        let n = Value::F64(f64::from_bits(bits | 0x7ff8_0000_0000_0000));
        prop_assert!(n.is_nan_value());
        prop_assert_eq!(n.cmp(&a), a.cmp(&n).reverse());
        prop_assert_eq!(n.cmp(&n), Ordering::Equal);
    }
}

trait IsNan {
    fn is_nan_value(&self) -> bool;
}
impl IsNan for Value {
    fn is_nan_value(&self) -> bool {
        matches!(self, Value::F64(x) if x.is_nan())
    }
}
