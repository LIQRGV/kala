use kala_bolt::packstream::{Value, decode, decode_with_limits, encode, encode_with_limits};
use proptest::prelude::*;
use std::collections::BTreeMap;

#[test]
fn round_trips_every_value_and_integer_boundary() {
    let mut values = vec![
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        Value::Float(1.5),
        Value::Float(f64::INFINITY),
        Value::String("Wren Åberg 🦉".into()),
        Value::List(vec![Value::Null, Value::Integer(-210)]),
        Value::Map(BTreeMap::from([(
            "name".into(),
            Value::String("Edwin".into()),
        )])),
        Value::Structure(
            0x4e,
            vec![
                Value::Integer(1),
                Value::List(vec![]),
                Value::Map(BTreeMap::new()),
                Value::String("CHARACTER#1".into()),
            ],
        ),
    ];
    for number in [
        i64::MIN,
        i32::MIN as i64 - 1,
        i32::MIN as i64,
        i16::MIN as i64 - 1,
        i16::MIN as i64,
        -129,
        -128,
        -17,
        -16,
        -1,
        0,
        127,
        128,
        255,
        256,
        32767,
        32768,
        i32::MAX as i64,
        i32::MAX as i64 + 1,
        i64::MAX,
    ] {
        values.push(Value::Integer(number));
    }
    for value in values {
        assert_eq!(decode(&encode(&value).unwrap()).unwrap(), value);
    }
}

#[test]
fn integer_and_utf8_encodings_match_protocol_bytes() {
    for (value, expected) in [
        (-16, vec![0xf0]),
        (-17, vec![0xc8, 0xef]),
        (127, vec![0x7f]),
        (128, vec![0xc9, 0, 0x80]),
        (32768, vec![0xca, 0, 0, 0x80, 0]),
    ] {
        assert_eq!(encode(&Value::Integer(value)).unwrap(), expected);
        assert_eq!(decode(&expected).unwrap(), Value::Integer(value));
    }
    assert_eq!(
        encode(&Value::String("é".into())).unwrap(),
        [0x82, 0xc3, 0xa9]
    );
}

#[test]
fn length_headers_are_bounded_by_remaining_payload_before_allocation() {
    for marker in [0xd2, 0xd6, 0xda] {
        assert!(decode(&[marker, 0xff, 0xff, 0xff, 0xff]).is_err());
    }
    assert!(decode(&[0xd1, 0xff, 0xff, b'x']).is_err());
    assert!(decode(&[0xa1, 0x81, b'x']).is_err());
    assert!(decode(&[0x81, 0xff]).is_err());
    assert!(
        decode(&[0xc0, 0xc0]).is_err(),
        "trailing data must not be ignored"
    );
    assert!(decode(&[0xa1, 1, 2]).is_err(), "map keys must be strings");
}

#[test]
fn depth_and_total_bytes_are_bounded_on_decode_and_encode() {
    let mut deep = vec![0x91; 34];
    deep.push(0xc0);
    assert!(decode(&deep).is_err());
    let mut value = Value::Null;
    for _ in 0..34 {
        value = Value::List(vec![value]);
    }
    assert!(encode(&value).is_err());
    assert!(decode_with_limits(&[0x82, b'a', b'b'], 2, 32).is_err());
    assert!(encode_with_limits(&Value::String("ab".into()), 2, 32).is_err());
}

#[test]
fn string_list_and_map_length_boundaries_use_the_right_headers() {
    for (len, string_marker, list_marker, map_marker) in [
        (15, 0x8f, 0x9f, 0xaf),
        (16, 0xd0, 0xd4, 0xd8),
        (255, 0xd0, 0xd4, 0xd8),
        (256, 0xd1, 0xd5, 0xd9),
        (65535, 0xd1, 0xd5, 0xd9),
        (65536, 0xd2, 0xd6, 0xda),
    ] {
        let values = [
            (Value::String("x".repeat(len)), string_marker),
            (Value::List(vec![Value::Null; len]), list_marker),
            (
                Value::Map((0..len).map(|n| (format!("{n:05}"), Value::Null)).collect()),
                map_marker,
            ),
        ];
        for (value, marker) in values {
            let encoded = encode(&value).unwrap();
            assert_eq!(encoded[0], marker, "length={len}");
            assert!(encoded.len() <= 1024 * 1024);
            assert_eq!(decode(&encoded).unwrap(), value);
        }
    }
}

fn recursive_value() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Integer),
        any::<f64>()
            .prop_filter("NaN is not reflexively equal", |n| !n.is_nan())
            .prop_map(Value::Float),
        ".{0,32}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::List),
            proptest::collection::btree_map("[a-z]{0,8}", inner.clone(), 0..8).prop_map(Value::Map),
            (any::<u8>(), proptest::collection::vec(inner, 0..8))
                .prop_map(|(tag, fields)| Value::Structure(tag, fields)),
        ]
    })
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn all_values_roundtrip(value in recursive_value()) {
        prop_assert_eq!(decode(&encode(&value).unwrap()).unwrap(), value);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode(&bytes);
    }

    #[test]
    fn integer_roundtrip(value in any::<i64>()) {
        prop_assert_eq!(decode(&encode(&Value::Integer(value)).unwrap()).unwrap(), Value::Integer(value));
    }

    #[test]
    fn string_roundtrip(value in ".{0,256}") {
        let value = Value::String(value);
        prop_assert_eq!(decode(&encode(&value).unwrap()).unwrap(), value);
    }
}
